/* duvm_kmod.h - Internal header for duvm kernel module */
#ifndef DUVM_KMOD_H
#define DUVM_KMOD_H

#include <linux/types.h>
#include <linux/module.h>
#include <linux/fs.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>

/* Ring buffer constants */
#define DUVM_RING_SIZE_DEFAULT  4096    /* entries (power of 2) */
#define DUVM_STAGING_PAGES      8192    /* staging buffer pages */
#define DUVM_TIMEOUT_MS         100     /* daemon response timeout */
#define DUVM_DEVICE_NAME        "duvm0"
#define DUVM_MAX_BATCH          64      /* max requests per batch */

/* Operation codes (must match duvm-common/src/protocol.rs) */
#define DUVM_OP_NOP        0
#define DUVM_OP_STORE      1
#define DUVM_OP_LOAD       2
#define DUVM_OP_INVALIDATE 3
#define DUVM_OP_PREFETCH   4

/*
 * Ring buffer request: kernel -> daemon.
 * 64 bytes, cache-line aligned.
 * Layout must match RingRequest in protocol.rs exactly.
 */
struct duvm_request {
    __u8  op;
    __u8  flags;
    __u8  _pad[2];
    __u32 seq;
    __u64 pfn;
    __u64 offset;
    __u32 staging_slot;
    __u8  _reserved[28];
} __attribute__((packed, aligned(64)));

/*
 * Ring buffer completion: daemon -> kernel.
 * 64 bytes, cache-line aligned.
 * Layout must match RingCompletion in protocol.rs exactly.
 */
struct duvm_completion {
    __u32 seq;
    __s32 result;
    __u64 handle;
    __u32 staging_slot;
    __u8  _reserved[40];
} __attribute__((packed, aligned(64)));

/*
 * Shared ring buffer header.
 * Both kernel and user-space access this via mmap.
 */
struct duvm_ring_header {
    __u32 write_idx;        /* kernel writes, daemon reads */
    __u8  _pad1[60];        /* cache line padding */
    __u32 read_idx;         /* daemon writes, kernel reads */
    __u8  _pad2[60];        /* cache line padding */
    __u32 capacity;         /* ring size (power of 2) */
    __u32 version;          /* protocol version */
} __attribute__((packed));

/* Ring buffer state (kernel-internal) */
struct duvm_ring {
    struct duvm_ring_header *header;
    struct duvm_request     *requests;
    struct duvm_completion  *completions;
    void                    *staging;    /* staging buffer for page data */
    unsigned long            staging_pages;
    struct page             *ring_page;  /* backing page for mmap */
    size_t                   ring_size;  /* total mmap size in bytes */
    atomic_t                 seq_counter;
    wait_queue_head_t        wait_queue;
    bool                     daemon_connected;
};

/* Module-global state */
struct duvm_state {
    struct duvm_ring    ring;
    struct miscdevice   misc;
    bool                initialized;
};

/* ring.c */
int  duvm_ring_init(struct duvm_ring *ring, unsigned int capacity,
                    unsigned long staging_pages);
void duvm_ring_destroy(struct duvm_ring *ring);
int  duvm_ring_submit(struct duvm_ring *ring, struct duvm_request *req);
int  duvm_ring_wait_completion(struct duvm_ring *ring, __u32 seq,
                               struct duvm_completion *comp, int timeout_ms);

/* swap.c */
int  duvm_swap_init(void);
void duvm_swap_cleanup(void);

#endif /* DUVM_KMOD_H */
