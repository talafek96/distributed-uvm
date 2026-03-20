# Architecture Options: Distributed UVM Middleware

This document evaluates five architectural directions for building the missing distributed memory abstraction layer. Each is analyzed on: transparency, performance, maintainability, deployability, and extensibility.

## Measured Latency Reference

Before evaluating options, grounding in measured latencies:

| Operation | Latency | Source |
|---|---|---|
| Local DRAM access | ~100ns | Hardware |
| CXL memory access (load/store) | ~200-400ns | DirectCXL (ATC '22) |
| userfaultfd missing page (local handler) | ~7.8us | makedist.com benchmark |
| userfaultfd-wp (threaded, poll-based) | ~4.74us | Peter Xu measurements |
| userfaultfd-wp (SIGBUS, single-thread) | ~1.85us | Peter Xu measurements |
| mprotect() + SIGSEGV | ~1.92us | Peter Xu measurements |
| RDMA one-sided read (4KB) | ~1-3us | libibverbs benchmarks |
| Infiniswap remote page fault | ~40us | Kona paper |
| LegoOS remote memory fetch | ~10us | LegoOS paper |
| Linux kernel swap-out path | ~5-20us | Kernel profiling |

**Key insight**: The interception mechanism overhead (1.85-7.8us) is comparable to or larger than the transport latency for fast backends (CXL: 0.2-0.4us, RDMA: 1-3us). This means the interception path is the critical bottleneck, not the network.

---

## Direction A: Pure Kernel Module

### Description
A loadable kernel module that hooks into the Linux memory management subsystem. Implements a `frontswap` backend for transparent swap interception and optionally hooks into the page demotion/promotion path for tiered memory. All transport backends compiled into or dynamically loaded by the module.

### Architecture
```
┌──────────────────────────────────────────┐
│           Applications (unmodified)      │
├──────────────────────────────────────────┤
│              Linux Kernel                │
│  ┌────────────────────────────────────┐  │
│  │      duvm.ko (kernel module)      │  │
│  │  ┌──────────┐  ┌───────────────┐  │  │
│  │  │frontswap │  │ page demotion │  │  │
│  │  │ backend  │  │   hooks       │  │  │
│  │  ├──────────┴──┴───────────────┤  │  │
│  │  │     Transport Backends      │  │  │
│  │  │  RDMA │ CXL │ Compress │... │  │  │
│  │  └─────────────────────────────┘  │  │
│  └────────────────────────────────────┘  │
├──────────────────────────────────────────┤
│         Hardware / Network               │
└──────────────────────────────────────────┘
```

### Precedent
Infiniswap, ODRP (NSDI '25).

### Pros
- **Best performance**: No user-kernel context switches on the fast path. Can serve pages directly from kernel context.
- **Full transparency**: Works through kernel swap. Zero application changes. Even kernel allocations can be offloaded.
- **Access to kernel internals**: Can hook into page reclaim, NUMA balancing, memory cgroups, LRU lists. Gets access to page hotness information that user-space cannot see.
- **Proven approach**: Infiniswap demonstrated this works for RDMA. ODRP improved on it with SmartNIC offloading.

### Cons
- **Kernel version coupling**: Must be maintained against each kernel version. API changes break things. Backporting is painful.
- **Hard to deploy**: Requires root, kernel module loading, potential secure boot issues. Enterprise/cloud customers often prohibit custom kernel modules.
- **Dangerous**: Bugs cause kernel panics, data corruption, security vulnerabilities. Testing is harder (no AddressSanitizer, no valgrind).
- **Transport backends are kernel-bound**: RDMA verbs have kernel drivers, but many CXL and NVLink paths are user-space APIs. Pulling them into kernel space is non-trivial or impossible.
- **Debugging difficulty**: printk, ftrace, and kprobes are the only tools. No GDB, no easy logging.
- **Monolithic**: Adding a new backend requires kernel module changes, testing, and redeployment. Not "pluggable" in the true sense.

### Verdict: **Rejected for primary architecture**
Performance is excellent, but the maintenance burden, deployment difficulty, and inability to easily use user-space transport APIs (CXL DAX, NVSHMEM) makes this unsuitable as the sole approach. However, a **thin kernel helper module** is valuable as an optional accelerator (see Direction C).

---

## Direction B: Pure User-Space (userfaultfd + LD_PRELOAD)

### Description
Entirely user-space. Uses `userfaultfd` to intercept page faults on registered memory regions. Uses `LD_PRELOAD` to intercept `malloc`/`mmap` and redirect allocations to managed regions. A daemon process handles fault resolution, policy, and backend communication.

### Architecture
```
┌──────────────────────────────────────────┐
│         Applications                     │
│    (LD_PRELOAD libduvm.so intercepts     │
│     malloc/mmap -> managed regions)      │
├──────────────────────────────────────────┤
│     libduvm.so (user-space library)      │
│  ┌──────────┐  ┌──────────────────────┐  │
│  │ allocator │  │ userfaultfd handler │  │
│  │ intercept │  │ (fault thread)      │  │
│  ├──────────┴──┴──────────────────────┤  │
│  │  duvm-daemon (policy + backends)   │  │
│  │  ┌──────────────────────────────┐  │  │
│  │  │   Backend Plugin Interface   │  │  │
│  │  │ RDMA │ CXL │ NVLink │ Zswap │  │  │
│  │  └──────────────────────────────┘  │  │
│  └────────────────────────────────────┘  │
├──────────────────────────────────────────┤
│              Linux Kernel                │
│         (standard, unmodified)           │
└──────────────────────────────────────────┘
```

### Precedent
AIFM, FluidMem, UMap (LLNL).

### Pros
- **No kernel changes**: Works on any standard Linux kernel >= 5.7 (for userfaultfd WP mode). Easy to deploy as a package.
- **Easy debugging**: Standard tools (GDB, valgrind, sanitizers, perf). Standard languages (C/C++/Rust).
- **Truly pluggable**: Backends are shared libraries loaded at runtime. Adding a new backend is writing a .so file.
- **Safe**: Bugs crash the daemon, not the kernel. Can be restarted. Can be sandboxed.
- **User-space transport APIs are native**: CXL via DAX mmap, NVSHMEM for GPU, libibverbs for RDMA — all user-space APIs.

### Cons
- **userfaultfd overhead**: Every page fault requires a context switch from faulting thread -> handler thread -> kernel -> back. Measured at 4.74us (threaded) or 7.8us (poll-based). This is **25-50x slower than a CXL load/store** (200ns).
- **LD_PRELOAD fragility**: Doesn't intercept all allocations (static allocations, custom allocators, Fortran, Go's runtime). Doesn't work for `exec()` calls or statically linked binaries. Some programs break with LD_PRELOAD.
- **No access to kernel page hotness**: Cannot see LRU position, page access bits, NUMA balancing hints. Must infer hotness from fault patterns only.
- **Incomplete transparency**: Programs that don't use malloc (e.g., use mmap directly, or GPU allocations via CUDA) aren't intercepted unless explicitly handled.
- **Scalability limits**: userfaultfd has one file descriptor per registered region. High-throughput page faulting saturates the handler thread. io_uring can help but adds complexity.

### Verdict: **Rejected as sole approach**
Excellent for development velocity and safety, but the userfaultfd overhead is a fundamental performance bottleneck for latency-sensitive workloads, and LD_PRELOAD-based transparency is too fragile for production. However, userfaultfd is the right **fallback mechanism** for deployments where kernel modules aren't available, and LD_PRELOAD is useful for quick prototyping.

---

## Direction C: Hybrid Kernel + User-Space (Recommended)

### Description
A thin, stable kernel module handles the fast-path page interception (frontswap + demotion hooks). A user-space daemon handles all policy decisions, backend management, and orchestration. Communication between them uses a shared-memory ring buffer (io_uring-style) for minimal overhead. A user-space library (`libduvm`) provides the opt-in rich API for applications that want fine-grained control.

### Architecture
```
┌─────────────────────────────────────────────────────────┐
│   Applications                                          │
│   ┌─────────────────┐  ┌────────────────────────────┐   │
│   │ Unmodified apps  │  │ Optimized apps             │   │
│   │ (transparent via │  │ (use libduvm API for       │   │
│   │  kernel swap +   │  │  prefetch hints, placement │   │
│   │  page demotion)  │  │  directives, far pointers) │   │
│   └────────┬────────┘  └─────────────┬──────────────┘   │
│            │                         │                   │
├────────────┼─────────────────────────┼───────────────────┤
│            │    User-Space           │                   │
│            │              ┌──────────▼──────────┐        │
│            │              │     libduvm.so      │        │
│            │              │  (rich API, far-ptr, │        │
│            │              │   prefetch, place)   │        │
│            │              └──────────┬──────────┘        │
│            │                         │                   │
│            │    ┌────────────────────▼──────────────┐    │
│            │    │         duvm-daemon               │    │
│            │    │  ┌────────────┐ ┌──────────────┐  │    │
│            │    │  │  Policy    │ │  Backend     │  │    │
│            │    │  │  Engine    │ │  Manager     │  │    │
│            │    │  │ (place,    │ │ (load .so    │  │    │
│            │    │  │  migrate,  │ │  plugins)    │  │    │
│            │    │  │  prefetch, │ ├──────────────┤  │    │
│            │    │  │  evict)    │ │ rdma.so      │  │    │
│            │    │  └────────────┘ │ cxl.so       │  │    │
│            │    │                 │ nvlink.so    │  │    │
│            │    │                 │ compress.so  │  │    │
│            │    │                 └──────────────┘  │    │
│            │    └────────────┬──────────────────────┘    │
│            │                 │                            │
│            │    ┌────────────▼──────────────────────┐    │
│            │    │  Shared Ring Buffer (mmap'd)      │    │
│            │    │  (page fault requests/responses,  │    │
│            │    │   migration commands, stats)      │    │
│            │    └────────────┬──────────────────────┘    │
│            │                 │                            │
├────────────┼─────────────────┼────────────────────────────┤
│            │    Kernel       │                            │
│   ┌────────▼─────────────────▼──────────────────────┐    │
│   │           duvm-kmod.ko (thin module)            │    │
│   │  ┌──────────────┐  ┌────────────────────────┐   │    │
│   │  │  frontswap   │  │  NUMA demotion/        │   │    │
│   │  │  backend     │  │  promotion hooks       │   │    │
│   │  ├──────────────┴──┴────────────────────────┤   │    │
│   │  │  Ring buffer (kernel side)               │   │    │
│   │  │  Fast-path page cache (optional)         │   │    │
│   │  └──────────────────────────────────────────┘   │    │
│   └─────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────┘
```

### Precedent
- ODRP uses frontswap + RNIC offloading
- Meta TMO uses kernel page reclaim with PSI (pressure stall information) feedback
- PageFlex (ATC '25) uses eBPF to delegate page policy to user-space
- KVM's virtio-balloon uses a similar kernel-module + user-space-daemon pattern

### Pros
- **Fast interception**: The kernel module catches page demotion/promotion events in-kernel with no user-space context switch for already-cached pages. Only cold-path events (actual remote fetches) go to user-space.
- **Thin kernel module**: The module is ~1000-2000 lines. It only does: frontswap store/load, ring buffer management, optional fast-path cache. All policy and transport logic lives in user-space. This is maintainable across kernel versions.
- **Full transparency**: Unmodified applications get distributed memory through the kernel swap path. The kernel decides what to demote/promote; the module intercepts and routes to user-space daemon.
- **Pluggable backends**: The daemon loads backend plugins (.so files) at runtime. Each plugin implements a simple interface: `connect()`, `read_page()`, `write_page()`, `disconnect()`. Adding RDMA, CXL, NVLink, or compression is a ~500-line .so.
- **Rich opt-in API**: `libduvm` provides far-memory pointers, prefetch hints, placement directives, and remoteable containers for applications that want to optimize. It communicates with the daemon via shared memory.
- **Graceful degradation**: If the kernel module isn't available, the system falls back to userfaultfd-based interception (slower but still functional). If the daemon crashes, the module falls back to local swap. Defense in depth.
- **Debuggable**: The daemon and library are standard user-space code. Only the thin kernel module needs kernel debugging tools.
- **eBPF-ready**: When PageFlex-style eBPF hooks become stable in mainline kernels, the system can use eBPF programs for in-kernel policy decisions (eliminating the user-space round-trip for policy), while keeping the daemon for backend management.

### Cons
- **Two components to deploy**: A kernel module + a daemon. More complex than pure user-space. But the kernel module is optional (fallback to userfaultfd), making deployment flexible.
- **Kernel module still needed for best performance**: Without the module, falls back to userfaultfd with its ~5-8us overhead. For CXL-class latencies (~200-400ns), the kernel module is essential.
- **Ring buffer design is critical**: The shared ring buffer between kernel and user-space is a latency-critical data structure. Must be lock-free, cache-line-aligned, and carefully designed to avoid contention.
- **Kernel module maintenance**: Even a thin module needs maintenance across kernel versions. The frontswap API was recently deprecated in favor of zswap's internal API in Linux 6.x. Must track these changes.

### Verdict: **RECOMMENDED**
This is the architecture that balances all requirements: transparent + opt-in, fast + maintainable, pluggable + deployable. The thin kernel module keeps the fast path in-kernel while pushing all complexity to user-space. The fallback to userfaultfd makes it deployable even without the kernel module.

---

## Direction D: eBPF-First

### Description
Use eBPF programs attached to page fault and paging event hooks (as in PageFlex, eBPF-mm) for in-kernel policy decisions. A user-space daemon manages backend connections. eBPF maps provide shared state between kernel and user-space.

### Architecture
```
┌─────────────────────────────────────────────┐
│          Applications (unmodified)          │
├─────────────────────────────────────────────┤
│   Kernel (with eBPF hooks)                  │
│   ┌───────────────────────────────────┐     │
│   │  eBPF programs:                   │     │
│   │   - page_fault_handler.bpf.c      │     │
│   │   - demotion_policy.bpf.c         │     │
│   │   - prefetch_hint.bpf.c           │     │
│   ├───────────────────────────────────┤     │
│   │  eBPF maps (shared state)         │     │
│   └──────────────┬────────────────────┘     │
├──────────────────┼──────────────────────────┤
│   User-Space     │                          │
│   ┌──────────────▼────────────────────┐     │
│   │  duvm-daemon (backends + config)  │     │
│   └───────────────────────────────────┘     │
└─────────────────────────────────────────────┘
```

### Precedent
PageFlex (ATC '25), eBPF-mm (2024), FetchBPF (ATC '24).

### Pros
- **No kernel module**: eBPF programs are verified and safe. No kernel panics. Easy to load/unload.
- **In-kernel speed**: eBPF runs in kernel context. Policy decisions happen without context switches.
- **Flexible**: eBPF programs can be updated at runtime without restarting the daemon or unloading modules.
- **Upstream friendly**: eBPF is a first-class Linux facility with a growing ecosystem.

### Cons
- **Too early**: PageFlex was published at ATC '25 (July 2025). The required kernel hooks (page fault tracepoints with write-back ability) are not in mainline Linux yet. Requires custom kernel patches (608 LoC of kernel changes in PageFlex).
- **eBPF limitations**: Cannot do complex operations (no loops over arbitrary bounds, limited stack, no sleeping, no blocking I/O). Cannot directly issue RDMA operations or mmap CXL memory from eBPF context. The actual data transfer must still happen in user-space or via a helper.
- **Not a full solution**: eBPF can decide *what* to do (which page to evict, where to place a page), but it cannot *do* the actual remote memory operation. Still needs a user-space daemon or kernel helper for transport. So it's really a policy-only enhancement, not a complete architecture.
- **Debugging**: eBPF programs are harder to debug than user-space code. verifier errors are cryptic.

### Verdict: **Rejected as primary, adopted as future policy enhancement**
eBPF is the right tool for in-kernel policy decisions once mainline support matures. In the hybrid architecture (Direction C), eBPF programs can optionally replace the user-space policy engine for the hot path. But it cannot replace the backends, daemon, or library. It's a component, not an architecture.

---

## Direction E: Hypervisor / VMM Level

### Description
Implement distributed memory at the hypervisor level (like VMware's Project Capitola). The hypervisor presents virtual machines with a unified memory view, transparently mapping guest physical memory to a pool that spans local DRAM, CXL, RDMA, and compressed tiers.

### Architecture
```
┌─────────────────────────────────────────────┐
│   VM 1         VM 2         VM 3            │
│   (guest OS)   (guest OS)   (guest OS)      │
├─────────────────────────────────────────────┤
│   Hypervisor (KVM/QEMU or custom)           │
│   ┌───────────────────────────────────┐     │
│   │  Memory disaggregation layer      │     │
│   │  (virtual NUMA nodes, hot/cold    │     │
│   │   tracking, transparent paging    │     │
│   │   to remote memory)              │     │
│   ├───────────────────────────────────┤     │
│   │  Backend transports               │     │
│   └───────────────────────────────────┘     │
├─────────────────────────────────────────────┤
│   Host kernel + hardware                    │
└─────────────────────────────────────────────┘
```

### Precedent
VMware Project Capitola, QEMU post-copy live migration (uses userfaultfd).

### Pros
- **Perfect transparency**: Guest OS and applications see normal memory. The hypervisor handles everything.
- **Multi-tenant isolation**: Each VM gets its own view of disaggregated memory. Security boundaries are hardware-enforced.
- **Cloud-native**: Cloud providers already run hypervisors. This fits their deployment model.

### Cons
- **Requires virtualization**: Bare-metal workloads (HPC, AI training) don't run in VMs. Containers don't use hypervisors (usually). This excludes a large fraction of target workloads.
- **Double paging**: Guest OS has its own page management. Hypervisor has another layer. Two levels of paging decisions can conflict, causing thrashing.
- **Hypervisor dependency**: Locked to KVM, Xen, or a custom hypervisor. Not portable.
- **GPU passthrough complexity**: GPUs are typically passed through to VMs via VFIO. The hypervisor has limited visibility into GPU memory and cannot easily integrate it into the disaggregated pool.
- **Not our niche**: VMware and cloud providers are better positioned to build this. We should target the bare-metal / container / HPC space where hypervisor-level solutions don't apply.

### Verdict: **Rejected**
The hypervisor approach is valid for cloud providers but excludes bare-metal, containers, and GPU-heavy workloads. It's being pursued by VMware and cloud hyperscalers already. The middleware layer we're building should work at the OS level, below or alongside VMs, not depend on them.

---

## Final Comparison Matrix

| Criterion | A: Kernel Module | B: User-Space | C: Hybrid (chosen) | D: eBPF-First | E: Hypervisor |
|---|---|---|---|---|---|
| **Transparency** | Excellent | Good (fragile) | Excellent | Excellent | Excellent |
| **Fast-path latency** | ~0 (in-kernel) | ~5-8us (uffd) | ~0 (in-kernel) | ~0 (in-kernel) | ~0 (in-VMM) |
| **Cold-path latency** | Low | Medium | Low-Medium | Low | Medium |
| **Maintainability** | Poor | Excellent | Good | Good | Poor |
| **Deployability** | Hard (root+module) | Easy (LD_PRELOAD) | Flexible (module optional) | Medium (custom kernel) | Hypervisor-only |
| **Pluggable backends** | Hard (kernel APIs) | Easy (user-space) | Easy (user-space) | Medium (eBPF limits) | Medium |
| **GPU support** | Very Hard | Possible | Possible | Very Hard | Very Hard |
| **Fault tolerance** | Module bugs = panic | Safe | Module: thin, daemon: safe | Safe | Hypervisor: critical |
| **Future-proof** | Kernel API churn | Stable | Kernel module thin + stable | Waiting on mainline | Hypervisor evolution |

---

## Decision: Direction C — Hybrid Kernel + User-Space

**Why this direction wins:**

1. **The latency math demands kernel interception for the fast path.** CXL memory is 200-400ns. userfaultfd adds 5-8us. That's a 15-25x overhead that makes pure user-space non-competitive for CXL-class latencies. But with a thin frontswap backend in-kernel, already-cached pages are served at kernel speed.

2. **All the complexity belongs in user-space.** Policy decisions (what to migrate, where to place, when to prefetch) change frequently and benefit from rich debugging, testing, and iteration. Backend transport code (RDMA verbs, CXL DAX mmap, NVSHMEM calls) are natively user-space APIs. Forcing these into kernel space would be engineering malpractice.

3. **The thin kernel module is maintainable.** It implements exactly two kernel interfaces: frontswap backend + optional NUMA demotion hooks. ~1500 lines of C. No transport logic, no policy logic. When frontswap APIs change (as they did in Linux 6.x), the module adapts. When transport libraries change, the daemon adapts. Changes are isolated.

4. **Graceful degradation preserves deployability.** Without the kernel module: fall back to userfaultfd (slower but works). Without the daemon: fall back to local swap (no remote memory but no crashes). Without any backend: compressed local memory still works. Each layer is independently optional.

5. **The eBPF path is a future upgrade, not a prerequisite.** When PageFlex-style hooks reach mainline Linux (likely 2026-2027), the daemon's policy engine can be optionally replaced by eBPF programs for zero-context-switch policy decisions. The architecture is designed to accommodate this.
