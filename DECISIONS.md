# Distributed UVM — Decisions and Considerations

This document consolidates all architectural decisions, trade-offs considered, and implementation considerations for the duvm project. It serves as a single reference for understanding *why* things are the way they are.

For the original detailed analysis, see `research/decisions.md`, `research/architecture-options.md`, and `research/architecture.md`.

---

## 1. Architecture: Hybrid Kernel + User-Space

**Decision:** A thin kernel module handles swap interception; all policy, backend management, and network I/O live in a user-space daemon written in Rust.

**Alternatives evaluated:**

| Direction | Verdict | Reason |
|-----------|---------|--------|
| Pure kernel module | Rejected | Kernel-version coupling, crash risk, monolithic, cannot host RDMA/CXL libraries safely |
| Pure user-space (userfaultfd) | Rejected | 4.7-7.8us overhead is 25-50x slower than CXL (200ns); LD_PRELOAD is fragile |
| **Hybrid kernel + user-space** | **Chosen** | Thin kernel relay (~300 LoC C) + full policy in Rust; best balance of performance, safety, and maintainability |
| eBPF-first | Deferred | mm hooks not in mainline yet (PageFlex ATC '25); adopt later as policy enhancement |
| Hypervisor-level | Rejected | Requires virtualization; excludes bare-metal, containers, HPC |

**Key insight:** The interception mechanism overhead (1.85-7.8us) is comparable to transport latency for fast backends (CXL 0.2-0.4us, RDMA 1-3us). Therefore the interception path is the critical bottleneck, not the network — justifying a thin kernel module.

**Graceful degradation chain:**
```
Full system (kmod + daemon + backends)
  → daemon crash: kernel module falls back to local page store
    → kmod not loaded: userfaultfd fallback (user-space only)
      → duvm not installed: standard Linux memory management
```

Each level degrades performance, never correctness.

---

## 2. Swap Interception: Virtual Block Device (not frontswap)

**Decision:** The kernel module implements a virtual block device (`/dev/duvm_swap0`) as a swap target using the blk-mq API.

**Why not frontswap:** frontswap was completely removed from Linux 6.17. zswap is now hardcoded in the core MM code with no pluggable interface.

**Why block device:**
- Proven approach: Infiniswap (NSDI '17) used exactly this pattern
- Stable API: `blk_mq_ops` is mature and well-documented
- Works with zswap: kernel compresses first, then writes to our device (free compression)
- Standard tooling: `mkswap`, `swapon -p 100`, `/proc/swaps` all work
- Easy prioritization: `swapon -p 100` gives higher priority than disk swap

**Current state:** Kernel module compiles for Linux 6.17 on aarch64. Local xarray-based page store provides standalone fallback when daemon is not connected. Ring buffer infrastructure exists for daemon communication.

---

## 3. Symmetric Node Architecture

**Decision:** Every node is equal — runs applications, provides memory to the cluster, and uses remote memory from other nodes.

**Rejected alternative:** Asymmetric (dedicated compute vs. memory nodes). The user requirement explicitly specified all nodes must be equal peers.

**Implication:** Each node runs `duvm-daemon` + `duvm-memserver`. When calc1 overflows, cold pages go to calc2's memserver, and vice versa. No single point of failure in the memory plane.

---

## 4. Backend Plugin Architecture

**Decision:** Backends implement a `DuvmBackend` trait with 11 methods. Each backend is a separate crate.

**Trait design considerations:**
- `alloc_page()` / `free_page()` — explicit lifecycle for backends that need it (RDMA registration, GPU pinning)
- `store_page()` / `load_page()` — synchronous 4KB page I/O; batched variants have default loop implementations
- `capacity()` — returns `(total, used)` for policy decisions
- `latency_ns()` — reported latency for tier selection
- `is_healthy()` — health check for failover
- `Send + Sync` bounds — backends must be thread-safe

**Tier hierarchy:**
```
Tier::Local      — kernel-managed DRAM (not a duvm backend)
Tier::Compressed — LZ4 local memory (~1-5us)
Tier::Cxl        — CXL-attached memory (~200-400ns) [planned]
Tier::Rdma       — Remote TCP/RDMA (~1-250us depending on transport)
Tier::Gpu        — GPU HBM (~0.3-5us) [planned]
```

**Implemented backends:**
| Backend | ID | Tier | Status |
|---------|---:|------|--------|
| memory | 0 | Compressed | Complete — HashMap reference impl |
| compress | 1 | Compressed | Complete — LZ4 with ratio tracking |
| tcp | 2 | Rdma | Complete — TCP client to memserver |

---

## 5. Policy Engine Design

**Decision:** LRU-based tier selection with per-page metadata tracking. CLOCK-Pro is planned for Phase 2.

**Current implementation:** The `PolicyEngine` tracks `PageMeta` (handle, backend_id, tier, access_count, last_access, flags) per page. It records stores and loads, updating access patterns.

**What was stubbed:** `select_tier()` returned a constant. This has been upgraded to consider backend availability and capacity, with LRU-based eviction candidates.

**Considerations:**
- Access counting uses `saturating_add` to prevent overflow
- `Instant::now()` timestamps enable age-based eviction
- `PageFlags` (DIRTY, PINNED, PREFETCHED, MIGRATING) are tracked but migration is not yet implemented
- Prefetch depth is configurable but prefetch logic is deferred to Phase 2

---

## 6. Ring Buffer Design

**Decision:** Lock-free SPSC (single-producer, single-consumer) ring buffer with cache-line-padded indices, io_uring-style.

**Layout:**
```
[RingHeader: 256 bytes]  — 4 indices with 60-byte cache padding each
[Request entries]         — N × 64 bytes (cache-aligned)
[Completion entries]      — N × 64 bytes (cache-aligned)
[Staging buffer]          — M × 4096 bytes (page data transfer area)
```

**Design choices:**
- 64-byte structs match cache line size → no false sharing
- Memory barriers (`smp_wmb`, `smp_rmb` in C; `Ordering::Acquire/Release` in Rust) for correctness on aarch64
- Power-of-2 capacity for efficient masking
- Separate request and completion rings (not interleaved) for spatial locality
- Staging buffer for page data transfer without extra copies

**Proven:** 671M push+pop/sec (1 ns/op) in benchmarks.

---

## 7. userfaultfd as Fallback

**Decision:** userfaultfd is maintained as code but is not required for standard deployment. The kernel module (block device) is the primary mechanism.

**Why keep it:**
- Works without kernel module (deployment flexibility)
- Useful for specific memory regions via libduvm API
- Proven at 22us/fault on aarch64

**Implementation note:** A C helper (`uffd_helper.c`) wraps userfaultfd ioctls because Rust's variadic function ABI is unreliable on aarch64 for ioctl calls.

---

## 8. Development Safety: QEMU/KVM

**Decision:** Develop and test the kernel module in QEMU/KVM VMs. Deploy to real hardware only after VM validation.

**Rationale:** DGX Spark has unified memory where OOM freezes the entire system. Kernel panics require physical power cycling. QEMU VM crashes restart in seconds.

---

## 9. Wire Protocol (TCP)

**Decision:** Simple binary request/response protocol over TCP for the memserver.

**Format:**
```
ALLOC:  client→[4]              server→[0][offset:8]
STORE:  client→[1][offset:8][data:4096]  server→[0]
LOAD:   client→[2][offset:8]    server→[0][data:4096]
FREE:   client→[3][offset:8]    server→[0]
STATUS: client→[5]              server→[0][used:8][total:8]
```

**Considerations:**
- TCP_NODELAY enabled for latency
- Single byte status (0=OK, 1=ERR) keeps parsing simple
- Little-endian encoding (matches aarch64 and x86_64 native)
- Per-connection page state (each client has isolated namespace)
- No authentication yet — designed for trusted network (ConnectX-7 direct cables)

---

## 10. C FFI for libduvm

**Decision:** Expose a minimal C API via cbindgen for applications that want explicit control.

**API surface:**
```c
void     duvm_init(void);
uint64_t duvm_store_page(const uint8_t *data);
int32_t  duvm_load_page(uint64_t handle, uint8_t *buf);
int32_t  duvm_free_page(uint64_t handle);
uint64_t duvm_capacity_total(void);
uint64_t duvm_capacity_used(void);
```

**Considerations:**
- Global singleton via `OnceLock` — simple but limits to one pool per process
- Null pointer checks on all inputs
- Error returns as negative integers (matching POSIX convention)
- cbindgen auto-generates `include/duvm.h` at build time

---

## 11. Configuration

**Decision:** TOML configuration file at `/etc/duvm/duvm.toml` with sensible defaults.

**Design:** All fields have defaults. Missing config file → full defaults. Invalid config → warn and use defaults. This means the daemon always starts, never fails due to configuration.

**Current configurable items:**
- `daemon.log_level`, `daemon.socket_path`, `daemon.metrics_port`
- `policy.strategy` (lru), `policy.prefetch_depth`
- `backends.memory.enabled`, `backends.memory.max_pages`
- `backends.compress.enabled`, `backends.compress.max_pages`

---

## Identified Gaps (addressed in this session)

| Gap | Description | Resolution |
|-----|-------------|------------|
| Stubbed policy | `select_tier()` always returned `Tier::Compressed` | Implemented real LRU with tier awareness and capacity checking |
| No capacity overflow | Backends bail when full, no fallback | Added cascading tier selection: try preferred → try others → error |
| No error stats | store_errors/load_errors never incremented | Wired error tracking into engine operations |
| Engine ignores TCP | TCP backend never initialized in Engine | Added configurable TCP backend support |
| Incomplete tier mapping | `tier_to_backend_id` only handled Compressed | Maps all tiers to available backends |
| No error-path tests | All tests were happy-path only | Added comprehensive error, concurrency, capacity, and negative-path tests |
| No config tests | Config parsing never tested | Added config loading, defaults, and validation tests |
| No daemon integration test | Engine.run() never tested | Added end-to-end daemon socket communication test |
