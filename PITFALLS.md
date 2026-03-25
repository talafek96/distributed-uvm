# Pitfalls

## Daemon polling adds 0-100us latency

**Symptom:** Page fault latency has a variable 0-100us component from the daemon's polling loop.
**Cause:** `kmod_ring.rs` `run_loop()` spins 1000 times then sleeps 100us. If a request arrives right after the daemon goes to sleep, it waits up to 100us.
**Fix needed:** Replace polling with eventfd notification. Kernel writes to eventfd after posting ring request. Daemon blocks on epoll/io_uring waiting for eventfd. Expected wake-up latency: 1-5us.
**Tracking:** Performance-critical. Must fix before production use.

## Kernel ring timeout is 5 seconds

**Symptom:** If daemon is slow or dead, kernel thread waits up to 5 seconds before falling back to xarray.
**Cause:** `ring.c` `duvm_ring_submit_and_wait()` uses `wait_event_timeout(ring->comp_wait, ..., msecs_to_jiffies(5000))`.
**Clarification:** `wait_event_timeout` does NOT freeze the kernel. It puts the calling thread to sleep on a wait queue. Other processes, interrupts, and the scheduler continue running normally. But 5 seconds is too generous — should be 500ms or less.
**Fix needed:** Reduce timeout. Consider adaptive timeout based on measured daemon response time.

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
