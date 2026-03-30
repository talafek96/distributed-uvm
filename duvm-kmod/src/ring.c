/*
 * ring.c - Shared ring buffer for kernel-daemon communication.
 *
 * Allocates a contiguous set of pages that can be mapped into both
 * kernel space and user space (daemon). Uses lock-free SPSC protocol
 * with memory barriers.
 *
 * Memory layout (all page-aligned):
 *   [ring_header]          - 1 page
 *   [request entries]      - ceil(capacity * 64 / PAGE_SIZE) pages
 *   [completion entries]   - ceil(capacity * 64 / PAGE_SIZE) pages
 *   [staging buffer]       - staging_pages pages
 */

#include <linux/slab.h>
#include <linux/mm.h>
#include <linux/gfp.h>
#include <linux/vmalloc.h>
#include <linux/wait.h>
#include <linux/sched.h>
#include <linux/log2.h>

#include "duvm_kmod.h"

int duvm_ring_init(struct duvm_ring *ring, unsigned int capacity,
                   unsigned long staging_pg)
{
    size_t header_sz, req_sz, comp_sz, staging_sz, total_sz;
    unsigned int total_pages, i;
    struct page **pages;
    void *vaddr;

    header_sz   = PAGE_SIZE;  /* 1 page for header */
    req_sz      = PAGE_ALIGN(capacity * sizeof(struct duvm_request));
    comp_sz     = PAGE_ALIGN(capacity * sizeof(struct duvm_completion));
    staging_sz  = staging_pg * PAGE_SIZE;
    total_sz    = header_sz + req_sz + comp_sz + staging_sz;
    total_pages = total_sz / PAGE_SIZE;

    pr_info("duvm: ring init: capacity=%u, staging=%lu pages, total=%u pages (%zu bytes)\n",
            capacity, staging_pg, total_pages, total_sz);

    /* Allocate page array */
    pages = kvmalloc_array(total_pages, sizeof(struct page *), GFP_KERNEL);
    if (!pages)
        return -ENOMEM;

    /* Allocate individual pages (allows mmap via vm_insert_page) */
    for (i = 0; i < total_pages; i++) {
        pages[i] = alloc_page(GFP_KERNEL | __GFP_ZERO);
        if (!pages[i]) {
            pr_err("duvm: failed to allocate ring page %u/%u\n", i, total_pages);
            goto err_free_pages;
        }
    }

    /* Map all pages contiguously into kernel virtual address space */
    vaddr = vmap(pages, total_pages, VM_MAP, PAGE_KERNEL);
    if (!vaddr) {
        pr_err("duvm: vmap failed for %u pages\n", total_pages);
        goto err_free_pages;
    }

    ring->ring_pages    = pages;
    ring->nr_ring_pages = total_pages;
    ring->ring_size     = total_sz;

    /* Set up pointers into the mapped region */
    ring->header      = (struct duvm_ring_header *)vaddr;
    ring->requests    = (struct duvm_request *)((char *)vaddr + header_sz);
    ring->completions = (struct duvm_completion *)((char *)vaddr + header_sz + req_sz);
    ring->staging     = (char *)vaddr + header_sz + req_sz + comp_sz;
    ring->staging_pages = staging_pg;

    /* Initialize header */
    ring->header->req_write_idx  = 0;
    ring->header->req_read_idx   = 0;
    ring->header->comp_write_idx = 0;
    ring->header->comp_read_idx  = 0;
    ring->header->capacity       = capacity;
    ring->header->version        = 2;  /* v2 = block device model */
    ring->header->staging_pages  = staging_pg;

    atomic_set(&ring->seq_counter, 0);
    init_waitqueue_head(&ring->comp_wait);
    init_waitqueue_head(&ring->req_wait);
    ring->daemon_connected = false;

    /* Staging slot bitmap allocator */
    ring->staging_bitmap = bitmap_zalloc(staging_pg, GFP_KERNEL);
    if (!ring->staging_bitmap) {
        pr_err("duvm: failed to allocate staging bitmap\n");
        vunmap(vaddr);
        goto err_free_pages;
    }
    spin_lock_init(&ring->staging_lock);

    pr_info("duvm: ring buffer initialized (%u pages, %zu bytes)\n",
            total_pages, total_sz);
    return 0;

err_free_pages:
    while (i-- > 0)
        __free_page(pages[i]);
    kvfree(pages);
    return -ENOMEM;
}

void duvm_ring_destroy(struct duvm_ring *ring)
{
    unsigned int i;

    if (!ring->ring_pages)
        return;

    bitmap_free(ring->staging_bitmap);
    ring->staging_bitmap = NULL;

    /* Unmap virtual address */
    if (ring->header)
        vunmap(ring->header);

    /* Free individual pages */
    for (i = 0; i < ring->nr_ring_pages; i++) {
        if (ring->ring_pages[i])
            __free_page(ring->ring_pages[i]);
    }

    kvfree(ring->ring_pages);
    ring->ring_pages = NULL;
    ring->header = NULL;
}

/*
 * Submit a request to the ring without waiting for completion.
 * Returns 0 on success, -EAGAIN if ring full, -ENODEV if daemon disconnected.
 * The caller must set req->seq before calling (use atomic_inc_return on seq_counter).
 */
int duvm_ring_submit(struct duvm_ring *ring, struct duvm_request *req)
{
    __u32 capacity, mask, write_idx, next_write;

    if (!ring->daemon_connected)
        return -ENODEV;

    capacity  = ring->header->capacity;
    mask      = capacity - 1;
    write_idx = ring->header->req_write_idx;
    next_write = (write_idx + 1) & mask;

    if (next_write == READ_ONCE(ring->header->req_read_idx))
        return -EAGAIN;

    memcpy(&ring->requests[write_idx], req, sizeof(*req));

    smp_wmb();
    WRITE_ONCE(ring->header->req_write_idx, next_write);

    wake_up(&ring->req_wait);
    return 0;
}

/*
 * Poll the completion ring for one completed request.
 * Returns 0 and fills comp if a completion was found, -EAGAIN if empty.
 */
int duvm_ring_poll_completion(struct duvm_ring *ring,
                              struct duvm_completion *comp)
{
    __u32 cidx, cwrite, mask;

    mask   = ring->header->capacity - 1;
    cidx   = ring->header->comp_read_idx;
    cwrite = READ_ONCE(ring->header->comp_write_idx);

    if (cidx == cwrite)
        return -EAGAIN;

    smp_rmb();
    memcpy(comp, &ring->completions[cidx], sizeof(*comp));
    WRITE_ONCE(ring->header->comp_read_idx, (cidx + 1) & mask);
    return 0;
}

/*
 * Submit a request and wait for the matching completion.
 *
 * This is the synchronous path used by the block device for swap I/O.
 * For async operation, the daemon polls the request ring and writes
 * completions independently.
 *
 * Returns 0 on success, negative errno on failure.
 */
int duvm_ring_submit_and_wait(struct duvm_ring *ring,
                              struct duvm_request *req,
                              struct duvm_completion *comp,
                              int timeout_ms)
{
    __u32 capacity, mask, write_idx, next_write;
    __u32 seq;
    long timeout_jiffies;
    long ret;

    if (!ring->daemon_connected)
        return -ENODEV;

    capacity  = ring->header->capacity;
    mask      = capacity - 1;
    write_idx = ring->header->req_write_idx;
    next_write = (write_idx + 1) & mask;

    /* Check if ring is full */
    if (next_write == READ_ONCE(ring->header->req_read_idx))
        return -EAGAIN;

    /* Assign sequence number */
    seq = atomic_inc_return(&ring->seq_counter);
    req->seq = seq;

    /* Write request to ring */
    memcpy(&ring->requests[write_idx], req, sizeof(*req));

    /* Barrier: ensure request data visible before updating index */
    smp_wmb();
    WRITE_ONCE(ring->header->req_write_idx, next_write);

    /* Wake daemon immediately (it may be blocked in poll/epoll on /dev/duvm_ctl) */
    wake_up(&ring->req_wait);

    /* Wait for matching completion */
    timeout_jiffies = msecs_to_jiffies(timeout_ms);
    ret = wait_event_timeout(ring->comp_wait,
        ({
            bool found = false;
            __u32 cidx = ring->header->comp_read_idx;
            __u32 cwrite = READ_ONCE(ring->header->comp_write_idx);

            while (cidx != cwrite) {
                smp_rmb();
                if (ring->completions[cidx].seq == seq) {
                    memcpy(comp, &ring->completions[cidx], sizeof(*comp));
                    /* Advance read index past this completion */
                    WRITE_ONCE(ring->header->comp_read_idx,
                               (cidx + 1) & mask);
                    found = true;
                    break;
                }
                cidx = (cidx + 1) & mask;
            }
            found || !ring->daemon_connected;
        }),
        timeout_jiffies);

    if (!ring->daemon_connected)
        return -ENODEV;
    if (ret == 0)
        return -ETIMEDOUT;

    return comp->result;
}
