# Prior Art — Phase 2: Kernel-Userspace Communication, Testing, OOM, GPU UVM, x86

Research conducted March 2026 to fill knowledge gaps for duvm's next implementation phases.

---

## 1. Kernel ↔ Userspace Block Device Communication

The missing piece in duvm is connecting the kernel module's block I/O to the userspace daemon. Here's how every relevant system does it:

### ublk (Linux 6.0+, mainline) — RECOMMENDED

**What:** A mainline Linux subsystem that lets userspace implement block devices. The kernel routes blk-mq requests to a userspace process via io_uring.

**How it works:**
```
Application → blk-mq request → ublk kernel driver → io_uring → userspace server → io_uring completion → kernel
```

**Key design points:**
- Uses io_uring for both directions (request delivery + completion notification)
- Supports zero-copy via `UBLK_F_SUPPORT_ZERO_COPY` — page data stays in kernel, userspace gets a reference
- Supports batch I/O — multiple requests per io_uring submission
- Supports user recovery — if the userspace server crashes, it can reconnect and resume
- Each I/O queue has its own io_uring instance (one per CPU, matching blk-mq's hw queues)

**Performance:** ~2-5us per I/O operation for the kernel↔userspace round trip. Comparable to TCMU, faster than NBD.

**Source:** `docs.kernel.org/block/ublk.html`, `github.com/ublk-org/ublksrv`

**Status:** Mainline since Linux 6.0. Actively maintained. Feature additions ongoing (zero-copy in 6.8, batch IO, auto buffer registration).

**Relevance to duvm:** This is the modern, correct way to do what duvm needs. Two options:
1. **Replace our custom kernel module with ublk** — use the mainline ublk driver and write our daemon as a ublk server. Eliminates all kernel module maintenance. The daemon receives block I/O requests via io_uring and sends them to remote machines.
2. **Adopt ublk's io_uring pattern in our existing module** — keep our custom ring buffer but switch to io_uring for the kernel↔daemon transport. More work, less benefit vs. option 1.

### TCMU (Target Core Module in Userspace)

**What:** LIO (Linux SCSI target) subsystem that allows userspace implementations of SCSI block devices. Uses shared memory ring buffer.

**How it works:**
- Kernel allocates shared memory region, userspace mmaps it
- Ring buffer in shared memory carries SCSI commands + data
- Mailbox pattern: kernel writes commands, sets doorbell, userspace reads and responds
- Data area: configurable size, carries page data inline

**Performance:** ~5-10us per I/O. Higher overhead than ublk due to SCSI command layer.

**Relevance:** Our current ring buffer design is essentially a simplified TCMU. TCMU validates the approach but ublk supersedes it.

### NBD (Network Block Device)

**What:** Kernel module that forwards block I/O to a userspace process over a socket (Unix or TCP).

**How it works:** Kernel sends struct nbd_request over socket, userspace reads it, processes it, sends nbd_reply back. Page data follows the request/reply headers.

**Performance:** 50-500us per page due to socket overhead and context switches.

**Concern for swap:** NBD under memory pressure can deadlock — the kernel needs memory to send swap pages over the socket, but the system is out of memory. Linux has `PF_MEMALLOC` and memory reserves for this, but it's fragile.

**Relevance:** Too slow and fragile for swap. Not recommended.

### Infiniswap's approach

**What:** Custom kernel module that does RDMA directly from kernel space. No userspace daemon for the data path.

**How it works:**
- Kernel module registers memory with RDMA and does one-sided RDMA READ/WRITE directly
- Slab-based: divides swap space into 1GB slabs, each mapped to a remote machine
- Daemon only handles control plane (slab management, eviction)
- Data path: kernel → RDMA verbs → remote memory (no userspace involvement)

**Performance:** 3-10us per page (one-sided RDMA).

**Relevance:** Shows that for RDMA, bypassing userspace entirely is faster. Our TCP backend goes through userspace, which is fine for TCP. Future RDMA backend should consider kernel-direct RDMA like Infiniswap.

### Fastswap's approach

**What:** Hooks into the kernel's frontswap interface (deprecated in Linux 6.x) and does RDMA from kernel space.

**How it works:** Like Infiniswap but uses frontswap hooks instead of a block device. Kernel calls `frontswap_store()` / `frontswap_load()` which directly do RDMA.

**Performance:** 2-5us per page.

**Relevance:** Frontswap was removed from Linux 6.17 (our kernel). Cannot use this approach. But the performance numbers show what's achievable with kernel-direct RDMA.

### Recommendation for duvm

**Option A (recommended): Replace our kernel module with ublk.** We write a ublk userspace server in Rust. The kernel's ublk driver handles all the blk-mq plumbing. Our server receives I/O requests via io_uring and routes them to backends. Benefits: no kernel module to maintain, mainline support, zero-copy available, crash recovery built in.

**Option B: Keep our module, use io_uring for communication.** Our kernel module's ring buffer becomes an io_uring instance. Similar to what ublk does internally but we own the kernel code. More control, more maintenance burden.

**Option C (current design): Keep our module, use our custom ring buffer.** The daemon mmaps `/dev/duvm_ctl` and polls the ring buffer. Simplest to implement since the ring buffer already exists. Lower performance than io_uring but simpler. Good for initial proof, migrate to ublk later.

---

## 2. Testing Distributed Memory Systems

### How every existing project tests

| System | Year | Test Method | Requires Special HW? | Open Source Tests? |
|--------|------|-------------|----------------------|-------------------|
| Infiniswap | 2017 | Real Mellanox IB cluster | Yes | Shell scripts |
| Fastswap | 2020 | Real Mellanox IB cluster | **DRAM backend option** | Python CFM framework |
| AIFM | 2020 | CloudLab xl170 bare-metal | Yes | Shell scripts + CloudLab profile |
| Canvas | 2023 | Real IB cluster | Yes | Build scripts |
| LegoOS | 2018 | Real IB cluster (CloudLab R320) | Yes | Manual |
| TMO/Meta | 2022 | Production fleet | Yes | Not public |

**Key finding: No project has QEMU-based multi-VM testing.** Every single one requires real hardware with RDMA. This is a major gap.

### Fastswap's DRAM backend — the one exception

Fastswap provides `make BACKEND=DRAM` which allocates 32GB of local RAM as fake remote memory. This lets you test the swap path without RDMA hardware. The pattern:
- Kernel module stores/loads pages to local DRAM buffer instead of RDMA
- Same code path exercised (page fault → store → load)
- No network involved, but proves the kernel logic works

**duvm already has this** — our memory backend is exactly this pattern. The TCP backend adds real network testing without requiring RDMA.

### QEMU multi-VM testing — what's possible

QEMU supports socket-based networking between VMs:
```bash
# VM A: listen on host port 9100
qemu-system-aarch64 ... -netdev socket,id=net0,listen=:9100 -device virtio-net-device,netdev=net0

# VM B: connect to VM A
qemu-system-aarch64 ... -netdev socket,id=net0,connect=127.0.0.1:9100 -device virtio-net-device,netdev=net0
```

Both VMs get virtual NICs and can communicate via TCP. Performance is limited (~100-500 Mbps) but sufficient for functional testing.

**What we'd build (novel — nobody has done this):**
- Two QEMU VMs, each with our kernel module + daemon + memserver
- Automated: boot both, configure networking, load modules, activate swap, run memory pressure test
- Proves the full distributed swap path works without any special hardware
- Runnable on any Linux x86 or ARM machine with QEMU installed

### Benchmark methodology from the literature

Fastswap's CFM framework is the gold standard for reproducible benchmarks:
- Uses cgroup v2 `memory.max` to limit local memory (forces swapping)
- Parameterized by `ratio` (local memory / total working set)
- Standard workloads: quicksort, linpack, TensorFlow, Spark, memcached, STREAM
- Measures execution time vs. ratio curve

We should adopt this methodology: use cgroups to force swapping, measure degradation curves.

---

## 3. OOM Handling When Remote Memory Is Exhausted

### What every system does

| System | When Remote Full | Graceful? | Mechanism |
|--------|-----------------|-----------|-----------|
| **Infiniswap** | Evicts 1GB slabs to local backup disk | Yes | Slab eviction + local disk partition |
| **Fastswap** | Not handled (pre-sized) | No | Fixed allocation |
| **AIFM** | Crashes (assumes infinite remote) | No | None |
| **TMO/Meta** | PSI feedback loop backs off | Yes | zswap → SSD → feedback |
| **zswap** | LRU eviction to backing swap + hysteresis threshold | Yes | `accept_threshold_percent` |
| **Linux kernel** | Priority cascade → OOM killer | Yes | `swapon -p` priorities |
| **Kubernetes** | Pod eviction by QoS class | Yes | kubelet eviction manager |

### Key patterns for graceful degradation

**1. Swap priority cascade (Linux built-in)**
```bash
swapon -p 100 /dev/duvm_swap0     # remote RAM (fastest)
swapon -p  50 /dev/duvm_compress0  # local compressed (zswap-like)
swapon -p  10 /swapfile            # local SSD (slowest, but always works)
```
When duvm is full, kernel automatically falls through. No code needed.

**2. PSI-based feedback (TMO/Meta pattern)**
Monitor `/proc/pressure/memory`. When `some` or `full` metrics exceed a threshold, the system is thrashing. Back off — reduce how aggressively we push pages to remote. Prevents the scenario where the network becomes the bottleneck.

**3. zswap as first tier**
Insert compressed local memory as a fast cache in front of remote swap:
```
Page evicted → zswap compresses it (1-3us) → if hot, stays in compressed cache
                                            → if cold, zswap evicts to duvm (remote)
                                            → if remote full, falls to local SSD
```
This is how TMO works at Meta. zswap is built into the kernel and works with any swap device.

**4. Infiniswap's slab eviction**
When remote is full, evict entire 1GB slabs (oldest/coldest) from remote to local backup disk. Frees remote capacity for new pages. The evicted data is still accessible from local disk, just slower.

### What duvm should implement

**Phase 1 (free — Linux handles it):** Configure swap priority cascade. Document it.

**Phase 2 (small):** Add monitoring. `duvm-ctl status` shows remote utilization. Log warnings at 80% and 95% full.

**Phase 3 (medium):** Add PSI-based pressure monitoring. When the system is thrashing (PSI `full` > 5%), reduce prefetch aggressiveness and log alerts.

**Phase 4 (optional):** Implement slab eviction like Infiniswap — when remote is 90% full, proactively move coldest remote pages to local SSD to make room for new ones.

---

## 4. GPU Unified Virtual Memory on Non-ATS Hardware

### The three tiers of GPU UVM

**Tier 1: ATS over NVLink-C2C (DGX Spark, Grace Hopper)**
- GPU uses CPU page tables directly
- ALL memory is unified — `malloc`, `mmap`, `cudaMallocManaged` all work
- Swap works transparently (GPU page fault → ATS → CPU fault → swap-in)
- No driver or API changes needed
- **duvm works perfectly here — proven on our hardware**

**Tier 2: HMM on PCIe (Turing+ NVIDIA, AMD CDNA2+, any Linux 6.1+)**
- GPU mirrors CPU page tables via MMU notifiers
- `malloc`/`mmap` memory accessible from GPU (HMM makes it transparent)
- When CPU pages are swapped out, MMU notifier fires → GPU driver unmaps the page
- When GPU needs the page: `hmm_range_fault()` → triggers swap-in → page restored
- **Swap through duvm works, but with 4KB page migration granularity and software overhead**

Requirements:
- NVIDIA: CUDA 12.2+, open kernel modules (r535+), kernel 6.1.24+, Turing or newer, x86_64 only
- AMD: ROCm with `HSA_XNACK=1`, CDNA2+ (MI200+), kernel 5.15+
- Intel: Level Zero USM, kernel 6.2+ (xe driver), integrated GPUs get this for free

**Tier 3: No HMM (older GPUs, older kernels)**
- Only `cudaMallocManaged` / `hipMallocManaged` provides UVM
- GPU driver handles page migration internally
- CPU pages backing managed memory do go through swap normally
- GPU faults trigger CUDA/ROCm runtime page migration, which triggers swap-in
- **duvm distributed swap still works for the CPU pages. GPU managed memory pages that get swapped to duvm come back when CUDA triggers migration.**

**Tier 4: No GPU**
- Just distributed RAM via swap device
- Still very useful for CPU workloads (databases, in-memory analytics, Java heaps)

### Key finding: duvm's swap device works at every tier

Because duvm operates at the swap layer (block device), it's below the GPU driver. The GPU driver handles its own page table management (via ATS, HMM, or CUDA UVM). When any of those mechanisms need a page that was swapped out, they trigger a standard CPU page fault, which goes through the swap path, which goes through duvm. The GPU vendor's driver doesn't need to know about duvm.

The only difference between tiers is **how the GPU triggers the page fault**:
- ATS: GPU hardware fault → CPU page table → swap entry → swap-in
- HMM: GPU driver `hmm_range_fault()` → CPU page fault → swap entry → swap-in
- CUDA UVM: CUDA runtime migration → host page access → CPU page fault → swap entry → swap-in

All three end up at the same place: the kernel's swap-in path reading from `/dev/duvm_swap0`.

---

## 5. x86 Virtual Block Device Swap Targets

### Systems that have done this

| System | Architecture | Block Layer | Kernel Version | Performance | Status |
|--------|-------------|-------------|---------------|-------------|--------|
| **Infiniswap** | Block device + RDMA kmod | Custom blk-mq | 3.13-4.11 | 3-10us/page | Abandoned (2017) |
| **Fastswap** | Frontswap hooks + RDMA | frontswap (removed in 6.x) | 4.11 patched | 2-5us/page | Abandoned (2020) |
| **Canvas** | Custom frontswap + RDMA | frontswap | 5.5 patched | ~3us/page | Abandoned (2023) |
| **NBD** | Socket-based block device | Standard blk-mq | Any | 50-500us/page | Mainline, active |
| **ublk** | io_uring block device | Standard blk-mq | 6.0+ | 2-5us/page (local) | Mainline, active |
| **zswap** | In-kernel compressed cache | Frontswap/built-in | 3.11+ | <1us/page | Mainline, active |
| **TMO/TPP** | Kernel NUMA tiering + cgroup | Upstream MM | 5.18+ | <1us (tier) | Mainline, active |
| **duvm** | Custom blk-mq block device | Standard blk-mq | 6.17 | TBD | In development |

### x86-specific considerations

1. **Infiniswap was x86-only.** Its kernel module used x86 memory barriers and RDMA verbs that assumed x86. Porting to ARM required changes.

2. **ublk is architecture-independent.** It uses io_uring which works on x86, ARM, RISC-V. If duvm migrates to ublk, x86 support comes for free.

3. **Our current kernel module** uses standard `blk-mq` APIs and `kmap_local_page()` which are architecture-independent. The ring buffer uses `smp_wmb()`/`smp_rmb()` which compile to the correct barriers on both x86 and ARM. **The module should compile on x86 without changes**, but hasn't been tested.

4. **Deadlock risk under memory pressure** (all systems share this): When the system is low on memory and trying to swap, it needs memory to process the swap I/O. The kernel has `PF_MEMALLOC` reserves for this, but complex userspace paths (TCP socket → daemon → remote server) can exhaust those reserves. Infiniswap avoids this by using one-sided RDMA (no CPU involvement on the remote side). NBD is known to deadlock. ublk uses io_uring which has better memory reservation semantics.

### Recommended path for x86 support

1. **Short term:** Test our existing kernel module on x86 in QEMU (should compile as-is)
2. **Medium term:** Migrate to ublk — eliminates the kernel module entirely, gets x86/ARM/any-arch for free
3. **Long term:** Add kernel-direct RDMA backend for maximum performance (like Infiniswap)

---

## 6. Key Decisions Informed by This Research

### Decision: Kernel↔daemon connection approach

**Chosen: Option C (custom ring buffer) for initial proof, migrate to ublk for production.**

Rationale: Our ring buffer exists and the infrastructure is built. Wiring it up proves the concept. Then we can migrate to ublk which gives us mainline support, crash recovery, zero-copy, and architecture independence.

### Decision: Testing approach

**Chosen: QEMU multi-VM test (novel — no prior art exists for this).**

Rationale: Every prior system requires real RDMA hardware. We'd be the first to provide a self-contained QEMU-based distributed memory test. This dramatically lowers the barrier to contribution and testing.

### Decision: OOM handling

**Chosen: Swap priority cascade (Phase 1) + PSI monitoring (Phase 2).**

Rationale: Linux swap priorities give us graceful degradation for free. PSI monitoring (the TMO/Meta pattern) adds intelligence. No need for complex slab eviction initially.

### Decision: GPU UVM on non-ATS hardware

**Chosen: No special code needed — document the tiers.**

Rationale: Our swap device works at every tier because it's below the GPU driver. The GPU driver's own mechanisms (ATS, HMM, CUDA UVM) all end up triggering CPU page faults which go through our swap device. We just need to document which hardware gets which level of transparency.
