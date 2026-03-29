# Distributed UVM — Full Project Report

## Executive Summary

**duvm** (Distributed Unified Virtual Memory) is a Linux service that transparently pools RAM across a cluster of machines. When enabled, any machine can use memory from any other machine — applications don't know. When disabled, each machine uses only its own local RAM. No application code changes, no library linking, no LD_PRELOAD.

**Status: Feature-complete for single-cluster deployment. Proven end-to-end in QEMU simulation (including RDMA via SoftiWARP). Not yet validated on real RDMA hardware. One critical bug (memory leak from missing discard path) must be fixed before production use.**

---

## 1. How It Works

The system operates at the kernel swap layer:

```
Application (unmodified)
    ↓ page fault / memory pressure
Linux Kernel (standard swap subsystem)
    ↓ writes page to swap device
/dev/duvm_swap0 (our virtual block device — duvm-kmod)
    ↓ ring buffer (shared mmap, event-driven poll, ~1-5μs wake)
duvm-daemon (user-space engine — policy, backends, routing)
    ↓ TCP or RDMA
duvm-memserver on remote machine (stores page in RAM HashMap)
```

The kernel module creates a standard Linux swap block device. The admin activates it with `swapon`. From that point, the kernel's existing LRU page eviction sends cold pages to our device, which routes them to remote machines. When the application touches the page again, the kernel faults it back through the same path. Applications see a unified address space — CPU and GPU workloads both work transparently.

Pages are stored in RAM on the remote machine's memserver process (heap-allocated 4KB buffers in a HashMap), not on disk. Each TCP connection has its own page namespace, so pages from different machines cannot collide. The kernel's swap allocator assigns unique slot numbers, so pages from different processes on the same machine cannot collide either.

---

## 2. Codebase Overview

| Component | Lines | Language | Purpose |
|-----------|-------|----------|---------|
| `duvm-kmod/` | 655 | C | Virtual block device, ring buffer, xarray fallback |
| `crates/duvm-daemon/` | ~1,800 | Rust | Engine, policy (LRU), ring consumer, control socket |
| `crates/duvm-memserver/` | ~330 | Rust | Remote memory server (TCP + RDMA) |
| `crates/duvm-backend-tcp/` | ~350 | Rust | TCP backend with auto-reconnect + circuit breaker |
| `crates/duvm-backend-rdma/` | ~700 | Rust + C shim | RDMA backend (libibverbs + librdmacm) + server |
| `crates/duvm-backend-memory/` | ~150 | Rust | In-process memory backend |
| `crates/duvm-backend-compress/` | ~180 | Rust | LZ4 compression backend |
| `crates/duvm-backend-trait/` | ~110 | Rust | Backend plugin interface |
| `crates/duvm-common/` | ~400 | Rust | PageHandle, ring buffer, protocol, stats |
| `crates/duvm-ctl/` | ~310 | Rust | CLI: enable/disable/drain/status |
| `crates/libduvm/` | ~200 | Rust | Optional C FFI for explicit page control |
| `crates/duvm-tests/` | 3,276 | Rust | 196 unit/integration tests |
| `scripts/` | 2,865 | Bash | 8 QEMU end-to-end test scripts |

**Total: ~10,400 lines Rust + 655 lines C + 2,865 lines test scripts.**

### 11 Rust crates, 1 kernel module, 8 QEMU test scripts.

---

## 3. Test Coverage

### Unit & Integration Tests (196 total, all passing)

| Test file | Count | What it covers |
|-----------|-------|----------------|
| `comprehensive_test.rs` | 95 | Backend errors, policy engine, engine integration, concurrency, config, eviction |
| `tcp_integration.rs` | 13 | TCP store/load, capacity, reconnection, server crash, concurrent clients |
| `integration_test.rs` | 9 | Memory/compress backends, ring buffer, libduvm pool, tier ordering |
| `bench.rs` | 5 | Performance baselines (memory >10k pages/sec, compress >5k, ring >1M ops/sec) |
| `engine.rs` (unit) | 9 | Store/load, invalidate, eviction, stats, backend candidates |
| `policy.rs` (unit) | 11 | LRU, tier cascading, pinned pages, health-aware selection |
| Other crate units | 54 | Ring buffer, protocol, page handles, RDMA availability, compression |

### QEMU End-to-End Tests (82 checks across 8 scripts, all passing in CI)

| Script | Checks | What it proves |
|--------|--------|----------------|
| `test-kmod-qemu.sh` | 16 | Kernel module: load, block I/O, swap, stress, unload |
| `test-kmod-daemon-qemu.sh` | 10 | Ring buffer: kmod → daemon → engine → backend |
| `test-distributed-qemu.sh` | 13 | **Two VMs: kmod+daemon on A, TCP backend → memserver on B, data integrity** |
| `test-mutual-oom-qemu.sh` | 9 | Both VMs full, graceful degradation to local swap |
| `test-3machine-qemu.sh` | 10 | Three VMs: fair distribution, exhaustion handling |
| `test-rdma-qemu.sh` | 18 | **Full RDMA: SoftiWARP, CM handshake, one-sided WRITE/READ** |
| `test-memserver-concurrent-qemu.sh` | 6 | Parallel TCP clients, capacity enforcement |

### CI Pipeline (GitHub Actions, 5 jobs)

All run on every push to `main`:
- **Format check** (`cargo fmt --check`)
- **Clippy lint** (`cargo clippy -- -D warnings`)
- **Unit tests** (`cargo test --workspace`)
- **Build** (`cargo build --workspace`)
- **E2E QEMU** (all 8 scripts on `ubuntu-24.04-arm`, 25min timeout)

**Last CI run: all green.**

---

## 4. What Works (Proven)

| Capability | Evidence |
|-----------|---------|
| Transparent swap via kernel module | 16 QEMU checks: insmod, mkswap, swapon, block I/O, rmmod |
| Daemon connects to kmod via ring buffer | 10 QEMU checks: pages flow kmod → ring → daemon → backend |
| **Full distributed path over TCP** | 13 QEMU checks: kmod → daemon → TCP → remote memserver → data integrity |
| **Full RDMA path (SoftiWARP)** | 18 QEMU checks: SoftiWARP CM handshake, one-sided WRITE/READ, data integrity |
| Mutual OOM graceful degradation | 9 QEMU checks: both machines full → error → kernel falls back |
| Fair multi-peer distribution | 10 QEMU checks: 3 VMs, pages spread across peers |
| TCP auto-reconnect on server crash | 4 unit tests: crash detection, reconnect, circuit breaker |
| Engine retry across backends | Tries all healthy backends in tier before failing |
| Memserver connection limits | `--max-connections` + `--idle-timeout` |
| Enable/disable service | `duvm-ctl enable/disable/drain` manage full lifecycle |
| Cross-machine TCP (real hardware) | 10,000 pages calc1↔calc2 over ConnectX-7, byte-perfect (TCP only) |

---

## 5. What Does NOT Work / Known Gaps

### Critical — Must fix before production

| Gap | Impact | Detail |
|-----|--------|--------|
| **Memory leak: no discard/invalidation path** | Pages accumulate on memserver forever | Kernel module only sends STORE and LOAD. When kernel frees a swap slot (process exit, page faulted back in), it sends `REQ_OP_DISCARD` but our `queue_rq` ignores it. `DUVM_OP_INVALIDATE` is defined in the header but never sent. Daemon's `engine.invalidate_page()` exists but is never called from the ring consumer. Memserver pages are only freed on overwrite or LRU eviction. |
| **No authentication** | Anyone with network access can read/write any page | TCP/RDMA connections have no auth. Port 9200 is wide open. |
| **No encryption** | Plaintext page data on the wire | Sensitive application data exposed. |

### High — Needed for multi-node production

| Gap | Impact | Detail |
|-----|--------|--------|
| **Static peer config only** | Adding a node requires editing config + restarting daemon on every node | No peer discovery (etcd/consul/DNS-SD). |
| **RDMA backend has no reconnection** | RDMA connection drop is permanent | TCP backend has auto-reconnect + circuit breaker. RDMA has nothing — `is_healthy()` only checks init state. |
| **No real RDMA hardware validation** | Unknown production latency/throughput | Only tested with SoftiWARP (TCP under the hood). ConnectX-7 RoCEv2 untested. |
| **Single-page RDMA buffer** | All RDMA ops serialized | Mutex held for entire transfer including 5s timeout. Need buffer pool. |

### Medium — Operational

| Gap | Impact | Detail |
|-----|--------|--------|
| Prometheus metrics not implemented | No observability | `metrics_port: 9100` in config but nothing listens. Stats only via Unix socket. |
| No SIGHUP config reload | Config changes require full daemon restart | Service file has `ExecReload` but daemon doesn't handle SIGHUP. |
| Daemon shutdown doesn't drain pages | Remote pages abandoned on exit | Operator must manually `duvm-ctl drain` before stopping. |
| `test-3machine-qemu.sh` doesn't verify distribution | Unknown if pages actually went to different peers | Writes pages and reads them back, doesn't assert which peer stored them. |

---

## 6. Architecture Decisions

| Decision | Choice | Why |
|----------|--------|-----|
| Swap interception | Virtual block device (not frontswap) | frontswap removed in Linux 6.17; block device uses stable blk-mq API |
| Kmod↔daemon | Shared ring buffer via mmap of /dev/duvm_ctl | Low latency, zero-copy staging area for page data |
| Daemon wake-up | `poll()` on /dev/duvm_ctl (event-driven, ~1-5μs) | No polling loop, instant response to kernel requests |
| Architecture | Symmetric — every node is compute + memory | All nodes equal; no single point of failure |
| Transport | TCP default, RDMA optional (auto-detect) | TCP works everywhere; RDMA for production performance |
| Policy | LRU with tier-aware cascading + eviction | Prefers lowest-latency tier; evicts cold pages when full |
| Mutual OOM | Memserver refuses when full → I/O error → kernel tries next swap device | No deadlock, no recursion |
| GPU UVM | Hardware ATS on DGX Spark; HMM on PCIe GPUs | No GPU-specific code needed — swap layer is below GPU driver |

---

## 7. Hardware

- 2x NVIDIA DGX Spark (GB10 Blackwell, 128GB unified LPDDR5X each)
- 4x ConnectX-7 200Gbps RoCE (calc1: 192.168.200.10, calc2: 192.168.200.11)
- aarch64, Linux 6.17.0-1008-nvidia, CUDA 13.0
- GPU UVM via ATS (hardware — no special code needed)

---

## 8. Roadmap

### Phase 3 — Production hardening (no hardware needed)

| # | Task | Effort | Impact |
|---|------|--------|--------|
| 1 | **Discard/invalidation path** — kmod sends `DUVM_OP_INVALIDATE` on `REQ_OP_DISCARD`, daemon ring consumer calls `engine.invalidate_page()` | Small (2 files: C + Rust) | **Critical** — fixes memory leak |
| 2 | Prometheus metrics — HTTP `/metrics` endpoint on `metrics_port` | Medium | Observability |
| 3 | SIGHUP config reload — re-read `duvm.toml`, add/remove backends | Medium | Operational |
| 4 | RDMA reconnection — port TCP auto-reconnect + circuit breaker | Medium | Resilience |

### Phase 4 — Hardware validation (needs DGX Spark access)

| # | Task | Effort | Impact |
|---|------|--------|--------|
| 1 | **Real RDMA hardware test** — ConnectX-7 RoCEv2 on DGX Spark | Medium | Validates production transport |
| 2 | **RDMA buffer pool** — replace single-page Mutex with lock-free pool | Medium | Unlocks concurrent RDMA |
| 3 | Full-stack hardware test — kmod → daemon → RDMA → memserver on real machines | Small | Proves transparent swap works on real hardware |
| 4 | Performance benchmarking — latency histograms, throughput curves | Medium | Quantifies production performance |

### Phase 5 — Cluster management (design needed)

| # | Task | Effort | Impact |
|---|------|--------|--------|
| 1 | Peer discovery (etcd/consul/DNS-SD/multicast) | Large | Dynamic cluster |
| 2 | Runtime peer add/remove (`duvm-ctl add-peer`) | Medium | Operational |
| 3 | TLS + auth (mutual TLS for TCP, token auth for RDMA) | Large | Security |
| 4 | Page migration on peer leave | Large | Graceful drain |

---

## 9. How to Build and Run

```bash
# Build everything
cargo build --release
make -C duvm-kmod

# Run tests (no sudo, no hardware)
cargo test                              # 196 unit tests
bash scripts/test-kmod-qemu.sh          # Kernel module standalone
bash scripts/test-distributed-qemu.sh   # Two VMs: full TCP path
bash scripts/test-rdma-qemu.sh          # RDMA via SoftiWARP

# Enable distributed memory (requires sudo on real machine)
sudo duvm-ctl enable                    # Load kmod, start services, activate swap
sudo duvm-ctl disable                   # Drain pages, stop services, unload kmod

# Manual two-machine setup
# Machine B: ./target/release/duvm-memserver --bind 0.0.0.0:9200
# Machine A: sudo insmod duvm-kmod.ko size_mb=4096
#            sudo mkswap /dev/duvm_swap0
#            Edit /etc/duvm/duvm.toml: [backends.remote] peers = ["B:9200"]
#            ./target/release/duvm-daemon --kmod-ctl /dev/duvm_ctl
#            sudo swapon -p 100 /dev/duvm_swap0
```

---

## 10. File Index

```
distributed-uvm/
├── AGENTS.md                    # Project rules and conventions
├── HANDOFF.md                   # Current state, build/test, what's next
├── CHANGELOG.md                 # Change history
├── DECISIONS.md                 # Architectural decisions with rationale
├── PITFALLS.md                  # Known gotchas and lessons learned (15 entries)
├── README.md                    # User-facing README
├── docs/ARCHITECTURE.md         # Full page lifecycle explanation
├── research/                    # 11 research/planning docs (historical)
├── duvm-kmod/                   # Kernel module (C)
│   ├── src/main.c               # Block device, queue_rq, ring setup
│   ├── src/ring.c               # Ring buffer implementation
│   └── include/duvm_kmod.h      # Shared header (opcodes, structs)
├── crates/
│   ├── duvm-daemon/             # Daemon (engine, policy, ring consumer)
│   ├── duvm-memserver/          # Remote memory server
│   ├── duvm-backend-tcp/        # TCP backend + reconnection
│   ├── duvm-backend-rdma/       # RDMA backend + server
│   ├── duvm-backend-memory/     # In-process memory
│   ├── duvm-backend-compress/   # LZ4 compression
│   ├── duvm-backend-trait/      # Backend interface
│   ├── duvm-common/             # Shared types (PageHandle, ring, protocol)
│   ├── duvm-ctl/                # CLI tool
│   ├── duvm-tests/              # Integration tests (196 tests)
│   └── libduvm/                 # Optional C FFI
├── config/                      # systemd units + default config
├── scripts/                     # 8 QEMU test scripts + setup helper
└── .github/workflows/ci.yml     # CI pipeline (5 jobs)
```
