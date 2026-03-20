# Distributed UVM — Project Handoff

## What This Is

**duvm** (Distributed Unified Virtual Memory) — a middleware that makes remote and heterogeneous memory transparently available to unmodified applications. Sits between applications and pluggable memory backends (RDMA, CXL, NVLink, compressed local).

## Current State

**Phase: Implementation Phase 1 (in progress — core framework complete)**

### What's Built

| Component | Status | Location |
|---|---|---|
| **duvm-common** | Complete | `crates/duvm-common/` — PageHandle, ring buffer, protocol, stats |
| **duvm-backend-trait** | Complete | `crates/duvm-backend-trait/` — Backend plugin interface |
| **duvm-backend-memory** | Complete | `crates/duvm-backend-memory/` — In-memory reference backend |
| **duvm-backend-compress** | Complete | `crates/duvm-backend-compress/` — LZ4 compression backend |
| **duvm-daemon** | Complete | `crates/duvm-daemon/` — Policy engine, backend management, Unix socket control |
| **duvm-ctl** | Complete | `crates/duvm-ctl/` — CLI tool (status, stats, backends, ping) |
| **libduvm** | Complete | `crates/libduvm/` — Rust API + C FFI (cbindgen-generated header) |
| **duvm-kmod** | Complete | `duvm-kmod/` — Kernel module compiles against Linux 6.17 |
| **Integration tests** | Complete | `crates/duvm-tests/` — 9 integration tests |

### Quality Gates

All pass:
- `cargo fmt --check` — clean
- `cargo clippy -D warnings` — zero warnings
- `cargo test` — **42 tests passing** (33 unit + 9 integration)
- `make kmod` — kernel module compiles (`duvm-kmod.ko`)

### Documents

| Document | Purpose |
|---|---|
| `README.md` | User-facing guide: quick start, project structure, crate guide, config |
| `research/prior-art.md` | Landscape of 20+ existing solutions |
| `research/gap-analysis.md` | Analysis of the specific gap this project fills |
| `research/architecture-options.md` | Five candidate architectures evaluated with pros/cons |
| `research/architecture.md` | Complete architecture design for the chosen approach |

## How to Build and Test

```bash
make build          # Build all Rust crates
make test           # Run all 42 tests
make check          # Format + lint + test
make kmod           # Build kernel module
```

## What's Next

Implementation Phase 1 (remaining):
- [ ] Wire userfaultfd path into daemon for end-to-end transparent page fault handling
- [ ] Kernel module swap hooks (frontswap/zswap, currently stub)
- [ ] RDMA backend plugin
- [ ] Integration test: redis with dataset > local RAM, overflow to remote memory

Implementation Phase 2:
- [ ] CXL backend plugin
- [ ] CLOCK-Pro hotness tracking (currently LRU)
- [ ] Background migration between tiers
- [ ] Prefetching (sequential + strided)
- [ ] Prometheus metrics endpoint

## Key Technical Decisions

| Decision | Choice | Why |
|---|---|---|
| Interception mechanism | frontswap / swap backend in kernel | userfaultfd adds 5-8us overhead — unacceptable for CXL-class latencies (200-400ns) |
| Daemon language | Rust | Memory safety without GC; no GC pauses in the memory manager itself |
| Kernel-user communication | Lock-free shared ring buffer | io_uring-style design, <2us round-trip |
| Backend interface | Trait-based plugins | Runtime-loadable, independently developed, ~500 LoC each |
| Policy | LRU now, CLOCK-Pro planned | Start simple, upgrade when multi-tier placement matters |
| Kernel module | Thin relay, no policy | All complexity in user-space; module is ~300 LoC C |
| Fault tolerance | Erasure coding for RDMA (planned) | Carbink-inspired, configurable redundancy per tier |

## Key References

| Name | Relevance | URL |
|---|---|---|
| Infiniswap | Transparent RDMA swap (validates frontswap approach) | https://github.com/SymbioticLab/Infiniswap |
| ODRP | Frontswap + SmartNIC offloading (NSDI '25) | https://github.com/SJTU-IPADS/On-Demand-Remote-Paging |
| AIFM | Object-level far memory API (informs libduvm design) | https://github.com/AIFM-sys/AIFM |
| PageFlex | eBPF page fault delegation (future policy path) | USENIX ATC '25 |
| Peter Xu uffd-wp measurements | userfaultfd latency data | https://xzpeter.org/userfaultfd-wp-latency-measurements/ |
| Linux frontswap docs | Kernel swap backend API | https://www.kernel.org/doc/html/next/mm/frontswap.html |
| CXL DAX driver docs | CXL memory mapping | https://docs.kernel.org/driver-api/cxl/allocation/dax.html |
