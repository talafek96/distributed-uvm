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
| Daemon sees request | **1-5us** | **poll() wake-up from kernel** |
| Engine store/load | 1-5us | LZ4 compress or HashMap lookup |
| TCP to remote | 130-220us | Network RTT |
| Total (TCP, same rack) | **~140-250us** | Network dominates |
| Total (RDMA, future) | **~5-15us** | Ring buffer + RDMA |

### Performance design

The daemon uses `poll()` on `/dev/duvm_ctl`. The kernel module implements the `poll` file operation — when a new request is posted to the ring buffer, it calls `wake_up()` on a wait queue, and the daemon wakes within 1-5 microseconds. There is no polling loop with sleep. The kernel timeout for waiting on a completion is 500ms (safety net — normal completions arrive in microseconds).

### Remaining performance improvement

**TCP latency (130-220us per page):** TCP goes through the kernel network stack on both sides, adding overhead. RDMA one-sided WRITE bypasses both CPUs entirely — the NIC writes directly to the remote machine's registered memory. Expected improvement: 50-100x.

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

---

## libduvm: The Optional Explicit API

`libduvm` is a **separate, optional** library for applications that want direct control over distributed memory. It is NOT needed for the transparent swap path. Most users will never use it.

### What it provides

```rust
use duvm::Pool;

let pool = Pool::standalone()?;

// Explicitly store a 4KB page
let data = [0u8; 4096];
let handle = pool.store(&data)?;

// Load it back (from local compressed backend or remote)
let loaded = pool.load(handle)?;

// Free when done
pool.free(handle)?;

// Check capacity
let (total, used) = pool.capacity();
```

### C API (for non-Rust applications)

```c
#include <duvm.h>

duvm_init();                              // Initialize
uint64_t h = duvm_store_page(data);       // Store 4KB page, get handle
duvm_load_page(h, buffer);                // Load page by handle
duvm_free_page(h);                        // Free
uint64_t total = duvm_capacity_total();   // Total capacity
uint64_t used  = duvm_capacity_used();    // Used capacity
```

### When would you use libduvm?

| Use case | Use libduvm? | Why |
|----------|-------------|-----|
| Normal application (Python, Java, etc.) | **No** | Transparent swap handles it |
| Benchmarking duvm's page throughput | Yes | Measure store/load latency directly |
| Application with its own memory manager | Maybe | Can explicitly place data on specific backends |
| Custom caching layer | Maybe | Store/load 4KB blocks to/from remote RAM |
| GPU application managing its own buffers | Maybe | Explicitly place buffers in remote memory |

### How it differs from the transparent path

| | Transparent path (swap) | libduvm (explicit) |
|---|---|---|
| Application changes | None | Must call store/load/free |
| Granularity | 4KB pages (kernel decides which) | 4KB pages (app decides which) |
| When pages move | Kernel decides (under memory pressure) | App decides (explicit calls) |
| Backend selection | Daemon's policy engine decides | Library picks (currently prefers compress) |
| Requires daemon | Yes (kmod + daemon + memserver) | No (standalone mode, or daemon mode) |

### Strategy note

libduvm exists as a development and testing tool. The product goal is full transparency via the swap path. libduvm may be useful as a building block for future features (explicit memory tiering hints, prefetch APIs) but is not on the critical path.

---

## Connecting Multiple Machines

### Two-machine setup (what we have today)

```
Machine A (calc1)                       Machine B (calc2)
┌─────────────────────┐                ┌─────────────────────┐
│ duvm-kmod loaded     │                │ duvm-kmod loaded     │
│ duvm-daemon          │                │ duvm-daemon          │
│   backend: TCP → B   │───TCP/RDMA───►│ duvm-memserver:9200  │
│ duvm-memserver:9200  │◄──TCP/RDMA────│   backend: TCP → A   │
└─────────────────────┘                └─────────────────────┘

A's cold pages → B's memserver (in B's RAM)
B's cold pages → A's memserver (in A's RAM)
```

Setup commands (on each machine):

```bash
# 1. Load kernel module
sudo insmod duvm-kmod.ko size_mb=4096

# 2. Create and activate swap
sudo mkswap /dev/duvm_swap0
sudo swapon -p 100 /dev/duvm_swap0

# 3. Start memory server (serves pages to other machines)
duvm-memserver --bind 0.0.0.0:9200 &

# 4. Start daemon (connects kmod to engine, engine to remote memserver)
duvm-daemon --kmod-ctl /dev/duvm_ctl &
```

### N-machine cluster setup

For N machines, each machine runs all three components. Each daemon connects to every other machine's memserver:

```
Machine 1                Machine 2                Machine 3
┌──────────┐            ┌──────────┐            ┌──────────┐
│ kmod     │            │ kmod     │            │ kmod     │
│ daemon ──┼──TCP/RDMA──┼─► ms     │            │          │
│    │     │            │ daemon ──┼──TCP/RDMA──┼─► ms     │
│    └─────┼──TCP/RDMA──┼──────────┼────────────┼─► ms     │
│ ms ◄─────┼────────────┼── daemon │            │          │
│ ms ◄─────┼────────────┼──────────┼────────────┼── daemon │
└──────────┘            └──────────┘            └──────────┘

ms = memserver
Each daemon has TCP backends to every other machine's memserver.
Each memserver serves pages to any machine that asks.
```

The daemon's policy engine decides which remote machine gets each page. With N machines, total available remote memory = (N-1) × memory_per_machine.

### How pages are routed in a cluster

When machine 1 needs to swap out a page:

1. Daemon's policy engine checks which remote backends have capacity
2. Picks the one with lowest latency and available space
3. Sends the page to that machine's memserver
4. Records where the page went (offset → backend mapping in the policy engine)

When machine 1 needs the page back:

1. Policy engine looks up: "offset 1000 → backend tcp(machine2:9200)"
2. Sends LOAD request to machine 2's memserver
3. Gets the 4KB data back
4. Returns it to the kernel via the ring buffer

### Enabling and disabling

```bash
# Enable on a machine (adds it to the pool):
sudo swapon -p 100 /dev/duvm_swap0

# Disable on a machine (removes it from the pool):
sudo swapoff /dev/duvm_swap0    # kernel moves all pages back to local RAM
sudo rmmod duvm_kmod             # device disappears

# Applications continue running. No data loss.
```

`swapoff` is safe — the kernel migrates all swapped pages back into local RAM before deactivating the swap device. This may take seconds to minutes depending on how many pages are remote, but applications are not interrupted — they just get their pages back in local RAM.

### Failure handling

| Failure | What happens | Data loss? |
|---------|-------------|------------|
| Daemon crashes | Kernel falls back to xarray (local storage) | No |
| Memserver on machine B crashes | Daemon gets TCP error, falls back to local backends or other remotes | Pages on B are lost — kernel sees I/O error, OOM killer may activate |
| Network cable pulled | TCP timeouts, kernel falls back to local | No (if local swap available) |
| Machine B power failure | Same as memserver crash | Pages on B are lost |
| `swapoff` on machine A | Kernel moves all pages back to local RAM | No |
| `rmmod` without `swapoff` | Refused by kernel (device is busy) | No |

For production use, the RDMA backend would use one-sided RDMA with registered memory regions, making page loss on remote failure detectable. Future work includes page replication across multiple remotes for fault tolerance.
