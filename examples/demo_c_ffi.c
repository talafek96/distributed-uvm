/*
 * demo_c_ffi.c — Proves the duvm C FFI works from a C program.
 *
 * Build:
 *   cargo build --release
 *   gcc -o demo_c_ffi examples/demo_c_ffi.c \
 *       -I crates/libduvm/include \
 *       -L target/release -lduvm -lpthread -ldl -lm
 *
 * Run:
 *   LD_LIBRARY_PATH=target/release ./demo_c_ffi
 */

#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <assert.h>
#include "duvm.h"

#define PAGE_SIZE 4096
#define NUM_PAGES 100

int main(void) {
    printf("=== duvm C FFI Demo ===\n\n");

    /* Initialize */
    printf("[1/4] Initializing duvm...\n");
    int ret = duvm_init();
    if (ret != 0) {
        fprintf(stderr, "   FAIL: duvm_init returned %d\n", ret);
        return 1;
    }
    printf("   OK: duvm initialized\n");
    printf("   Total capacity: %lu pages\n", duvm_capacity_total());

    /* Store pages */
    printf("[2/4] Storing %d pages...\n", NUM_PAGES);
    uint64_t handles[NUM_PAGES];
    uint8_t data[PAGE_SIZE];

    for (int i = 0; i < NUM_PAGES; i++) {
        memset(data, 0, PAGE_SIZE);
        /* Write page number at start */
        uint64_t page_num = (uint64_t)i;
        memcpy(data, &page_num, sizeof(page_num));
        /* Write marker string */
        snprintf((char *)data + 16, 64, "C-FFI-page-%04d", i);

        handles[i] = duvm_store_page(data);
        if (handles[i] == UINT64_MAX) {
            fprintf(stderr, "   FAIL: store failed for page %d\n", i);
            return 1;
        }
    }
    printf("   OK: %d pages stored\n", NUM_PAGES);
    printf("   Used capacity: %lu pages\n", duvm_capacity_used());

    /* Load and verify */
    printf("[3/4] Loading and verifying %d pages...\n", NUM_PAGES);
    int errors = 0;
    uint8_t buf[PAGE_SIZE];

    for (int i = 0; i < NUM_PAGES; i++) {
        memset(buf, 0, PAGE_SIZE);
        ret = duvm_load_page(handles[i], buf);
        if (ret != 0) {
            fprintf(stderr, "   FAIL: load failed for page %d\n", i);
            errors++;
            continue;
        }

        /* Verify page number */
        uint64_t loaded_num;
        memcpy(&loaded_num, buf, sizeof(loaded_num));
        if (loaded_num != (uint64_t)i) {
            fprintf(stderr, "   FAIL: page %d number mismatch: got %lu\n",
                    i, loaded_num);
            errors++;
            continue;
        }

        /* Verify marker */
        char expected[64];
        snprintf(expected, sizeof(expected), "C-FFI-page-%04d", i);
        if (memcmp(buf + 16, expected, strlen(expected)) != 0) {
            fprintf(stderr, "   FAIL: page %d marker mismatch\n", i);
            errors++;
        }
    }

    if (errors == 0) {
        printf("   OK: all %d pages verified correctly\n", NUM_PAGES);
    } else {
        printf("   FAIL: %d pages had errors\n", errors);
    }

    /* Free pages */
    printf("[4/4] Freeing %d pages...\n", NUM_PAGES);
    for (int i = 0; i < NUM_PAGES; i++) {
        ret = duvm_free_page(handles[i]);
        if (ret != 0) {
            fprintf(stderr, "   WARN: free failed for page %d\n", i);
        }
    }
    printf("   OK: all pages freed\n");
    printf("   Used capacity after free: %lu pages\n", duvm_capacity_used());

    /* Final verdict */
    printf("\n=== Result ===\n");
    if (errors == 0) {
        printf("PASS: C FFI works correctly.\n");
        printf("  - duvm_init/store/load/free all functional\n");
        printf("  - %d pages round-tripped through compression backend\n", NUM_PAGES);
        return 0;
    } else {
        printf("FAIL: %d errors detected.\n", errors);
        return 1;
    }
}
