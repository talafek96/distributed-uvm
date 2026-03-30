# Distributed UVM — Project Handoff

## What This Is

**duvm** (Distributed Unified Virtual Memory) — a service that pools RAM across a cluster of machines, making remote memory transparently available to unmodified applications. CPU and GPU workloads both benefit. Can be enabled/disabled at runtime.

## Current State

**Full-stack swap proven on real hardware (4GB, 14.6M pages, 0 errors). Dynamic RDMA memory allocation in progress — ODP + CPU pre-fault works but needs the slab-based grow/shrink design implemented.**

### What Works (Proven)

| What | Evidence | Command |
|---|---|---|
| **Full-stack swap PROVEN** | 4GB (14.6M pages) swapped through kmod→daemon→RDMA→memserver, 0 errors, system stable | `MADV_PAGEOUT` test on calc2→calc1 |
| **RDMA on real hardware** | 10,000 pages via ConnectX-7 RoCEv2, 15μs/page, 0 errors | `cargo run --release --example demo_rdma -p duvm-daemon` |
| **Async kmod I/O** | queue_rq never blocks; completion harvester thread + 5s blk-mq timeout | All QEMU tests pass in CI |
| **Dynamic RDMA memory (partial)** | ODP + CPU pre-fault: RSS grows from 4MB→20GB on connect, DONTNEED releases on disconnect | Tested, works but pre-faults full slab at once |
| TCP on real hardware | 10,000 pages calc1↔calc2 over ConnectX-7, 102μs/page, byte-perfect | `cargo run --example demo_distributed --release -p duvm-daemon` |
| Enable/disable service | `duvm-ctl enable/disable/drain` manage full lifecycle | `sudo duvm-ctl enable` / `sudo duvm-ctl disable` |
| Full distributed TCP path | kmod→daemon→TCP→memserver on B, data integrity verified, 13/13 | `bash scripts/test-distributed-qemu.sh` |
| Engine + policy + eviction | LRU, tier cascading, backend retry/fallback, 196 Rust tests | `cargo test` |

### Performance (measured on ConnectX-7 RoCEv2)

| Metric | TCP | RDMA (pre-faulted) | RDMA (ODP) | Hardware limit |
|---|---|---|---|---|
| Store latency | 102 μs/page | 15 μs/page | ~100 μs/page | <1 μs/page |
| Throughput | 24 MB/s | 274 MB/s | ~64 MB/s | **~23 GB/s** (190 Gbps) |

**We are using 1.2% of available bandwidth.** See "Immediate Next Step" for the fix.

### Hardware

- **calc1** (192.168.200.10, also .12): DGX Spark, 128GB, Secure Boot ON — used as memserver
- **calc2** (192.168.200.11, also .13): DGX Spark, 128GB, Secure Boot OFF — used as compute node
- **4x ConnectX-7 100Gbps RoCE** — two QSFP links active:
  - `rocep1s0f0` → `enp1s0f0np0` (Up) — subnet 192.168.200.x
  - `roceP2p1s0f0` → `enP2p1s0f0np0` (Up) — subnet 192.168.201.x (NOT USED YET)
- Combined bandwidth: **~190 Gbps** (92 + 97 per NVIDIA benchmarks)
- aarch64, Linux 6.17.0-1008-nvidia, CUDA 13.0
- Memory guard: `memory-watchdog` + `earlyoom` installed (must be stopped for swap testing)

## Immediate Next Step: Dynamic RDMA Memory + Performance

### The Problem

The RDMA memserver must dynamically borrow memory from calc1 — grow when calc2 needs it, shrink when done. We tried three approaches:

1. **MAP_POPULATE (pre-allocate all)**: Works, fast (15μs/page), but wastes memory. If `--max-pages=10M` (40GB), 40GB is pinned immediately whether used or not. **Unacceptable** — calc1 has its own workloads.

2. **Pure ODP (demand-page via NIC)**: Zero pre-allocation, RSS grows dynamically. But NIC ODP page faults are slow (~100μs/page vs 15μs pre-faulted) and fail with `IBV_WC_TRANSPORT_RETRY_COUNTER_EXCEEDED` under rapid sequential writes. Fixed with longer QP timeout (`set_qp_timeout(21, 7)` in shim.c), but throughput drops to ~64 MB/s.

3. **ODP + CPU pre-fault per client slab**: `mmap(MAP_NORESERVE)` + `ibv_reg_mr(ON_DEMAND)`, then `MADV_POPULATE_WRITE` on the client's slab at connect time. Works — RSS grows from 4MB to slab size on connect, drops on disconnect via `MADV_DONTNEED`. But it pre-faults the entire client slab at once (e.g., 20GB), which is back to the pre-allocation problem.

### The Correct Design (not yet implemented)

**Slab-based dynamic grow/shrink with CPU pre-faulting:**

1. `mmap(MAP_NORESERVE)` for max capacity — virtual only, 0 physical pages
2. `ibv_reg_mr(ON_DEMAND)` — one rkey for the whole range
3. Start with one small slab (e.g., 256MB) pre-faulted via `MADV_POPULATE_WRITE`
4. **Background monitor thread**: when current slabs are 75% full, pre-fault the next 256MB slab
5. NIC always writes to pre-faulted pages — 15μs/page, no ODP penalty
6. When a client disconnects, `MADV_DONTNEED` on all its slabs — physical pages released
7. No protocol changes — same single rkey + contiguous address space

This gives: **dynamic memory (grows 256MB at a time) + fast I/O (15μs/page) + observable RSS.**

### Also needed for full bandwidth

Per the [NVIDIA DGX Spark performance guide](https://github.com/NVIDIA/dgx-spark-playbooks/blob/main/nvidia/connect-two-sparks/assets/performance_benchmarking_guide.md#dual-spark-1):

- **Use BOTH QSFP links** (192.168.200.x and 192.168.201.x) — currently only using one
- **RDMA buffer pool** — replace single-page Mutex with N concurrent transfers
- **TX depth 128** — pipeline many RDMA WRITEs instead of one-at-a-time
- Target: **10+ GB/s** (from current 274 MB/s)

### Files to Change

| File | What |
|---|---|
| `crates/duvm-backend-rdma/src/server.rs` | Slab-based dynamic allocation + background monitor |
| `crates/duvm-backend-rdma/src/lib.rs` | Buffer pool (N concurrent transfers), use both links |
| `crates/duvm-backend-rdma/src/shim.c` | `duvm_set_qp_timeout` already added |
| `crates/duvm-backend-rdma/src/ffi.rs` | `IBV_ACCESS_ON_DEMAND`, `set_qp_timeout` already added |

## How to Run the Hardware Test

### On calc1 (memserver — no sudo needed):
```bash
cd ~/projects/distributed-uvm
./target/release/duvm-memserver --bind 192.168.200.10:9200 --rdma --rdma-port 9201 --max-pages 10000000 --rdma-pages-per-client 5000000
```

### On calc2 (compute node — needs sudo for kmod):
```bash
cd ~/projects/distributed-uvm
sudo bash scripts/setup-hardware-test.sh   # stops watchdog, loads kmod, mkswap, swapon
sudo sysctl vm.swappiness=100              # needed to trigger swap

cat > /tmp/duvm-hw.toml << 'EOF'
[daemon]
log_level = "info"
socket_path = "/tmp/duvm.sock"

[backends.memory]
enabled = false

[backends.remote]
enabled = true
transport = "auto"
peers = ["192.168.200.10:9201"]
max_pages_per_peer = 5000000
EOF

./target/release/duvm-daemon --config /tmp/duvm-hw.toml --kmod-ctl /dev/duvm_ctl &
sleep 5
/tmp/force_swap   # or ./scripts/swap_pressure_test

sudo bash scripts/setup-hardware-test.sh --teardown
sudo systemctl start memory-watchdog earlyoom
sudo sysctl vm.swappiness=10
```

### Monitoring on calc1:
```bash
watch -n 1 'echo "RSS: $(grep VmRSS /proc/$(pgrep duvm-memserver)/status | awk "{print \$2/1024\"MB\"}")" && rdma stat show link rocep1s0f0/1 | tr " " "\n" | grep -A1 rx_write'
```

## Key Findings This Session

### RDMA FFI constants were wrong
- `IBV_SEND_SIGNALED` was `1<<2` (=4, actually `IBV_SEND_SOLICITED`), correct: `1<<1` (=2)
- `IBV_WR_RDMA_READ` was 3, correct: 4
- SoftiWARP was lenient, real ConnectX-7 is strict

### Async kmod queue_rq prevents DGX Spark freeze
- Synchronous `submit_and_wait` in the kernel's memory reclaim path froze the system (3 power cycles)
- Converted to async: `queue_rq` submits to ring, returns immediately, completion harvester thread calls `blk_mq_end_request`
- Added 5s blk-mq timeout for orphaned requests (daemon killed by OOM)
- Daemon sets `oom_score_adj=-999` to survive OOM killer

### DGX Spark UMA is fragile
- System freezes when `MemFree < ~1GB` — NVIDIA GPU driver enters D-state
- `MemAvailable` is misleading on UMA — includes reclaimable cache that GPU can't wait for
- Must check `MemFree` for safety thresholds
- `memory-watchdog` and `earlyoom` must be stopped during swap testing (they kill the daemon)
- `vm.swappiness=100` alone doesn't trigger swap — need `MADV_PAGEOUT` for recently-touched pages

### ODP page faults are slow on ConnectX-7
- Pure ODP: NIC faults pages on DMA write, ~100μs per page (vs 15μs pre-faulted)
- Under rapid sequential writes: `IBV_WC_TRANSPORT_RETRY_COUNTER_EXCEEDED` (vendor_err=135)
- Fixed by `set_qp_timeout(21, 7)` — 8.6s per retry instead of 67ms
- ODP pages don't appear in VmRSS or MemFree — managed by RDMA subsystem
- CPU pre-fault via `MADV_POPULATE_WRITE` makes pages visible in RSS and fast for NIC

### Two QSFP links available
- `rocep1s0f0` (192.168.200.x) + `roceP2p1s0f0` (192.168.201.x) = ~190 Gbps
- Currently only using the first link
- NVIDIA benchmark shows 92 + 97 Gbps with `ib_write_bw`

## Known Gaps

### Performance (most impactful)

| Gap | Impact | Detail |
|---|---|---|
| **Single-page RDMA Mutex** | Using 1.2% of bandwidth | One transfer at a time. Need buffer pool with N pre-registered buffers for concurrent RDMA ops. |
| **Only using one QSFP link** | Using 50% of links | Second link (192.168.201.x) not configured. Need multi-path or bonding. |
| **No discard/invalidation** | Memory leak on memserver | Kernel sends `REQ_OP_DISCARD` but kmod ignores it. Pages accumulate forever. |

### Operational

| Gap | Impact | Detail |
|---|---|---|
| No authentication / TLS | Security | Anyone on the network can read/write pages. |
| Static peer config | Operational | Adding a node requires config edit + restart. |
| RDMA backend has no reconnection | Resilience | TCP has auto-reconnect + circuit breaker; RDMA doesn't. |

## Test Summary

| Test | Checks | Status |
|---|---|---|
| `cargo test` | 196 pass | ✅ All green |
| QEMU E2E (8 scripts) | 82 checks | ✅ All green in CI |
| RDMA hardware (demo_rdma.rs) | 10K pages | ✅ 15μs/page, 0 errors |
| Full-stack swap (force_swap) | 4GB / 14.6M pages | ✅ 0 errors, system stable |

## Files Changed This Session

| File | Change |
|---|---|
| `duvm-kmod/src/main.c` | Async queue_rq, completion thread, staging bitmap, timeout handler |
| `duvm-kmod/src/ring.c` | `duvm_ring_submit()`, `duvm_ring_poll_completion()`, staging bitmap |
| `duvm-kmod/include/duvm_kmod.h` | `duvm_cmd` PDU, `comp_thread`, bitmap, new ring APIs |
| `crates/duvm-daemon/src/kmod_ring.rs` | write() to ctl fd after completion (wake kernel) |
| `crates/duvm-daemon/src/main.rs` | oom_score_adj=-999 |
| `crates/duvm-daemon/examples/demo_rdma.rs` | RDMA hardware test binary |
| `crates/duvm-backend-rdma/src/ffi.rs` | Fix FFI constants, add ON_DEMAND + set_qp_timeout |
| `crates/duvm-backend-rdma/src/lib.rs` | rnr_retry_count, QP timeout for ODP |
| `crates/duvm-backend-rdma/src/server.rs` | ODP + slab pre-fault + DONTNEED on disconnect |
| `crates/duvm-backend-rdma/src/shim.c` | `duvm_set_qp_timeout()` wrapper |
| `crates/duvm-backend-rdma/build.rs` | rerun-if-changed for shim.c |
| `scripts/setup-hardware-test.sh` | One-command hardware test setup |
| `scripts/swap_pressure_test.c` | Safe MemFree-based swap test |
