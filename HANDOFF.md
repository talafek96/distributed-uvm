# Distributed UVM — Project Handoff

## What This Is

**duvm** (Distributed Unified Virtual Memory) — a middleware that makes remote and heterogeneous memory transparently available to unmodified applications. Sits between applications and pluggable memory backends (RDMA, CXL, NVLink, compressed local).

## Current State

**Phase: Architecture Design (complete)**

### Documents

| Document | Purpose |
|---|---|
| `research/prior-art.md` | Landscape of 20+ existing solutions — production systems, commercial products, research prototypes, industry specs |
| `research/gap-analysis.md` | Analysis of the specific gap: no pluggable, transport-agnostic distributed memory middleware exists |
| `research/architecture-options.md` | Five candidate architectures evaluated with pros/cons + measured latency data |
| `research/architecture.md` | Complete architecture design for the chosen approach |

### Architecture Decision

**Chosen: Hybrid Kernel + User-Space** (see `architecture-options.md` for why)

Three components:
1. **duvm-kmod** — Thin kernel module (~1500 LoC C) for fast-path page interception via swap backend + ring buffer
2. **duvm-daemon** — User-space daemon (Rust) for policy, backend plugins, cluster management
3. **libduvm** — Optional user-space library (Rust + C FFI) for apps that want fine-grained control

Key properties:
- Unmodified apps get distributed memory transparently via kernel swap path
- Backends are pluggable .so files (RDMA, CXL, NVLink, compression)
- Falls back to userfaultfd if kernel module isn't available
- Falls back to local swap if daemon crashes
- GPU memory integration via phased approach

## What's Next

Implementation Phase 1 (MVP):
- [ ] Kernel module: frontswap backend + shared ring buffer
- [ ] Daemon: ring buffer reader + RDMA backend plugin
- [ ] Basic LRU page placement policy
- [ ] duvm-ctl status command
- [ ] Integration test: redis with dataset > local RAM, overflow to remote RDMA

**"Done" for Phase 1**: `redis-benchmark` completes with dataset larger than local RAM, using duvm for overflow to RDMA remote memory.

## Key Technical Decisions

| Decision | Choice | Why |
|---|---|---|
| Interception mechanism | frontswap / swap backend in kernel | userfaultfd adds 5-8us overhead — unacceptable for CXL-class latencies (200-400ns) |
| Daemon language | Rust | Memory safety without GC; no GC pauses in the memory manager itself |
| Kernel-user communication | Lock-free shared ring buffer | io_uring-style design, <2us round-trip |
| Backend interface | Shared library plugins (.so) | Runtime-loadable, independently developed, ~500 LoC each |
| Policy | CLOCK-Pro hotness tracking + tiered placement | Proven algorithm, low metadata overhead (24 bytes/page) |
| GPU integration | Phased (Tier 1: GPU as storage, Tier 2: GPU spill, Tier 3: unified) | Pragmatic — Phase 1-2 work today, Phase 3 is research |
| Fault tolerance | Erasure coding for RDMA (Carbink-inspired) | Configurable redundancy per tier |

## Key References

| Name | Relevance | URL |
|---|---|---|
| Infiniswap | Transparent RDMA swap (validates frontswap approach) | https://github.com/SymbioticLab/Infiniswap |
| ODRP | Frontswap + SmartNIC offloading (NSDI '25) | https://github.com/SJTU-IPADS/On-Demand-Remote-Paging |
| AIFM | Object-level far memory API (informs libduvm design) | https://github.com/AIFM-sys/AIFM |
| PageFlex | eBPF page fault delegation (future policy path) | USENIX ATC '25 |
| Eden | Developer-friendly far memory (NSDI '25) | USENIX NSDI '25 |
| Peter Xu uffd-wp measurements | userfaultfd latency data | https://xzpeter.org/userfaultfd-wp-latency-measurements/ |
| Linux frontswap docs | Kernel swap backend API | https://www.kernel.org/doc/html/next/mm/frontswap.html |
| CXL DAX driver docs | CXL memory mapping | https://docs.kernel.org/driver-api/cxl/allocation/dax.html |
