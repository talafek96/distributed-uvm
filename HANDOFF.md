# Distributed UVM — Project Handoff

## What This Is

Research and design exploration for a **unified abstraction layer for distributed memory** — a middleware that sits between existing applications and heterogeneous distributed memory backends (RDMA, CXL, NVLink, compressed local, etc.), providing seamless integration.

## Current State

**Phase: Research / Prior Art Analysis (complete)**

Extensive landscape research has been conducted across academic papers, commercial products, open-source projects, and industry specifications. Findings are documented in:

- `research/prior-art.md` — Full landscape of existing solutions (production systems, commercial products, research prototypes, industry specs)
- `research/gap-analysis.md` — Analysis of the specific gap this project targets: no existing system provides a pluggable, transport-agnostic, application-transparent distributed memory abstraction layer

## Key Finding

**The "distributed memory hypervisor" layer does not exist.** Every existing solution is locked to a specific transport, requires application changes, or only works within a single node/rack. No middleware provides:

1. Application transparency (no code changes) AND a rich opt-in API
2. Pluggable backends (RDMA, CXL, NVLink, compressed local)
3. Cross-machine distributed memory (not just intra-node)
4. GPU memory as a first-class citizen alongside CPU memory
5. Fault tolerance
6. Production readiness

See `research/gap-analysis.md` for the full breakdown.

## What's Next

- [ ] Define the architecture of the abstraction layer
- [ ] Identify which existing components can be reused vs. built from scratch
- [ ] Prototype a minimal viable layer (likely: userfaultfd + one backend)
- [ ] Decide on language, licensing, and project structure

## Key References

| Name | Type | URL |
|---|---|---|
| AIFM | Closest research prototype | https://github.com/AIFM-sys/AIFM |
| Intel UMF | Allocator framework (pluggable, but local-only) | https://github.com/oneapi-src/unified-memory-framework |
| Infiniswap | Transparent remote swap (RDMA-only) | https://github.com/SymbioticLab/Infiniswap |
| Samsung SMDK | CXL memory SDK | https://github.com/OpenMPDK/SMDK |
| OCP CMS | Industry spec for composable memory orchestration | https://www.opencompute.org/wiki/Server/CMS |
| "Pointers in Far Memory" | Vision paper describing the shim stack | https://queue.acm.org/detail.cfm?id=3606029 |
| Rcmp | Hybrid CXL+RDMA memory pooling | https://github.com/PDS-Lab/Rcmp |
