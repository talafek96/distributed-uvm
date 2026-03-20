# Architecture: Distributed UVM (duvm)

**Chosen direction**: Hybrid Kernel + User-Space (see `architecture-options.md` for alternatives analysis).

**Design principles**: Efficiency first. Maintainability via separation of concerns. Transparency by default, optimization by opt-in. Pluggable at every layer.

---

## Table of Contents

1. [System Overview](#1-system-overview)
2. [Component Architecture](#2-component-architecture)
3. [Kernel Module: duvm-kmod](#3-kernel-module-duvm-kmod)
4. [User-Space Daemon: duvm-daemon](#4-user-space-daemon-duvm-daemon)
5. [Backend Plugin Interface](#5-backend-plugin-interface)
6. [Policy Engine](#6-policy-engine)
7. [User-Space Library: libduvm](#7-user-space-library-libduvm)
8. [GPU Memory Integration](#8-gpu-memory-integration)
9. [Ring Buffer Protocol](#9-ring-buffer-protocol)
10. [Fault Tolerance](#10-fault-tolerance)
11. [Observability](#11-observability)
12. [Deployment Modes](#12-deployment-modes)
13. [Data Flow: Life of a Page Fault](#13-data-flow-life-of-a-page-fault)
14. [Implementation Plan](#14-implementation-plan)

---

## 1. System Overview

duvm is a distributed memory abstraction layer that makes remote and heterogeneous memory transparently available to unmodified applications. It consists of three components:

1. **duvm-kmod** — Thin kernel module (~1500 LoC) that intercepts page demotion/swap events.
2. **duvm-daemon** — User-space daemon that manages policy, backends, and orchestration.
3. **libduvm** — Optional user-space library for applications that want fine-grained control.

```
┌──────────────────────────────────────────────────────────────┐
│                      Applications                            │
│                                                              │
│  ┌─────────────────────┐         ┌─────────────────────────┐ │
│  │   Unmodified apps   │         │   Optimized apps        │ │
│  │                     │         │   #include <duvm.h>     │ │
│  │   (fully transparent│         │   duvm_ptr<T> p = ...   │ │
│  │    via kernel swap) │         │   duvm_prefetch(p)      │ │
│  └──────────┬──────────┘         └────────────┬────────────┘ │
│             │                                 │              │
│             │ page fault                      │ libduvm API  │
│             ▼                                 ▼              │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  KERNEL        ┌─────────────────────────────────────────┐   │
│                │            duvm-kmod.ko                  │   │
│                │                                         │   │
│                │  ┌───────────────┐  ┌────────────────┐  │   │
│                │  │  Swap Backend │  │  Page Event    │  │   │
│                │  │  (frontswap / │  │  Hooks         │  │   │
│                │  │   zswap hook) │  │  (demotion,    │  │   │
│                │  │               │  │   reclaim)     │  │   │
│                │  └───────┬───────┘  └───────┬────────┘  │   │
│                │          │                  │           │   │
│                │  ┌───────▼──────────────────▼────────┐  │   │
│                │  │     Shared Ring Buffer            │  │   │
│                │  │     (lock-free, mmap'd to daemon) │  │   │
│                │  └───────────────┬───────────────────┘  │   │
│                └─────────────────┼───────────────────────┘   │
│                                  │                           │
├──────────────────────────────────┼───────────────────────────┤
│                                  │                           │
│  USER-SPACE                      ▼                           │
│                ┌─────────────────────────────────────────┐   │
│                │           duvm-daemon                    │   │
│                │                                         │   │
│                │  ┌─────────────┐  ┌──────────────────┐  │   │
│                │  │   Policy    │  │  Cluster         │  │   │
│                │  │   Engine    │  │  Manager         │  │   │
│                │  │             │  │  (discovery,     │  │   │
│                │  │  placement  │  │   health,        │  │   │
│                │  │  migration  │  │   membership)    │  │   │
│                │  │  prefetch   │  │                  │  │   │
│                │  │  eviction   │  └──────────────────┘  │   │
│                │  └─────────────┘                        │   │
│                │                                         │   │
│                │  ┌──────────────────────────────────┐   │   │
│                │  │      Backend Plugin Manager      │   │   │
│                │  │                                  │   │   │
│                │  │  ┌────────┐ ┌──────┐ ┌────────┐  │   │   │
│                │  │  │ RDMA   │ │ CXL  │ │Compress│  │   │   │
│                │  │  │backend │ │backend│ │backend │  │   │   │
│                │  │  │ (.so)  │ │ (.so) │ │ (.so)  │  │   │   │
│                │  │  └────────┘ └──────┘ └────────┘  │   │   │
│                │  │  ┌────────┐ ┌──────────────────┐  │   │   │
│                │  │  │NVLink  │ │  Custom backend  │  │   │   │
│                │  │  │backend │ │  (user-provided) │  │   │   │
│                │  │  │ (.so)  │ │  (.so)           │  │   │   │
│                │  │  └────────┘ └──────────────────┘  │   │   │
│                │  └──────────────────────────────────┘   │   │
│                └─────────────────────────────────────────┘   │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

---

## 2. Component Architecture

### Component Responsibilities

| Component | Responsibility | Language | Size Estimate |
|---|---|---|---|
| **duvm-kmod** | Page event interception, ring buffer kernel side, fast-path local cache | C | ~1500 LoC |
| **duvm-daemon** | Policy decisions, backend lifecycle, cluster management, ring buffer user side | Rust | ~8000-12000 LoC |
| **libduvm** | Far-pointer API, prefetch/placement hints, daemon IPC | Rust + C FFI | ~3000 LoC |
| **Backend plugins** | Transport-specific read/write operations | Rust (or C) | ~500-1000 LoC each |
| **duvm-ctl** | CLI management tool (config, status, metrics) | Rust | ~1500 LoC |

### Language Choices

- **Kernel module**: C (mandatory for Linux kernel modules).
- **Daemon + library + plugins**: Rust.
  - Memory safety without GC (critical for a memory management system — we cannot have GC pauses in the memory manager).
  - Excellent FFI with C (for kernel module communication, libibverbs, CUDA APIs).
  - Strong type system prevents classes of bugs (use-after-free, data races) that are devastating in infrastructure software.
  - async/await for efficient I/O multiplexing in the daemon.
  - Mature ecosystem for RDMA (rdma-sys), io_uring (io-uring crate), shared memory.

---

## 3. Kernel Module: duvm-kmod

### Design Philosophy
The module is a **thin relay**. It intercepts kernel memory events and relays them to user-space via a shared ring buffer. It does NOT make policy decisions, manage connections, or implement transport protocols.

### Interfaces

#### 3.1 Swap Interception

Modern Linux (6.x) has consolidated the swap compression path under zswap. The module hooks into the page writeback / swap-out path:

```c
// Hooks into the kernel's page reclaim path
// When a page is selected for swap-out:
static int duvm_page_store(unsigned type, pgoff_t offset,
                           struct page *page)
{
    // 1. Check if daemon is connected
    if (!duvm_daemon_connected())
        return -1;  // fallback to normal swap

    // 2. Write page descriptor to ring buffer
    struct duvm_request req = {
        .op = DUVM_OP_STORE,
        .pfn = page_to_pfn(page),
        .offset = offset,
        .flags = 0,
    };

    // 3. Copy page data to shared staging buffer
    copy_page_to_staging(page, req.staging_slot);

    // 4. Submit to ring buffer (non-blocking)
    if (duvm_ring_submit(&req) < 0)
        return -1;  // ring full, fallback to local swap

    return 0;  // page will be stored by daemon
}

// When a page is needed back:
static int duvm_page_load(unsigned type, pgoff_t offset,
                          struct page *page)
{
    // Submit load request and wait for completion
    struct duvm_request req = {
        .op = DUVM_OP_LOAD,
        .pfn = page_to_pfn(page),
        .offset = offset,
    };

    duvm_ring_submit_and_wait(&req);
    copy_staging_to_page(req.staging_slot, page);
    return 0;
}
```

#### 3.2 Page Demotion Hooks (Optional, for NUMA/CXL tiering)

On systems with CXL or multi-tier NUMA, the kernel already has page demotion logic (since Linux 5.18+). The module can optionally hook into this path to influence which pages get demoted and where:

```c
// Hook into NUMA demotion path
// This allows the policy engine to influence placement
static int duvm_demotion_target(struct page *page, int src_nid)
{
    // Query cached policy from daemon (via shared memory map)
    int target = duvm_policy_lookup(page_to_pfn(page));
    if (target >= 0)
        return target;  // daemon wants this page on specific node
    return -1;  // let kernel decide
}
```

#### 3.3 Statistics Export

The module exports access statistics via a shared memory region that the daemon polls:

- Pages stored/loaded per second
- Ring buffer utilization
- Fallback-to-local-swap events
- Average latency per operation

### Module Parameters

```
# /etc/modprobe.d/duvm.conf
options duvm ring_size=4096          # ring buffer entries (power of 2)
options duvm staging_pages=8192      # staging buffer size (pages)
options duvm local_cache_mb=512      # fast-path local page cache (MB)
options duvm fallback_on_timeout=1   # fallback to swap if daemon unresponsive
options duvm timeout_ms=100          # daemon response timeout
```

---

## 4. User-Space Daemon: duvm-daemon

### Architecture

The daemon is a single-process, multi-threaded Rust application:

```
duvm-daemon
├── main thread: startup, config, signal handling
├── ring_reader thread: reads requests from kernel ring buffer
├── completion thread: writes completions back to ring buffer
├── policy thread: runs async policy decisions
├── backend_pool: thread pool for backend I/O (one pool per backend)
├── cluster thread: cluster membership, health checking
├── metrics thread: prometheus exporter, stats aggregation
└── libduvm_server thread: serves libduvm API requests (Unix socket)
```

### Configuration

```toml
# /etc/duvm/duvm.toml

[daemon]
ring_device = "/dev/duvm0"            # kernel module char device
log_level = "info"
metrics_port = 9100

[policy]
strategy = "adaptive"                  # adaptive | lru | frequency | custom
local_cache_ratio = 0.2               # keep 20% of managed memory local
prefetch_depth = 4                    # prefetch N pages ahead on sequential access
migration_threshold_ms = 10           # migrate page if access latency > threshold

[backends]

[backends.compress]
enabled = true
plugin = "libduvm_compress.so"
algorithm = "lz4"                     # lz4 | zstd | none
priority = 1                          # lowest latency tier

[backends.cxl]
enabled = true
plugin = "libduvm_cxl.so"
dax_device = "/dev/dax0.0"
capacity_gb = 256
priority = 2

[backends.rdma]
enabled = true
plugin = "libduvm_rdma.so"
peers = ["10.0.1.2:4791", "10.0.1.3:4791"]
transport = "rc"                      # rc (reliable connected) | ud (unreliable datagram)
priority = 3

[backends.nvlink]
enabled = false
plugin = "libduvm_nvlink.so"
gpu_devices = [0, 1, 2, 3]
priority = 2

[cluster]
enabled = true
discovery = "static"                  # static | etcd | consul | dns
peers = ["10.0.1.1:4790", "10.0.1.2:4790", "10.0.1.3:4790"]
heartbeat_interval_ms = 1000
failure_timeout_ms = 5000
```

### Request Processing Pipeline

```
Kernel ring buffer
       │
       ▼
  ┌─────────────┐
  │ Ring Reader  │──── Reads batch of requests (up to 64 at a time)
  └──────┬──────┘
         │
         ▼
  ┌─────────────┐     ┌─────────────┐
  │   Classify  │────►│ Page Store  │──► Select backend (policy engine)
  │   Request   │     │             │    ──► Serialize page to backend
  │             │     └─────────────┘    ──► Write completion to ring
  │             │
  │             │     ┌─────────────┐
  │             │────►│ Page Load   │──► Identify which backend has page
  │             │     │             │    ──► Fetch from backend
  │             │     └─────────────┘    ──► Copy to staging buffer
  │             │                        ──► Write completion to ring
  │             │
  │             │     ┌─────────────┐
  │             │────►│ Prefetch    │──► Speculatively load adjacent pages
  └─────────────┘     └─────────────┘    ──► Populate local cache
```

### Batching and Pipelining

Critical for bandwidth:

- **Batch reads**: The ring reader dequeues up to 64 requests per batch, reducing syscall overhead.
- **Pipeline backends**: While waiting for an RDMA read to complete, submit the next one. Overlap network latency.
- **Coalesce stores**: Group sequential page stores into a single large RDMA write.
- **Async completion**: The completion thread writes completions back to the ring without blocking the request processing pipeline.

---

## 5. Backend Plugin Interface

Each backend is a shared library (.so) that implements a trait:

```rust
/// Backend plugin interface.
/// Each backend manages one tier of remote/alternative memory.
pub trait DuvmBackend: Send + Sync {
    /// Human-readable name for logging and metrics.
    fn name(&self) -> &str;

    /// Initialize the backend with configuration.
    fn init(&mut self, config: &toml::Value) -> Result<()>;

    /// Allocate a slot for a page. Returns an opaque handle.
    fn alloc_page(&self) -> Result<PageHandle>;

    /// Free a previously allocated page slot.
    fn free_page(&self, handle: PageHandle) -> Result<()>;

    /// Store a 4KB page. `data` is a pinned page buffer.
    /// This must be async-safe (non-blocking or offloaded).
    fn store_page(&self, handle: PageHandle, data: &[u8; 4096]) -> Result<()>;

    /// Load a 4KB page into the provided buffer.
    fn load_page(&self, handle: PageHandle, buf: &mut [u8; 4096]) -> Result<()>;

    /// Batch store: store multiple pages in one operation.
    /// Default implementation calls store_page in a loop.
    fn store_pages(&self, pages: &[(PageHandle, &[u8; 4096])]) -> Result<()> {
        for (handle, data) in pages {
            self.store_page(*handle, data)?;
        }
        Ok(())
    }

    /// Batch load: load multiple pages in one operation.
    fn load_pages(&self, pages: &mut [(PageHandle, &mut [u8; 4096])]) -> Result<()> {
        for (handle, buf) in pages.iter_mut() {
            self.load_page(*handle, buf)?;
        }
        Ok(())
    }

    /// Report current capacity (total and used, in pages).
    fn capacity(&self) -> (u64, u64);

    /// Report average access latency in nanoseconds.
    fn latency_ns(&self) -> u64;

    /// Health check. Returns false if backend is degraded/unreachable.
    fn is_healthy(&self) -> bool;

    /// Shutdown and release resources.
    fn shutdown(&mut self) -> Result<()>;
}

/// Opaque handle to a page stored in a backend.
/// Encodes backend ID + internal offset for efficient lookup.
#[derive(Copy, Clone, Debug)]
pub struct PageHandle(u64);
```

### Backend Implementations

#### Compression Backend (`libduvm_compress.so`)
- Compresses pages using LZ4 (fast) or ZSTD (better ratio) into a local memory pool.
- Equivalent to zswap but managed by duvm for unified policy.
- Latency: ~1-5us (compression + memcpy).

#### CXL Backend (`libduvm_cxl.so`)
- Opens a CXL DAX device (`/dev/daxN.Y`) and mmaps it.
- Store/load is `memcpy` to/from the mapped region.
- Latency: ~200-400ns (direct load/store via CXL.mem).
- Allocation: Simple bump allocator or slab allocator over the DAX region.

#### RDMA Backend (`libduvm_rdma.so`)
- Uses `libibverbs` for one-sided RDMA read/write.
- Maintains queue pairs (QPs) to remote memory servers.
- Store: RDMA WRITE to remote memory.
- Load: RDMA READ from remote memory.
- Batch: Posts multiple RDMA operations and polls for completions.
- Latency: ~1-3us per 4KB page.

#### NVLink Backend (`libduvm_nvlink.so`)
- Uses CUDA APIs (`cudaMemcpy`, `cudaMallocManaged`) or NVSHMEM for GPU memory access.
- Can store cold CPU pages in idle GPU HBM.
- Can expose GPU memory to CPU applications.
- Latency: ~1-5us via PCIe, ~0.3us via NVLink.

#### Custom Backend Template
```rust
// Minimal backend implementation:
pub struct MyBackend { /* ... */ }

impl DuvmBackend for MyBackend {
    fn name(&self) -> &str { "my-backend" }
    fn init(&mut self, config: &toml::Value) -> Result<()> { /* ... */ Ok(()) }
    fn alloc_page(&self) -> Result<PageHandle> { /* ... */ }
    fn free_page(&self, handle: PageHandle) -> Result<()> { /* ... */ }
    fn store_page(&self, handle: PageHandle, data: &[u8; 4096]) -> Result<()> { /* ... */ }
    fn load_page(&self, handle: PageHandle, buf: &mut [u8; 4096]) -> Result<()> { /* ... */ }
    fn capacity(&self) -> (u64, u64) { /* ... */ }
    fn latency_ns(&self) -> u64 { /* ... */ }
    fn is_healthy(&self) -> bool { true }
    fn shutdown(&mut self) -> Result<()> { Ok(()) }
}
```

---

## 6. Policy Engine

The policy engine decides **where** to place pages and **when** to migrate them. It runs in the daemon and operates on metadata only (not page data).

### Tier Selection

Each backend has a priority (latency tier). The policy engine selects the appropriate tier:

```
Tier 0: Local DRAM (managed by kernel, not duvm)
Tier 1: Compressed local memory (compress backend)
Tier 2: CXL-attached memory (cxl backend)
Tier 3: Remote DRAM via RDMA (rdma backend)
Tier 4: GPU HBM overflow (nvlink backend)
```

### Page Placement Strategy

On **store** (page being evicted from local DRAM):

1. **Hot page** (accessed recently, likely to be accessed again soon): Tier 1 (compressed local) — lowest latency for re-access.
2. **Warm page** (moderate access frequency): Tier 2 (CXL) — low latency, higher capacity.
3. **Cold page** (infrequent access): Tier 3 (RDMA) — highest capacity, acceptable latency.
4. **Overflow**: If preferred tier is full, cascade to next available tier.

### Hotness Tracking

The daemon maintains a **CLOCK-Pro** inspired access tracker:

```rust
struct PageMetadata {
    handle: PageHandle,           // where the page is stored
    backend_id: u8,               // which backend
    access_count: u32,            // access frequency (decayed)
    last_access: Instant,         // timestamp of last load
    flags: PageFlags,             // dirty, pinned, prefetched, etc.
}

// Compact: 24 bytes per tracked page.
// 1M pages (4GB) = 24MB of metadata. Fits in local RAM easily.
```

### Migration

The policy engine periodically scans metadata and migrates pages between tiers:

- **Promote**: If a page in Tier 3 (RDMA) is accessed frequently, migrate it to Tier 2 (CXL) or Tier 1 (compressed).
- **Demote**: If a page in Tier 1 hasn't been accessed, demote it to Tier 2 or 3.
- **Background migration**: Runs asynchronously. Doesn't block page faults.
- **Rate limiting**: Max N pages migrated per second to avoid network saturation.

### Prefetching

On sequential access patterns:

```
If page N is loaded, speculatively fetch pages N+1, N+2, ..., N+prefetch_depth
from the backend into local cache (staging buffer).
```

The prefetcher tracks access patterns per memory region:
- **Sequential**: stride-1 access. Prefetch the next `prefetch_depth` pages.
- **Strided**: stride-N access (e.g., column access in a matrix). Prefetch with stride.
- **Random**: No pattern detected. Disable prefetching for this region.

Pattern detection uses a simple stride predictor (like hardware prefetchers):
```
If (page_N - page_N-1) == (page_N-1 - page_N-2):
    stride = page_N - page_N-1
    prefetch page_N + stride, page_N + 2*stride, ...
```

---

## 7. User-Space Library: libduvm

For applications that want to go beyond transparent swap and optimize their memory usage.

### C API (for maximum language compatibility)

```c
#include <duvm.h>

// Allocate memory from the distributed pool.
// The returned pointer is valid in the local address space.
// Pages are faulted in on first access (lazy allocation).
void *duvm_alloc(size_t size, int flags);

// Free distributed memory.
void duvm_free(void *ptr);

// Hint: this memory region will be accessed soon. Prefetch it.
void duvm_prefetch(void *ptr, size_t size);

// Hint: this memory region is no longer needed. Evict it.
void duvm_evict(void *ptr, size_t size);

// Hint: place this allocation on a specific tier.
//   DUVM_TIER_LOCAL, DUVM_TIER_COMPRESSED, DUVM_TIER_CXL,
//   DUVM_TIER_RDMA, DUVM_TIER_GPU
void duvm_set_tier(void *ptr, size_t size, int tier);

// Pin memory locally (prevent migration).
void duvm_pin(void *ptr, size_t size);
void duvm_unpin(void *ptr, size_t size);

// Query: where is this page currently stored?
int duvm_get_tier(void *ptr);

// Query: current pool statistics.
struct duvm_stats duvm_get_stats(void);

// Flags for duvm_alloc:
#define DUVM_ALLOC_LOCAL    0x01  // prefer local DRAM
#define DUVM_ALLOC_REMOTE   0x02  // prefer remote memory
#define DUVM_ALLOC_GPU      0x04  // prefer GPU memory
#define DUVM_ALLOC_HUGE     0x08  // use huge pages (2MB)
```

### Rust API (native, zero-cost)

```rust
use duvm::{Pool, FarPtr, Tier};

let pool = Pool::connect()?;  // connect to local daemon

// Allocate a remoteable vector
let mut data: FarPtr<Vec<f64>> = pool.alloc_far(vec![0.0; 1_000_000])?;

// Access transparently (page faults handled automatically)
data[500] = 42.0;

// Explicit prefetch for performance
pool.prefetch(&data[1000..2000]);

// Explicit tier placement
pool.set_tier(&data, Tier::Rdma);

// The FarPtr can be serialized and shared with other processes
let token = data.export()?;  // produces a transferable token
```

### Implementation

libduvm uses `mmap` with `MAP_ANONYMOUS | MAP_NORESERVE` to create virtual address regions, then registers them with the daemon via a Unix domain socket. The daemon coordinates with the kernel module (or userfaultfd fallback) to intercept page faults on these regions.

---

## 8. GPU Memory Integration

GPU memory integration is the hardest part and is designed as a **phased approach**.

### Phase 1: GPU as a Storage Tier (CPU apps use GPU memory)

Cold CPU pages can be stored in idle GPU HBM via the NVLink backend:
- `cudaMalloc` allocates a pool on the GPU.
- Page store: `cudaMemcpy(Host -> Device)`.
- Page load: `cudaMemcpy(Device -> Host)`.
- Works today with standard CUDA APIs.

### Phase 2: GPU Memory Spill to Remote (GPU apps use remote CPU memory)

For CUDA applications that exceed GPU memory:
- Intercept `cudaMallocManaged` via LD_PRELOAD of the CUDA runtime.
- When GPU memory is exhausted, back new allocations with duvm-managed CPU memory.
- UVM page faults on the GPU trigger migration from remote memory.
- Requires coordination between NVIDIA's UVM driver and duvm-daemon.
- Implementation: Use NVIDIA's open-source GPU kernel modules to understand the UVM page fault path, then hook into it via a cuda memory advice call (`cudaMemAdvise`) or custom UVM policy.

### Phase 3: Unified CPU-GPU Distributed Memory

Long-term goal:
- duvm manages both CPU and GPU memory as part of the same pool.
- A page can live on local DRAM, CXL, remote RDMA, or any GPU's HBM.
- Migration decisions consider both CPU and GPU access patterns.
- Requires deep integration with NVIDIA's UVM or AMD's SVM (Shared Virtual Memory).

**Phase 3 is research-grade and out of scope for initial production. Phases 1 and 2 are practical today.**

---

## 9. Ring Buffer Protocol

The ring buffer is the critical fast-path between kernel module and daemon.

### Design

```
Shared memory region (mmap'd by both kernel module and daemon):

┌──────────────────────────────────────────────────────┐
│  Header (cache-line aligned, 64 bytes)               │
│  ┌────────────┬────────────┬────────────────────┐    │
│  │ write_idx  │ read_idx   │ flags, version     │    │
│  │ (kernel    │ (daemon    │                    │    │
│  │  writes)   │  writes)   │                    │    │
│  └────────────┴────────────┴────────────────────┘    │
├──────────────────────────────────────────────────────┤
│  Request Ring (N entries, each 64 bytes)              │
│  ┌──────┬──────┬──────┬─────────────────────────┐    │
│  │ op   │ pfn  │offset│ staging_slot │ flags    │    │
│  ├──────┼──────┼──────┼─────────────┼──────────┤    │
│  │ ...  │ ...  │ ...  │ ...         │ ...      │    │
│  └──────┴──────┴──────┴─────────────┴──────────┘    │
├──────────────────────────────────────────────────────┤
│  Completion Ring (N entries, each 64 bytes)           │
│  (daemon writes, kernel reads)                       │
├──────────────────────────────────────────────────────┤
│  Staging Buffer (M × 4KB pages)                      │
│  (page data copied here for transfer)                │
└──────────────────────────────────────────────────────┘
```

### Protocol

- **Store flow**: Kernel copies page to staging slot, writes request to ring. Daemon reads request, copies page from staging to backend, writes completion. Kernel reads completion, marks page as swapped.
- **Load flow**: Kernel writes request to ring. Daemon reads request, fetches page from backend to staging slot, writes completion. Kernel copies from staging to page table, wakes faulting process.
- **Lock-free**: Single producer (kernel) / single consumer (daemon) for requests. Reverse for completions. Uses `smp_wmb()` / `smp_rmb()` memory barriers. No locks, no CAS, no spinlocks.
- **Batching**: Kernel can submit up to 64 requests before signaling the daemon (via eventfd). Daemon processes them in a batch.

### Performance Target

- Ring buffer round-trip (kernel -> daemon -> kernel): **< 2us** (measured by similar io_uring designs).
- Bottleneck shifts to backend latency, not ring buffer overhead.

---

## 10. Fault Tolerance

### Daemon Crash Recovery

1. Kernel module detects daemon disconnection (eventfd goes stale).
2. Module enters **fallback mode**: all new swap-outs go to local swap device.
3. Pages already stored in backends are lost unless the page table still has them (clean pages can be re-read from filesystem; dirty pages in remote memory are lost).
4. Daemon restarts, re-registers with module, rebuilds page index from backends.

### Remote Memory Node Failure

For RDMA backends:
- **Erasure coding** (Carbink-style): Store parity shards across N memory servers. Tolerate F failures where F < N/2. Configurable redundancy level.
- **Replication** (simple mode): Store each page on 2 nodes. Tolerate 1 failure.
- **No redundancy** (performance mode): Fastest, but page loss on node failure.

Configuration per tier:
```toml
[backends.rdma]
redundancy = "erasure"       # none | replicate | erasure
erasure_data_shards = 4
erasure_parity_shards = 2    # tolerate 2 node failures
```

### Graceful Degradation Ladder

```
Full system operational
    │
    ▼ (daemon crash)
Kernel module falls back to local swap
    │
    ▼ (kernel module not loaded)
userfaultfd fallback (user-space only)
    │
    ▼ (duvm not installed)
Standard Linux memory management (no remote memory)
```

Each level degrades performance, never correctness.

---

## 11. Observability

### Metrics (Prometheus-compatible)

```
duvm_pages_stored_total{backend="rdma",tier="3"}
duvm_pages_loaded_total{backend="cxl",tier="2"}
duvm_page_fault_latency_us{quantile="p50"}
duvm_page_fault_latency_us{quantile="p99"}
duvm_backend_capacity_pages{backend="rdma"}
duvm_backend_used_pages{backend="rdma"}
duvm_backend_healthy{backend="rdma"}
duvm_ring_buffer_utilization_pct
duvm_prefetch_hit_rate
duvm_migration_pages_per_sec
duvm_fallback_events_total
```

### Logging

Structured logging (JSON) to stdout/journald. Levels: error, warn, info, debug, trace.

### CLI Tool: duvm-ctl

```bash
duvm-ctl status              # show daemon status, backends, cluster
duvm-ctl stats               # show real-time metrics
duvm-ctl backends            # list loaded backends and their capacity
duvm-ctl tier-map            # show how many pages are on each tier
duvm-ctl migrate --from rdma --to cxl --pages 1000
duvm-ctl config reload       # hot-reload configuration
```

---

## 12. Deployment Modes

### Mode 1: Full (kernel module + daemon)
Best performance. Requires root for module loading.
```bash
modprobe duvm-kmod
systemctl start duvm-daemon
```

### Mode 2: User-Space Only (daemon with userfaultfd)
No kernel module. Slightly higher latency (~5-8us per fault vs ~2us). No root needed (except for userfaultfd sysctl).
```bash
# sysctl vm.unprivileged_userfaultfd=1
duvm-daemon --mode=userfaultfd
```

### Mode 3: Library Only (no daemon, embedded mode)
For applications that want to manage their own remote memory directly via libduvm, without the daemon overhead.
```bash
# In application code:
let pool = duvm::Pool::standalone(config)?;
```

### Mode 4: Container Sidecar
Run duvm-daemon as a sidecar container sharing the PID namespace.
```yaml
# Kubernetes pod spec
containers:
  - name: app
    image: my-app
  - name: duvm
    image: duvm:latest
    securityContext:
      capabilities:
        add: ["SYS_PTRACE"]  # for userfaultfd
```

---

## 13. Data Flow: Life of a Page Fault

### Transparent Mode (unmodified application)

```
1. Application writes to virtual address 0x7fff1234000
2. Page not present in page table → CPU raises #PF
3. Kernel page fault handler invoked
4. Kernel checks if page was swapped out via duvm-kmod
5. duvm-kmod writes LOAD request to ring buffer
6. duvm-kmod signals daemon via eventfd
7. Daemon ring_reader thread wakes, reads request
8. Daemon looks up page in metadata index → stored on RDMA backend, node 10.0.1.2
9. Daemon issues RDMA READ (4KB) to remote node
10. RDMA NIC DMA's page data into staging buffer (~2us)
11. Daemon writes completion to ring buffer
12. Kernel module reads completion, copies staging → page table
13. Kernel restarts the faulting instruction
14. Application continues (unaware anything happened)

Total latency: ~5-10us (ring buffer ~2us + RDMA read ~2-3us + kernel overhead ~1-2us)
```

### Optimized Mode (application using libduvm)

```
1. Application calls duvm_prefetch(ptr, 64 * 4096)   // prefetch 64 pages
2. libduvm sends prefetch request to daemon (Unix socket)
3. Daemon issues 64 RDMA READs in a batch (pipelined)
4. Pages arrive and are placed in local cache
5. Application accesses ptr[0]..ptr[64*4096-1]
6. All accesses hit local cache → no page faults

Total latency per page: ~0 (prefetched)
Prefetch overhead: ~10-20us for the batch (amortized: ~0.2us per page)
```

---

## 14. Implementation Plan

### Phase 1: Foundation (MVP)

**Goal**: Unmodified application can use remote memory via RDMA, transparently.

1. Kernel module with frontswap backend + ring buffer
2. Daemon with ring buffer reader + single RDMA backend
3. Basic LRU page placement policy
4. duvm-ctl status command
5. Integration test: run redis/memcached with more data than local RAM, pages swap to remote RDMA memory

**"Done" condition**: `redis-benchmark` completes successfully with dataset larger than local RAM, using duvm for overflow to RDMA remote memory on a separate machine.

### Phase 2: Multi-Backend + Policy

**Goal**: Multiple backends, intelligent placement.

6. Compression backend
7. CXL backend
8. CLOCK-Pro hotness tracking
9. Tier-aware page placement
10. Background migration between tiers
11. Prefetching (sequential + strided)

**"Done" condition**: On a machine with CXL + RDMA, hot pages auto-migrate to CXL, cold pages to RDMA.

### Phase 3: Production Hardening

12. Fault tolerance (erasure coding for RDMA)
13. userfaultfd fallback (no kernel module mode)
14. Prometheus metrics + Grafana dashboards
15. Hot config reload
16. Comprehensive test suite (unit, integration, chaos)
17. Performance benchmarking suite

### Phase 4: libduvm + GPU

18. libduvm C/Rust API
19. NVLink backend (GPU as storage tier)
20. GPU memory spill (UVM integration)
21. Container deployment mode
22. Documentation and examples

### Phase 5: Advanced

23. eBPF policy hooks (when mainline supports it)
24. Huge page support (2MB, 1GB)
25. NUMA-aware placement
26. Multi-tenant isolation (cgroup integration)
27. Persistent memory support
