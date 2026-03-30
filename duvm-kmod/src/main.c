/*
 * duvm-kmod: Virtual block device for distributed swap.
 *
 * Creates /dev/duvm_swap0 — a virtual block device that can be used as
 * a Linux swap target. When the kernel swaps pages out, they go through
 * this device to the duvm-daemon via a shared ring buffer. The daemon
 * stores them on remote nodes.
 *
 * Also creates /dev/duvm_ctl — a control device that the daemon mmaps
 * to access the shared ring buffer and staging area.
 *
 * ASYNC I/O MODEL (critical for DGX Spark UMA):
 *   queue_rq() submits to the ring buffer and returns IMMEDIATELY.
 *   A completion harvester thread polls for daemon responses and calls
 *   blk_mq_end_request(). This prevents blocking in the kernel's memory
 *   reclaim path, which would freeze UMA systems where GPU and CPU share
 *   the same memory pool.
 *
 * Usage:
 *   sudo insmod duvm-kmod.ko size_mb=4096
 *   sudo mkswap /dev/duvm_swap0
 *   sudo swapon -p 100 /dev/duvm_swap0
 *   # Now start duvm-daemon, which mmaps /dev/duvm_ctl
 */

#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/blkdev.h>
#include <linux/blk-mq.h>
#include <linux/mm.h>
#include <linux/slab.h>
#include <linux/vmalloc.h>
#include <linux/miscdevice.h>
#include <linux/fs.h>
#include <linux/poll.h>
#include <linux/kthread.h>
#include <linux/delay.h>

#include "duvm_kmod.h"

MODULE_LICENSE("GPL");
MODULE_AUTHOR("duvm project");
MODULE_DESCRIPTION("Distributed UVM - virtual block device swap target");
MODULE_VERSION("0.3.0");

/* Module parameters */
static unsigned int size_mb = DUVM_DEFAULT_SIZE_MB;
module_param(size_mb, uint, 0444);
MODULE_PARM_DESC(size_mb, "Virtual device size in MB (default 4096)");

static unsigned int ring_entries = DUVM_RING_ENTRIES;
module_param(ring_entries, uint, 0444);
MODULE_PARM_DESC(ring_entries, "Ring buffer entries, power of 2 (default 4096)");

/* Global device state */
static struct duvm_device duvm_dev;
static int duvm_major;

/*
 * Fallback: in-memory page storage for when daemon is not connected.
 * Pages are stored in a simple radix tree keyed by sector offset.
 * This allows the swap device to work standalone for testing.
 */
static DEFINE_XARRAY(duvm_page_store);

/* ── Staging slot allocator ─────────────────────────────────────── */

static int duvm_alloc_staging_slot(struct duvm_ring *ring)
{
    unsigned long bit;
    unsigned long flags;

    spin_lock_irqsave(&ring->staging_lock, flags);
    bit = find_first_zero_bit(ring->staging_bitmap, ring->staging_pages);
    if (bit >= ring->staging_pages) {
        spin_unlock_irqrestore(&ring->staging_lock, flags);
        return -1;  /* no free slots */
    }
    set_bit(bit, ring->staging_bitmap);
    spin_unlock_irqrestore(&ring->staging_lock, flags);
    return (int)bit;
}

static void duvm_free_staging_slot(struct duvm_ring *ring, unsigned int slot)
{
    unsigned long flags;

    spin_lock_irqsave(&ring->staging_lock, flags);
    clear_bit(slot, ring->staging_bitmap);
    spin_unlock_irqrestore(&ring->staging_lock, flags);
}

/* ── Completion harvester thread ────────────────────────────────── */

/*
 * This thread runs continuously while the daemon is connected.
 * It polls the completion ring for responses from the daemon,
 * matches each completion to its in-flight request (via seq → blk-mq tag),
 * performs the staging→bio copy for LOAD requests, and calls
 * blk_mq_end_request() to complete the I/O.
 *
 * This is the key to non-blocking I/O: queue_rq() never waits.
 */
static int duvm_completion_thread(void *data)
{
    struct duvm_device *dev = data;
    struct duvm_ring *ring = &dev->ring;

    pr_info("duvm: completion harvester thread started\n");

    while (!kthread_should_stop()) {
        struct duvm_completion comp;
        int drained = 0;

        /* Drain all available completions */
        while (duvm_ring_poll_completion(ring, &comp) == 0) {
            struct request *rq;
            struct duvm_cmd *cmd;
            int tag;

            /*
             * Map seq → blk-mq tag → request.
             * We store (tag + 1) as seq so seq is never 0.
             */
            tag = (int)comp.seq - 1;
            if (tag < 0 || tag >= (int)dev->tag_set.queue_depth) {
                pr_warn_ratelimited("duvm: bad completion seq=%u\n",
                                     comp.seq);
                continue;
            }

            rq = blk_mq_tag_to_rq(dev->tag_set.tags[0], tag);
            if (!rq) {
                pr_warn_ratelimited("duvm: no request for tag=%d\n",
                                     tag);
                continue;
            }

            cmd = blk_mq_rq_to_pdu(rq);

            /* For LOAD requests, copy data from staging to bio pages */
            if (cmd->is_read && comp.result == 0) {
                struct bio_vec bvec;
                struct req_iterator iter;
                void *staging_page = (char *)ring->staging +
                                     (cmd->staging_slot * PAGE_SIZE);

                rq_for_each_segment(bvec, rq, iter) {
                    void *dst = kmap_local_page(bvec.bv_page) +
                                bvec.bv_offset;
                    memcpy(dst, staging_page, bvec.bv_len);
                    kunmap_local(dst);
                    staging_page += bvec.bv_len;
                }
            }

            /* Free the staging slot */
            duvm_free_staging_slot(ring, cmd->staging_slot);

            /* Complete the I/O */
            if (comp.result == 0) {
                blk_mq_end_request(rq, BLK_STS_OK);
            } else {
                pr_warn_ratelimited("duvm: daemon error (%d) for %s seq=%u\n",
                                     comp.result,
                                     cmd->is_read ? "LOAD" : "STORE",
                                     comp.seq);
                blk_mq_end_request(rq, BLK_STS_IOERR);
            }
            drained++;
        }

        if (drained == 0) {
            /*
             * No completions available. Wait for daemon to write one.
             * Use a short timeout so we can check kthread_should_stop().
             */
            wait_event_timeout(ring->comp_wait,
                READ_ONCE(ring->header->comp_write_idx) !=
                ring->header->comp_read_idx ||
                kthread_should_stop(),
                msecs_to_jiffies(100));
        }
    }

    pr_info("duvm: completion harvester thread stopped\n");
    return 0;
}

/* ── blk-mq queue_rq — ASYNC (non-blocking) ────────────────────── */

/*
 * blk-mq queue_rq callback: processes block I/O requests.
 *
 * When daemon is connected: copies data to staging (for STORE),
 * submits to the ring buffer, and returns IMMEDIATELY. The completion
 * harvester thread will call blk_mq_end_request() when the daemon
 * responds.
 *
 * When daemon is not connected: uses local xarray page store as fallback
 * (synchronous, for testing without a daemon).
 */
static blk_status_t duvm_queue_rq(struct blk_mq_hw_ctx *hctx,
                                   const struct blk_mq_queue_data *bd)
{
    struct request *rq = bd->rq;
    struct duvm_cmd *cmd = blk_mq_rq_to_pdu(rq);
    sector_t sector = blk_rq_pos(rq);
    int is_write = (rq_data_dir(rq) == WRITE);

    /*
     * Async path: if daemon is connected, submit to ring and return.
     * Only handles single-page requests (swap I/O is always page-aligned).
     */
    if (duvm_dev.ring.daemon_connected) {
        struct bio_vec bvec;
        struct req_iterator iter;
        unsigned long idx;
        int slot;
        struct duvm_request req = {};
        void *staging_page;
        int ret;

        /* Allocate a staging slot */
        slot = duvm_alloc_staging_slot(&duvm_dev.ring);
        if (slot < 0) {
            /* No staging slots — tell blk-mq to retry later */
            return BLK_STS_RESOURCE;
        }

        staging_page = (char *)duvm_dev.ring.staging + (slot * PAGE_SIZE);
        idx = sector / DUVM_PAGE_SECTORS;

        if (is_write) {
            /* STORE: copy page data to staging before submitting */
            rq_for_each_segment(bvec, rq, iter) {
                void *src = kmap_local_page(bvec.bv_page) + bvec.bv_offset;
                memcpy(staging_page, src, bvec.bv_len);
                kunmap_local(src);
                staging_page += bvec.bv_len;
            }
            req.op = DUVM_OP_STORE;
        } else {
            req.op = DUVM_OP_LOAD;
        }

        req.offset = idx;
        req.staging_slot = slot;
        /*
         * Use blk-mq tag + 1 as seq (so seq is never 0).
         * The completion thread maps seq back to the request via
         * blk_mq_tag_to_rq().
         */
        req.seq = rq->tag + 1;

        /* Store command metadata for the completion thread */
        cmd->seq = req.seq;
        cmd->staging_slot = slot;
        cmd->is_read = !is_write;

        blk_mq_start_request(rq);

        ret = duvm_ring_submit(&duvm_dev.ring, &req);
        if (ret) {
            duvm_free_staging_slot(&duvm_dev.ring, slot);
            if (ret == -EAGAIN)
                return BLK_STS_RESOURCE;
            blk_mq_end_request(rq, BLK_STS_IOERR);
            return BLK_STS_OK;
        }

        /* Request is now in-flight. Completion thread will finish it. */
        return BLK_STS_OK;
    }

    /* Slow path: no daemon — use local xarray storage (synchronous) */
    blk_mq_start_request(rq);

    if (is_write) {
        sector_t cur_sector = sector;
        struct bio_vec bvec;
        struct req_iterator iter;

        rq_for_each_segment(bvec, rq, iter) {
            void *src = kmap_local_page(bvec.bv_page) + bvec.bv_offset;
            unsigned long idx = cur_sector / DUVM_PAGE_SECTORS;
            struct page *stored;

            stored = xa_load(&duvm_page_store, idx);
            if (!stored) {
                stored = alloc_page(GFP_NOIO);
                if (!stored) {
                    kunmap_local(src);
                    blk_mq_end_request(rq, BLK_STS_RESOURCE);
                    return BLK_STS_OK;
                }
                xa_store(&duvm_page_store, idx, stored, GFP_NOIO);
            }

            memcpy(page_address(stored) + (bvec.bv_offset % PAGE_SIZE),
                   src, bvec.bv_len);
            kunmap_local(src);

            cur_sector += bvec.bv_len / DUVM_SECTOR_SIZE;
        }
    } else {
        sector_t cur_sector = sector;
        struct bio_vec bvec;
        struct req_iterator iter;

        rq_for_each_segment(bvec, rq, iter) {
            void *dst = kmap_local_page(bvec.bv_page) + bvec.bv_offset;
            unsigned long idx = cur_sector / DUVM_PAGE_SECTORS;
            struct page *stored;

            stored = xa_load(&duvm_page_store, idx);
            if (stored) {
                memcpy(dst,
                       page_address(stored) + (bvec.bv_offset % PAGE_SIZE),
                       bvec.bv_len);
            } else {
                memset(dst, 0, bvec.bv_len);
            }
            kunmap_local(dst);

            cur_sector += bvec.bv_len / DUVM_SECTOR_SIZE;
        }
    }

    blk_mq_end_request(rq, BLK_STS_OK);
    return BLK_STS_OK;
}

static const struct blk_mq_ops duvm_mq_ops = {
    .queue_rq = duvm_queue_rq,
};

/*
 * Block device operations for /dev/duvm_swap0.
 * Minimal — the actual I/O is handled by blk-mq queue_rq callback.
 */
static const struct block_device_operations duvm_bdev_fops = {
    .owner = THIS_MODULE,
};

/*
 * Control device (/dev/duvm_ctl) file operations.
 * The daemon opens this to mmap the ring buffer.
 */
static int duvm_ctl_open(struct inode *inode, struct file *filp)
{
    if (duvm_dev.ring.daemon_connected) {
        pr_warn("duvm: daemon already connected\n");
        return -EBUSY;
    }
    duvm_dev.ring.daemon_connected = true;

    /* Start the completion harvester thread */
    duvm_dev.comp_thread = kthread_run(duvm_completion_thread, &duvm_dev,
                                        "duvm_comp");
    if (IS_ERR(duvm_dev.comp_thread)) {
        pr_err("duvm: failed to start completion thread\n");
        duvm_dev.ring.daemon_connected = false;
        return PTR_ERR(duvm_dev.comp_thread);
    }

    pr_info("duvm: daemon connected via /dev/duvm_ctl\n");
    return 0;
}

static int duvm_ctl_release(struct inode *inode, struct file *filp)
{
    duvm_dev.ring.daemon_connected = false;

    /* Stop the completion thread */
    if (duvm_dev.comp_thread) {
        kthread_stop(duvm_dev.comp_thread);
        duvm_dev.comp_thread = NULL;
    }

    wake_up_all(&duvm_dev.ring.comp_wait);
    pr_info("duvm: daemon disconnected\n");
    return 0;
}

static int duvm_ctl_mmap(struct file *filp, struct vm_area_struct *vma)
{
    unsigned long size = vma->vm_end - vma->vm_start;
    unsigned int i;
    unsigned long addr = vma->vm_start;

    if (size > duvm_dev.ring.ring_size) {
        pr_err("duvm: mmap size %lu exceeds ring size %zu\n",
               size, duvm_dev.ring.ring_size);
        return -EINVAL;
    }

    /* Map each ring page into user space */
    for (i = 0; i < duvm_dev.ring.nr_ring_pages && addr < vma->vm_end; i++) {
        if (vm_insert_page(vma, addr, duvm_dev.ring.ring_pages[i])) {
            pr_err("duvm: vm_insert_page failed at page %u\n", i);
            return -EAGAIN;
        }
        addr += PAGE_SIZE;
    }

    pr_info("duvm: ring buffer mapped to daemon (%u pages)\n",
            duvm_dev.ring.nr_ring_pages);
    return 0;
}

/*
 * poll() on /dev/duvm_ctl: returns POLLIN when there are pending requests.
 * The daemon uses epoll/poll on this fd instead of busy-polling the ring.
 */
static __poll_t duvm_ctl_poll(struct file *filp, poll_table *wait)
{
    __poll_t mask = 0;

    poll_wait(filp, &duvm_dev.ring.req_wait, wait);

    if (READ_ONCE(duvm_dev.ring.header->req_write_idx) !=
        READ_ONCE(duvm_dev.ring.header->req_read_idx)) {
        mask |= EPOLLIN | EPOLLRDNORM;
    }

    return mask;
}

/*
 * write() on /dev/duvm_ctl: daemon signals that completions are available.
 * This wakes the completion harvester thread immediately instead of waiting
 * for its 100ms poll timeout.
 */
static ssize_t duvm_ctl_write(struct file *filp, const char __user *buf,
                               size_t count, loff_t *ppos)
{
    wake_up(&duvm_dev.ring.comp_wait);
    return count;
}

static const struct file_operations duvm_ctl_fops = {
    .owner   = THIS_MODULE,
    .open    = duvm_ctl_open,
    .release = duvm_ctl_release,
    .mmap    = duvm_ctl_mmap,
    .poll    = duvm_ctl_poll,
    .write   = duvm_ctl_write,
};

/*
 * Module initialization
 */
static int __init duvm_init(void)
{
    struct queue_limits lim = { };
    int ret;

    pr_info("duvm: initializing (size=%u MB, ring=%u entries)\n",
            size_mb, ring_entries);

    if (!is_power_of_2(ring_entries)) {
        pr_err("duvm: ring_entries must be power of 2\n");
        return -EINVAL;
    }

    memset(&duvm_dev, 0, sizeof(duvm_dev));
    duvm_dev.size_pages = (unsigned long)size_mb * 256; /* MB to 4K pages */

    /* Register block device major number */
    duvm_major = register_blkdev(0, DUVM_DEVICE_NAME);
    if (duvm_major < 0) {
        pr_err("duvm: register_blkdev failed: %d\n", duvm_major);
        return duvm_major;
    }
    pr_info("duvm: registered block device major=%d\n", duvm_major);

    /* Initialize ring buffer */
    ret = duvm_ring_init(&duvm_dev.ring, ring_entries, ring_entries);
    if (ret) {
        pr_err("duvm: ring init failed: %d\n", ret);
        goto err_blkdev;
    }

    /* Set up blk-mq tag set */
    duvm_dev.tag_set.ops = &duvm_mq_ops;
    duvm_dev.tag_set.nr_hw_queues = 1;
    duvm_dev.tag_set.queue_depth = ring_entries;  /* match ring capacity */
    duvm_dev.tag_set.numa_node = NUMA_NO_NODE;
    duvm_dev.tag_set.cmd_size = sizeof(struct duvm_cmd);
    duvm_dev.tag_set.flags = 0;

    ret = blk_mq_alloc_tag_set(&duvm_dev.tag_set);
    if (ret) {
        pr_err("duvm: tag set alloc failed: %d\n", ret);
        goto err_ring;
    }

    /* Allocate gendisk with blk-mq */
    lim.logical_block_size = DUVM_SECTOR_SIZE;
    lim.physical_block_size = PAGE_SIZE;
    lim.max_hw_sectors = PAGE_SIZE / DUVM_SECTOR_SIZE * 32;

    duvm_dev.disk = blk_mq_alloc_disk(&duvm_dev.tag_set, &lim, &duvm_dev);
    if (IS_ERR(duvm_dev.disk)) {
        ret = PTR_ERR(duvm_dev.disk);
        pr_err("duvm: disk alloc failed: %d\n", ret);
        goto err_tag_set;
    }

    pr_info("duvm: blk_mq_alloc_disk succeeded, configuring...\n");

    /* Configure disk */
    duvm_dev.disk->major = duvm_major;
    duvm_dev.disk->first_minor = 0;
    duvm_dev.disk->minors = 1;
    duvm_dev.disk->fops = &duvm_bdev_fops;
    snprintf(duvm_dev.disk->disk_name, DISK_NAME_LEN, "%s0", DUVM_DEVICE_NAME);
    set_capacity(duvm_dev.disk,
                 duvm_dev.size_pages * DUVM_PAGE_SECTORS);

    pr_info("duvm: calling add_disk (capacity=%lu sectors)...\n",
            duvm_dev.size_pages * DUVM_PAGE_SECTORS);

    /* Register disk */
    ret = add_disk(duvm_dev.disk);
    if (ret) {
        pr_err("duvm: add_disk failed: %d\n", ret);
        goto err_disk;
    }

    /* Register control device for daemon communication */
    duvm_dev.ctl_misc.minor = MISC_DYNAMIC_MINOR;
    duvm_dev.ctl_misc.name = "duvm_ctl";
    duvm_dev.ctl_misc.fops = &duvm_ctl_fops;

    ret = misc_register(&duvm_dev.ctl_misc);
    if (ret) {
        pr_err("duvm: misc_register failed: %d\n", ret);
        goto err_del_disk;
    }

    duvm_dev.initialized = true;
    pr_info("duvm: /dev/%s0 created (%u MB, %lu pages) [ASYNC I/O]\n",
            DUVM_DEVICE_NAME, size_mb, duvm_dev.size_pages);
    pr_info("duvm: /dev/duvm_ctl created for daemon communication\n");
    pr_info("duvm: Use: mkswap /dev/%s0 && swapon -p 100 /dev/%s0\n",
            DUVM_DEVICE_NAME, DUVM_DEVICE_NAME);
    return 0;

err_del_disk:
    del_gendisk(duvm_dev.disk);
err_disk:
    put_disk(duvm_dev.disk);
err_tag_set:
    blk_mq_free_tag_set(&duvm_dev.tag_set);
err_ring:
    duvm_ring_destroy(&duvm_dev.ring);
err_blkdev:
    unregister_blkdev(duvm_major, DUVM_DEVICE_NAME);
    return ret;
}

static void __exit duvm_exit(void)
{
    unsigned long idx;
    struct page *page;

    pr_info("duvm: shutting down\n");

    /* Stop completion thread if running */
    if (duvm_dev.comp_thread) {
        kthread_stop(duvm_dev.comp_thread);
        duvm_dev.comp_thread = NULL;
    }

    misc_deregister(&duvm_dev.ctl_misc);
    del_gendisk(duvm_dev.disk);
    put_disk(duvm_dev.disk);
    blk_mq_free_tag_set(&duvm_dev.tag_set);
    duvm_ring_destroy(&duvm_dev.ring);
    unregister_blkdev(duvm_major, DUVM_DEVICE_NAME);

    /* Free all stored pages */
    xa_for_each(&duvm_page_store, idx, page) {
        __free_page(page);
    }
    xa_destroy(&duvm_page_store);

    duvm_dev.initialized = false;
    pr_info("duvm: unloaded\n");
}

module_init(duvm_init);
module_exit(duvm_exit);
