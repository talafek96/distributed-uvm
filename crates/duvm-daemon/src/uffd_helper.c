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

/* Demo handler state */
struct duvm_demo_state {
    int uffd;
    unsigned long base;
    int num_pages;
    int faults;
};

static void *demo_handler(void *arg) {
    struct duvm_demo_state *s = arg;
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

    struct duvm_demo_state st = {
        .uffd = uffd, .base = (unsigned long)base,
        .num_pages = num_pages, .faults = 0
    };
    pthread_t t;
    pthread_create(&t, NULL, demo_handler, &st);
    usleep(50000);

    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);

    unsigned long errors = 0;
    for (int i = 0; i < num_pages; i++) {
        volatile unsigned char *p = (volatile unsigned char *)base + (long)i * DUVM_PAGE_SIZE;
        unsigned char v = *p;
        if (v != (unsigned char)(i % 251)) errors++;
        *p = 0xAB;
        *(p + DUVM_PAGE_SIZE - 1) = 0xCD;
    }

    clock_gettime(CLOCK_MONOTONIC, &t1);

    for (int i = 0; i < num_pages; i++) {
        volatile unsigned char *p = (volatile unsigned char *)base + (long)i * DUVM_PAGE_SIZE;
        if (*p != 0xAB || *(p + DUVM_PAGE_SIZE - 1) != 0xCD) errors++;
    }

    close(uffd);
    pthread_join(t, NULL);
    munmap(base, region_size);

    *out_faults = st.faults;
    *out_errors = errors;
    *out_elapsed_us = (t1.tv_sec - t0.tv_sec) * 1000000UL +
                      (t1.tv_nsec - t0.tv_nsec) / 1000UL;
    return 0;
}
