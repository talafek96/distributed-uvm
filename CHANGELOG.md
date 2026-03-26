# Changelog

## 2026-03-26 — TCP backend reconnection and circuit breaker

- **Auto-reconnect:** TCP backend now clears broken streams on I/O error and automatically reconnects on the next operation. Memserver restarts no longer brick the backend.
- **Circuit breaker:** After 5 consecutive failures, the backend backs off for 5 seconds before retrying, preventing reconnect storms against a down server.
- **Accurate health:** `is_healthy()` now returns false when disconnected (was always true if stream was ever set).
- **Connect timeout:** `init()` and reconnect use `connect_timeout(3s)` instead of blocking indefinitely.
- Tests: `tcp_server_crash_marks_unhealthy`, `tcp_reconnect_after_server_restart`, `tcp_connect_to_dead_server_is_unhealthy`, `tcp_store_failure_clears_stream_and_reconnects`.
- Total: 196 unit tests + 81 QEMU checks across 7 test scripts.

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
