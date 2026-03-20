# Gap Analysis: The Missing Distributed Memory Abstraction Layer

## The Gap

No existing system provides a **pluggable, transport-agnostic middleware layer** that sits between applications and distributed memory backends, offering both transparent operation (for legacy apps) and a rich API (for apps that want to optimize).

Every existing solution locks into a specific position on the stack and makes hard trade-offs:

```
┌─────────────────────────────────────────────────┐
│  Applications                                   │
│  (unmodified binaries, CUDA, JVM, Python, ...)  │
├─────────────────────────────────────────────────┤
│                                                 │
│           ??? THE MISSING LAYER ???             │
│                                                 │
│  Unified API + transparent fallback             │
│  Pluggable backends                             │
│  Policy engine (what goes where)                │
│                                                 │
├──────────┬──────────┬───────────┬───────────────┤
│  RDMA    │  CXL     │  NVLink   │  Compressed   │
│  (IB,    │  (2.0,   │  (intra-  │  Local        │
│  RoCE)   │  3.0+)   │  rack)    │  (zswap,SSD)  │
├──────────┴──────────┴───────────┴───────────────┤
│  Physical memory across cluster                 │
└─────────────────────────────────────────────────┘
```

## Why Each Existing Solution Falls Short

### Transparent but not pluggable

| System | Problem |
|---|---|
| **Infiniswap** | Kernel module, RDMA-only. Can't add CXL or NVLink backends. No GPU memory. |
| **Meta TMO** | Local-only (compressed memory / SSD). Not distributed. |
| **Google Far Memory** | Local-only (compression). Not distributed. |
| **NVIDIA UVM** | Single-node only (CPU <-> GPU). Not distributed across machines. |

These systems are transparent to applications, but each is locked to one transport and one scope.

### Rich API but tightly coupled

| System | Problem |
|---|---|
| **AIFM** | Requires annotating allocations as remoteable. Locked to RDMA/Shenango. |
| **FaRM** | Must use FaRM transaction API. Locked to RDMA + NVRAM. |
| **NVSHMEM** | Must use PGAS put/get API. Locked to NVLink/InfiniBand. |
| **Rcmp** | Must use Read/Write/CAS API. Locked to CXL + RDMA (better, but still rigid). |

These offer programming models for distributed memory but require code changes and are coupled to specific transports.

### Framework but not distributed

| System | Problem |
|---|---|
| **Intel UMF** | Pluggable providers, but local memory only. No remote/distributed support. |
| **Samsung SMDK** | CXL memory SDK. Local node only. |
| **emucxl** | CXL emulation API. Single node. No real backends. |

These have the right architectural instinct (pluggable, abstract) but only work within a single machine.

### Distributed but requires a new OS/runtime

| System | Problem |
|---|---|
| **LegoOS** | A complete new OS. Can't run existing applications unmodified. |
| **Semeru** | JVM-only. Requires Java. |
| **Kona** | Assumes specific hardware primitives. Research prototype. |

These prove distributed memory works but require replacing the entire software stack.

### Orchestration but not data-plane

| System | Problem |
|---|---|
| **OCP CMS / CFM** | Control plane for composable memory. Manages pools. Doesn't provide application API or runtime. |
| **Liqid Matrix** | Composes hardware resources. No application-facing memory abstraction. |

These manage the infrastructure but don't solve how applications actually use the memory.

## The Properties No Single System Has

A complete solution would need ALL of these:

### 1. Dual-mode interface
- **Transparent mode**: Applications use `malloc()`, `mmap()`, or CUDA allocations unchanged. The system intercepts at the page-fault or allocator level and serves memory from the distributed pool.
- **Explicit mode**: Applications that want fine-grained control use a rich API (remoteable pointers, prefetch hints, placement directives, object-level operations).

No existing system offers both modes through the same middleware.

### 2. Pluggable transport backends
The middleware should abstract over:
- **RDMA** (InfiniBand, RoCE) — for cross-rack, cross-datacenter
- **CXL** (2.0, 3.0+) — for intra-rack, low-latency coherent access
- **NVLink** — for GPU-to-GPU within NVLink domains
- **Compressed local** — for local memory tiering (zswap, SSD-backed)
- **Future transports** — pluggable interface allows new backends without changing the API

No existing system supports more than 2 of these, and none are designed to add new ones.

### 3. GPU memory as first-class citizen
- GPU memory (HBM, VRAM) should be addressable in the same distributed pool as CPU DRAM.
- CUDA/HIP allocations should be able to spill to remote CPU memory or other GPUs.
- CPU applications should be able to use GPU memory as a fast tier.

Only NVIDIA's own stack (UVM, NVLink, NVSHMEM) touches GPU memory, and it's all proprietary and hardware-locked.

### 4. Policy engine
- Decides what data goes where based on access patterns, latency requirements, cost, and locality.
- Application-transparent by default, but accepts hints from applications.
- Handles: placement, migration, prefetching, eviction, replication.

Google's Far Memory and Meta's TMO have sophisticated local policies. AIFM has application-informed policies. But no system has a pluggable policy engine that works across heterogeneous distributed backends.

### 5. Fault tolerance
- Remote memory nodes will fail. The system must handle this transparently.
- Carbink (erasure coding) and Hydra (resilient remote memory) provide the building blocks, but they're standalone systems, not integrated into a middleware.

### 6. Production-grade
- Performance: Local memory access must remain fast. Remote access should be optimized via caching, prefetching, and batching.
- Reliability: Crash recovery, graceful degradation, monitoring.
- Operations: Easy deployment, configuration, observability.

## Architectural Positioning

The gap exists because existing work is vertically integrated — each system owns the full stack from application interface to transport. The missing piece is a **horizontal layer** that decouples the interface from the transport:

```
EXISTING APPROACH (vertical silos):

  App──►Infiniswap──►RDMA     App──►UVM──►NVLink     App──►TMO──►SSD
  (each is a complete, closed system)

MISSING APPROACH (horizontal layer):

  App ──► [Unified Memory Layer] ──► RDMA
                                 ──► CXL
                                 ──► NVLink
                                 ──► Compressed Local
                                 ──► (future transport)
```

## Closest Existing Building Blocks

If building this layer, these components could be reused or learned from:

| Component Need | Best Existing Solution | What to Reuse |
|---|---|---|
| Transparent page-fault interception | `userfaultfd` (Linux kernel), UMap (LLNL) | Mechanism for intercepting page faults in user-space |
| Object-level far memory API | AIFM | Smart pointer design, remoteable containers, prefetching |
| Pluggable allocator architecture | Intel UMF | Provider/pool abstraction pattern |
| RDMA transport | Infiniswap, AIFM (Shenango) | One-sided RDMA slab management |
| CXL transport | Samsung SMDK, Rcmp | CXL memory mapping and access |
| Fault tolerance | Carbink, Hydra | Erasure coding for remote memory |
| Policy/placement | Meta TMO, Google Far Memory | Page hotness tracking, cold page identification |
| GPU memory management | NVIDIA UVM driver (open source parts) | Page migration between CPU/GPU |
| Orchestration/control plane | OCP CMS/CFM | Pool management, fabric discovery |

## Key Design Questions for the Missing Layer

1. **Where in the stack?** Kernel module (most transparent, hardest to deploy) vs. user-space library (easier to deploy, requires linking or LD_PRELOAD) vs. hypervisor-level (transparent to VMs)?

2. **Granularity?** Page-level (transparent, amplification issues) vs. object-level (efficient, requires API changes) vs. both?

3. **Coherence model?** Strict consistency (expensive, correct) vs. relaxed (fast, application must tolerate staleness)?

4. **GPU integration approach?** Hook into UVM driver? Use HMM? Separate GPU-specific path?

5. **How to handle the latency gap?** Local DRAM: ~100ns. CXL: ~200-400ns. RDMA: ~1-40us. Each backend has fundamentally different performance characteristics. How does the policy engine handle this?

## Conclusion

The distributed memory abstraction layer is an **identified gap** in the ecosystem. The building blocks exist (userfaultfd, RDMA verbs, CXL drivers, UVM, erasure coding), the vision has been articulated ("Pointers in Far Memory"), and the industry is converging on CXL as the hardware standard. But nobody has assembled these pieces into a unified middleware that applications can use seamlessly across heterogeneous memory backends.

This is the opportunity for the distributed-uvm project.
