# Prior Art: Memory Disaggregation & Distributed UVM

Research conducted March 2026. Covers production systems, commercial products, open-source projects, academic research, and industry specifications.

---

## Table of Contents

1. [Production-Deployed Systems (Hyperscalers)](#1-production-deployed-systems-hyperscalers)
2. [Commercial Products & Startups](#2-commercial-products--startups)
3. [Major Vendor Software Stacks](#3-major-vendor-software-stacks)
4. [Academic / Research Systems](#4-academic--research-systems)
5. [NVIDIA UVM Ecosystem](#5-nvidia-uvm-ecosystem)
6. [Industry Specifications & Standards](#6-industry-specifications--standards)
7. [Abstraction Layer Attempts](#7-abstraction-layer-attempts)
8. [Summary Table](#8-summary-table)

---

## 1. Production-Deployed Systems (Hyperscalers)

### Google — Software-Defined Far Memory (ASPLOS '19)

- **What**: Proactively compresses cold memory pages to create a far memory tier entirely in software. No hardware changes required.
- **Deployment**: Google warehouse-scale computers, production.
- **Results**: 67%+ memory cost reduction at ~6us access latency for cold data. ~20% of data stored in far memory on average.
- **Transport**: Local compression (zswap-like). Not distributed across machines.
- **Paper**: Lagar-Cavilla et al., "Software-Defined Far Memory in Warehouse-Scale Computers," ASPLOS 2019.
- **Relevance**: Proves software-only memory tiering works at scale, but local-only.

### Google — Carbink (OSDI '22)

- **What**: Fault-tolerant far memory using erasure coding + one-sided RDMA.
- **Key contribution**: Addresses the missing piece in prior disaggregated memory work — what happens when a remote memory node fails.
- **Mechanism**: Remote memory compaction, offloadable parity calculations, erasure coding.
- **Transport**: RDMA.
- **Paper**: Zhou et al., "Carbink: Fault-Tolerant Far Memory," OSDI 2022.
- **Relevance**: Critical for any production distributed memory system. Fault tolerance is non-negotiable.

### Meta — TMO: Transparent Memory Offloading (ASPLOS '22)

- **What**: Kernel-level, application-transparent memory offloading to compressed memory or SSDs.
- **Deployment**: Production since 2021 across millions of servers at Meta.
- **Results**: Saves 20-32% of total memory per server.
- **Mechanism**: Automatically identifies cold pages and offloads them. Holistic — covers application containers and sidecar containers.
- **Transport**: Local (compressed memory, SSD). Not distributed.
- **Upstream**: Upstreamed to the Linux kernel.
- **Paper**: Weiner et al., "TMO: Transparent Memory Offloading in Datacenters," ASPLOS 2022.
- **Relevance**: Gold standard for application-transparent memory management. But local-only.

### Microsoft Azure — Pond (ASPLOS '23, Distinguished Paper)

- **What**: First CXL-based memory pooling system designed for public cloud platforms.
- **Key insight**: Analysis of Azure production traces showed pooling across 8-16 sockets captures most benefits. Enables small-pool design with low access latency.
- **Status**: Software simulation and design study. Targets CXL-era hardware.
- **Paper**: Li et al., "Pond: CXL-Based Memory Pooling Systems for Cloud Platforms," ASPLOS 2023.
- **Code**: https://github.com/MoatLab/Pond
- **Relevance**: Defines how cloud providers should think about CXL memory pooling. Not a middleware.

---

## 2. Commercial Products & Startups

### MemVerge

- **Founded**: 2018, VC-backed (Crunchbase).
- **Product**: Memory Machine (now MemMachine).
- **Original tech**: Combined DRAM + Intel Optane into unified memory pool using patented DMO (Dynamic Memory Orchestration) technology.
- **Current direction**: CXL-based memory tiering/pooling; AI workload optimization.
  - **GISMO**: Software-defined elastic CXL memory for AI inference KV cache offloading. Partnered with XConn (CXL switches).
  - **MemMachine AI**: Expanded into AI agent memory systems.
- **Partners**: XConn, Liqid, Intel, Tencent Cloud.
- **URL**: https://memverge.com/
- **Relevance**: Closest to a "memory hypervisor" commercial product, but focused on CXL/Optane tiering within a node or small pool. Not a general-purpose distributed middleware.

### Liqid

- **Product**: Liqid Matrix — Composable Disaggregated Infrastructure (CDI) software.
- **Capabilities**: Composes GPUs, FPGAs, NVMe, NICs, and CXL 2.0 memory (up to 100TB disaggregated per node).
- **Transports**: PCIe, CXL, Ethernet, InfiniBand.
- **Partners**: Samsung, XConn, NVIDIA.
- **URL**: https://www.liqid.com/
- **Relevance**: Orchestration-level composability. Software composes hardware resources but doesn't provide a programming abstraction or runtime for applications.

### TORmem

- **What**: Startup building a rack-scale memory disaggregation platform for AI infrastructure.
- **Tech**: CXL 2.0 + PCIe Gen5. Built on IBM OpenCAPI Memory Interface (OMI) heritage.
- **Partnership**: ASUS for enterprise deployment.
- **Status**: PoC deployments (as of mid-2025).
- **URL**: https://www.tormem.com/
- **Relevance**: Hardware-software platform, not a middleware layer.

### Elastics.cloud

- **What**: CXL-based memory disaggregation and pooling solution.
- **Status**: Demo/PoC stage. Demonstrated local memory expansion via CXL.
- **Relevance**: Early-stage, CXL-only.

### Astera Labs

- **Product**: Leo CXL Smart Memory Controller — first purpose-built CXL silicon for memory expansion + pooling.
- **Status**: Shipped to hyperscaler customers.
- **URL**: https://www.asteralabs.com/
- **Relevance**: Hardware silicon with management software. Enables CXL memory pooling but doesn't provide an application-facing abstraction.

### GigaIO

- **Product**: FabreX — software-first composable infrastructure platform.
- **Capabilities**: Enables configurations like 32 GPUs in a single node via PCIe fabric composability.
- **URL**: https://gigaio.com/
- **Relevance**: Infrastructure composition, not application-level memory abstraction.

---

## 3. Major Vendor Software Stacks

### Samsung — SMDK (Scalable Memory Development Kit)

- **What**: Open-source full-stack software solution for CXL memory platforms.
- **Code**: https://github.com/OpenMPDK/SMDK
- **Features**: Libraries and APIs for heterogeneous memory (DRAM + CXL) to work together seamlessly. Memory virtualization support.
- **License**: Open source.
- **Roadmap**: Targets full-stack "Software-Defined Memory" systems, roadmap to rack-level CXL disaggregation.
- **Relevance**: CXL-specific SDK. Local node only (no distributed/remote memory). Useful as a potential backend provider but not the middleware layer itself.

### VMware Research — Kona & Project Capitola

- **Kona** (ASPLOS '21): Software runtime for disaggregated memory. Improves access time by 1.7-5x vs. state-of-art. Consists of a runtime library (KLib) and daemon processes.
  - Paper: Calciu et al., "Rethinking Software Runtimes for Disaggregated Memory," ASPLOS 2021.
  - Code: https://github.com/project-kona/asplos21-ae
- **Project Capitola**: VMware's effort to make ESXi a "disaggregated memory hypervisor." Aggregates DRAM + PMEM for VMs.
- **Logical Memory Pools** (HotNets '23): CXL-era architecture proposal — carve out parts of local memory from each server to create shared pools.
  - Paper: Amaro et al., "Logical Memory Pools: Flexible and Local Disaggregated Memory," HotNets 2023.
- **Relevance**: Kona is the most relevant as a runtime design, but assumes specific hardware primitives and is a research prototype.

---

## 4. Academic / Research Systems

### Infiniswap (Michigan / SymbioticLab, NSDI '17)

- **What**: Remote memory paging system over RDMA. Transparently exposes unused cluster memory as swap space.
- **Mechanism**: Linux block device. Divides swap space into slabs distributed across machines. One-sided RDMA bypasses remote CPUs. Decentralized placement via power-of-two-choices.
- **Transparency**: Fully application-transparent (kernel swap device).
- **Transport**: RDMA only.
- **Code**: https://github.com/SymbioticLab/Infiniswap
- **Paper**: Gu et al., "Efficient Memory Disaggregation with Infiniswap," NSDI 2017.
- **Relevance**: Proves transparent remote memory works. But kernel module, RDMA-only, no pluggable backends, no GPU support.

### Leap (SymbioticLab)

- **What**: Prefetching and efficient data path for memory disaggregation. Extends Infiniswap ideas.
- **Code**: https://github.com/SymbioticLab/Leap (69 stars)
- **Relevance**: Improves on Infiniswap's data path but same limitations.

### LegoOS (UCSD, OSDI '18)

- **What**: Full distributed OS for hardware resource disaggregation. "Splitkernel" — splits OS functions across compute, memory, and storage components.
- **Mechanism**: Each resource type independently managed/scaled. ~10us remote memory latency.
- **Paper**: Shan et al., "LegoOS: A Disseminated, Distributed OS for Hardware Resource Disaggregation," OSDI 2018.
- **Relevance**: Radical approach — a new OS. Not a middleware layer for existing applications.

### AIFM — Application-Integrated Far Memory (MIT/Brown, OSDI '20)

- **What**: Exposes far memory as `far-memory pointers` and `remoteable containers` at the C++ language level. Runtime handles swapping objects in/out, prefetching, memory evacuation.
- **Key insight**: Exposing application-level semantics to the runtime enables efficient remoteable memory.
- **Mechanism**: Green threads, pauseless memory evacuator. Object-granularity (not page-granularity).
- **Transport**: RDMA (over Shenango runtime).
- **Code**: https://github.com/AIFM-sys/AIFM (124 stars, MIT license)
- **Paper**: Ruan et al., "AIFM: High-Performance, Application-Integrated Far Memory," OSDI 2020.
- **Relevance**: Closest to a rich application-facing API for far memory. But requires code changes (annotate remoteable allocations), single transport backend, research prototype.

### Semeru (UCLA, OSDI '20)

- **What**: Distributed JVM that transparently uses disaggregated memory. One CPU server + multiple memory servers.
- **Mechanism**: Co-designs with Java GC — remote memory managed alongside garbage collection.
- **Code**: https://github.com/uclasystem/Semeru
- **Paper**: Wang et al., "Semeru: A Memory-Disaggregated Managed Runtime," OSDI 2020.
- **Relevance**: Transparent for Java apps, but JVM-only. Tightly coupled to GC semantics. Not general-purpose.

### Hydra (FAST '22)

- **What**: Resilient remote memory with single-digit us read/write using erasure coding + RDMA.
- **Results**: 1.6x lower memory overhead vs. in-memory replication with similar performance.
- **Paper**: Lee et al., "Hydra: Resilient and Highly Available Remote Memory," FAST 2022.
- **Relevance**: Key contribution to fault-tolerant remote memory. Could be a component in a larger system.

### Clio (ASPLOS '22)

- **What**: Hardware-software co-designed disaggregated memory system. Clean-slate design with hardware-based memory nodes.
- **Paper**: Guo et al., "Clio: A Hardware-Software Co-Designed Disaggregated Memory System," ASPLOS 2022.
- **Relevance**: Co-design approach. Requires custom hardware.

### Rcmp (ACM TACO '23)

- **What**: Hybrid CXL + RDMA memory pooling. CXL for coherent intra-rack access, RDMA for cross-rack.
- **Features**: Page-based global memory management. Unified Read/Write/CAS API.
- **Code**: https://github.com/PDS-Lab/Rcmp (64 stars)
- **Paper**: Wang et al., "Rcmp: Reconstructing RDMA-based Memory Disaggregation via CXL," TACO 2023.
- **Relevance**: Multi-transport (CXL + RDMA), but user-space library requiring API usage, not transparent.

### UniMem (USENIX ATC '24)

- **What**: Cache-coherent disaggregated memory with unified local-remote memory hierarchy. Removes extra indirection on remote memory access path.
- **Features**: Redesigned local cache to prevent thrashing/pollution. Page migration for hot pages.
- **Status**: Simulation-only (Intel Pin tool).
- **Code**: https://github.com/yijieZ/UniMem
- **Paper**: Zhong et al., "UniMem: Redesigning Disaggregated Memory within A Unified Local-Remote Memory Hierarchy," ATC 2024.
- **Relevance**: Optimizes the memory hierarchy, but not a middleware layer. Simulation only.

### ThymesisFlow (IBM, MICRO '20)

- **What**: Software-defined HW/SW co-designed interconnect for rack-scale disaggregation on POWER9 with OpenCAPI.
- **Code**: https://github.com/OpenCAPI/ThymesisFlow (38 stars, Apache-2.0)
- **Relevance**: POWER9/OpenCAPI specific. Demonstrates HW/SW co-design.

### FluidMem

- **What**: Open memory disaggregation framework.
- **Code**: https://github.com/blakecaldwell/fluidmem (25 stars)
- **Relevance**: Small research project.

### DiLOS (KAIST, EuroSys '23)

- **What**: OS for memory disaggregation that preserves compatibility while achieving performance.
- **Code**: https://github.com/ANLAB-KAIST/dilos (20 stars)
- **Relevance**: OS-level, not middleware.

### CUDA-DTM (LSU, NetSys '19)

- **What**: Distributed Transactional Memory for GPU clusters. Threads across GPUs access a coherent shared address space via transactional memory.
- **Results**: Tested on 256 GPU devices, up to 115x faster than CPU clusters.
- **Paper**: Workshop paper from LSU, 2019.
- **Relevance**: Research prototype only. Not maintained. Not a "primary contender" despite some claims. Interesting as a proof of concept for GPU distributed shared memory.

### ArgoDSM

- **What**: Page-based software distributed shared memory (DSM) system.
- **Code**: https://github.com/etascale/argodsm (45 stars, actively maintained)
- **Relevance**: Classic DSM approach. CPU-only, no GPU support. Research/HPC focused.

### FaRM (Microsoft Research, NSDI '14)

- **What**: Exposes cluster memory as a shared address space with location-transparent object access via transactions.
- **Mechanism**: Lock-free reads over RDMA, object collocation, function shipping.
- **Transport**: RDMA + NVRAM.
- **Paper**: Dragojevic et al., "FaRM: Fast Remote Memory," NSDI 2014.
- **Relevance**: Rich shared-address-space abstraction, but application must use FaRM's transaction API. Tightly coupled to RDMA. Not a middleware layer.

### UMap (LLNL)

- **What**: Library providing mmap()-like interface to user-space page fault handler based on userfaultfd.
- **Code**: https://github.com/LLNL/umap
- **Use case**: Application-specific page caching from large files (out-of-core execution).
- **Relevance**: Demonstrates userfaultfd as a building block for custom paging. Could be a component in a distributed memory system.

---

## 5. NVIDIA UVM Ecosystem

### NVIDIA Unified Virtual Memory (UVM)

- **What**: Creates a unified virtual address space between CPU and GPU memory within a single node. Automatic page migration via demand paging.
- **Scope**: Single node only (CPU <-> GPU).
- **Used by**: cuDF/RAPIDS for out-of-GPU-memory workloads, CUDA applications via `cudaMallocManaged`.
- **Upstream**: Linux HMM (Heterogeneous Memory Management) is the kernel foundation.
- **Relevance**: The canonical "UVM" but strictly local to one machine.

### Multi-Node NVLink (MNNVL) / NVL72

- **What**: NVLink + NVSwitch creates a unified memory fabric across GPUs within a rack.
- **Scale**: GB200/GB300 NVL72 connects 72 GPUs in a rack-scale domain with up to 37TB shared memory.
- **Marketing**: NVIDIA markets it as "acting as a single, massive GPU."
- **Management**: NVIDIA Fabric Manager + IMEX service.
- **Constraint**: Hardware-bound to NVLink-connected nodes in a single rack. Not general-purpose.
- **Relevance**: Closest to "distributed UVM" for GPUs but requires specific NVIDIA hardware topology.

### NVSHMEM

- **What**: Partitioned Global Address Space (PGAS) library for NVIDIA GPU clusters.
- **Mechanism**: Creates a global address space spanning multiple GPU memories. Accessed via put/get/atomic APIs. GPU-initiated operations.
- **Integration**: In PyTorch as a backend.
- **Docs**: https://docs.nvidia.com/nvshmem/
- **Relevance**: Real distributed GPU memory, but explicit PGAS programming (not transparent UVM). Requires symmetric memory allocations and NVSHMEM API usage.

### Linux HMM (Heterogeneous Memory Management)

- **What**: Kernel subsystem that mirrors CPU page tables to device page tables. Handles page faults for device-private memory.
- **Scope**: Strictly local (CPU <-> device within one machine).
- **Status**: Upstream in Linux kernel.
- **Note**: No "Scale-Out HMM" or "Distributed HMM" exists despite some claims. HMM is entirely local.
- **Relevance**: Foundation for UVM-like behavior in Linux. Potential building block.

---

## 6. Industry Specifications & Standards

### OCP Composable Memory Systems (CMS)

- **What**: Open Compute Project sub-project defining architecture and nomenclature for composable memory.
- **Workstreams**: Composable Workloads (Uber), DC Memory Fabric Orchestration (Intel), AI & HPC Systems & Fabric (Meta, Microsoft), Computational Programming (Elephance Memory), Academia Research (Micron).
- **Key documents**:
  - CMS Logical System Architecture White Paper
  - Memory Fabric Orchestration (MFO) Architecture White Paper (Dec 2025)
  - Composable Fabric Management (CFM) Software and APIs Spec v1.1 (Seagate contribution)
- **Code**: https://github.com/opencomputeproject/OCP-SVR-CMS-CFM-Composability_Fabric_Manager (Go service)
- **URL**: https://www.opencompute.org/wiki/Server/CMS
- **Relevance**: Industry-wide effort to standardize composable memory management. Focused on control plane (orchestration, allocation) not data plane (runtime, application API). Closest to an industry standard for memory fabric management.

### CXL Specification (3.0 / 3.1)

- **CXL 2.0**: Switching, memory pooling (single host to pool).
- **CXL 3.0**: Multi-headed devices (HDM-DB), fabric-attached memory (FAM), multi-host hardware-managed coherency.
- **CXL 3.1**: Refinements to sharing and coherency.
- **Status**: CXL 2.0 silicon reaching early production. CXL 3.0+ multi-host coherency is mostly FPGA prototypes (e.g., Altera Agilex). Commodity silicon 1-3 years away.
- **Relevance**: The hardware foundation. Defines what future disaggregated memory hardware will look like. But hardware alone doesn't solve the software abstraction problem.

---

## 7. Abstraction Layer Attempts

These are the systems that come closest to a "middleware between applications and distributed memory backends."

### Intel UMF (Unified Memory Framework)

- **What**: Library for constructing allocators and memory pools with pluggable memory providers and pool allocators.
- **Goal**: "Unify path for heterogeneous memory allocations among higher-level runtimes (SYCL, OpenMP, MPI, oneCCL, etc.)"
- **Architecture**: Provider abstraction — write a provider for any memory type. Pool allocators sit on top.
- **Code**: https://github.com/oneapi-src/unified-memory-framework (actively maintained)
- **Limitation**: Allocator framework only. No remote memory transport, no page migration, no distributed address space. Solves "how to allocate from heterogeneous local memory types" not "how to transparently use remote memory."

### emucxl

- **What**: Emulation framework providing standardized API for CXL disaggregated memory applications.
- **Goal**: "Provide a standardized view of the CXL emulation platform and the software interfaces and abstractions for disaggregated memory."
- **Code**: https://github.com/cloudarxiv/emucxl
- **Limitation**: Emulation only. CXL-only. Single node. No real backends.

### "Pointers in Far Memory" Vision (ACM Queue / CACM '23)

- **What**: Vision paper by Ethan Miller et al. describing a stack of memory shims at different levels.
- **Proposed layers**:
  1. Language runtime shim — Python/Java GC transparently backs objects with far memory
  2. Library shim — malloc replacement using far memory
  3. OS shim — kernel page fault handler routes to remote memory
  4. Hardware shim — CXL controllers present remote memory as local
- **Key quote**: "Once the shim is integrated into the operating system itself, all applications can use disaggregated memory transparently without modifications to their source code."
- **Paper**: Miller et al., "Pointers in Far Memory," ACM Queue / Communications of the ACM, 2023.
- **Limitation**: Vision paper only. No implementation.

### "Exploring the Disaggregated Memory Interface Design Space" (WORD '19)

- **What**: Workshop paper laying out the design dimensions for disaggregated memory interfaces.
- **Key taxonomy**: Implicit interfaces (cache-like, transparent) vs. explicit interfaces (API-based). Write-back vs. write-through. Granularity (page vs. object vs. byte).
- **Paper**: Pemberton, "Exploring the Disaggregated Memory Interface Design Space," WORD 2019.
- **Limitation**: Taxonomy paper, not an implementation.

### "Systems for Memory Disaggregation: Challenges & Opportunities" (2022)

- **What**: Survey paper discussing the design challenge of choosing the right interface for disaggregated memory — transparent vs. expressive.
- **Key insight**: Transparent interfaces (no app changes) sacrifice performance. Expressive interfaces (rich API) sacrifice adoption.
- **Relevance**: Frames the fundamental tension in the middleware layer.

---

## 8. Summary Table

| Solution | Type | Transport | App Transparency | Distributed | GPU Support | Pluggable Backends | Status |
|---|---|---|---|---|---|---|---|
| Google Far Memory | SW tier | Compression | Yes | No (local) | No | No | Production |
| Meta TMO | Kernel offload | SSD/zswap | Yes | No (local) | No | No | Production |
| MemVerge MemMachine | Commercial | CXL/Optane | Mostly | Limited | No | No | Shipping |
| Liqid Matrix | Commercial | PCIe/CXL/IB | Orchestrated | Yes | Indirect | Partial | Shipping |
| Samsung SMDK | SDK | CXL | API-level | No (local) | No | No | Available |
| TORmem | HW+SW platform | CXL 2.0 | Platform | Rack-scale | No | No | PoC |
| Infiniswap | Research | RDMA | Yes (swap) | Yes | No | No | Open source |
| LegoOS | Research OS | RDMA | OS-level | Yes | No | No | Prototype |
| AIFM | Research runtime | RDMA | Partial (annotate) | Yes | No | No | Open source |
| Semeru | Research runtime | RDMA | Yes (JVM) | Yes | No | No | Open source |
| FaRM | Research | RDMA+NVRAM | No (API) | Yes | No | No | Prototype |
| Rcmp | Research | CXL+RDMA | No (API) | Yes | No | Partial (2) | Open source |
| Intel UMF | Framework | Local | API-level | No | No | Yes (local) | Active |
| NVIDIA UVM | GPU runtime | PCIe/NVLink | Yes (malloc) | No (local) | Yes | No | Production |
| NVIDIA MNNVL | GPU fabric | NVLink | Kernel-level | Rack-only | Yes | No | Production |
| NVSHMEM | PGAS library | NVLink/IB | No (API) | Yes | Yes | No | Production |
| OCP CMS/CFM | Spec + impl | CXL | N/A (control plane) | Yes | No | Spec-level | Draft |
