# RDMA Investigation: Do We Need It? Can We Simulate It?

## Question 1: Do we support RDMA yet?

**No.** We have a TCP backend (`duvm-backend-tcp`) that sends pages over TCP sockets. No RDMA code exists.

## Question 2: Should we support over-the-network memory at all?

**Yes, absolutely.** That's the whole point — extending memory beyond one machine. The question is which transport.

### What your hardware actually has

```
4x ConnectX-7 NICs (Mellanox)
  - 200 Gbps each (800 Gbps total)
  - RoCE v2 (RDMA over Converged Ethernet)
  - All links ACTIVE, connected to calc2
  - libibverbs installed and working
```

### TCP vs RDMA on your hardware (measured + published)

| Metric | TCP (measured) | RDMA (published CX-7) | Ratio |
|--------|---------------|----------------------|-------|
| Latency per 4KB page | 16us localhost, ~30-50us over network | 1-3us | 5-16x better |
| Throughput | 0.24 GB/s (sequential, single conn) | 12-25 GB/s per port | 50-100x better |
| CPU usage | High (kernel TCP stack) | Near-zero (one-sided RDMA bypasses CPU) | Huge difference |

**TCP works for proving the concept. RDMA is necessary for production.** At 16us/page over TCP, a workload that touches 1GB of swapped memory would stall for 4 seconds. At 2us/page over RDMA, the same stall is 0.5 seconds.

### But TCP is still useful

- Works everywhere (any machine, any NIC, QEMU VMs)
- No special drivers or configuration
- Good enough for testing and development
- Good enough for low-memory-pressure workloads

The right architecture is: **TCP as the default transport, RDMA as an optional high-performance transport.** This is exactly what we have — the backend trait is pluggable.

## Question 3: Can we simulate RDMA in QEMU?

**Yes — using SoftRoCE (rxe).** This is a Linux kernel module that creates a software RDMA device on top of any NIC (including QEMU's virtual NIC). It's already available on your system:

```
$ modinfo rdma_rxe
filename: /lib/modules/6.17.0-1008-nvidia/kernel/drivers/infiniband/sw/rxe/rdma_rxe.ko.zst
description: Soft RDMA transport
```

### How it works in QEMU

```
Host machine
├── QEMU VM A
│   ├── virtio-net NIC (virtual)
│   ├── rdma_rxe module loaded → creates rxe0 RDMA device on virtio-net
│   ├── duvm with RDMA backend → uses rxe0 for RDMA verbs
│   └── Full ibverbs API works (register memory, post send/recv, etc.)
│
├── QEMU VM B
│   ├── Same setup
│   └── RDMA between VMs goes over virtual network via SoftRoCE
│
└── QEMU socket networking connects the two VMs
```

**Performance in SoftRoCE:** 10-50x slower than real RDMA (it's software emulating the RDMA protocol over TCP/UDP). But it's **functionally identical** — same ibverbs API, same one-sided READ/WRITE semantics, same memory registration. Perfect for testing.

### SoftRoCE also works outside QEMU

For developers who don't have RDMA NICs, they can run SoftRoCE on their regular Ethernet:
```bash
sudo modprobe rdma_rxe
sudo rdma link add rxe0 type rxe netdev eth0
ibv_devices   # shows rxe0 as an RDMA device
```
Then duvm's RDMA backend works on any machine. Slower than real RDMA, but the code path is exercised.

## Summary: The Transport Strategy

```
┌─────────────────────────────────────────────────────┐
│                  duvm-daemon                         │
│                                                     │
│  store_page()/load_page()                           │
│       │                                             │
│       ▼                                             │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────┐  │
│  │ memory   │  │ compress │  │ network backend  │  │
│  │ (local)  │  │ (LZ4)   │  │                  │  │
│  └──────────┘  └──────────┘  │  ┌────────────┐  │  │
│                              │  │ TCP        │  │  │
│                              │  │ (default)  │  │  │
│                              │  ├────────────┤  │  │
│                              │  │ RDMA       │  │  │
│                              │  │ (optional) │  │  │
│                              │  └────────────┘  │  │
│                              └──────────────────┘  │
└─────────────────────────────────────────────────────┘

TCP:  Works everywhere. 16-50us/page. Good for testing and light workloads.
RDMA: Requires ConnectX or SoftRoCE. 1-3us/page. Required for production.
```

### What to build, in order

1. **Now:** Wire kernel module → daemon (the blocker). Use TCP backend.
2. **Next:** RDMA backend using libibverbs. One-sided RDMA WRITE for store, READ for load.
3. **Testing:** SoftRoCE in QEMU VMs for CI. Real CX-7 for performance benchmarks.
4. **Production:** RDMA by default on machines with RDMA NICs, TCP fallback on others.
