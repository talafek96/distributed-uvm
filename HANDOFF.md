# Distributed UVM — Project Handoff

## What This Is

**duvm** (Distributed Unified Virtual Memory) — a service that pools RAM across a cluster of machines, making remote memory transparently available to unmodified applications. CPU and GPU workloads both benefit. Can be enabled/disabled at runtime.

## Current State

**Phase: RDMA end-to-end working. Pages flow via one-sided RDMA WRITE/READ. Verified in QEMU.**

### What Works (Proven)

| What | Evidence | Command |
|---|---|---|
| RDMA end-to-end (SoftiWARP) | Two VMs: daemon → RDMA WRITE → memserver MR, 18/18 | `bash scripts/test-rdma-qemu.sh` |
| Kernel module ↔ daemon ring buffer | Pages flow kmod → ring → daemon → backend, 10/10 QEMU | `bash scripts/test-kmod-daemon-qemu.sh` |
| Two-VM distributed test | VM-A (kmod+daemon) talks to VM-B (memserver), 12/12 | `bash scripts/test-distributed-qemu.sh` |
| Kernel module standalone | insmod, mkswap, swapon, block I/O, rmmod — 16/16 QEMU | `bash scripts/test-kmod-qemu.sh` |
| Cross-machine TCP (real hardware) | 10,000 pages calc1↔calc2 over ConnectX-7, byte-perfect | `cargo run --example demo_distributed --release -p duvm-daemon` |
| Engine + policy + eviction + transport | LRU, tier cascading, transport modes, 189 Rust tests | `cargo test` |

### Test Summary

| Test | Checks | What it proves |
|---|---|---|
| `cargo test` | 189 pass | User-space engine, policy, backends, config, transport modes |
| `test-rdma-qemu.sh` | 18/18 | **Full RDMA: SoftiWARP, CM handshake, one-sided WRITE/READ, data integrity** |
| `test-kmod-qemu.sh` | 16/16 | Kernel module: load, block I/O, swap, unload |
| `test-kmod-daemon-qemu.sh` | 10/10 | Ring buffer: kmod → daemon → engine → backend |
| `test-distributed-qemu.sh` | 12/12 | Two VMs: kmod+daemon on A, memserver on B, network I/O |

### Components

| Component | Status | Location |
|---|---|---|
| **duvm-kmod** | Complete + tested | `duvm-kmod/` — Virtual block device, ring buffer, xarray fallback |
| **duvm-daemon** | Complete + tested | `crates/duvm-daemon/` — Engine, policy, ring consumer, control socket |
| **duvm-memserver** | Complete + tested | `crates/duvm-memserver/` — Remote memory server |
| **duvm-common** | Complete | `crates/duvm-common/` — PageHandle, ring buffer, protocol, stats |
| **duvm-backend-trait** | Complete | `crates/duvm-backend-trait/` — Backend plugin interface |
| **duvm-backend-memory** | Complete | `crates/duvm-backend-memory/` — In-memory backend |
| **duvm-backend-compress** | Complete | `crates/duvm-backend-compress/` — LZ4 compression backend |
| **duvm-backend-tcp** | Complete | `crates/duvm-backend-tcp/` — TCP remote memory backend |
| **duvm-backend-rdma** | Complete — end-to-end verified | `crates/duvm-backend-rdma/` — RDMA (libibverbs + librdmacm) backend + server |
| **duvm-ctl** | Complete | `crates/duvm-ctl/` — CLI tool |
| **libduvm** | Complete | `crates/libduvm/` — Rust + C FFI library |

### Hardware

- 2x NVIDIA DGX Spark (GB10 Blackwell, 128GB unified LPDDR5X each)
- 4x ConnectX-7 200Gbps RoCE (calc1: 192.168.200.10, calc2: 192.168.200.11)
- aarch64, Linux 6.17.0-1008-nvidia, CUDA 13.0
- GPU UVM via ATS (hardware — no special code needed)

## How to Build and Test

```bash
# Build everything
cargo build --release
make -C duvm-kmod

# Run Rust tests (189 tests, no sudo needed)
cargo test

# QEMU tests (no sudo needed)
bash scripts/test-kmod-qemu.sh            # Kernel module standalone
bash scripts/test-kmod-daemon-qemu.sh      # Kmod + daemon ring buffer
bash scripts/test-distributed-qemu.sh      # Two VMs: distributed memory
bash scripts/test-rdma-qemu.sh             # SoftRoCE + auto-fallback

# Cross-machine TCP test (no sudo, needs calc2 reachable)
# Terminal 1: ssh calc2-104004, start memserver
# Terminal 2: cargo run --example demo_distributed --release -p duvm-daemon

# Real hardware test (needs sudo for insmod):
sudo bash scripts/setup-kmod-for-testing.sh        # Load kmod, set permissions
cargo run --release -p duvm-daemon -- --kmod-ctl /dev/duvm_ctl  # Start daemon
sudo swapon -p 100 /dev/duvm_swap0                 # Activate swap
sudo bash scripts/setup-kmod-for-testing.sh --teardown  # Cleanup
```

## What's Next

1. **Real RDMA hardware test** — run on DGX Spark's ConnectX-7 (RoCEv2), measure latency vs TCP
2. **Enable/disable service** — systemd units, `duvm-ctl enable`/`duvm-ctl disable` across cluster
3. **CI** — add `test-rdma-qemu.sh` to GitHub Actions workflow

## Key Technical Decisions

See `DECISIONS.md` for full rationale. See `research/` for prior art surveys. See `docs/ARCHITECTURE.md` for full page lifecycle.

| Decision | Choice | Why |
|---|---|---|
| Swap interception | Virtual block device (not frontswap) | frontswap removed in Linux 6.17; block device uses stable blk-mq API |
| Kmod↔daemon | Shared ring buffer via mmap of /dev/duvm_ctl | Low latency, zero-copy staging area for page data |
| Daemon wake-up | poll() on /dev/duvm_ctl (event-driven, ~1-5us) | No polling loop, instant response to kernel requests |
| Architecture | Symmetric — every node is compute + memory | All nodes equal; no single point of failure |
| Transport | TCP default, RDMA optional (auto-detect) | TCP works everywhere; RDMA for production performance |
| Multi-peer | Round-robin across peers in same tier | Fair distribution; config: `peers = [...]` |
| Policy | LRU with tier-aware cascading + eviction | Prefers lowest-latency tier; evicts cold pages when full |
| Mutual OOM | Memserver refuses when full → I/O error → kernel tries next swap device | No deadlock, no recursion |
| GPU UVM | Hardware ATS on DGX Spark; HMM on PCIe GPUs | No GPU-specific code needed — swap layer is below GPU driver |
| OOM safety | Linux swap priority cascade | `swapon -p 100` for remote, `-p 10` for local SSD fallback |
| Testing | QEMU VMs for kernel module safety | Crashes don't affect host; no special hardware needed |
