# duvm — Distributed Unified Virtual Memory

A middleware that makes remote and heterogeneous memory transparently available to unmodified applications. Sits between applications and pluggable memory backends (RDMA, CXL, NVLink, compressed local).

## Architecture

```
┌─────────────────────────────────────────────────┐
│  Applications                                   │
│  ┌──────────────────┐  ┌──────────────────────┐ │
│  │ Unmodified apps   │  │ Optimized apps       │ │
│  │ (transparent via  │  │ (use libduvm API)    │ │
│  │  kernel swap)     │  │                      │ │
│  └────────┬─────────┘  └──────────┬───────────┘ │
├───────────┼────────────────────────┼─────────────┤
│ KERNEL    │    duvm-kmod.ko (thin relay)         │
│           └──► ring buffer ──────────────────┐   │
├──────────────────────────────────────────────┤   │
│ USER-SPACE                                   │   │
│           ┌──────────────────────────────────┘   │
│           ▼                                      │
│   duvm-daemon (Rust)                             │
│   ├── Policy engine (LRU / CLOCK-Pro)            │
│   ├── Backend plugins (.so)                      │
│   │   ├── memory  (in-memory, for testing)       │
│   │   ├── compress (LZ4 compression)             │
│   │   ├── rdma    (planned)                      │
│   │   └── cxl     (planned)                      │
│   └── Cluster management                         │
└──────────────────────────────────────────────────┘
```

Three components:

| Component | Description | Language |
|---|---|---|
| **duvm-kmod** | Thin kernel module — intercepts page swap events, relays to daemon via lock-free ring buffer | C |
| **duvm-daemon** | User-space daemon — policy engine, backend plugins, cluster management | Rust |
| **libduvm** | Optional library — rich API for apps that want fine-grained control (Rust + C FFI) | Rust |

## Quick Start

### Prerequisites

- Rust 1.75+ (install via [rustup](https://rustup.rs))
- GCC (for kernel module)
- Linux kernel headers (for kernel module, optional)

### Build

```bash
# Build all Rust crates
make build

# Build in release mode (optimized)
make release

# Build kernel module (optional, requires kernel headers)
make kmod
```

### Test

```bash
# Run all tests (42 tests: unit + integration)
make test

# Run with verbose output
make test-verbose

# Run only unit tests
make test-unit

# Run only integration tests
make test-integration
```

### Quality Checks

```bash
# Run all checks (format + lint + test)
make check

# Individual checks
make fmt-check    # Check formatting
make clippy       # Run clippy linter
```

## Project Structure

```
distributed-uvm/
├── Cargo.toml                    # Workspace root
├── Makefile                      # Easy-to-use build commands
├── README.md                     # This file
├── AGENTS.md                     # AI agent rules and conventions
├── HANDOFF.md                    # Current state and what's next
│
├── crates/
│   ├── duvm-common/              # Shared types: PageHandle, ring buffer, protocol
│   ├── duvm-backend-trait/       # Backend plugin interface (trait)
│   ├── duvm-backend-memory/      # In-memory backend (testing/development)
│   ├── duvm-backend-compress/    # LZ4 compression backend
│   ├── duvm-daemon/              # User-space daemon binary
│   ├── duvm-ctl/                 # CLI management tool
│   ├── libduvm/                  # User-space library (Rust + C FFI)
│   └── duvm-tests/               # Integration tests
│
├── duvm-kmod/                    # Linux kernel module (C)
│   ├── Makefile
│   ├── include/duvm_kmod.h
│   └── src/{main,ring,swap}.c
│
└── research/                     # Architecture design documents
    ├── prior-art.md              # 20+ existing solutions analyzed
    ├── gap-analysis.md           # The gap this project fills
    ├── architecture-options.md   # 5 directions evaluated with pros/cons
    └── architecture.md           # Complete architecture design
```

## Crate Guide

### duvm-common

Shared types used across all components. Start here to understand the data model.

- **`page.rs`** — `PageHandle` (backend_id + offset encoding), `Tier` enum, `PageFlags`
- **`protocol.rs`** — `RingRequest` / `RingCompletion` (64-byte cache-line aligned structs matching the kernel module layout)
- **`ring.rs`** — Lock-free SPSC ring buffer (io_uring-style)
- **`stats.rs`** — Atomic counters and serializable snapshots

### duvm-backend-trait

The trait that all backends implement:

```rust
pub trait DuvmBackend: Send + Sync {
    fn name(&self) -> &str;
    fn tier(&self) -> Tier;
    fn init(&mut self, config: &BackendConfig) -> Result<()>;
    fn alloc_page(&self) -> Result<PageHandle>;
    fn free_page(&self, handle: PageHandle) -> Result<()>;
    fn store_page(&self, handle: PageHandle, data: &PageBuffer) -> Result<()>;
    fn load_page(&self, handle: PageHandle, buf: &mut PageBuffer) -> Result<()>;
    fn capacity(&self) -> (u64, u64);  // (total, used)
    fn latency_ns(&self) -> u64;
    fn is_healthy(&self) -> bool;
    fn shutdown(&mut self) -> Result<()>;
}
```

To write a new backend: implement this trait, build as `.so`, drop into the plugin directory.

### duvm-backend-memory

Reference backend implementation. Stores pages in a `HashMap`. Use for testing.

### duvm-backend-compress

LZ4 compression backend. Compresses pages and stores them in local memory. Tracks compression ratio.

### duvm-daemon

The main daemon. Initializes backends, processes page store/load/invalidate requests, and listens for control commands on a Unix socket.

```bash
# Run with defaults
duvm-daemon

# Run with custom config
duvm-daemon --config /path/to/duvm.toml

# Run in userfaultfd-only mode (no kernel module)
duvm-daemon --uffd-mode
```

### duvm-ctl

CLI tool for interacting with a running daemon:

```bash
duvm-ctl status     # Daemon status + tracked pages
duvm-ctl stats      # Runtime statistics
duvm-ctl backends   # List active backends
duvm-ctl ping       # Health check
```

### libduvm

User-space library for applications that want fine-grained control.

**Rust API:**
```rust
use duvm::Pool;

let pool = Pool::standalone()?;

// Store a page
let data = [0u8; 4096];
let handle = pool.store(&data)?;

// Load it back
let loaded = pool.load(handle)?;
assert_eq!(data, loaded);

// Free when done
pool.free(handle)?;
```

**C API** (generated header at `crates/libduvm/include/duvm.h`):
```c
#include <duvm.h>

duvm_init();
uint64_t handle = duvm_store_page(data);
duvm_load_page(handle, buffer);
duvm_free_page(handle);
```

### duvm-kmod (Kernel Module)

Thin kernel relay (~300 LoC across 3 files). Provides `/dev/duvm0` for the daemon to mmap the shared ring buffer.

```bash
# Build
cd duvm-kmod && make

# Load (requires root)
sudo insmod duvm-kmod.ko

# Check
ls /dev/duvm0

# Unload
sudo rmmod duvm-kmod
```

Module parameters:
```
ring_size=4096       # Ring buffer entries (power of 2)
staging_pages=8192   # Staging buffer size in pages
```

## Configuration

The daemon reads `/etc/duvm/duvm.toml` (or uses defaults):

```toml
[daemon]
log_level = "info"
socket_path = "/run/duvm/duvm.sock"
metrics_port = 9100

[policy]
strategy = "lru"
prefetch_depth = 4

[backends.memory]
enabled = true
max_pages = 262144

[backends.compress]
enabled = true
max_pages = 262144
```

## Graceful Degradation

```
Full system (kernel module + daemon + backends)
    ↓ daemon crashes
Kernel module falls back to local swap
    ↓ kernel module not loaded
userfaultfd fallback (user-space only)
    ↓ duvm not installed
Standard Linux memory management
```

Each level degrades performance, never correctness.

## Makefile Targets

```
make help           # Show all targets
make build          # Build (debug)
make release        # Build (optimized)
make test           # Run all 42 tests
make clippy         # Lint with clippy
make fmt            # Format code
make check          # format + lint + test
make kmod           # Build kernel module
make doc            # Generate docs
make install        # Install binaries
make clean          # Clean everything
```

## Design Documents

For the full architecture rationale, see `research/`:

| Document | What's in it |
|---|---|
| `prior-art.md` | Landscape of 20+ existing solutions |
| `gap-analysis.md` | Why no existing solution is sufficient |
| `architecture-options.md` | 5 architectural directions with measured latency data |
| `architecture.md` | Complete design for the chosen hybrid approach |

## License

Apache-2.0
