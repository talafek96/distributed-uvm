/*
 * swap.c - Placeholder for swap-specific helpers.
 *
 * In the block device model, swap interception happens through the
 * standard block I/O path (blk-mq queue_rq callback in main.c).
 * No separate swap hooks are needed.
 *
 * This file is kept for any future swap-specific optimizations
 * (e.g., detecting swap patterns, prefetch hints).
 */

#include <linux/kernel.h>
#include <linux/module.h>

#include "duvm_kmod.h"

int duvm_swap_init(void)
{
    /* Block device model: swap I/O goes through blk-mq, no extra hooks needed */
    return 0;
}

void duvm_swap_cleanup(void)
{
    /* Nothing to clean up */
}
