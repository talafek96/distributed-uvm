# How duvm Works — Architecture Explanation

## The One-Sentence Version

duvm creates a virtual swap device that sends pages to other machines' RAM instead of to a local disk. The kernel's normal swap system handles everything — applications don't know.

## The Full Page Lifecycle

### Setup (one-time, per machine)

```
1. Load kernel module:     sudo insmod duvm-kmod.ko size_mb=4096
2. Create swap:            sudo mkswap /dev/duvm_swap0
3. Start daemon:           duvm-daemon --kmod-ctl /dev/duvm_ctl
4. Start memory server:    duvm-memserver --bind 0.0.0.0:9200
5. Activate swap:          sudo swapon -p 100 /dev/duvm_swap0
```

After step 5, the kernel uses `/dev/duvm_swap0` as a swap device. Applications are unaware.

### When a page is swapped OUT (machine A → machine B)

```
 App on machine A calls malloc() and uses the memory.
 System runs low on RAM.
 The kernel picks a cold page to evict (standard LRU, nothing duvm-specific).
 
 ┌─ KERNEL ────────────────────────────────────────────────────────┐
 │                                                                  │
 │  1. Kernel swap subsystem decides to write page to swap device   │
 │  2. Block layer sends WRITE request to /dev/duvm_swap0           │
 │  3. Our queue_rq() callback fires:                               │
 │     a. Copies 4KB page data into STAGING BUFFER (shared memory)  │
 │     b. Writes a STORE request to the RING BUFFER                 │
 │     c. Sleeps, waiting for completion                             │
 │                                                                  │
 └──────────────────────────────────────────────────────────────────┘
             │ staging buffer + ring buffer (mmap'd shared memory)
             ▼
 ┌─ DAEMON (user-space, same machine) ─────────────────────────────┐
 │                                                                  │
 │  4. Poll loop sees new request in ring buffer                    │
 │  5. Reads 4KB from staging buffer                                │
 │  6. Calls engine.store_page(offset, data)                        │
 │  7. Engine picks a backend (TCP to machine B)                    │
 │  8. TCP backend sends: [OP_STORE][offset][4096 bytes]            │
 │  9. Writes COMPLETION to ring buffer (result=0)                  │
 │                                                                  │
 └──────────────────────────────────────────────────────────────────┘
             │ TCP socket over network
             ▼
 ┌─ MEMSERVER (machine B) ─────────────────────────────────────────┐
 │                                                                  │
 │  10. Receives STORE request                                      │
 │  11. Allocates Box<[u8; 4096]> on the HEAP (= RAM)              │
 │  12. Stores in HashMap: pages[offset] = data                     │
 │  13. Sends back [RESP_OK]                                        │
 │                                                                  │
 │  The page now lives in machine B's RAM.                          │
 │  Machine A's kernel can use that RAM for something else.         │
 │                                                                  │
 └──────────────────────────────────────────────────────────────────┘
```

### When a page is swapped IN (machine B → machine A)

```
 App on machine A touches the swapped-out page.
 CPU triggers a page fault (the PTE says "page is in swap").
 
 Kernel reads the swap slot number from the PTE.
 Block layer sends READ request to /dev/duvm_swap0.
 
 queue_rq() → ring buffer LOAD request → daemon →
   engine.load_page() → TCP backend → machine B's memserver →
     HashMap lookup → 4KB data back over TCP →
       daemon writes to staging buffer → completion →
         kernel copies staging to page frame → PTE updated →
           app resumes. Never knew anything happened.
```

### When the service is disabled

```
 sudo swapoff /dev/duvm_swap0    ← kernel swaps all pages back to RAM
 sudo rmmod duvm_kmod            ← device disappears
 
 All pages are back in local RAM. Applications continue running.
 No data loss, no restart needed.
```

## Why Pages Don't Collide

### Between processes on the same machine

The kernel's swap allocator assigns **unique swap slot numbers** to each page. When Process A's page is swapped out, it gets slot 1000. When Process B's page is swapped out, it gets slot 1001. Our kernel module sees these as different sector offsets. It's impossible for two processes to get the same slot — the kernel manages this, just like it manages physical page frame numbers.

### Between machines

Each TCP connection to the memserver gets its **own HashMap**:

```rust
fn handle_client(mut stream: TcpStream, max_pages: u64) -> Result<()> {
    let mut pages: HashMap<u64, Box<[u8; PAGE_SIZE]>> = HashMap::new();
    // This HashMap is private to this connection.
    // Machine A's offset 1000 and machine C's offset 1000
    // are in different HashMaps.
```

Machine A's daemon has one TCP connection to B's memserver. Machine C's daemon has a different TCP connection. Their pages are in completely separate HashMaps. No collision possible.

## Performance Characteristics

### Current implementation (what we have now)

| Step | Latency | Bottleneck |
|------|---------|------------|
| Kernel → staging buffer | <1us | memcpy 4KB |
| Ring buffer post | <1us | cache-line write |
| Daemon sees request | **0-100us** | **Polling loop with sleep** |
| Engine store/load | 1-5us | LZ4 compress or HashMap lookup |
| TCP to remote | 130-220us | Network RTT |
| Total (TCP, same rack) | **~150-350us** | Network dominates |
| Total (RDMA, future) | **~5-15us** | Ring buffer + RDMA |

### Known performance issues to fix

1. **Daemon polling delay (0-100us):** The daemon currently spins for 1000 iterations then sleeps 100us. This adds up to 100us of unnecessary latency. Fix: use eventfd — the kernel posts to the ring buffer AND writes to an eventfd, the daemon blocks on epoll/io_uring waiting for the eventfd. Wake-up latency drops to ~1-5us.

2. **5-second kernel timeout:** The `wait_event_timeout` in ring.c uses a 5000ms timeout. This does NOT freeze the kernel — `wait_event_timeout` puts the thread to sleep on a wait queue, and the kernel continues running other processes and handling interrupts. But 5 seconds is too generous for a safety timeout. Should be reduced to 500ms or 1 second for swap I/O.

3. **TCP latency (130-220us per page):** TCP goes through the kernel network stack on both sides, adding overhead. RDMA one-sided WRITE bypasses both CPUs entirely — the NIC writes directly to the remote machine's registered memory. Expected improvement: 50-100x.

## Strategy: What Each Component Is For

| Component | Purpose | Required for transparency? |
|-----------|---------|--------------------------|
| **duvm-kmod** | Creates swap device, ring buffer | **YES** — this is how the kernel talks to us |
| **duvm-daemon** | Processes ring buffer requests, routes to backends | **YES** — bridges kernel to network |
| **duvm-memserver** | Stores pages in RAM on remote machines | **YES** — this is where remote pages live |
| **duvm-ctl** | CLI for enable/disable/status | No — operational convenience |
| **libduvm** | Explicit API for power users | **NO** — optional, for apps that want direct control |
| **duvm-backend-tcp** | TCP transport | YES for now, replaced by RDMA later |
| **duvm-backend-memory** | Local RAM backend | Testing/fallback only |
| **duvm-backend-compress** | LZ4 compression | Optional — reduces network bandwidth |

The `libduvm` library is a tool for benchmarking and for applications that want to manage memory placement explicitly. It is NOT part of the transparent path. The transparent path is: kernel module + daemon + memserver. No application code involved.
