# Plan: CI/CD for QEMU Tests, RDMA Backend, 3-Machine Testing

## 1. Add QEMU Tests to CI/CD

### What to add
The existing CI only runs `test-kmod-qemu.sh` (standalone kernel module). Add:
- `test-kmod-daemon-qemu.sh` — kmod + daemon ring buffer (10 checks, ~10s)
- `test-distributed-qemu.sh` — two VMs distributed path (12 checks, ~21s)
- `test-mutual-oom-qemu.sh` — mutual OOM degradation (9 checks, ~2min)

### CI considerations
- All QEMU tests require `ubuntu-24.04-arm` runner (aarch64 kernel + headers)
- All need: qemu-system-arm, busybox-static, linux-headers, build-essential, cpio, gzip
- The daemon tests also need Rust toolchain (for building daemon/memserver binaries)
- Total QEMU CI time: ~3 minutes (all four tests sequential)

### Implementation
Add a new CI job `e2e-distributed` that runs all four QEMU tests.

---

## 2. RDMA Backend

### Architecture

The RDMA backend replaces TCP for production performance (2us vs 200us per page).

**Key difference from TCP:** No memserver process needed on the remote side for the data path. RDMA one-sided WRITE/READ goes directly to pre-registered memory on the remote NIC. The remote CPU is not involved in page transfers.

However, we still need a **setup daemon** on each remote machine that:
1. Registers a memory region with the RDMA NIC
2. Shares the memory region key (rkey) and base address with connecting clients
3. Manages the registered memory lifecycle

This setup daemon can be the existing memserver, extended with RDMA support.

### Design

```
duvm-backend-rdma (new crate)
├── Uses `rdma` crate (Rust bindings for libibverbs)
├── Implements DuvmBackend trait
├── On init: connects to remote, gets rkey + remote_addr
├── store_page: one-sided RDMA WRITE to remote registered memory
├── load_page: one-sided RDMA READ from remote registered memory
├── No remote CPU involvement for data path
└── Falls back gracefully if RDMA not available
```

### Configuration

```toml
# /etc/duvm/duvm.toml

[backends.remote]
# Transport: "auto" detects RDMA, falls back to TCP
# "rdma" forces RDMA (fails if not available)
# "tcp" forces TCP
transport = "auto"

# Peer machines (all must run duvm-memserver)
peers = [
    "192.168.200.11:9200",   # calc2
    "192.168.200.12:9200",   # calc3 (future)
]

# Maximum pages to store across all remotes combined
max_pages = 1000000

# RDMA-specific (only used if transport is "rdma" or "auto" selects RDMA)
rdma_device = ""             # empty = auto-detect first active device
rdma_port = 1
rdma_gid_index = 0
```

### Testing strategy
- Unit tests: mock RDMA device (or skip if not available)
- Integration: SoftRoCE (rdma_rxe) in QEMU for CI
- Performance: real ConnectX-7 between calc1 and calc2

### Implementation order
1. Add `rdma` crate dependency
2. Implement `RdmaBackend` struct with `DuvmBackend` trait
3. Auto-detection: check `ibv_get_device_list()` at startup
4. Connection setup: exchange rkey/addr via TCP handshake, then use RDMA for data
5. Test with SoftRoCE locally, then real hardware

---

## 3. Three-Machine QEMU Test

### What to test

Three VMs, each with only 1 page of remote capacity, acting as both client and server. Tests:

1. **Fair distribution:** VM-A stores 3 pages — should go 1 to B, 1 to C, 1 fails (both full). Not all 3 to B.

2. **Round-robin exhaustion:** All three machines fill up. Each should use the other two before falling back to local.

3. **Asymmetric load:** VM-A is memory-hungry (stores many pages), VM-B and VM-C have spare capacity. Pages should spread across B and C, not pile up on one.

4. **Chain failure:** VM-C crashes. VM-A and VM-B should continue working with each other. Pages that were on VM-C are lost but the system doesn't hang.

5. **Simultaneous pressure:** All three VMs under memory pressure at the same time. Should degrade gracefully — use remotes when available, fall to local when not.

### Architecture

```
VM-A (10.0.0.1)           VM-B (10.0.0.2)           VM-C (10.0.0.3)
├── kmod + daemon          ├── kmod + daemon          ├── kmod + daemon
├── memserver (1 page)     ├── memserver (1 page)     ├── memserver (1 page)
├── backend → B:9200       ├── backend → A:9200       ├── backend → A:9200
├── backend → C:9200       ├── backend → C:9200       ├── backend → B:9200
```

### QEMU networking for 3 VMs

QEMU socket networking is point-to-point. For 3 VMs we need a virtual switch. Options:
- **Hub mode:** QEMU `-netdev hubport` creates a shared hub
- **Multicast:** QEMU `-netdev socket,mcast=` for multicast group
- **Bridge on host:** Create a tap bridge (needs root)

Simplest: use QEMU multicast socket backend — all VMs join the same multicast group.

```bash
# All three VMs use the same mcast address:
-netdev socket,id=net0,mcast=230.0.0.1:19400
-device virtio-net-device,netdev=net0
```

### Daemon: balanced distribution

Currently the daemon's engine stores pages in one backend. For fair distribution across N remotes, the engine needs to **round-robin or least-loaded** selection among remote backends.

This requires the engine to be aware of multiple remote backends and distribute pages across them. Current design has one backend per tier — we need to support multiple backends in the same tier (e.g., two TCP backends both at Rdma tier).

### Implementation plan
1. Extend engine to support multiple backends per tier
2. Add round-robin or least-used selection within a tier
3. Build 3-VM QEMU test with multicast networking
4. Test fair distribution, exhaustion, crash recovery

### Naming note
The daemon is still correctly called a "daemon" — it's a long-running background service. The fact that it uses poll() instead of sleeping doesn't change that. "Daemon" means "background service process," not "polling loop."

---

## Execution Order

### Phase 1: CI/CD (small, do first)
- Add `e2e-distributed` job to `.github/workflows/ci.yml`
- Runs all four QEMU tests
- Estimated: 30 minutes to implement + test

### Phase 2: Multi-backend engine (prerequisite for RDMA + 3-machine)
- Engine supports multiple backends in same tier
- Round-robin or least-loaded selection
- Config: `peers = [...]` creates one TCP backend per peer
- Estimated: 2-3 hours

### Phase 3: 3-machine QEMU test
- Three VMs with multicast networking
- Tests fair distribution, exhaustion, crash recovery
- Estimated: 2-3 hours

### Phase 4: RDMA backend
- New crate `duvm-backend-rdma`
- Uses `rdma` crate for libibverbs
- Auto-detect RDMA at daemon startup
- Test with SoftRoCE, then real ConnectX-7
- Estimated: 1-2 days
