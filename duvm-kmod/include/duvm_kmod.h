/* duvm_kmod.h - Internal header for duvm kernel module (block device swap target) */
#ifndef DUVM_KMOD_H
#define DUVM_KMOD_H

#include <linux/types.h>
#include <linux/module.h>
#include <linux/blkdev.h>
#include <linux/blk-mq.h>
#include <linux/miscdevice.h>
#include <linux/wait.h>
#include <linux/mm.h>

/* Constants */
#define DUVM_DEVICE_NAME    "duvm_swap"
#define DUVM_SECTOR_SIZE    512
#define DUVM_PAGE_SECTORS   (PAGE_SIZE / DUVM_SECTOR_SIZE)  /* 8 sectors per page */
#define DUVM_DEFAULT_SIZE_MB  4096    /* Default device size: 4GB */
#define DUVM_RING_ENTRIES     4096    /* Ring buffer entries (power of 2) */
#define DUVM_MAX_BATCH        64      /* Max requests per batch */

/* Operation codes (must match protocol.rs) */
#define DUVM_OP_NOP        0
#define DUVM_OP_STORE      1
#define DUVM_OP_LOAD       2
#define DUVM_OP_INVALIDATE 3

/*
 * Ring buffer request: kernel -> daemon.
 * 64 bytes, cache-line aligned.
 */
struct duvm_request {
    __u8  op;
    __u8  flags;
    __u8  _pad[2];
    __u32 seq;
    __u64 pfn;
    __u64 offset;         /* page-aligned offset into the virtual device */
    __u32 staging_slot;
    __u8  _reserved[28];
} __attribute__((packed, aligned(64)));

/*
 * Ring buffer completion: daemon -> kernel.
 * 64 bytes, cache-line aligned.
 */
struct duvm_completion {
    __u32 seq;
    __s32 result;         /* 0 = success, negative = error */
    __u64 handle;
    __u32 staging_slot;
    __u8  _reserved[40];
} __attribute__((packed, aligned(64)));

/*
 * Shared ring buffer header.
 * Mapped into daemon's address space via mmap of /dev/duvm_ctl.
 */
struct duvm_ring_header {
    __u32 req_write_idx;    /* kernel writes, daemon reads */
    __u8  _pad1[60];        /* cache line padding */
    __u32 req_read_idx;     /* daemon writes, kernel reads */
    __u8  _pad2[60];        /* cache line padding */
    __u32 comp_write_idx;   /* daemon writes, kernel reads */
    __u8  _pad3[60];        /* cache line padding */
    __u32 comp_read_idx;    /* kernel writes, daemon reads */
    __u8  _pad4[60];        /* cache line padding */
    __u32 capacity;         /* ring size (power of 2) */
    __u32 version;          /* protocol version */
    __u32 staging_pages;    /* number of staging buffer pages */
    __u32 _reserved;
} __attribute__((packed));

/* Ring buffer (kernel internal) */
struct duvm_ring {
    struct duvm_ring_header *header;
    struct duvm_request     *requests;
    struct duvm_completion  *completions;
    void                    *staging;       /* staging buffer for page data */
    unsigned long            staging_pages;
    struct page            **ring_pages;    /* backing pages for mmap */
    unsigned int             nr_ring_pages;
    size_t                   ring_size;     /* total size in bytes */
    atomic_t                 seq_counter;
    wait_queue_head_t        comp_wait;     /* kernel waits here for completions */
    wait_queue_head_t        req_wait;      /* daemon waits here for new requests (poll) */
    bool                     daemon_connected;
};

/* Per-device state */
struct duvm_device {
    struct gendisk         *disk;
    struct blk_mq_tag_set   tag_set;
    struct duvm_ring         ring;
    struct miscdevice        ctl_misc;   /* /dev/duvm_ctl for daemon mmap */
    unsigned long            size_pages; /* device size in pages */
    bool                     initialized;
};

/* ring.c */
int  duvm_ring_init(struct duvm_ring *ring, unsigned int capacity,
                    unsigned long staging_pages);
void duvm_ring_destroy(struct duvm_ring *ring);
int  duvm_ring_submit_and_wait(struct duvm_ring *ring,
                               struct duvm_request *req,
                               struct duvm_completion *comp,
                               int timeout_ms);

#endif /* DUVM_KMOD_H */
