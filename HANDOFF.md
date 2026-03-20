# Distributed UVM — Project Handoff

## What This Is

**duvm** (Distributed Unified Virtual Memory) — middleware that makes remote and heterogeneous memory transparently available to unmodified applications. Sits between applications and pluggable memory backends (RDMA, CXL, NVLink, compressed local).

## Current State

**Phase: Implementation — core framework complete, all bugs fixed, comprehensive testing done, LRU eviction working**

### What Works (Proven)

| What | Evidence | Command |
|---|---|---|
| Engine data path (store/load/invalidate) | 500 pages through LZ4 backend, byte-perfect | `make demo` |
| LRU policy with tier cascading | Prefers low-latency, cascades when full, skips unhealthy | `cargo run --example demo_proof --release -p duvm-daemon` |
| LRU eviction under memory pressure | Hot pages survive, cold pages evicted, new stores succeed | `cargo run --example demo_proof --release -p duvm-daemon` |
| Double-store handle leak fixed | Re-storing at same offset frees old page, capacity stays correct | `cargo run --example demo_proof --release -p duvm-daemon` |
| Config validation | Rejects max_pages=0, unknown strategy; CLI --socket override works | `cargo run --example demo_proof --release -p duvm-daemon` |
| Multi-backend cascading | Compress full → falls back to memory | `cargo run --example demo_proof --release -p duvm-daemon` |
| Cross-machine memory (calc1 ↔ calc2) | 10,000 pages over ConnectX-7, byte-perfect | `cargo run --example demo_distributed --release -p duvm-daemon` |
| Transparent page fault handling | 256 pages via userfaultfd, 22us/fault, zero errors | `cargo run --example demo_uffd --release -p duvm-daemon` |
| TCP remote memory backend | 100 pages round-tripped via TCP, all freed | `cargo run --example demo_proof --release -p duvm-daemon` |
| Daemon socket IPC | ping, status, backends, stats — all verified, JSON-safe | `cargo run --example demo_proof --release -p duvm-daemon` |
| Concurrent operations | 8 threads × 100 pages, thread-safe | `cargo run --example demo_proof --release -p duvm-daemon` |
| C FFI | 100 pages round-tripped from C program | `make demo-c` |
| Kernel module | Compiles as virtual block device for Linux 6.17 | `make kmod` |
| Test suite | 165 tests passing (unit + integration + comprehensive) | `cargo test` |
| Code quality | clippy -D warnings clean, rustfmt clean | `cargo clippy --all-targets -- -D warnings` |
| End-to-end proof | 12/12 subsystems verified in single demo | `cargo run --example demo_proof --release -p duvm-daemon` |

### Components

| Component | Status | Location |
|---|---|---|
| **duvm-common** | Complete | `crates/duvm-common/` — PageHandle, ring buffer, protocol, stats |
| **duvm-backend-trait** | Complete | `crates/duvm-backend-trait/` — Backend plugin interface |
| **duvm-backend-memory** | Complete | `crates/duvm-backend-memory/` — In-memory reference backend |
| **duvm-backend-compress** | Complete | `crates/duvm-backend-compress/` — LZ4 compression backend |
| **duvm-backend-tcp** | Complete | `crates/duvm-backend-tcp/` — TCP remote memory backend |
| **duvm-daemon** | Complete | `crates/duvm-daemon/` — Policy, backends, control socket, uffd |
| **duvm-ctl** | Complete | `crates/duvm-ctl/` — CLI tool (status, stats, backends, ping) |
| **duvm-memserver** | Complete | `crates/duvm-memserver/` — Remote memory server binary |
| **libduvm** | Complete | `crates/libduvm/` — Rust API + C FFI |
| **duvm-kmod** | Compiles | `duvm-kmod/` — Virtual block device swap target |

### Hardware Tested On

- 2x NVIDIA DGX Spark (128GB unified LPDDR5X each)
- 4x ConnectX-7 200Gbps RoCE direct cables
- aarch64, Linux 6.17.0-1008-nvidia, CUDA 13.0

## How to Build and Test

```bash
make build          # Build all Rust crates
make test           # Run all 165 tests
make check          # Format + lint + test
make kmod           # Build kernel module
make demo           # Engine demo
make demo-c         # C FFI demo
make bench          # Performance benchmarks
bash scripts/preflight.sh  # Verify all prerequisites

# End-to-end proof demo (exercises all 10 subsystems):
cargo run --example demo_proof --release -p duvm-daemon
```

## Prerequisites

```bash
# Required on compute nodes (where apps run):
sudo sysctl -w vm.unprivileged_userfaultfd=1
echo "vm.unprivileged_userfaultfd=1" | sudo tee /etc/sysctl.d/90-duvm.conf

# Required for kernel module (optional, for best performance):
sudo apt install linux-headers-$(uname -r)

# Required for QEMU testing (optional, for safe kmod development):
sudo apt install qemu-system-arm qemu-utils
```

## What's Next

Remaining for production:
- [ ] Wire userfaultfd handler to TCP backend (currently uses local pattern fill)
- [ ] Kernel module: test insmod + mkswap + swapon in QEMU VM
- [ ] Symmetric deployment: every node runs daemon + memserver
- [ ] Install script hardening and testing
- [ ] RDMA backend (libibverbs, bypasses TCP for 200Gbps wire-rate)

## Key Technical Decisions

See `DECISIONS.md` for comprehensive rationale and `research/decisions.md` for historical context.

| Decision | Choice | Why |
|---|---|---|
| Swap interception | Virtual block device (not frontswap) | frontswap removed in Linux 6.17; block device uses stable blk-mq API |
| Architecture | Symmetric — every node is compute + memory | User requirement: all nodes equal |
| Policy engine | LRU with tier-aware cascading | Prefers lowest-latency tier; cascades when full; skips unhealthy backends |
| Fallback mode | userfaultfd (C helper for aarch64 ABI) | Works without kernel module; proven at 22us/fault |
| Development safety | QEMU/KVM for kernel module testing | Crashes don't affect host |
| Kernel module dev | calc2 for hardware integration testing | Two identical machines available |
