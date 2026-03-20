/*
 * duvm-kmod: Thin kernel module for distributed UVM page interception.
 *
 * This module provides a /dev/duvm0 character device that the duvm-daemon
 * mmaps to establish a shared ring buffer. The module intercepts page
 * swap events and relays them to user-space via this ring buffer.
 *
 * Design philosophy: The module is a thin relay. It intercepts kernel memory
 * events and shuttles them to user-space. It does NOT make policy decisions,
 * manage connections, or implement transport protocols.
 */

#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>
#include <linux/fs.h>
#include <linux/mm.h>
#include <linux/slab.h>
#include <linux/miscdevice.h>
#include <linux/uaccess.h>

#include "duvm_kmod.h"

MODULE_LICENSE("GPL");
MODULE_AUTHOR("duvm project");
MODULE_DESCRIPTION("Distributed UVM - thin kernel relay for page interception");
MODULE_VERSION("0.1.0");

/* Module parameters */
static unsigned int ring_size = DUVM_RING_SIZE_DEFAULT;
module_param(ring_size, uint, 0444);
MODULE_PARM_DESC(ring_size, "Ring buffer entries (power of 2, default 4096)");

static unsigned long staging_pages = DUVM_STAGING_PAGES;
module_param(staging_pages, ulong, 0444);
MODULE_PARM_DESC(staging_pages, "Staging buffer size in pages (default 8192)");

/* Global state */
static struct duvm_state duvm;

/*
 * File operations for /dev/duvm0
 */

static int duvm_open(struct inode *inode, struct file *filp)
{
    if (duvm.ring.daemon_connected) {
        pr_warn("duvm: daemon already connected\n");
        return -EBUSY;
    }
    duvm.ring.daemon_connected = true;
    pr_info("duvm: daemon connected\n");
    return 0;
}

static int duvm_release(struct inode *inode, struct file *filp)
{
    duvm.ring.daemon_connected = false;
    wake_up_all(&duvm.ring.wait_queue);
    pr_info("duvm: daemon disconnected, entering fallback mode\n");
    return 0;
}

/*
 * mmap: Map the ring buffer + staging area into user-space (daemon).
 *
 * Layout of the mapped region:
 *   [0 .. header_size)        = ring header
 *   [header .. +req_size)     = request ring entries
 *   [.. +comp_size)           = completion ring entries
 *   [.. +staging_size)        = staging buffer (page data)
 */
static int duvm_mmap(struct file *filp, struct vm_area_struct *vma)
{
    unsigned long size = vma->vm_end - vma->vm_start;

    if (size != duvm.ring.ring_size) {
        pr_err("duvm: mmap size mismatch: expected %zu, got %lu\n",
               duvm.ring.ring_size, size);
        return -EINVAL;
    }

    /* Map the pre-allocated ring buffer memory to user-space */
    if (remap_pfn_range(vma, vma->vm_start,
                        page_to_pfn(duvm.ring.ring_page),
                        size, vma->vm_page_prot)) {
        pr_err("duvm: remap_pfn_range failed\n");
        return -EAGAIN;
    }

    pr_info("duvm: ring buffer mapped to user-space (size=%zu)\n",
            duvm.ring.ring_size);
    return 0;
}

static long duvm_ioctl(struct file *filp, unsigned int cmd, unsigned long arg)
{
    /* Reserved for future control commands */
    return -ENOTTY;
}

static const struct file_operations duvm_fops = {
    .owner          = THIS_MODULE,
    .open           = duvm_open,
    .release        = duvm_release,
    .mmap           = duvm_mmap,
    .unlocked_ioctl = duvm_ioctl,
};

/*
 * Module init/exit
 */

static int __init duvm_init(void)
{
    int ret;

    pr_info("duvm: initializing (ring_size=%u, staging_pages=%lu)\n",
            ring_size, staging_pages);

    /* Validate parameters */
    if (!is_power_of_2(ring_size)) {
        pr_err("duvm: ring_size must be power of 2\n");
        return -EINVAL;
    }

    /* Initialize ring buffer */
    ret = duvm_ring_init(&duvm.ring, ring_size, staging_pages);
    if (ret) {
        pr_err("duvm: ring init failed: %d\n", ret);
        return ret;
    }

    /* Register misc device */
    duvm.misc.minor = MISC_DYNAMIC_MINOR;
    duvm.misc.name  = DUVM_DEVICE_NAME;
    duvm.misc.fops  = &duvm_fops;

    ret = misc_register(&duvm.misc);
    if (ret) {
        pr_err("duvm: misc_register failed: %d\n", ret);
        duvm_ring_destroy(&duvm.ring);
        return ret;
    }

    /* Initialize swap interception */
    ret = duvm_swap_init();
    if (ret) {
        pr_warn("duvm: swap interception init failed: %d (continuing without)\n",
                ret);
        /* Non-fatal: module still works for userfaultfd mode */
    }

    duvm.initialized = true;
    pr_info("duvm: initialized successfully (/dev/%s)\n", DUVM_DEVICE_NAME);
    return 0;
}

static void __exit duvm_exit(void)
{
    pr_info("duvm: shutting down\n");

    duvm_swap_cleanup();
    misc_deregister(&duvm.misc);
    duvm_ring_destroy(&duvm.ring);
    duvm.initialized = false;

    pr_info("duvm: unloaded\n");
}

module_init(duvm_init);
module_exit(duvm_exit);
