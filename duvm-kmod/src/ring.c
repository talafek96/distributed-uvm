/*
 * ring.c - Lock-free SPSC ring buffer for kernel-daemon communication.
 *
 * The ring buffer is allocated as contiguous kernel memory and can be
 * mmap'd into the daemon's address space. Communication uses memory
 * barriers (smp_wmb/smp_rmb) instead of locks.
 */

#include <linux/slab.h>
#include <linux/mm.h>
#include <linux/gfp.h>
#include <linux/wait.h>
#include <linux/sched.h>
#include <linux/log2.h>

#include "duvm_kmod.h"

/*
 * Initialize the ring buffer.
 *
 * Allocates a contiguous region containing:
 *   - Ring header (struct duvm_ring_header)
 *   - Request entries (capacity * sizeof(struct duvm_request))
 *   - Completion entries (capacity * sizeof(struct duvm_completion))
 *   - Staging buffer (staging_pages * PAGE_SIZE)
 */
int duvm_ring_init(struct duvm_ring *ring, unsigned int capacity,
                   unsigned long staging_pg)
{
    size_t header_size, req_size, comp_size, staging_size, total_size;
    unsigned int order;
    void *buf;

    header_size  = PAGE_ALIGN(sizeof(struct duvm_ring_header));
    req_size     = PAGE_ALIGN(capacity * sizeof(struct duvm_request));
    comp_size    = PAGE_ALIGN(capacity * sizeof(struct duvm_completion));
    staging_size = staging_pg * PAGE_SIZE;
    total_size   = header_size + req_size + comp_size + staging_size;

    /* Allocate contiguous pages */
    order = get_order(total_size);
    if (order > MAX_PAGE_ORDER) {
        pr_err("duvm: ring buffer too large (order %u > %d)\n",
               order, MAX_PAGE_ORDER);
        return -ENOMEM;
    }

    buf = (void *)__get_free_pages(GFP_KERNEL | __GFP_ZERO, order);
    if (!buf) {
        pr_err("duvm: failed to allocate ring buffer (%zu bytes)\n",
               total_size);
        return -ENOMEM;
    }

    ring->ring_page    = virt_to_page(buf);
    ring->ring_size    = (1UL << order) * PAGE_SIZE;
    ring->header       = (struct duvm_ring_header *)buf;
    ring->requests     = (struct duvm_request *)((char *)buf + header_size);
    ring->completions  = (struct duvm_completion *)((char *)buf + header_size + req_size);
    ring->staging      = (char *)buf + header_size + req_size + comp_size;
    ring->staging_pages = staging_pg;

    /* Initialize header */
    ring->header->write_idx = 0;
    ring->header->read_idx  = 0;
    ring->header->capacity  = capacity;
    ring->header->version   = 1;

    atomic_set(&ring->seq_counter, 0);
    init_waitqueue_head(&ring->wait_queue);
    ring->daemon_connected = false;

    pr_info("duvm: ring buffer initialized (capacity=%u, staging=%lu pages, "
            "total=%zu bytes, order=%u)\n",
            capacity, staging_pg, total_size, order);
    return 0;
}

void duvm_ring_destroy(struct duvm_ring *ring)
{
    if (ring->header) {
        unsigned int order = get_order(ring->ring_size);
        free_pages((unsigned long)ring->header, order);
        ring->header = NULL;
    }
}

/*
 * Submit a request to the ring buffer (kernel side, non-blocking).
 *
 * Returns 0 on success, -EAGAIN if ring is full, -ENODEV if daemon
 * is not connected.
 */
int duvm_ring_submit(struct duvm_ring *ring, struct duvm_request *req)
{
    __u32 capacity, mask, write_idx, next_write, read_idx;

    if (!ring->daemon_connected)
        return -ENODEV;

    capacity  = ring->header->capacity;
    mask      = capacity - 1;
    write_idx = ring->header->write_idx;
    read_idx  = READ_ONCE(ring->header->read_idx);
    next_write = (write_idx + 1) & mask;

    if (next_write == read_idx)
        return -EAGAIN;  /* ring full */

    /* Assign sequence number */
    req->seq = atomic_inc_return(&ring->seq_counter);

    /* Write request entry */
    memcpy(&ring->requests[write_idx], req, sizeof(*req));

    /* Memory barrier: ensure request data is visible before updating index */
    smp_wmb();

    WRITE_ONCE(ring->header->write_idx, next_write);

    /* Wake daemon if it's polling */
    wake_up(&ring->wait_queue);

    return 0;
}

/*
 * Wait for a completion with matching sequence number.
 * Used for synchronous operations (e.g., page load that must block).
 *
 * Returns 0 on success, -ETIMEDOUT on timeout, -ENODEV if daemon disconnects.
 */
int duvm_ring_wait_completion(struct duvm_ring *ring, __u32 seq,
                              struct duvm_completion *comp, int timeout_ms)
{
    long timeout_jiffies = msecs_to_jiffies(timeout_ms);
    long ret;

    /*
     * Simple polling approach for now. In production, the daemon writes
     * completions and signals via eventfd. Here we poll the completion ring.
     */
    ret = wait_event_timeout(ring->wait_queue,
        ({
            __u32 mask = ring->header->capacity - 1;
            __u32 read_idx = ring->header->read_idx;
            __u32 write_idx = READ_ONCE(ring->header->write_idx);
            bool found = false;
            __u32 idx = read_idx;
            while (idx != write_idx) {
                smp_rmb();
                if (ring->completions[idx].seq == seq) {
                    memcpy(comp, &ring->completions[idx], sizeof(*comp));
                    found = true;
                    break;
                }
                idx = (idx + 1) & mask;
            }
            found || !ring->daemon_connected;
        }),
        timeout_jiffies);

    if (!ring->daemon_connected)
        return -ENODEV;
    if (ret == 0)
        return -ETIMEDOUT;
    return 0;
}
