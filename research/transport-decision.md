# Transport Decision: TCP + RDMA Strategy

## The Measured Reality

Measured on your actual hardware (calc1 → calc2 over ConnectX-7 200Gbps):

| Transport | Latency per 4KB page | Time to swap 1 GB | Usable? |
|-----------|---------------------|-------------------|---------|
| RDMA one-sided | ~2us | 0.5s | Yes — production workloads |
| TCP pipelined | ~22us | ~6s | Marginal — small overflows only |
| TCP sequential | ~220us | 58s | No — unusable for real workloads |

**ICMP RTT to calc2: 210us.** This is the floor. TCP can't go below this for a round trip. RDMA bypasses the network stack entirely (one-sided WRITE goes directly to remote NIC → remote RAM, no CPU involvement on remote side).

## Decision: Support Both, Default to RDMA Where Available

```toml
# /etc/duvm/duvm.toml

[backends.remote]
# Transport selection:
#   "rdma"     - RDMA verbs (requires RDMA NIC or SoftRoCE)
#   "tcp"      - TCP sockets (works everywhere, high latency)
#   "auto"     - Use RDMA if available, fall back to TCP
transport = "auto"    # default

# Remote node address
address = "192.168.200.11:9200"
```

### Why both?

**RDMA is the production transport.** On your DGX Sparks with ConnectX-7, TCP adds 100x overhead for no benefit. RDMA should be the default.

**TCP is the development/testing transport.** It works in QEMU VMs, on laptops, on cloud instances without RDMA hardware, and for anyone who wants to try duvm without special NICs. It also works as a fallback if RDMA fails.

### Default behavior

```
1. Check for RDMA devices (ibv_get_device_list)
2. If found → use RDMA → one-sided RDMA WRITE for store, READ for load
3. If not found → fall back to TCP → warn that performance will be limited
4. User can force either transport via config
```

### When TCP is actually fine

- **Same-machine testing** (localhost): 16us/page — acceptable
- **Development/CI** (QEMU with SoftRoCE or just TCP): functional testing
- **Very small overflows** (< 10 MB swapped): barely noticeable
- **Non-latency-sensitive workloads**: batch processing, cold data

### When TCP is not fine

- **Any real workload** that swaps > 100 MB over a network
- **Interactive workloads** where 220us page faults are noticeable
- **GPU workloads** where the GPU stalls waiting for pages

## What Needs to Be Built

### RDMA Backend Architecture

```rust
pub struct RdmaBackend {
    // RDMA connection
    ctx: *mut ibv_context,
    pd: *mut ibv_pd,
    mr: *mut ibv_mr,           // registered memory region for page buffer
    qp: *mut ibv_qp,           // queue pair for RDMA operations
    
    // Remote memory mapping
    remote_addr: u64,           // remote buffer base address
    remote_rkey: u32,           // remote memory region key
    
    // Page tracking
    pages_used: AtomicU64,
    max_pages: u64,
}

impl DuvmBackend for RdmaBackend {
    fn store_page(&self, handle: PageHandle, data: &PageBuffer) -> Result<()> {
        // One-sided RDMA WRITE: local data → remote memory
        // No CPU involvement on remote side
        let offset = handle.offset() * PAGE_SIZE;
        ibv_post_send(self.qp, &wr_write(data, self.remote_addr + offset, self.remote_rkey))?;
        poll_completion(self.cq)?;
        Ok(())
    }
    
    fn load_page(&self, handle: PageHandle, buf: &mut PageBuffer) -> Result<()> {
        // One-sided RDMA READ: remote memory → local buffer
        let offset = handle.offset() * PAGE_SIZE;
        ibv_post_send(self.qp, &wr_read(buf, self.remote_addr + offset, self.remote_rkey))?;
        poll_completion(self.cq)?;
        Ok(())
    }
}
```

Key differences from TCP backend:
- **No memserver process on remote side** — RDMA writes directly to pre-registered remote memory
- **No serialization/deserialization** — raw page data goes on the wire
- **No CPU involvement on remote side** — NIC handles everything
- Remote side just runs a small setup daemon that registers memory and shares the rkey

### Implementation Order

1. Wire kernel module → daemon via ring buffer (current blocker)
2. RDMA backend using `rdma-core` Rust bindings or raw `libibverbs` FFI
3. Auto-detection: check for RDMA devices at daemon startup
4. SoftRoCE testing in QEMU for CI
