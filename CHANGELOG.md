# Changelog

## 2026-03-25 — Phase 1+2 bug fixes and service management

### Phase 1: Bug fixes
- **RDMA server CQ leak:** Per-connection CQs tracked in HashMap, destroyed on disconnect and shutdown.
- **`alloc_page()` TOCTOU race:** Replaced load-then-fetch_add with `compare_exchange` CAS loop in RDMA backend, TCP backend, and memserver.
- **`rdma_cm_event` struct padding:** `_pad: [u8; 36]` now matches `sizeof(rdma_cm_event) = 80`.
- **Memserver single-threaded:** `thread::spawn` per TCP client for concurrent connections.

### Phase 2: Service management
- **`duvm-ctl enable/disable/drain`:** Full lifecycle commands that load kmod, start services, activate swap, and reverse on disable.
- **Systemd units:** `duvm-daemon.service`, `duvm-memserver.service`, `duvm-kmod.service`.
- **`install.sh` updated:** Installs all three systemd units.

### Hardening
- **duvm-ctl security:** All system commands use absolute paths (`/sbin/modprobe`, `/usr/bin/systemctl`, etc.) to prevent PATH injection when running as root.
- **duvm-ctl error reporting:** Service start/stop failures are now logged with the actual error instead of silently discarded.

### Tests added
- `tcp_capacity_limit_enforced` — proves CAS loop enforces max_pages in TCP backend.
- `tcp_capacity_recovers_after_free` — proves capacity restores after free_page.
- `tcp_concurrent_clients` — proves 4 parallel TCP clients with data isolation.
- `test-memserver-concurrent-qemu.sh` — QEMU e2e: 3 concurrent clients, capacity enforcement (6 checks).
- Total: 192 unit tests + 81 QEMU checks across 7 test scripts.
