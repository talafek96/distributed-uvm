# Plan: Distributed UVM for Everyone + OOM Safety

## Context

Three questions were asked:
1. Can we provide distributed UVM to machines WITHOUT special hardware (no ATS/C2C)?
2. Can we simulate and test multi-machine scenarios with QEMU?
3. What happens when both machines are out of memory? Can we control the fallback?

---

## Question 1: Distributed UVM Without Special Hardware

### What special hardware gives us (DGX Spark)

On DGX Spark, the GPU uses ATS over NVLink-C2C to share the CPU's page tables. This means GPU + CPU + swap all go through the same page fault path. duvm's swap device works for both CPU and GPU automatically.

### What happens on normal machines

| Machine Type | CPU-GPU UVM | Distributed RAM via duvm | What Works |
|---|---|---|---|
| DGX Spark / Grace Hopper (ATS + C2C) | Hardware — fully transparent | Yes (swap device) | Everything |
| PCIe GPU + Linux 6.1+ (HMM) | Software — transparent for managed memory | Yes (swap device) | CPU swap is transparent; GPU uses HMM page migration |
| PCIe GPU, older kernel (no HMM) | cudaMallocManaged only | Yes (swap device) | CPU swap transparent; GPU managed memory needs CUDA hints |
| No GPU at all | N/A | Yes (swap device) | Full distributed RAM, just no GPU angle |
| x86 machine (Intel/AMD) | Via HMM if kernel supports it | Yes (swap device) | Same as above |

**Key insight: the distributed RAM part (swap device + daemon + TCP backend) works on ANY Linux machine with a 6.x kernel.** It doesn't need NVIDIA, doesn't need ARM, doesn't need ATS. The kernel module uses standard `blk-mq` APIs that work everywhere.

The GPU-specific part varies:
- **ATS machines** (DGX Spark, Grace Hopper): fully transparent, nothing to do
- **HMM machines** (PCIe GPU + kernel 6.1+): mostly transparent, kernel handles GPU page migration via HMM
- **No HMM**: GPU uses `cudaMallocManaged`; the CPU pages that back managed memory go through swap normally, and CUDA's UVM driver handles the GPU side
- **No GPU**: just distributed RAM, still very useful

### What we need to build

Nothing new for the core. The swap device already works on any Linux. What we should add:

1. **x86 build and test support** — currently only tested on aarch64. The kernel module C code and Rust code should work on x86 but hasn't been verified.
2. **HMM integration test** — on a PCIe GPU machine, verify that pages backed by `cudaMallocManaged` correctly swap through duvm and come back to the GPU.
3. **Documentation** — explain which tier of UVM support each hardware config gets.

---

## Question 2: Multi-Machine QEMU Simulation

Yes, QEMU supports networking between VMs. We can simulate the full two-machine distributed setup:

### Architecture

```
Host machine
├── QEMU VM "node-a" (256 MB RAM)
│   ├── duvm-kmod loaded
│   ├── duvm-daemon running (connects to node-b:9200)
│   ├── duvm-memserver on port 9200
│   └── Virtual NIC → socket network
│
├── QEMU VM "node-b" (256 MB RAM)
│   ├── duvm-kmod loaded
│   ├── duvm-daemon running (connects to node-a:9200)
│   ├── duvm-memserver on port 9200
│   └── Virtual NIC → socket network
│
└── Socket pair connecting the two VMs
```

### How QEMU networking works for this

```bash
# VM A: -netdev socket,id=net0,listen=:9100
# VM B: -netdev socket,id=net0,connect=127.0.0.1:9100
```

Both VMs get virtual NICs. They can ping each other. TCP works. duvm-memserver listens, duvm-daemon connects — exact same as real machines.

### What the test would prove

1. Node A fills its 256 MB RAM
2. Cold pages swap to `/dev/duvm_swap0`
3. duvm-daemon sends them to node B's memserver over the virtual network
4. Node A's apps keep running with the pages on node B
5. When accessed again, pages come back to node A
6. Also test the reverse: node B borrows from node A

### Prerequisites

This requires the kernel module ↔ daemon ring buffer connection to be working. Without it, the kernel module stores pages locally and never sends them over the network. That connection is the current blocker.

### Implementation plan

1. Wire kernel module ring buffer → daemon (the existing TODO)
2. Build a `scripts/test-distributed-qemu.sh` that:
   - Creates two initramfs images (each with kmod + daemon + memserver)
   - Boots two QEMU VMs connected via socket networking
   - Configures swap on both
   - Runs a memory pressure test on VM A
   - Verifies pages ended up on VM B
   - Runs the reverse
3. This works on any Linux host (x86 or ARM) — no special hardware needed

---

## Question 3: OOM Behavior and Swap Control

### How Linux handles swap exhaustion

Linux supports multiple swap devices with priorities. The kernel uses them in priority order:

```
Priority 100: /dev/duvm_swap0  →  remote machine RAM (fast)
Priority  10: /swapfile        →  local SSD (slower fallback)
Priority  -1: (none)           →  OOM killer
```

When duvm_swap0 is full (remote machine has no more RAM), Linux automatically falls back to the next swap device. When ALL swap is full, the OOM killer activates and kills the largest process.

### What happens in our two-machine scenario

```
1. Machine A uses 128 GB local RAM          ← everything fine
2. Overflow pages go to machine B via duvm   ← transparent
3. Machine B's memserver fills up            ← duvm store_page() fails
4. duvm kernel module returns I/O error      ← kernel falls to next swap device
5. Next swap device (local SSD) is used      ← slower but works
6. Local SSD fills up too                    ← OOM killer activates
```

### What we should implement

**Configurable swap cascade with explicit controls:**

```toml
# /etc/duvm/duvm.toml

[swap]
# When remote backends are full, what to do?
# "fallback" = let kernel fall through to lower-priority swap (default)
# "reject"   = fail the I/O, forcing OOM sooner (useful if you'd rather
#              kill a process than use slow disk swap)
overflow_policy = "fallback"

# Optional: maximum pages to store remotely (cap remote usage)
max_remote_pages = 0  # 0 = unlimited

# Optional: warn when remote utilization exceeds this percentage
warn_threshold_percent = 80
```

**Setup script changes:**

```bash
# install.sh should configure the cascade:
sudo swapon -p 100 /dev/duvm_swap0       # remote RAM (highest priority)
sudo swapon -p 10  /swapfile             # local SSD (fallback)

# Users can also disable local swap entirely if they prefer OOM over slow:
sudo swapoff /swapfile                    # no local fallback — OOM when remote full
```

**Runtime control:**

```bash
# Check current state
duvm-ctl status     # shows: remote capacity, local swap status, overflow policy

# Dynamically change overflow behavior
duvm-ctl set overflow_policy reject    # prefer OOM over slow swap
duvm-ctl set overflow_policy fallback  # prefer slow swap over OOM
```

### Current state

This swap cascade **already works** with standard Linux tools. No code changes needed for the basic behavior. What we'd add is:
1. Monitoring/alerting when remote swap is nearly full
2. The `overflow_policy` config option in duvm-daemon
3. Clear documentation of the cascade behavior
4. Stats tracking: `ring_full_events` counter already exists, just needs to be wired up

---

## Implementation Roadmap

### Phase 1: Wire the last piece (kernel → daemon)
**This is the blocker for everything else.**
- Connect the kernel module's ring buffer to the daemon
- When kernel swaps a page out: kmod → ring buffer → daemon → TCP → remote
- When kernel swaps a page in: kmod ← ring buffer ← daemon ← TCP ← remote
- Test with real `swapon` on the host

### Phase 2: Multi-machine QEMU test
- Two QEMU VMs with socket networking
- Both run kmod + daemon + memserver
- Automated test: fill RAM on VM A, verify pages on VM B, read them back
- This proves the product works without any special hardware

### Phase 3: OOM safety
- Add swap cascade documentation
- Add `overflow_policy` to config
- Add remote utilization monitoring to `duvm-ctl status`
- Wire up `ring_full_events` stat counter

### Phase 4: x86 support
- Cross-compile kernel module for x86_64
- Test in x86 QEMU VM
- CI pipeline for both architectures

### Phase 5: HMM integration test
- On a PCIe GPU machine (not Grace/DGX Spark)
- Verify cudaMallocManaged pages swap through duvm correctly
- Document which GPU features work on which hardware tier
