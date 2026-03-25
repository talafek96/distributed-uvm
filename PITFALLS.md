# Pitfalls

## ~~Daemon polling adds 0-100us latency~~ FIXED

**Symptom:** Page fault latency had a variable 0-100us component from the daemon's polling loop.
**Cause:** `kmod_ring.rs` `run_loop()` spun 1000 times then slept 100us.
**Fix:** Implemented `poll()` file operation on `/dev/duvm_ctl`. Kernel calls `wake_up(&ring->req_wait)` after posting a request. Daemon blocks on `poll()` waiting for POLLIN. Wake-up latency is now ~1-5us.
**Commit:** Current session.

## ~~Kernel ring timeout is 5 seconds~~ FIXED

**Symptom:** If daemon is slow or dead, kernel thread waited up to 5 seconds before fallback.
**Cause:** `ring.c` used `msecs_to_jiffies(5000)`.
**Clarification:** `wait_event_timeout` does NOT freeze the kernel — it sleeps the calling thread while the scheduler continues.
**Fix:** Reduced to 500ms. The daemon now responds in microseconds via poll() wake-up, so 500ms is only hit if the daemon is dead.
**Commit:** Current session.

## QEMU KVM requires -cpu host, not cortex-a72

**Symptom:** `kvm_init_vcpu: kvm_arch_init_vcpu failed` when using `-cpu cortex-a72` with KVM.
**Cause:** KVM on aarch64 doesn't support emulating a different CPU model. Must use `-cpu host`.
**Fix:** Use `-cpu host` when KVM is available, `-cpu cortex-a72` for TCG fallback.
**Commit:** 94a276f

## Kernel module needs register_blkdev before add_disk

**Symptom:** `add_disk` returns -EINVAL.
**Cause:** Setting `disk->major = 0` without calling `register_blkdev()` first. The kernel needs a registered major number.
**Fix:** Call `register_blkdev(0, "duvm_swap")` to get a dynamic major, set `disk->major = duvm_major`.
**Commit:** 29b06ff

## Kernel module needs disk->fops set

**Symptom:** NULL pointer dereference in `__add_disk()`.
**Cause:** `blk_mq_alloc_disk()` does not set the `fops` field. `add_disk()` dereferences it.
**Fix:** Create a minimal `block_device_operations` struct with `.owner = THIS_MODULE` and set `disk->fops`.
**Commit:** 29b06ff

## Memserver ALLOC error must send 9 bytes

**Symptom:** Client hangs when memserver is full.
**Cause:** Memserver sent 1-byte error for ALLOC, but client always reads 9 bytes.
**Fix:** Always send 9-byte response for ALLOC (status + 8 bytes offset, zeroed on error).
**Commit:** 44b5724

## Rustup toolchain can disappear after account recreation

**Symptom:** `rustup could not choose a version of cargo to run`.
**Cause:** User account was recreated, ~/.rustup/toolchains was lost.
**Fix:** `rustup default stable`.

## Mutual OOM deadlock: both machines full, swapping to each other

**Symptom:** Machine A swaps to B, B allocates to store the page, B needs to swap, B swaps to A, A allocates... infinite recursion.
**Cause:** The original memserver did `Box::new()` on every STORE — a heap allocation that could trigger swap on the receiving machine.
**Fix:** Two-part fix:
1. Memserver refuses STORE when at `max_pages` (returns RESP_ERR immediately, no allocation).
2. Kernel module returns `BLK_STS_IOERR` when daemon returns an error, so the kernel tries the next swap device (local SSD).
**Design rule:** Receiving a remote page must never trigger the receiver's swap path. Check capacity before allocating.
**Commit:** 2bbfa1e

## SoftRoCE in QEMU needs provider config

**Symptom:** `ibv_devices` shows no devices even though rxe0 exists in sysfs.
**Cause:** libibverbs needs `/etc/libibverbs.d/rxe.driver` containing `driver rxe` to find the provider library.
**Fix:** Create the config file in the initramfs. Also copy librxe provider .so and all libibverbs deps.
**Modules needed:** udp_tunnel, ip6_udp_tunnel, ib_core, ib_uverbs, rdma_rxe (all .ko.zst, decompress with zstd).
**Verification:** `ibv_devices` shows `rxe0` with a GUID.

## ibv_post_send / ibv_poll_cq are inline functions

**Symptom:** Linker error: `undefined reference to ibv_post_send` / `ibv_poll_cq` when linking against libibverbs.
**Cause:** These are `static inline` functions in `<infiniband/verbs.h>`, not exported symbols in `libibverbs.so`. They call through function pointers in the QP/CQ context structures.
**Fix:** Created a C shim (`crates/duvm-backend-rdma/src/shim.c`) with wrapper functions `duvm_ibv_post_send` and `duvm_ibv_poll_cq`. Used `#[link_name]` attribute in the FFI bindings to map Rust names to shim names. Compiled via `cc` crate in `build.rs`.
**Commit:** 5ca616b

## SoftRoCE provider library must be at compile-time path

**Symptom:** `libibverbs: Warning: couldn't load driver 'librxe-rdmav34.so'` in QEMU VM even with `RDMAV_DRIVER_PATH` set.
**Cause:** Modern rdma-core (libibverbs) uses `dlopen` with a compile-time search path (`/usr/lib/aarch64-linux-gnu/libibverbs/`). The `RDMAV_DRIVER_PATH` env var is ignored in newer versions.
**Fix:** Copy `librxe-rdmav34.so` to both `/lib/libibverbs/` and `/usr/lib/aarch64-linux-gnu/libibverbs/` in the initramfs. The latter is the path libibverbs actually searches.
**Commit:** d7e1a33

## Hand-written FFI structs must match C sizes exactly

**Symptom:** Potential UB from provider reading past Rust struct allocation on stack.
**Cause:** `ibv_send_wr` Rust struct was 84 bytes but C struct is 128 bytes. Field offsets were correct (alignment padding coincidentally filled the `imm_data` gap), but total size was too small.
**Fix:** Use `cc` to compile a size-check program against actual headers. Verified: `ibv_send_wr`=128, `ibv_wc`=48, `ibv_mr`=48, `rdma_cm_id`=416, `ibv_qp`=168. Set `_pad` arrays to match exact C sizes. Fields we access (`wr.rdma.remote_addr` at offset 40, `wr.rdma.rkey` at 48, `mr.lkey` at 36, `cm_id.verbs` at 0, `cm_id.qp` at 24) are all verified correct.
**Rule:** Always run `offsetof()` checks against system headers before trusting hand-written FFI bindings.
**Commit:** a9ddad4

## RDMA CM event constants had wrong values

**Symptom:** `rdma_resolve_route` succeeded (event=2) but code rejected it because `RDMA_CM_EVENT_ROUTE_RESOLVED` was defined as 1.
**Cause:** The C enum has `ADDR_ERROR=1` between `ADDR_RESOLVED=0` and `ROUTE_RESOLVED=2`. Our FFI had `ROUTE_RESOLVED=1`.
**Fix:** Verified all enum values against the header: ADDR_RESOLVED=0, ADDR_ERROR=1, ROUTE_RESOLVED=2, ROUTE_ERROR=3, CONNECT_REQUEST=4, ..., ESTABLISHED=9.
**Rule:** Never assume enum values. Always verify with a C test program.
**Commit:** b064cba

## SoftRoCE (rxe) doesn't work over QEMU socket networking

**Symptom:** `rdma_connect` times out between two QEMU VMs with SoftRoCE.
**Cause:** SoftRoCE uses UDP port 4791 for RoCEv2 encapsulation. QEMU's `-netdev socket` passes Ethernet frames but the UDP tunnel packets from the rxe driver don't traverse correctly.
**Fix:** Use SoftiWARP (siw) instead — it uses TCP for RDMA transport, which works fine over QEMU socket networking. Both rxe and siw need explicit device creation on modern kernels: `rdma link add siw0 type siw netdev eth0`.
**Commit:** b064cba

## iWARP RDMA listener conflicts with TCP on same port

**Symptom:** `rdma_listen failed: -1` when memserver's RDMA listener uses the same port as TCP listener.
**Cause:** SoftiWARP (iWARP) uses TCP as its transport. Both `TcpListener::bind(9200)` and `rdma_bind_addr(9200)` try to use the same TCP port.
**Fix:** Use a separate port for RDMA (default: TCP port + 1). Added `--rdma-port` flag to memserver.
**Commit:** b064cba

## rdma_get_cm_event blocks forever without timeout

**Symptom:** Daemon hangs during RDMA connection if server doesn't respond.
**Cause:** `rdma_get_cm_event` is a blocking call with no timeout. If the RDMA server is down or unreachable, the daemon blocks indefinitely.
**Fix:** Use `poll()` on the event channel fd (`ec->fd`) before calling `rdma_get_cm_event`. Added `wait_cm_event(ec, timeout_ms)` helper. Connect timeout = 10s, resolve timeouts = 5s.
**Commit:** b064cba

## TCP backend alloc_page TOCTOU was missed in bulk fix

**Symptom:** RDMA backend and memserver `alloc_page()` TOCTOU races were fixed with `compare_exchange` CAS loops, but TCP backend was overlooked — still used `fetch_add(1)` with no capacity check.
**Cause:** The bug report said "Same pattern in TCP backend and memserver" but the fix only covered RDMA and memserver. TCP backend's `alloc_page()` is different (it talks to a remote server first), so it wasn't caught by a simple grep.
**Fix:** Added CAS loop to TCP backend's `alloc_page()`: reserve a slot atomically before the remote call, release it on failure. Added `tcp_capacity_limit_enforced` and `tcp_capacity_recovers_after_free` tests.
**Lesson:** When a bug report lists N locations, verify all N are fixed. Add a test for each.
**Commit:** c6c424a

## duvm-ctl ran system commands via relative PATH as root

**Symptom:** `duvm-ctl enable` called `modprobe`, `systemctl` etc. by name without full paths — vulnerable to PATH injection when running as root via sudo.
**Cause:** Used `Command::new("modprobe")` instead of `Command::new("/sbin/modprobe")`.
**Fix:** Added constants for all system commands with absolute paths (`/sbin/modprobe`, `/usr/bin/systemctl`, etc.).
**Lesson:** Any tool that calls `check_root()` / runs as root must use absolute paths for all subprocess calls.
**Commit:** c6c424a
