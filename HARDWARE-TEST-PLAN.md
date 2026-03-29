# Hardware Testing Plan — duvm on DGX Spark

## Hardware Available

- **calc1:** DGX Spark, 192.168.200.10, 128GB LPDDR5X, aarch64, Linux 6.17.0-1008-nvidia
- **calc2:** DGX Spark, 192.168.200.11, 128GB LPDDR5X, aarch64, Linux 6.17.0-1008-nvidia
- **Interconnect:** 4x ConnectX-7 200Gbps RoCEv2 (direct cable, mlx5_0..mlx5_3)
- **GPU:** Blackwell GB10, CUDA 13.0, UVM via hardware ATS

## What's Already Been Proven

| What | How | Transport |
|------|-----|-----------|
| Full RDMA verbs path (WRITE/READ, CM handshake, data integrity) | SoftiWARP in QEMU (18 checks) | Software RDMA over TCP |
| Cross-machine TCP (10,000 pages, byte-perfect) | `demo_distributed.rs` on calc1→calc2 | TCP over ConnectX-7 |
| Full kmod→daemon→TCP→memserver path | QEMU two-VM test (13 checks) | TCP in QEMU |

**What has NOT been tested:** Real RoCEv2 RDMA on the ConnectX-7 hardware. SoftiWARP proves the API works but uses TCP underneath — it does not exercise one-sided RDMA WRITE/READ bypassing the remote CPU, which is the entire production performance story.

## Test Plan — Step by Step

### Test 1: Verify RDMA hardware is working

**On both machines:**

```bash
# Check devices exist
ibv_devices
# Expected: mlx5_0, mlx5_1, mlx5_2, mlx5_3

# Check link is active
ibv_devinfo mlx5_0 | grep -E "state|active_speed"
# Expected: state: PORT_ACTIVE, active_speed: 200 Gbps (HDR)

# Verify RoCE GID table has a routable address
ibv_devinfo mlx5_0 -v | grep GID
# Expected: GID[0] with a valid address
```

**Between machines:**

```bash
# From calc1, ping calc2's RDMA address
rping -c -a 192.168.200.11 -C 10 -v
# Expected: 10 successful RDMA pings
```

**Pass condition:** `rping` succeeds. If it fails, RoCE configuration (PFC, ECN, routing) needs fixing before proceeding.

### Test 2: RDMA backend direct test (no kernel module)

This tests the RDMA backend in isolation — same as the SoftiWARP QEMU test but on real hardware.

**On calc2:**

```bash
cargo build --release -p duvm-memserver
./target/release/duvm-memserver --bind 0.0.0.0:9200 --rdma --rdma-port 9201 --max-pages 1000000
```

**On calc1:**

Need a test binary that uses `RdmaBackend` directly. The existing `demo_distributed.rs` uses `TcpBackend`. Either:

**(a)** Duplicate it with `RdmaBackend`:

```rust
// crates/duvm-daemon/examples/demo_rdma.rs
use duvm_backend_rdma::RdmaBackend;
// Same 10,000-page store/load/verify pattern as demo_distributed.rs
// but using RdmaBackend instead of TcpBackend
const REMOTE_ADDR: &str = "192.168.200.11:9201"; // RDMA port
```

**(b)** Or add a `--transport rdma` flag to the existing demo.

**What to measure:**
- Store latency per page (expect 1-5μs, TCP baseline was ~16μs)
- Load latency per page (expect 1-5μs)
- Throughput (expect 5-25 GB/s, TCP baseline was 0.24 GB/s)
- Data integrity (10,000 pages, byte-for-byte)

**Pass condition:** All 10,000 pages verified, latency <10μs/page, throughput >1 GB/s.

### Test 3: Full-stack transparent swap over RDMA

This is the real product test: unmodified application memory transparently swapping to a remote machine over RDMA.

**On calc2:**

```bash
./target/release/duvm-memserver --bind 0.0.0.0:9200 --rdma --rdma-port 9201 --max-pages 2000000
```

**On calc1:**

```bash
# Build and load kernel module
make -C duvm-kmod
sudo insmod duvm-kmod/duvm-kmod.ko size_mb=4096 ring_entries=256
sudo mkswap /dev/duvm_swap0

# Configure daemon with RDMA backend
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
peers = ["192.168.200.11:9201"]
max_pages_per_peer = 1000000
EOF

# Start daemon
./target/release/duvm-daemon --config /tmp/duvm-hw.toml --kmod-ctl /dev/duvm_ctl &

# Activate swap (priority 100 = prefer over local SSD)
sudo swapon -p 100 /dev/duvm_swap0
```

**Then run a real workload that triggers swap:**

```bash
# Simple: allocate more memory than available
# (stress-ng or a custom malloc loop)
stress-ng --vm 1 --vm-bytes 140G --vm-keep --timeout 30s

# Or run a CUDA workload that uses unified memory
# (GPU UVM pages go through the same swap path via ATS)
```

**What to verify:**
- `duvm-ctl status` shows pages being stored
- `cat /proc/swaps` shows duvm_swap0 with used pages
- Daemon logs show "Remote TCP backend connected" or "Remote RDMA backend connected"
- Workload completes without crashes
- `sudo swapoff /dev/duvm_swap0` drains pages back (may take a while)

**Pass condition:** Workload completes, pages swapped to calc2 and back, no data corruption, no hang.

### Test 4: Performance benchmarking

**After Test 3 passes**, measure production performance:

```bash
# On calc1, with the full stack running:

# Measure page fault latency under swap pressure
perf stat -e page-faults stress-ng --vm 1 --vm-bytes 140G --timeout 10s

# Measure daemon's store/load latency
duvm-ctl stats
# Look at avg_store_ns, avg_load_ns

# Compare RDMA vs TCP:
# Reconfigure with transport = "tcp", repeat, compare latencies
```

**Expected results:**

| Metric | TCP (measured) | RDMA (expected) |
|--------|---------------|-----------------|
| Store latency | ~16μs | 1-5μs |
| Load latency | ~16μs | 1-5μs |
| Throughput | 0.24 GB/s | 5-25 GB/s |
| CPU usage | High (kernel TCP stack) | Near-zero (one-sided bypass) |

## Prerequisites / Blockers

| Item | Status | Notes |
|------|--------|-------|
| RDMA libs installed on both machines | Likely yes | `ibv_devices` should work. If not: `apt install libibverbs-dev librdmacm-dev ibverbs-utils rdmacm-utils` |
| RoCE properly configured | Unknown | May need PFC/ECN settings on ConnectX-7. If `rping` fails, this is the issue. |
| Kernel module builds on DGX Spark | Previously worked | `make -C duvm-kmod` with Linux 6.17 headers |
| sudo access on both machines | Required | For insmod, mkswap, swapon |
| `demo_rdma.rs` test binary | Not yet written | Need to create (copy `demo_distributed.rs`, swap TcpBackend → RdmaBackend) |

## Known Risks (from PITFALLS.md)

1. **SoftRoCE doesn't work over QEMU sockets** — this is why we used SoftiWARP. On real hardware with ConnectX-7 this shouldn't matter, but it means our QEMU RDMA tests used a fundamentally different transport than production.

2. **RDMA CM event constants were wrong** — fixed in commit b064cba. The FFI enum values now match the C header.

3. **`ibv_post_send`/`ibv_poll_cq` are inline C functions** — can't be called from Rust directly. Already solved with a C shim (`crates/duvm-backend-rdma/src/shim.c`).

4. **`rdma_get_cm_event` blocks forever without timeout** — already solved with `poll()` + timeout wrapper.

5. **Single-page RDMA buffer under Mutex** — all transfers serialized. This won't cause incorrect results but will bottleneck throughput. Buffer pool is Phase 4 item 2.

## Order of Operations

```
1. rping between calc1 and calc2                    (5 minutes)
   └── If fails: fix RoCE config (PFC/ECN)

2. Write demo_rdma.rs                               (30 minutes)
   └── Copy demo_distributed.rs, swap to RdmaBackend

3. Run RDMA direct test (Test 2)                     (10 minutes)
   └── If fails: check daemon logs, ibv_devinfo, firewall

4. Run full-stack test (Test 3)                      (30 minutes)
   └── If fails: check kmod dmesg, daemon logs, memserver logs

5. Performance benchmarking (Test 4)                 (1 hour)
   └── Record baseline numbers for TCP vs RDMA
```

Total estimated time: ~2 hours, assuming no RoCE configuration issues.
