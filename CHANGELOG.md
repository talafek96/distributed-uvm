# Changelog

## 2026-03-30 — FULL-STACK SWAP PROVEN on real hardware

**4GB (14.6M pages) swapped through kmod→daemon→RDMA→memserver on ConnectX-7 RoCEv2, zero errors, system stable.** Used `MADV_PAGEOUT` to force pages out through duvm_swap0. Pages verified byte-for-byte on readback.

Key findings:
- `vm.swappiness=100` alone doesn't trigger swap — kernel needs pages marked cold
- `MADV_PAGEOUT` (Linux 5.4+) explicitly tells kernel to page out to swap device
- DGX Spark UMA freezes when MemFree < ~1GB — must check MemFree (not MemAvailable)
- Memory watchdog/earlyoom must be stopped during swap testing (they kill the daemon)

## 2026-03-30 — Async kmod I/O, hardware validation, DGX Spark UMA work

### RDMA Hardware Validation
- **Fixed IBV_SEND_SIGNALED** (was `1<<2`=4, correct `1<<1`=2) and **IBV_WR_RDMA_READ** (was 3, correct 4). SoftiWARP was lenient; real ConnectX-7 hardware is strict — WRITE completions never arrived.
- **RDMA hardware test: PASS.** 10,000 pages via one-sided RDMA WRITE/READ on ConnectX-7 RoCEv2 (200Gbps), zero errors, 15μs/page, 273 MB/s. Compared to TCP: 6.8x lower latency, 11.2x higher throughput.
- Added `demo_rdma.rs` test binary for hardware validation.

### Async Kmod I/O (prevents system freeze)
- **Converted queue_rq from synchronous to fully async.** queue_rq submits to ring buffer and returns immediately. Completion harvester kthread polls completion ring and calls blk_mq_end_request. Modeled after Linux nbd driver.
- **Added blk-mq 5-second timeout.** If daemon dies (OOM kill, crash), orphaned requests are failed with BLK_EH_DONE. Kernel falls back to next swap device.
- **Staging slot bitmap allocator.** Replaced broken `idx % staging_pages` with proper bitmap. Prevents staging page collisions.
- **Daemon OOM protection.** Sets oom_score_adj=-999 on startup.
- **Daemon signals kernel on completion.** write() to /dev/duvm_ctl wakes completion thread.

### Full-Stack Swap Test
- 782MB successfully swapped to duvm_swap0 through kmod→daemon→RDMA→memserver on real hardware.
- System froze at MemFree=665MB (MemAvailable was 18GB) — a known DGX Spark UMA platform issue, not a duvm bug.
- Wrote `swap_pressure_test.c`: checks MemFree (not MemAvailable), drops page cache, 256MB chunks, 4GB safety threshold.
- Added `setup-hardware-test.sh`: one-command setup (stops watchdog, loads kmod, builds test).

### DGX Spark Lessons
- Secure Boot rejects unsigned kernel modules. Disabled on calc2 via `mokutil --disable-validation`.
- MemAvailable is misleading on UMA — includes cache that GPU driver can't wait for. Use MemFree.
- Memory watchdog/earlyoom kill daemon under swap pressure. Must stop them before testing.
- RDMA device names are `rocep1s0f0` (not `mlx5_0`).

## 2026-03-26 — Engine retry, distributed TCP test, memserver hardening

- **Engine store retry/fallback:** `store_page` now tries all healthy backends in the tier (least-loaded first) before returning an error. If the first backend fails (e.g., network error), the next one is attempted. Replaced `tier_to_backend_id` (single pick) with `tier_backend_candidates` (ranked list).
- **Distributed test TCP path:** `test-distributed-qemu.sh` now configures a `[backends.remote]` TCP backend pointing to VM-B's memserver. Pages actually flow kmod → daemon → TCP → memserver. Verifies daemon connected to remote backend + data integrity. (15 checks, up from 12.)
- **Memserver connection limits:** `--max-connections` (default 256) rejects new TCP connections when limit is reached. `--idle-timeout` (default 300s) closes connections with no activity. Prevents unbounded thread spawning.

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
