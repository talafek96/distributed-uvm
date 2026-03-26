# Distributed UVM — Project Handoff

## What This Is

**duvm** (Distributed Unified Virtual Memory) — a service that pools RAM across a cluster of machines, making remote memory transparently available to unmodified applications. CPU and GPU workloads both benefit. Can be enabled/disabled at runtime.

## Current State

**Phase: Service management implemented. Enable/disable/drain via duvm-ctl. All bugs from Phase 1 fixed.**

### What Works (Proven)

| What | Evidence | Command |
|---|---|---|
| **Enable/disable service** | `duvm-ctl enable/disable/drain` manage full lifecycle | `sudo duvm-ctl enable` / `sudo duvm-ctl disable` |
| RDMA end-to-end (SoftiWARP) | Two VMs: daemon → RDMA WRITE → memserver MR, 18/18 | `bash scripts/test-rdma-qemu.sh` |
| Kernel module ↔ daemon ring buffer | Pages flow kmod → ring → daemon → backend, 10/10 QEMU | `bash scripts/test-kmod-daemon-qemu.sh` |
| Two-VM distributed test | VM-A (kmod+daemon) talks to VM-B (memserver), 12/12 | `bash scripts/test-distributed-qemu.sh` |
| Kernel module standalone | insmod, mkswap, swapon, block I/O, rmmod — 16/16 QEMU | `bash scripts/test-kmod-qemu.sh` |
| Cross-machine TCP (real hardware) | 10,000 pages calc1↔calc2 over ConnectX-7, byte-perfect | `cargo run --example demo_distributed --release -p duvm-daemon` |
| Engine + policy + eviction + transport | LRU, tier cascading, transport modes, 196 Rust tests | `cargo test` |

### Test Summary

| Test | Checks | What it proves |
|---|---|---|
| `cargo test` | 196 pass | User-space engine, policy, backends, config, transport modes, TCP capacity, reconnection |
| `test-kmod-qemu.sh` | 16/16 | Kernel module: load, block I/O, swap, unload |
| `test-kmod-daemon-qemu.sh` | 10/10 | Ring buffer: kmod → daemon → engine → backend |
| `test-distributed-qemu.sh` | 12/12 | Two VMs: kmod+daemon on A, memserver on B, network I/O |
| `test-mutual-oom-qemu.sh` | 9/9 | Two VMs: mutual OOM degradation, graceful fallback |
| `test-3machine-qemu.sh` | 10/10 | Three VMs: fair distribution across peers, exhaustion handling |
| `test-rdma-qemu.sh` | 18/18 | **Full RDMA: SoftiWARP, CM handshake, one-sided WRITE/READ, data integrity** |
| `test-memserver-concurrent-qemu.sh` | 6/6 | **Concurrent TCP clients: parallel alloc, capacity enforcement under load** |

All QEMU tests run in CI (`e2e-kmod` job on `ubuntu-24.04-arm`).

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
| **duvm-backend-tcp** | Complete — reconnection | `crates/duvm-backend-tcp/` — TCP remote memory backend with auto-reconnect + circuit breaker |
| **duvm-backend-rdma** | Complete — end-to-end verified | `crates/duvm-backend-rdma/` — RDMA (libibverbs + librdmacm) backend + server |
| **duvm-ctl** | Complete — enable/disable/drain | `crates/duvm-ctl/` — CLI: status, stats, backends, ping, enable, disable, drain |
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

# Run Rust tests (196 tests, no sudo needed)
cargo test

# QEMU tests (no sudo needed)
bash scripts/test-kmod-qemu.sh            # Kernel module standalone
bash scripts/test-kmod-daemon-qemu.sh      # Kmod + daemon ring buffer
bash scripts/test-distributed-qemu.sh      # Two VMs: distributed memory
bash scripts/test-rdma-qemu.sh             # SoftRoCE + auto-fallback

# Enable/disable distributed memory (requires sudo):
sudo duvm-ctl enable                       # Load kmod, start services, activate swap
sudo duvm-ctl disable                      # Drain pages, stop services, unload kmod
sudo duvm-ctl drain                        # Migrate pages back, keep services running
duvm-ctl status                            # Check daemon status (no sudo)

# Manual setup (alternative to duvm-ctl enable):
sudo bash scripts/setup-kmod-for-testing.sh        # Load kmod, set permissions
cargo run --release -p duvm-daemon -- --kmod-ctl /dev/duvm_ctl  # Start daemon
sudo swapon -p 100 /dev/duvm_swap0                 # Activate swap
sudo bash scripts/setup-kmod-for-testing.sh --teardown  # Cleanup
```

## Known Gaps

### Bugs — all Phase 1 bugs fixed

| Gap | Status | Detail |
|---|---|---|
| ~~RDMA server CQ leak~~ | **Fixed** | Per-connection CQs tracked in HashMap, destroyed on disconnect and shutdown. |
| ~~`alloc_page()` TOCTOU race~~ | **Fixed** | Replaced load-then-fetch_add with `compare_exchange` loop in RDMA backend, TCP backend, and memserver. |
| ~~`rdma_cm_event` struct padding~~ | **Fixed** | `_pad: [u8; 36]` now matches real `sizeof(rdma_cm_event) = 80`. |

### Operational — Phase 2 complete

| Gap | Status | Detail |
|---|---|---|
| ~~No enable/disable service~~ | **Fixed** | `duvm-ctl enable/disable/drain` + systemd units for daemon, memserver, kmod. Uses absolute paths for security. |
| ~~No graceful drain~~ | **Fixed** | `duvm-ctl drain` runs swapoff, migrating remote pages back to local RAM. |
| ~~Memserver single-threaded~~ | **Fixed** | `thread::spawn` per TCP client. Multiple clients served concurrently. |

### Remaining gaps

#### Security (Critical for production)

| Gap | Severity | Detail |
|---|---|---|
| No authentication | Critical | TCP/RDMA connections have no auth. Anyone who can reach port 9200 can read/write any page. DECISIONS.md says "trusted network" — insufficient for multi-tenant or cloud. |
| No encryption | Critical | Pages transmitted in plaintext over TCP. Sensitive application data exposed on the wire. |
| Memserver accepts any client | High | No ACL, no connection allow-list. No way to restrict which machines can store/load pages. |

#### Cluster management (Critical for multi-node)

| Gap | Severity | Detail |
|---|---|---|
| Static peer config only | High | Peers must be listed in `duvm.toml` before daemon starts. Adding a node requires editing config + restarting daemon on every existing node. |
| No peer discovery | High | No integration with etcd/consul/DNS-SD. Each node must be manually configured to know about every other node. |
| No config reload | Medium | `duvm-daemon.service` has `ExecReload=/bin/kill -HUP $MAINPID` but daemon doesn't handle SIGHUP. Config changes require full restart. |
| No runtime peer add/remove | Medium | No `duvm-ctl add-peer` or `remove-peer` commands. Daemon socket only supports status/stats/backends/ping. |
| No cluster health monitoring | Medium | No periodic health checks on remote peers. Backend health only checked at page allocation time. |

#### Resilience

| Gap | Severity | Detail |
|---|---|---|
| RDMA backend has no reconnection | High | Unlike TCP (which now auto-reconnects), RDMA connection drop is permanent. `is_healthy()` only checks init state, doesn't detect broken connections. |
| Engine has no retry/fallback | Medium | If selected backend's `store_page`/`load_page` fails, daemon returns error immediately — doesn't try another backend of the same tier. |
| Daemon shutdown doesn't drain pages | Medium | `Ctrl+C` / SIGTERM calls `backend.shutdown()` but doesn't run `swapoff` first. Remote pages are abandoned. Operator must run `duvm-ctl drain` manually before stopping. |
| Memserver has no connection limits | Medium | Unbounded `thread::spawn` per client. No max connections, no idle timeout, no per-client page limits. DoS risk. |

#### Performance

| Gap | Severity | Detail |
|---|---|---|
| Single-page RDMA buffer | Medium | RDMA backend holds Mutex for entire transfer (including 5s timeout). All RDMA ops serialized. Need buffer pool. |
| No real RDMA hardware validation | Important | Only tested with SoftiWARP in QEMU. ConnectX-7 RoCEv2 on DGX Spark available but untested. |

#### Observability

| Gap | Severity | Detail |
|---|---|---|
| Prometheus metrics not implemented | Medium | Config has `metrics_port: 9100` but nothing listens on it. Stats exist in `DaemonStats` but only accessible via Unix socket. |

#### Test coverage

| Gap | Severity | Detail |
|---|---|---|
| `test-distributed-qemu.sh` doesn't test TCP path | High | Uses local memory backend, only checks TCP connectivity with `nc`. Pages don't actually flow kmod → daemon → TCP → memserver. |
| `test-3machine-qemu.sh` doesn't verify distribution | Medium | Writes pages and reads them back, but doesn't check that pages actually went to different peers (B and C). |
| No `duvm-ctl enable/disable` integration test | Medium | These commands are untested end-to-end. |
| No daemon crash recovery test | Medium | No test for: daemon dies mid-request → kmod doesn't hang → kernel falls back. |
| No multi-peer failover test | Medium | No test for: peer A dies → new pages route to peer B → no hang. |
| No RDMA negative tests | Medium | No tests for RDMA connection timeout, rejection, address resolution failure. |

## What's Next

### Phase 3 — Production hardening (no hardware needed)

1. **Fix `test-distributed-qemu.sh`** — configure TCP backend in daemon so pages actually flow kmod → daemon → TCP → memserver → verify data integrity. Highest-impact test gap.
2. **Engine retry/fallback** — when `store_page` fails on one backend, try the next healthy backend of the same tier before returning error to kernel.
3. **Prometheus metrics** — HTTP listener on `metrics_port` exposing `DaemonStats` + backend health in Prometheus exposition format.
4. **SIGHUP config reload** — daemon re-reads `duvm.toml` on SIGHUP, adds/removes backends for new/removed peers without full restart.
5. **Memserver connection limits** — max connections, idle timeout, per-client page quota.
6. **RDMA reconnection** — port the TCP auto-reconnect + circuit breaker pattern to RDMA backend.

### Phase 4 — Production validation (needs hardware)

1. **Real RDMA hardware test** — ConnectX-7 RoCEv2 on DGX Spark, measure latency vs TCP.
2. **RDMA buffer pool** — replace single-page Mutex buffer with lock-free pool for concurrent ops.
3. **Multi-peer failover QEMU test** — 3 VMs, kill one peer mid-operation, verify traffic reroutes.
4. **Daemon crash recovery QEMU test** — kill daemon mid-request, verify kmod timeout + kernel fallback.

### Phase 5 — Cluster management (design needed)

1. **Peer discovery** — etcd/consul/DNS-SD integration or simple multicast.
2. **Runtime peer add/remove** — `duvm-ctl add-peer` / `remove-peer` + daemon socket commands.
3. **TLS + auth** — mutual TLS for TCP, token-based auth for RDMA private data.
4. **Page migration on peer leave** — drain pages from departing node to remaining nodes.

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
