/*
 * swap.c - Swap interception hooks for duvm.
 *
 * This file provides the interface for intercepting page swap events.
 * The implementation depends on the kernel version:
 *
 * - Linux < 6.0: frontswap backend (frontswap_register_ops)
 * - Linux >= 6.0: The frontswap API was consolidated into zswap.
 *   We hook into the swap writeback path via alternative mechanisms.
 *
 * For the initial implementation, this is a stub that provides the
 * function signatures. The actual hooks will be implemented when
 * building against a specific kernel version.
 *
 * In the meantime, duvm works perfectly via the userfaultfd fallback
 * path in user-space, which doesn't need any kernel module.
 */

#include <linux/kernel.h>
#include <linux/module.h>
#include <linux/mm.h>

#include "duvm_kmod.h"

/*
 * Initialize swap interception hooks.
 *
 * This is a placeholder for the kernel-version-specific implementation.
 * Returns 0 on success, negative errno on failure.
 */
int duvm_swap_init(void)
{
    pr_info("duvm: swap interception not yet implemented for this kernel version\n");
    pr_info("duvm: use duvm-daemon --uffd-mode for user-space page fault handling\n");

    /*
     * TODO: Implement kernel-version-specific swap hooks:
     *
     * For Linux < 6.0:
     *   - Register a frontswap_ops with .store, .load, .invalidate_page
     *   - Pages selected for swap-out will be routed through our store handler
     *
     * For Linux >= 6.0:
     *   - Hook into zswap or the swap writeback path
     *   - Use memory_tier / demotion infrastructure for CXL-class tiering
     *
     * For now, the userfaultfd fallback in duvm-daemon provides the same
     * functionality at slightly higher latency (~5-8us vs ~2us).
     */

    return 0;  /* non-fatal: module works without swap hooks */
}

void duvm_swap_cleanup(void)
{
    /* Nothing to clean up in stub implementation */
}
