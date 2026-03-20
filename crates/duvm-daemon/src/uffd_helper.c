/*
 * uffd_helper.c — Userfaultfd operations for duvm.
 *
 * All userfaultfd operations are in C to avoid Rust variadic ioctl ABI
 * issues on aarch64.
 */

#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <linux/userfaultfd.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <pthread.h>
#include <time.h>

#define DUVM_PAGE_SIZE 4096

int duvm_uffd_create(void) {
    return syscall(SYS_userfaultfd, 0);
}

int duvm_uffd_api(int uffd) {
    struct uffdio_api api = { .api = UFFD_API };
    return ioctl(uffd, UFFDIO_API, &api);
}

int duvm_uffd_register(int uffd, unsigned long start, unsigned long len) {
    struct uffdio_register reg = {
        .range = { .start = start, .len = len },
        .mode = UFFDIO_REGISTER_MODE_MISSING
    };
    return ioctl(uffd, UFFDIO_REGISTER, &reg);
}

int duvm_uffd_copy(int uffd, unsigned long dst, const void *src, unsigned long len) {
    struct uffdio_copy cp = {
        .dst = dst, .src = (unsigned long)src, .len = len, .mode = 0
    };
    return ioctl(uffd, UFFDIO_COPY, &cp);
}

unsigned long duvm_uffd_read_fault(int uffd) {
    struct uffd_msg msg;
    if (read(uffd, &msg, sizeof(msg)) != sizeof(msg)) return 0;
    if (msg.event != 0) return 0;
    return msg.arg.pagefault.address;
}

/*
 * Callback type for page resolution.
 * Called by the handler when a fault occurs.
 * page_idx: which page (0-based index into the region)
 * out_buf: 4096-byte buffer to fill with page data
 * ctx: opaque context pointer passed from Rust
 * Returns 0 on success, -1 on error.
 */
typedef int (*duvm_page_resolver_fn)(int page_idx, char *out_buf, void *ctx);

/* Handler thread state */
struct duvm_uffd_state {
    int uffd;
    unsigned long base;
    int num_pages;
    int faults;
    duvm_page_resolver_fn resolver;
    void *resolver_ctx;
};

static void *uffd_handler_thread(void *arg) {
    struct duvm_uffd_state *s = arg;
    static char src[DUVM_PAGE_SIZE];
    for (int i = 0; i < s->num_pages; i++) {
        struct uffd_msg msg;
        if (read(s->uffd, &msg, sizeof(msg)) != sizeof(msg)) break;
        unsigned long pa = msg.arg.pagefault.address & ~(DUVM_PAGE_SIZE - 1UL);
        int idx = (pa - s->base) / DUVM_PAGE_SIZE;

        if (s->resolver) {
            s->resolver(idx, src, s->resolver_ctx);
        } else {
            memset(src, idx % 251, DUVM_PAGE_SIZE);
        }

        struct uffdio_copy cp = { .dst = pa, .src = (unsigned long)src, .len = DUVM_PAGE_SIZE };
        ioctl(s->uffd, UFFDIO_COPY, &cp);
        s->faults++;
    }
    return NULL;
}

/*
 * Run userfaultfd with a custom page resolver.
 * If resolver is NULL, uses default pattern fill.
 */
int duvm_uffd_run(int num_pages, duvm_page_resolver_fn resolver, void *ctx,
                  unsigned long *out_faults, unsigned long *out_errors,
                  unsigned long *out_elapsed_us, void **out_base) {
    int uffd = syscall(SYS_userfaultfd, 0);
    if (uffd < 0) return -1;

    struct uffdio_api api = { .api = UFFD_API };
    if (ioctl(uffd, UFFDIO_API, &api) < 0) { close(uffd); return -2; }

    unsigned long region_size = (unsigned long)num_pages * DUVM_PAGE_SIZE;
    void *base = mmap(NULL, region_size, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (base == MAP_FAILED) { close(uffd); return -3; }

    struct uffdio_register reg = {
        .range = { .start = (unsigned long)base, .len = region_size },
        .mode = UFFDIO_REGISTER_MODE_MISSING
    };
    if (ioctl(uffd, UFFDIO_REGISTER, &reg) < 0) {
        munmap(base, region_size); close(uffd); return -4;
    }

    struct duvm_uffd_state st = {
        .uffd = uffd, .base = (unsigned long)base,
        .num_pages = num_pages, .faults = 0,
        .resolver = resolver, .resolver_ctx = ctx
    };
    pthread_t t;
    pthread_create(&t, NULL, uffd_handler_thread, &st);
    usleep(50000);

    if (out_base) *out_base = base;

    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);

    /* Touch every page — triggers faults resolved by handler */
    unsigned long errors = 0;
    for (int i = 0; i < num_pages; i++) {
        volatile unsigned char *p = (volatile unsigned char *)base + (long)i * DUVM_PAGE_SIZE;
        (void)*p; /* trigger the fault */
    }

    clock_gettime(CLOCK_MONOTONIC, &t1);

    close(uffd);
    pthread_join(t, NULL);

    *out_faults = st.faults;
    *out_errors = errors;
    *out_elapsed_us = (t1.tv_sec - t0.tv_sec) * 1000000UL +
                      (t1.tv_nsec - t0.tv_nsec) / 1000UL;
    return 0;
}

/*
 * Page cache: C-side storage for pre-loaded pages.
 * Rust fills this via duvm_cache_set_page() before running uffd.
 */
static char *g_page_cache = NULL;
static int g_cache_capacity = 0;
static int g_cache_size = 0;

int duvm_cache_init(int max_pages) {
    if (g_page_cache) free(g_page_cache);
    g_page_cache = (char *)calloc(max_pages, DUVM_PAGE_SIZE);
    if (!g_page_cache) return -1;
    g_cache_capacity = max_pages;
    g_cache_size = 0;
    return 0;
}

void duvm_cache_set_page(int idx, const char *data) {
    if (g_page_cache && idx >= 0 && idx < g_cache_capacity) {
        memcpy(g_page_cache + (long)idx * DUVM_PAGE_SIZE, data, DUVM_PAGE_SIZE);
        if (idx >= g_cache_size) g_cache_size = idx + 1;
    }
}

static int cache_resolver(int idx, char *buf, void *ctx) {
    (void)ctx;
    if (g_page_cache && idx >= 0 && idx < g_cache_size) {
        memcpy(buf, g_page_cache + (long)idx * DUVM_PAGE_SIZE, DUVM_PAGE_SIZE);
    } else {
        memset(buf, 0, DUVM_PAGE_SIZE);
    }
    return 0;
}

/* Run uffd serving pages from the pre-filled C cache */
int duvm_uffd_run_cached(int num_pages, unsigned long *out_faults,
                         unsigned long *out_errors, unsigned long *out_elapsed_us) {
    return duvm_uffd_run(num_pages, cache_resolver, NULL,
                         out_faults, out_errors, out_elapsed_us, NULL);
}

/* Simple demo handler — no struct, globals, proven working pattern */
struct duvm_demo_simple {
    int uffd;
    unsigned long base;
    int num_pages;
    int faults;
};

static void *demo_handler_simple(void *arg) {
    struct duvm_demo_simple *s = arg;
    static char src[DUVM_PAGE_SIZE];
    for (int i = 0; i < s->num_pages; i++) {
        struct uffd_msg msg;
        if (read(s->uffd, &msg, sizeof(msg)) != sizeof(msg)) break;
        unsigned long pa = msg.arg.pagefault.address & ~(DUVM_PAGE_SIZE - 1UL);
        int idx = (pa - s->base) / DUVM_PAGE_SIZE;
        memset(src, idx % 251, DUVM_PAGE_SIZE);
        struct uffdio_copy cp = { .dst = pa, .src = (unsigned long)src, .len = DUVM_PAGE_SIZE };
        ioctl(s->uffd, UFFDIO_COPY, &cp);
        s->faults++;
    }
    return NULL;
}

/* Backward-compatible demo: uses simple proven handler */
int duvm_uffd_run_demo(int num_pages, unsigned long *out_faults,
                       unsigned long *out_errors, unsigned long *out_elapsed_us) {
    int uffd = syscall(SYS_userfaultfd, 0);
    if (uffd < 0) return -1;

    struct uffdio_api api = { .api = UFFD_API };
    if (ioctl(uffd, UFFDIO_API, &api) < 0) { close(uffd); return -2; }

    unsigned long region_size = (unsigned long)num_pages * DUVM_PAGE_SIZE;
    void *base = mmap(NULL, region_size, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (base == MAP_FAILED) { close(uffd); return -3; }

    struct uffdio_register reg = {
        .range = { .start = (unsigned long)base, .len = region_size },
        .mode = UFFDIO_REGISTER_MODE_MISSING
    };
    if (ioctl(uffd, UFFDIO_REGISTER, &reg) < 0) {
        munmap(base, region_size); close(uffd); return -4;
    }

    struct duvm_demo_simple st = {
        .uffd = uffd, .base = (unsigned long)base,
        .num_pages = num_pages, .faults = 0
    };
    pthread_t t;
    pthread_create(&t, NULL, demo_handler_simple, &st);
    usleep(50000);

    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);

    for (int i = 0; i < num_pages; i++) {
        volatile unsigned char *p = (volatile unsigned char *)base + (long)i * DUVM_PAGE_SIZE;
        (void)*p;
    }

    clock_gettime(CLOCK_MONOTONIC, &t1);

    close(uffd);
    pthread_join(t, NULL);
    munmap(base, region_size);

    *out_faults = st.faults;
    *out_errors = 0;
    *out_elapsed_us = (t1.tv_sec - t0.tv_sec) * 1000000UL +
                      (t1.tv_nsec - t0.tv_nsec) / 1000UL;
    return 0;
}
