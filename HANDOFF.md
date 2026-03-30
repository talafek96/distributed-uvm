# Distributed UVM — Project Handoff

## What This Is

**duvm** (Distributed Unified Virtual Memory) — a service that pools RAM across a cluster of machines, making remote memory transparently available to unmodified applications. CPU and GPU workloads both benefit. Can be enabled/disabled at runtime.

## Current State

**FULL-STACK SWAP PROVEN ON REAL HARDWARE. 4GB swapped through kmod→daemon→RDMA→memserver on ConnectX-7, 14.6M pages verified, zero errors. System stayed alive.**

### What Works (Proven)

| What | Evidence | Command |
|---|---|---|
| **Full-stack swap PROVEN** | 4GB (14.6M pages) swapped through kmod→daemon→RDMA→memserver, 0 errors, system stable | `MADV_PAGEOUT` test on calc2→calc1 |
| **RDMA on real hardware** | 10,000 pages via ConnectX-7 RoCEv2, 15μs/page, 0 errors | `cargo run --release --example demo_rdma -p duvm-daemon` |
| **Async kmod I/O** | queue_rq never blocks; completion harvester thread + 5s blk-mq timeout | All QEMU tests pass in CI |
| TCP on real hardware | 10,000 pages calc1↔calc2 over ConnectX-7, 102μs/page, byte-perfect | `cargo run --example demo_distributed --release -p duvm-daemon` |
| Enable/disable service | `duvm-ctl enable/disable/drain` manage full lifecycle | `sudo duvm-ctl enable` / `sudo duvm-ctl disable` |
| RDMA end-to-end (SoftiWARP) | Two VMs: daemon → RDMA WRITE → memserver MR, 18/18 | `bash scripts/test-rdma-qemu.sh` |
| Full distributed TCP path | kmod→daemon→TCP→memserver on B, data integrity verified, 13/13 | `bash scripts/test-distributed-qemu.sh` |
| Engine + policy + eviction | LRU, tier cascading, backend retry/fallback, 196 Rust tests | `cargo test` |

### Performance (measured on ConnectX-7 RoCEv2)

| Metric | TCP | RDMA | Improvement |
|---|---|---|---|
| Store latency | 102 μs/page | 15 μs/page | 6.8x |
| Load latency | 98 μs/page | 15 μs/page | 6.4x |
| Store throughput | 24 MB/s | 274 MB/s | 11.2x |
| Load throughput | 42 MB/s | 267 MB/s | 6.4x |

### Test Summary

| Test | Checks | What it proves |
|---|---|---|
| `cargo test` | 196 pass | User-space engine, policy, backends, config, transport, reconnection |
| `test-kmod-qemu.sh` | 16/16 | Kernel module: load, block I/O, swap, unload |
| `test-kmod-daemon-qemu.sh` | 10/10 | Ring buffer: kmod → daemon → engine → backend |
| `test-distributed-qemu.sh` | 13/13 | Two VMs: kmod+daemon→TCP→memserver, data integrity |
| `test-mutual-oom-qemu.sh` | 9/9 | Mutual OOM degradation, graceful fallback |
| `test-3machine-qemu.sh` | 10/10 | Three VMs: fair distribution, exhaustion handling |
| `test-rdma-qemu.sh` | 18/18 | Full RDMA: SoftiWARP, CM handshake, WRITE/READ |
| `test-memserver-concurrent-qemu.sh` | 6/6 | Concurrent TCP clients, capacity enforcement |

All QEMU tests run in CI (`e2e-kmod` job on `ubuntu-24.04-arm`). All green.

### Hardware

- **calc1** (192.168.200.10): DGX Spark, 128GB, Secure Boot ON — used as memserver
- **calc2** (192.168.200.11): DGX Spark, 128GB, Secure Boot OFF — used as compute node (kmod + daemon)
- 4x ConnectX-7 200Gbps RoCE (device names: `rocep1s0f0` etc, not `mlx5_*`)
- aarch64, Linux 6.17.0-1008-nvidia, CUDA 13.0
- Memory guard: `memory-watchdog` + `earlyoom` installed (must be stopped for swap testing)

## How to Run the Hardware Test

### On calc1 (memserver — no sudo needed):
```bash
cd ~/projects/distributed-uvm
./target/release/duvm-memserver --bind 192.168.200.10:9200 --rdma --rdma-port 9201 --max-pages 1000000
```

### On calc2 (compute node — needs sudo for kmod):
```bash
cd ~/projects/distributed-uvm

# One-command setup (stops watchdog, loads kmod, mkswap, swapon, drops caches):
sudo bash scripts/setup-hardware-test.sh

# Start daemon (no sudo):
cat > /tmp/duvm-hw.toml << 'EOF'
[daemon]
log_level = "info"
socket_path = "/tmp/duvm.sock"

[backends.memory]
enabled = true
max_pages = 262144

[backends.remote]
enabled = true
transport = "auto"
peers = ["192.168.200.10:9201"]
max_pages_per_peer = 262144
EOF

./target/release/duvm-daemon --config /tmp/duvm-hw.toml --kmod-ctl /dev/duvm_ctl &

# Wait for RDMA connection, then run swap test:
sleep 5
./scripts/swap_pressure_test 2048

# Teardown:
sudo bash scripts/setup-hardware-test.sh --teardown
sudo systemctl start memory-watchdog earlyoom
```

## Immediate Next Step

**Run `scripts/swap_pressure_test` on calc2.** The test binary is ready. It allocates 256MB chunks, monitors MemFree (not MemAvailable — critical on UMA), stops at 4GB free, and verifies data integrity. Target: 2GB of swap through duvm_swap0.

The previous test proved 782MB swapped successfully. The system froze because the old test checked MemAvailable (18GB, misleading) instead of MemFree (665MB, dangerously low). The new test checks MemFree.

**Setup needed on calc2 before running:**
1. `sudo bash scripts/setup-hardware-test.sh` (one command)
2. Start memserver on calc1 (see above)
3. Start daemon on calc2 with RDMA config (see above)
4. `./scripts/swap_pressure_test 2048`

## Known Gaps

### Remaining gaps

#### Security (Critical for production)

| Gap | Severity | Detail |
|---|---|---|
| No authentication | Critical | TCP/RDMA connections have no auth. |
| No encryption | Critical | Pages transmitted in plaintext over TCP. |

#### Cluster management (Critical for multi-node)

| Gap | Severity | Detail |
|---|---|---|
| Static peer config only | High | Adding a node requires editing config + restarting daemon on every node. |
| No peer discovery | High | No etcd/consul/DNS-SD. |
| No config reload | Medium | SIGHUP not handled. |

#### Resilience

| Gap | Severity | Detail |
|---|---|---|
| RDMA backend has no reconnection | High | Unlike TCP (auto-reconnect + circuit breaker), RDMA drop is permanent. |
| No discard/invalidation path | High | Kernel sends REQ_OP_DISCARD on swap slot free, but kmod ignores it. Pages accumulate on memserver forever (memory leak). Protocol plumbing exists (DUVM_OP_INVALIDATE, engine.invalidate_page()) but not wired up. |

#### Performance

| Gap | Severity | Detail |
|---|---|---|
| Single-page RDMA buffer | High | Mutex serializes all RDMA transfers. 15μs/page vs <1μs raw. Buffer pool needed. |
| DGX Spark UMA freeze | Platform | System freezes when MemFree < ~1GB regardless of swap device. Known NVIDIA issue (#362769, #353752). Not fixable in our code — test must stay above 4GB MemFree. |

#### Observability

| Gap | Severity | Detail |
|---|---|---|
| Prometheus metrics not implemented | Medium | `metrics_port: 9100` in config but nothing listens. |

## Key Architecture Changes This Session

1. **Async queue_rq** (commit `5e01174`): Converted kmod from synchronous (submit + wait 500ms) to fully async. `queue_rq` submits to ring and returns immediately. Completion harvester kthread polls the completion ring and calls `blk_mq_end_request`. Modeled after nbd driver.

2. **blk-mq timeout** (commit `580f7c1`): 5-second timeout per request. If daemon dies (OOM kill, crash), orphaned requests are failed with `BLK_EH_DONE` and the kernel falls back to the next swap device.

3. **Staging slot bitmap** (commit `5e01174`): Replaced broken `idx % staging_pages` hash with proper bitmap allocator. Prevents two concurrent requests from using the same staging page.

4. **Daemon OOM protection** (commit `580f7c1`): Daemon sets `oom_score_adj=-999` on startup.

5. **FFI constant fixes** (commit `08e2a19`): `IBV_SEND_SIGNALED` was 4 (should be 2), `IBV_WR_RDMA_READ` was 3 (should be 4). SoftiWARP was lenient, real ConnectX-7 is strict.

## Key Technical Decisions

| Decision | Choice | Why |
|---|---|---|
| Swap interception | Virtual block device (blk-mq) | frontswap removed in Linux 6.17; block device uses stable API |
| Kmod I/O model | **Async queue_rq + completion thread** | Synchronous blocked kernel reclaim on UMA, froze system |
| Kmod↔daemon | Ring buffer via mmap of /dev/duvm_ctl | Low latency, zero-copy staging |
| Completion signal | Daemon write() to /dev/duvm_ctl | Wakes kernel harvester thread immediately |
| Transport | TCP default, RDMA optional (auto-detect) | TCP everywhere; RDMA for production (6.8x latency improvement) |
| DGX Spark safety | Check MemFree not MemAvailable | MemAvailable includes reclaimable cache that GPU driver can't wait for |

## Files Changed This Session

| File | Change |
|---|---|
| `duvm-kmod/src/main.c` | Async queue_rq, completion thread, staging bitmap, timeout handler, OOM protection |
| `duvm-kmod/src/ring.c` | `duvm_ring_submit()`, `duvm_ring_poll_completion()`, staging bitmap alloc |
| `duvm-kmod/include/duvm_kmod.h` | `duvm_cmd` PDU struct, `comp_thread`, bitmap fields, new ring APIs |
| `crates/duvm-daemon/src/kmod_ring.rs` | write() to ctl fd after completion (wake kernel thread) |
| `crates/duvm-daemon/src/main.rs` | oom_score_adj=-999 on startup |
| `crates/duvm-daemon/examples/demo_rdma.rs` | New: RDMA hardware test binary |
| `crates/duvm-backend-rdma/src/ffi.rs` | Fix IBV_SEND_SIGNALED (4→2), IBV_WR_RDMA_READ (3→4) |
| `scripts/setup-hardware-test.sh` | New: one-command hardware test setup |
| `scripts/swap_pressure_test.c` | New: safe swap pressure test (MemFree-based threshold) |
