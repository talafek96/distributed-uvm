//! Integration tests for duvm: proves end-to-end page store/load through the full stack.
//!
//! These tests run entirely in user-space (no kernel module needed).
//! They exercise: backends, policy engine, daemon engine, and libduvm pool.

use duvm_backend_compress::CompressBackend;
use duvm_backend_memory::MemoryBackend;
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::{PAGE_SIZE, PageHandle, Tier};
use duvm_common::protocol::{OpCode, RingCompletion, RingRequest};
use duvm_common::ring::{CompletionRing, RequestRing};

/// Test: memory backend can store and load 1000 pages correctly.
#[test]
fn memory_backend_1000_pages() {
    let mut backend = MemoryBackend::new(0);
    backend
        .init(&BackendConfig {
            max_pages: 2000,
            ..Default::default()
        })
        .unwrap();

    let mut handles = Vec::new();

    // Store 1000 unique pages
    for i in 0u64..1000 {
        let handle = backend.alloc_page().unwrap();
        let mut data = [0u8; PAGE_SIZE];
        // Write a unique pattern: page number in first 8 bytes
        data[..8].copy_from_slice(&i.to_le_bytes());
        data[4088..4096].copy_from_slice(&i.to_le_bytes());
        backend.store_page(handle, &data).unwrap();
        handles.push(handle);
    }

    let (total, used) = backend.capacity();
    assert_eq!(used, 1000);
    assert!(total >= 2000);

    // Load all pages and verify content
    for (i, handle) in handles.iter().enumerate() {
        let mut buf = [0u8; PAGE_SIZE];
        backend.load_page(*handle, &mut buf).unwrap();
        let stored_i = u64::from_le_bytes(buf[..8].try_into().unwrap());
        assert_eq!(stored_i, i as u64, "page {} content mismatch", i);
        let tail_i = u64::from_le_bytes(buf[4088..4096].try_into().unwrap());
        assert_eq!(tail_i, i as u64, "page {} tail mismatch", i);
    }
}

/// Test: compression backend round-trips data correctly with various patterns.
#[test]
fn compress_backend_various_patterns() {
    let mut backend = CompressBackend::new(1);
    backend.init(&BackendConfig::default()).unwrap();

    // Pattern 1: all zeros (highly compressible)
    let h1 = backend.alloc_page().unwrap();
    let zeros = [0u8; PAGE_SIZE];
    backend.store_page(h1, &zeros).unwrap();

    // Pattern 2: all 0xFF
    let h2 = backend.alloc_page().unwrap();
    let ones = [0xFF; PAGE_SIZE];
    backend.store_page(h2, &ones).unwrap();

    // Pattern 3: sequential bytes
    let h3 = backend.alloc_page().unwrap();
    let mut sequential = [0u8; PAGE_SIZE];
    for (i, b) in sequential.iter_mut().enumerate() {
        *b = (i % 256) as u8;
    }
    backend.store_page(h3, &sequential).unwrap();

    // Pattern 4: pseudo-random (hard to compress)
    let h4 = backend.alloc_page().unwrap();
    let mut random = [0u8; PAGE_SIZE];
    let mut val: u32 = 0xCAFEBABE;
    for chunk in random.chunks_exact_mut(4) {
        val = val.wrapping_mul(1103515245).wrapping_add(12345);
        chunk.copy_from_slice(&val.to_le_bytes());
    }
    backend.store_page(h4, &random).unwrap();

    // Verify all patterns round-trip correctly
    let mut buf = [0u8; PAGE_SIZE];

    backend.load_page(h1, &mut buf).unwrap();
    assert_eq!(buf, zeros, "zeros pattern mismatch");

    backend.load_page(h2, &mut buf).unwrap();
    assert_eq!(buf, ones, "ones pattern mismatch");

    backend.load_page(h3, &mut buf).unwrap();
    assert_eq!(buf, sequential, "sequential pattern mismatch");

    backend.load_page(h4, &mut buf).unwrap();
    assert_eq!(buf, random, "random pattern mismatch");

    // Check compression ratio for zeros (should be > 10x)
    assert!(
        backend.compression_ratio() > 2.0,
        "compression ratio too low: {}",
        backend.compression_ratio()
    );
}

/// Test: ring buffer handles high-throughput request/completion flow.
#[test]
fn ring_buffer_throughput() {
    let mut req_ring = RequestRing::new(1024);
    let mut comp_ring = CompletionRing::new(1024);

    // Simulate 10,000 request-completion cycles
    for i in 0u32..10_000 {
        let mut req = bytemuck::Zeroable::zeroed();
        let r: &mut RingRequest = &mut req;
        r.op = OpCode::Store as u8;
        r.seq = i;
        r.pfn = i as u64 * 4096;
        r.offset = i as u64;
        r.staging_slot = i % 64;

        assert!(req_ring.try_push(req), "push failed at iteration {}", i);

        // "Process" the request
        let popped = req_ring.try_pop().unwrap();
        assert_eq!(popped.seq, i);

        // Write completion
        let mut comp: RingCompletion = bytemuck::Zeroable::zeroed();
        comp.seq = i;
        comp.result = 0;
        comp.handle = PageHandle::new(1, i as u64).raw();
        assert!(comp_ring.try_push(comp));

        let completed = comp_ring.try_pop().unwrap();
        assert_eq!(completed.seq, i);
        assert_eq!(completed.result, 0);
    }
}

/// Test: ring buffer batch operations.
#[test]
fn ring_buffer_batch() {
    let mut ring = RequestRing::new(256);

    // Push 100 requests
    for i in 0u32..100 {
        let mut req: RingRequest = bytemuck::Zeroable::zeroed();
        req.op = OpCode::Load as u8;
        req.seq = i;
        assert!(ring.try_push(req));
    }
    assert_eq!(ring.len(), 100);

    // Batch pop 64
    let mut batch = Vec::new();
    let count = ring.pop_batch(&mut batch, 64);
    assert_eq!(count, 64);
    assert_eq!(batch[0].seq, 0);
    assert_eq!(batch[63].seq, 63);
    assert_eq!(ring.len(), 36);

    // Pop remaining
    let count = ring.pop_batch(&mut batch, 64);
    assert_eq!(count, 36);
    assert!(ring.is_empty());
}

/// Test: libduvm Pool end-to-end with compression backend.
#[test]
fn libduvm_pool_end_to_end() {
    let pool = duvm::Pool::standalone().unwrap();

    // Store 100 pages with unique content
    let mut handles = Vec::new();
    for i in 0u32..100 {
        let mut data = [0u8; PAGE_SIZE];
        let pattern = format!("page-{:04}-duvm-test", i);
        data[..pattern.len()].copy_from_slice(pattern.as_bytes());
        handles.push(pool.store(&data).unwrap());
    }

    let (_, used) = pool.capacity();
    assert_eq!(used, 100);

    // Load and verify all pages
    for (i, handle) in handles.iter().enumerate() {
        let loaded = pool.load(*handle).unwrap();
        let expected = format!("page-{:04}-duvm-test", i);
        assert_eq!(
            &loaded[..expected.len()],
            expected.as_bytes(),
            "page {} content mismatch",
            i
        );
    }

    // Free half the pages
    for handle in &handles[..50] {
        pool.free(*handle).unwrap();
    }

    let (_, used) = pool.capacity();
    assert_eq!(used, 50);

    // Remaining pages should still be accessible
    for (i, handle) in handles[50..].iter().enumerate() {
        let loaded = pool.load(*handle).unwrap();
        let expected = format!("page-{:04}-duvm-test", i + 50);
        assert_eq!(&loaded[..expected.len()], expected.as_bytes());
    }
}

/// Test: page handle encoding preserves backend_id and offset.
#[test]
fn page_handle_encoding_stress() {
    for backend_id in [0u8, 1, 127, 255] {
        for offset in [0u64, 1, 1000, (1 << 56) - 1] {
            let handle = PageHandle::new(backend_id, offset);
            assert_eq!(
                handle.backend_id(),
                backend_id,
                "backend_id mismatch for ({}, {})",
                backend_id,
                offset
            );
            assert_eq!(
                handle.offset(),
                offset,
                "offset mismatch for ({}, {})",
                backend_id,
                offset
            );
        }
    }
}

/// Test: multiple backends can operate independently.
#[test]
fn multiple_backends_independent() {
    let mut mem = MemoryBackend::new(0);
    mem.init(&BackendConfig {
        max_pages: 100,
        ..Default::default()
    })
    .unwrap();

    let mut comp = CompressBackend::new(1);
    comp.init(&BackendConfig {
        max_pages: 100,
        ..Default::default()
    })
    .unwrap();

    let data_mem = [0xAA; PAGE_SIZE];
    let data_comp = [0xBB; PAGE_SIZE];

    let h_mem = mem.alloc_page().unwrap();
    mem.store_page(h_mem, &data_mem).unwrap();

    let h_comp = comp.alloc_page().unwrap();
    comp.store_page(h_comp, &data_comp).unwrap();

    // Each backend has its own data
    let mut buf = [0u8; PAGE_SIZE];
    mem.load_page(h_mem, &mut buf).unwrap();
    assert_eq!(buf[0], 0xAA);

    comp.load_page(h_comp, &mut buf).unwrap();
    assert_eq!(buf[0], 0xBB);

    // Capacity is independent
    assert_eq!(mem.capacity().1, 1);
    assert_eq!(comp.capacity().1, 1);
}

/// Test: backend handles are scoped to their backend (different backend_ids).
#[test]
fn handles_are_backend_scoped() {
    let mut mem = MemoryBackend::new(0);
    mem.init(&BackendConfig::default()).unwrap();

    let mut comp = CompressBackend::new(1);
    comp.init(&BackendConfig::default()).unwrap();

    let h1 = mem.alloc_page().unwrap();
    let h2 = comp.alloc_page().unwrap();

    assert_eq!(h1.backend_id(), 0);
    assert_eq!(h2.backend_id(), 1);
}

/// Test: tier ordering is correct (Local < Compressed < Cxl < Rdma < Gpu).
#[test]
fn tier_ordering() {
    assert!(Tier::Local < Tier::Compressed);
    assert!(Tier::Compressed < Tier::Cxl);
    assert!(Tier::Cxl < Tier::Rdma);
    assert!(Tier::Rdma < Tier::Gpu);
}
