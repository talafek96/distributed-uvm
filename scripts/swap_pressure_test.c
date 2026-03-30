/*
 * scripts/swap_pressure_test.c — Safe swap pressure test for DGX Spark UMA.
 *
 * Allocates memory gradually to push pages into duvm_swap0, then verifies
 * data integrity on readback. Designed to NOT freeze the system:
 *
 *   - Drops page cache before starting (maximizes MemFree)
 *   - Monitors MemFree (not MemAvailable — MemAvailable is misleading on UMA)
 *   - Stops well before MemFree reaches danger zone (~4GB threshold)
 *   - Allocates in 256MB chunks with brief pauses for reclaim
 *   - Reports swap usage at each step
 *
 * Build: gcc -O2 -o swap_pressure_test swap_pressure_test.c
 * Run:   ./swap_pressure_test [target_swap_mb]
 *        Default target: 2048 (2GB of swap — enough to prove the path works)
 *
 * SUCCESS = pages swapped to duvm_swap0 AND verified on readback.
 * We don't need to exhaust memory — just prove the swap path works.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHUNK_SIZE (256L * 1024 * 1024)  /* 256MB per allocation */
#define MAX_CHUNKS 512                    /* 128GB max */
#define MIN_FREE_MB 4096                  /* Stop if MemFree < 4GB */
#define PAGE_SIZE 4096

static long get_meminfo_kb(const char *field)
{
    FILE *f = fopen("/proc/meminfo", "r");
    if (!f) return -1;
    char line[256];
    long val = -1;
    size_t flen = strlen(field);
    while (fgets(line, sizeof(line), f)) {
        if (strncmp(line, field, flen) == 0) {
            sscanf(line + flen, " %ld kB", &val);
            break;
        }
    }
    fclose(f);
    return val;
}

static long get_duvm_swap_kb(void)
{
    FILE *f = fopen("/proc/swaps", "r");
    if (!f) return 0;
    char line[512];
    long used = 0;
    while (fgets(line, sizeof(line), f)) {
        if (strstr(line, "duvm_swap")) {
            char name[64], type[16];
            long size;
            sscanf(line, "%s %s %ld %ld", name, type, &size, &used);
            break;
        }
    }
    fclose(f);
    return used;
}

int main(int argc, char *argv[])
{
    long target_swap_mb = argc > 1 ? atol(argv[1]) : 2048;
    char *chunks[MAX_CHUNKS] = {0};
    int n_chunks = 0;
    long chunk_bytes = CHUNK_SIZE;
    long chunk_pages = chunk_bytes / PAGE_SIZE;

    printf("=== duvm Swap Pressure Test ===\n");
    printf("Target: %ldMB of swap through duvm_swap0\n", target_swap_mb);
    printf("Safety: stop if MemFree < %dMB\n", MIN_FREE_MB);
    printf("Chunk size: %ldMB\n\n", chunk_bytes / (1024*1024));

    /* Step 1: Drop page cache to maximize MemFree */
    printf("[1/4] Dropping page cache...\n");
    sync();
    FILE *dc = fopen("/proc/sys/vm/drop_caches", "w");
    if (dc) { fprintf(dc, "3"); fclose(dc); printf("  Done.\n"); }
    else { printf("  Skipped (no permissions). Run as root for best results.\n"); }

    long free_mb = get_meminfo_kb("MemFree:") / 1024;
    long avail_mb = get_meminfo_kb("MemAvailable:") / 1024;
    printf("  MemFree: %ldMB, MemAvailable: %ldMB\n\n", free_mb, avail_mb);

    /* Step 2: Allocate chunks until swap target reached or safety limit hit */
    printf("[2/4] Allocating memory to force swap...\n");
    long prev_swap = get_duvm_swap_kb();

    for (int i = 0; i < MAX_CHUNKS; i++) {
        free_mb = get_meminfo_kb("MemFree:") / 1024;
        long swap_kb = get_duvm_swap_kb();
        long swap_mb = swap_kb / 1024;

        /* Safety check: MemFree */
        if (free_mb < MIN_FREE_MB) {
            printf("  Stop: MemFree=%ldMB < %dMB safety limit\n", free_mb, MIN_FREE_MB);
            break;
        }

        /* Target reached? */
        if (swap_mb >= target_swap_mb) {
            printf("  Target reached: %ldMB swapped to duvm_swap0\n", swap_mb);
            break;
        }

        chunks[i] = malloc(chunk_bytes);
        if (!chunks[i]) {
            printf("  Stop: malloc failed at chunk %d\n", i);
            break;
        }

        /* Touch every page with a verifiable pattern */
        for (long j = 0; j < chunk_bytes; j += PAGE_SIZE) {
            chunks[i][j] = (char)((i * 1024 + j / PAGE_SIZE) & 0xFF);
        }
        n_chunks = i + 1;

        swap_kb = get_duvm_swap_kb();
        printf("  [%3d] +256MB = %5ldMB total, MemFree=%5ldMB, duvm_swap=%ldKB",
               n_chunks, (long)n_chunks * 256, free_mb, swap_kb);
        if (swap_kb > prev_swap) printf(" (+%ldKB)", swap_kb - prev_swap);
        if (swap_kb > 0 && prev_swap == 0) printf(" <<< SWAPPING STARTED >>>");
        printf("\n");
        prev_swap = swap_kb;

        /* Brief pause every 4 chunks to let reclaim breathe */
        if (n_chunks % 4 == 0) usleep(100000); /* 100ms */
    }

    long final_swap_kb = get_duvm_swap_kb();
    printf("\n  Allocated: %dMB in %d chunks\n", n_chunks * 256, n_chunks);
    printf("  duvm_swap0 used: %ldKB (%ldMB)\n\n", final_swap_kb, final_swap_kb / 1024);

    if (final_swap_kb == 0) {
        printf("[3/4] No pages swapped — not enough pressure. Try more chunks.\n");
        printf("VERDICT: SKIP (no swap triggered)\n");
        for (int i = 0; i < n_chunks; i++) free(chunks[i]);
        return 0;
    }

    /* Step 3: Verify data integrity (forces swap-in for swapped pages) */
    printf("[3/4] Verifying data integrity (%d chunks, %ld pages)...\n",
           n_chunks, (long)n_chunks * chunk_pages);
    printf("  (This triggers swap-in for pages that were swapped out)\n");

    long errors = 0;
    long pages_checked = 0;
    for (int i = 0; i < n_chunks; i++) {
        for (long j = 0; j < chunk_bytes; j += PAGE_SIZE) {
            char expected = (char)((i * 1024 + j / PAGE_SIZE) & 0xFF);
            if (chunks[i][j] != expected) errors++;
            pages_checked++;
        }
        /* Brief pause every 16 chunks during verify too */
        if ((i + 1) % 16 == 0) usleep(50000);
    }

    printf("  Checked %ld pages, errors: %ld\n\n", pages_checked, errors);

    /* Step 4: Summary */
    long post_swap_kb = get_duvm_swap_kb();
    printf("[4/4] Summary:\n");
    printf("  Memory allocated:    %dMB (%d chunks)\n", n_chunks * 256, n_chunks);
    printf("  Peak swap used:      %ldMB\n", final_swap_kb / 1024);
    printf("  Post-verify swap:    %ldMB\n", post_swap_kb / 1024);
    printf("  Data integrity:      %ld/%ld pages OK\n",
           pages_checked - errors, pages_checked);
    printf("\n");

    for (int i = 0; i < n_chunks; i++) free(chunks[i]);

    if (errors == 0 && final_swap_kb > 0) {
        printf("VERDICT: PASS\n");
        printf("  Pages successfully swapped to duvm_swap0 and verified on readback.\n");
        printf("  The full-stack path works: kmod -> daemon -> RDMA -> memserver.\n");
        return 0;
    } else if (errors > 0) {
        printf("VERDICT: FAIL (%ld data integrity errors)\n", errors);
        return 1;
    } else {
        printf("VERDICT: SKIP (no swap triggered)\n");
        return 0;
    }
}
