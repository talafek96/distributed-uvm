# Architecture Decisions Log

## Decision 1: Symmetric Node Architecture

**Date:** 2026-03-20
**Status:** Adopted

**Context:** Initial implementation treated one node as "compute" and the other as "memory server only." User correctly identified this is wrong.

**Decision:** Every node in the cluster is equal — it runs applications, provides memory to the cluster, and uses remote memory from other nodes.

**Architecture:**
```
calc1                                calc2
┌──────────────────────────┐    ┌──────────────────────────┐
│ Applications running     │    │ Applications running     │
│ duvm-daemon (policy,     │    │ duvm-daemon (policy,     │
│   backends, ring buffer) │    │   backends, ring buffer) │
│ duvm-memserver (serves   │    │ duvm-memserver (serves   │
│   local memory to peers) │    │   local memory to peers) │
│                          │    │                          │
│ 128 GB local RAM         │    │ 128 GB local RAM         │
│  ↕ overflow to calc2 ────┼────┼── overflow to calc1 ↕    │
└──────────────────────────┘    └──────────────────────────┘
```

Each node:
- Runs the kernel module (swap interception)
- Runs duvm-daemon (policy engine, backend management)
- Runs duvm-memserver (serves its unused memory to the cluster)
- Connects to other nodes' memservers as TCP/RDMA backends

**Implication:** When calc1's memory is full, cold pages swap to calc2. When calc2's memory is full, cold pages swap to calc1. Both nodes contribute and consume.

---

## Decision 2: Block Device Instead of Frontswap

**Date:** 2026-03-20
**Status:** Adopted

**Context:** The original architecture design assumed `frontswap_ops` for transparent swap interception. Research on the actual kernel (Linux 6.17.0-1008-nvidia) revealed:

- **frontswap has been completely removed** from Linux 6.17 (no `frontswap.h` exists)
- **zswap is hardcoded** — called directly from core MM, not through a pluggable interface
- No `frontswap_ops` struct, no registration function, no hook points

**Alternatives considered:**

| Approach | Pros | Cons |
|---|---|---|
| **kprobes/ftrace on zswap** | Non-invasive | Fragile, breaks across versions, overhead |
| **Custom filesystem with address_space_operations** | Uses stable API (swap_rw) | Complex filesystem implementation |
| **Virtual block device** | Proven (Infiniswap), stable API, standard `swapon` | Slightly more overhead than frontswap |
| **eBPF** | Safe, flexible | mm hooks not in mainline yet |

**Decision:** Implement a **virtual block device** that acts as a swap target.

**How it works:**
```
1. Kernel module creates /dev/duvm_swap0 (virtual block device)
2. Admin runs: mkswap /dev/duvm_swap0 && swapon -p 100 /dev/duvm_swap0
3. When kernel swaps out a page:
   kernel → block I/O → duvm block device → ring buffer → daemon → remote node
4. When kernel swaps in a page:
   kernel ← block I/O ← duvm block device ← ring buffer ← daemon ← remote node
5. Applications see nothing. Standard Linux swap, remote storage.
```

**Why this is the right choice:**
- **Proven approach:** Infiniswap (NSDI '17) used exactly this pattern — virtual block device as swap target, RDMA to remote memory. Published and peer-reviewed.
- **Stable kernel API:** Block device drivers use `blk_mq_ops` which is a mature, well-documented API. Much more stable across kernel versions than internal MM hooks.
- **Works WITH zswap:** If zswap is enabled, it compresses pages first, then writes to our device. We get compression for free.
- **Standard tooling:** `mkswap`, `swapon`, `swapoff`, `/proc/swaps` all work normally.
- **Easy to prioritize:** `swapon -p 100` gives our device higher priority than any disk-based swap.

**What this replaces in the kernel module:**
- OLD: frontswap backend hooks (store/load/invalidate)
- NEW: blk_mq_ops (queue_rq for writes, complete for reads) + ring buffer to daemon

---

## Decision 3: QEMU/KVM for Kernel Module Development

**Date:** 2026-03-20
**Status:** Adopted

**Context:** Kernel module bugs can crash the machine. The DGX Spark has unified memory where OOM causes full system freezes. Kernel panics would require physical power cycling.

**Decision:** Develop and test the kernel module in a QEMU/KVM virtual machine on calc1. Use calc2 for integration testing after QEMU validation.

**Workflow:**
```
1. Write kernel module code on calc1
2. Boot QEMU VM with matching kernel
3. insmod duvm-kmod.ko in VM
4. If panic → VM restarts (seconds), calc1 is fine
5. Once stable → scp to calc2 for real hardware test
6. Once proven on calc2 → deploy to calc1
```

**Requirements:**
- qemu-system-aarch64 (needs sudo apt install)
- /dev/kvm exists (verified: yes)
- Kernel image: /boot/vmlinuz-6.17.0-1008-nvidia (verified: exists)

---

## Decision 4: No userfaultfd Requirement

**Date:** 2026-03-20
**Status:** Adopted

**Context:** User requirement is full transparency for unmodified applications. userfaultfd only intercepts explicitly registered memory regions and requires either root or a sysctl change.

**Decision:** The kernel module (block device swap target) is the primary mechanism. userfaultfd is kept as code in the repo but is NOT required for the standard deployment.

**Implication:**
- No `vm.unprivileged_userfaultfd=1` sysctl needed
- No LD_PRELOAD, no libduvm linking, no code changes
- Applications just run. When memory is full, pages swap to remote nodes.
- userfaultfd remains available for special cases (e.g., specific memory regions managed by libduvm)

---

## Decision 5: Installation Must Handle All Prerequisites

**Date:** 2026-03-20
**Status:** Adopted

**Context:** User correctly identified that any system requirements (sysctl, kernel module loading, daemon startup) must be part of the installation process, not manual steps.

**Decision:** Provide:
1. `install.sh` — installs binaries, loads kernel module, starts daemon, configures swap
2. `duvm-daemon.service` — systemd unit for the daemon
3. `duvm-kmod.conf` — modprobe configuration
4. `duvm-setup.service` — one-shot systemd unit that creates and activates the swap device
5. Pre-flight check script that validates everything is correct before use
