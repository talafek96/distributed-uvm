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
