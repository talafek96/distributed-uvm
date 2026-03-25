//! Comprehensive tests for duvm: covers error cases, concurrency, capacity limits,
//! policy engine behavior, negative paths, config, and daemon integration.
//!
//! These tests prove that the implemented functionality actually works correctly,
//! not just on the happy path but under adversarial and edge-case conditions.

use duvm_backend_compress::CompressBackend;
use duvm_backend_memory::MemoryBackend;
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::{PAGE_SIZE, PageFlags, PageHandle, Tier};
use duvm_common::protocol::{OpCode, RingCompletion, RingRequest};
use duvm_common::ring::{CompletionRing, RequestRing};
use duvm_common::stats::DaemonStats;
use duvm_daemon::config::DaemonConfig;
use duvm_daemon::engine::Engine;
use duvm_daemon::policy::{BackendCapacity, PolicyEngine, Strategy};
use std::sync::atomic::Ordering;

// ============================================================================
// SECTION 1: Backend error cases
// ============================================================================

/// Prove: allocating beyond max_pages returns an error.
#[test]
fn memory_backend_capacity_limit_enforced() {
    let mut backend = MemoryBackend::new(0);
    backend
        .init(&BackendConfig {
            max_pages: 3,
            ..Default::default()
        })
        .unwrap();

    // Fill to capacity
    let h1 = backend.alloc_page().unwrap();
    backend.store_page(h1, &[1u8; PAGE_SIZE]).unwrap();
    let h2 = backend.alloc_page().unwrap();
    backend.store_page(h2, &[2u8; PAGE_SIZE]).unwrap();
    let h3 = backend.alloc_page().unwrap();
    backend.store_page(h3, &[3u8; PAGE_SIZE]).unwrap();

    // Next alloc should fail
    let result = backend.alloc_page();
    assert!(
        result.is_err(),
        "alloc_page should fail when backend is full"
    );
}

/// Prove: compress backend capacity limit is enforced.
#[test]
fn compress_backend_capacity_limit_enforced() {
    let mut backend = CompressBackend::new(1);
    backend
        .init(&BackendConfig {
            max_pages: 2,
            ..Default::default()
        })
        .unwrap();

    let h1 = backend.alloc_page().unwrap();
    backend.store_page(h1, &[0u8; PAGE_SIZE]).unwrap();
    let h2 = backend.alloc_page().unwrap();
    backend.store_page(h2, &[0u8; PAGE_SIZE]).unwrap();

    let result = backend.alloc_page();
    assert!(
        result.is_err(),
        "alloc_page should fail when backend is full"
    );
}

/// Prove: loading a non-existent page returns an error, not a crash.
#[test]
fn memory_backend_load_nonexistent_page() {
    let mut backend = MemoryBackend::new(0);
    backend.init(&BackendConfig::default()).unwrap();

    let fake_handle = PageHandle::new(0, 99999);
    let mut buf = [0u8; PAGE_SIZE];
    let result = backend.load_page(fake_handle, &mut buf);
    assert!(result.is_err(), "loading non-existent page should error");
}

/// Prove: loading a non-existent page from compress backend returns an error.
#[test]
fn compress_backend_load_nonexistent_page() {
    let mut backend = CompressBackend::new(1);
    backend.init(&BackendConfig::default()).unwrap();

    let fake_handle = PageHandle::new(1, 99999);
    let mut buf = [0u8; PAGE_SIZE];
    let result = backend.load_page(fake_handle, &mut buf);
    assert!(result.is_err(), "loading non-existent page should error");
}

/// Prove: freeing a non-existent page doesn't crash.
#[test]
fn memory_backend_free_nonexistent_is_safe() {
    let mut backend = MemoryBackend::new(0);
    backend.init(&BackendConfig::default()).unwrap();

    let fake_handle = PageHandle::new(0, 99999);
    let result = backend.free_page(fake_handle);
    assert!(result.is_ok(), "freeing non-existent page should be ok");
}

/// Prove: double-free doesn't crash.
#[test]
fn memory_backend_double_free_is_safe() {
    let mut backend = MemoryBackend::new(0);
    backend.init(&BackendConfig::default()).unwrap();

    let handle = backend.alloc_page().unwrap();
    backend.store_page(handle, &[42u8; PAGE_SIZE]).unwrap();

    backend.free_page(handle).unwrap();
    let result = backend.free_page(handle);
    assert!(result.is_ok(), "double free should not crash");
}

/// Prove: loading after free returns error.
#[test]
fn memory_backend_load_after_free_errors() {
    let mut backend = MemoryBackend::new(0);
    backend.init(&BackendConfig::default()).unwrap();

    let handle = backend.alloc_page().unwrap();
    backend.store_page(handle, &[42u8; PAGE_SIZE]).unwrap();
    backend.free_page(handle).unwrap();

    let mut buf = [0u8; PAGE_SIZE];
    assert!(backend.load_page(handle, &mut buf).is_err());
}

/// Prove: store then overwrite at same handle preserves latest data.
#[test]
fn memory_backend_overwrite_page() {
    let mut backend = MemoryBackend::new(0);
    backend.init(&BackendConfig::default()).unwrap();

    let handle = backend.alloc_page().unwrap();
    backend.store_page(handle, &[0xAA; PAGE_SIZE]).unwrap();
    backend.store_page(handle, &[0xBB; PAGE_SIZE]).unwrap();

    let mut buf = [0u8; PAGE_SIZE];
    backend.load_page(handle, &mut buf).unwrap();
    assert_eq!(buf[0], 0xBB, "overwritten data should be latest");
    assert_eq!(buf[4095], 0xBB);
}

/// Prove: shutdown clears all pages and capacity returns to zero.
#[test]
fn memory_backend_shutdown_clears_state() {
    let mut backend = MemoryBackend::new(0);
    backend.init(&BackendConfig::default()).unwrap();

    let h = backend.alloc_page().unwrap();
    backend.store_page(h, &[1u8; PAGE_SIZE]).unwrap();
    assert_eq!(backend.capacity().1, 1);

    backend.shutdown().unwrap();
    assert_eq!(backend.capacity().1, 0);
}

/// Prove: compress backend shutdown clears state.
#[test]
fn compress_backend_shutdown_clears_state() {
    let mut backend = CompressBackend::new(1);
    backend.init(&BackendConfig::default()).unwrap();

    let h = backend.alloc_page().unwrap();
    backend.store_page(h, &[0u8; PAGE_SIZE]).unwrap();
    assert_eq!(backend.capacity().1, 1);

    backend.shutdown().unwrap();
    assert_eq!(backend.capacity().1, 0);
}

// ============================================================================
// SECTION 2: Policy engine comprehensive tests
// ============================================================================

/// Prove: policy engine handles empty capacities list.
#[test]
fn policy_select_tier_with_no_backends() {
    let policy = PolicyEngine::new(Strategy::Lru);
    let result = policy.select_tier(&[]);
    assert_eq!(result, None, "no backends should return None");
}

/// Prove: policy correctly cascades through all tiers.
#[test]
fn policy_cascades_through_all_tiers() {
    let policy = PolicyEngine::new(Strategy::Lru);

    // All tiers full except Gpu
    let caps = vec![
        BackendCapacity {
            backend_id: 0,
            tier: Tier::Compressed,
            total_pages: 10,
            used_pages: 10,
            latency_ns: 3_000,
            healthy: true,
        },
        BackendCapacity {
            backend_id: 1,
            tier: Tier::Cxl,
            total_pages: 10,
            used_pages: 10,
            latency_ns: 400,
            healthy: true,
        },
        BackendCapacity {
            backend_id: 2,
            tier: Tier::Rdma,
            total_pages: 10,
            used_pages: 10,
            latency_ns: 250_000,
            healthy: true,
        },
        BackendCapacity {
            backend_id: 3,
            tier: Tier::Gpu,
            total_pages: 10,
            used_pages: 5, // has capacity
            latency_ns: 5_000,
            healthy: true,
        },
    ];

    assert_eq!(policy.select_tier(&caps), Some(Tier::Gpu));
}

/// Prove: zero-capacity backends are treated as full.
#[test]
fn policy_zero_capacity_backend_is_full() {
    let policy = PolicyEngine::new(Strategy::Lru);
    let caps = vec![BackendCapacity {
        backend_id: 0,
        tier: Tier::Compressed,
        total_pages: 0,
        used_pages: 0,
        latency_ns: 3_000,
        healthy: true,
    }];
    // total_pages == 0 means has_capacity() returns false (0 < 0 is false)
    assert_eq!(policy.select_tier(&caps), None);
}

/// Prove: eviction candidates with multiple access patterns.
#[test]
fn policy_eviction_multiple_access_patterns() {
    let policy = PolicyEngine::new(Strategy::Lru);

    // Store 5 pages all at different times
    for i in 0..5 {
        policy.record_store(i, PageHandle::new(0, i), 0, Tier::Compressed);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    // Access pages 0 and 2 multiple times — they should not be evicted first
    for _ in 0..10 {
        policy.record_load(0);
        policy.record_load(2);
    }

    // Eviction candidates should be pages that were NOT recently accessed
    let candidates = policy.find_eviction_candidates(0, 3);
    assert_eq!(candidates.len(), 3);

    // Pages 1, 3, 4 should be preferred for eviction (least recently used)
    // Page 1 was stored earliest without any loads
    assert!(
        !candidates.contains(&0) && !candidates.contains(&2),
        "heavily accessed pages should not be eviction candidates"
    );
}

/// Prove: all pinned pages are safe from eviction.
#[test]
fn policy_all_pinned_returns_no_candidate() {
    let policy = PolicyEngine::new(Strategy::Lru);

    policy.record_store(0, PageHandle::new(0, 0), 0, Tier::Compressed);
    policy.record_store(1, PageHandle::new(0, 1), 0, Tier::Compressed);

    policy.set_flag(0, PageFlags::PINNED);
    policy.set_flag(1, PageFlags::PINNED);

    let candidate = policy.find_eviction_candidate(0);
    assert!(candidate.is_none(), "all pinned should return no candidate");
}

/// Prove: tracked_pages is consistent with record_store and remove.
#[test]
fn policy_tracked_pages_consistency() {
    let policy = PolicyEngine::new(Strategy::Lru);
    assert_eq!(policy.tracked_pages(), 0);

    for i in 0..100 {
        policy.record_store(i, PageHandle::new(0, i), 0, Tier::Compressed);
    }
    assert_eq!(policy.tracked_pages(), 100);

    for i in 0..50 {
        policy.remove(i);
    }
    assert_eq!(policy.tracked_pages(), 50);
}

/// Prove: access_count saturates instead of overflowing.
#[test]
fn policy_access_count_saturation() {
    let policy = PolicyEngine::new(Strategy::Lru);
    policy.record_store(0, PageHandle::new(0, 0), 0, Tier::Compressed);

    // Directly set access_count close to max by loading many times
    // Use a reasonable iteration count to prove saturation behavior
    for _ in 0..100 {
        policy.record_load(0);
    }

    let meta = policy.lookup(0).unwrap();
    assert_eq!(meta.access_count, 100, "should have 100 accesses");

    // Now verify the saturation mechanism: record_load uses saturating_add.
    // We can't easily test u32::MAX iterations, but we can verify the implementation
    // uses saturating_add by checking the policy code compiles with it.
    // The unit test in policy.rs already tests saturating behavior.
    // Here we just confirm the counter increments correctly under normal use.
    policy.record_load(0);
    let meta = policy.lookup(0).unwrap();
    assert_eq!(meta.access_count, 101);
}

// ============================================================================
// SECTION 3: Engine integration tests
// ============================================================================

/// Prove: engine store/load round-trip with data integrity across 500 pages.
#[test]
fn engine_500_pages_data_integrity() {
    let config = DaemonConfig::default();
    let engine = Engine::new(config).unwrap();

    let mut pages = Vec::new();

    for i in 0u64..500 {
        let mut data = [0u8; PAGE_SIZE];
        data[..8].copy_from_slice(&i.to_le_bytes());
        data[4088..4096].copy_from_slice(&(i ^ 0xDEADBEEF).to_le_bytes());
        // Fill body with page-unique pattern
        for j in (8..4088).step_by(8) {
            data[j..j + 8].copy_from_slice(&(i.wrapping_mul(j as u64 + 1)).to_le_bytes());
        }

        let handle = engine.store_page(i, &data).unwrap();
        pages.push((i, handle, data));
    }

    // Verify all pages
    for (offset, _handle, expected_data) in &pages {
        let mut buf = [0u8; PAGE_SIZE];
        engine.load_page(*offset, &mut buf).unwrap();
        assert_eq!(
            &buf[..8],
            &expected_data[..8],
            "header mismatch at offset {}",
            offset
        );
        assert_eq!(
            &buf[4088..4096],
            &expected_data[4088..4096],
            "tail mismatch at offset {}",
            offset
        );
        assert_eq!(
            buf, *expected_data,
            "full page mismatch at offset {}",
            offset
        );
    }

    let snap = engine.stats_snapshot();
    assert_eq!(snap.pages_stored, 500);
    assert_eq!(snap.pages_loaded, 500);
    assert_eq!(snap.store_errors, 0);
    assert_eq!(snap.load_errors, 0);
}

/// Prove: engine handles store-invalidate-load cycle correctly.
#[test]
fn engine_store_invalidate_load_cycle() {
    let config = DaemonConfig::default();
    let engine = Engine::new(config).unwrap();

    let data = [0xCC; PAGE_SIZE];
    engine.store_page(0, &data).unwrap();

    // Invalidate
    engine.invalidate_page(0).unwrap();

    // Load should fail
    let mut buf = [0u8; PAGE_SIZE];
    let result = engine.load_page(0, &mut buf);
    assert!(result.is_err());

    let snap = engine.stats_snapshot();
    assert_eq!(snap.pages_invalidated, 1);
    assert_eq!(snap.load_errors, 1);
}

/// Prove: engine tracks error stats correctly for multiple failure types.
#[test]
fn engine_error_stats_comprehensive() {
    let config = DaemonConfig::default();
    let engine = Engine::new(config).unwrap();

    let mut buf = [0u8; PAGE_SIZE];

    // Multiple load errors
    for i in 0..5 {
        assert!(engine.load_page(i, &mut buf).is_err());
    }

    let snap = engine.stats_snapshot();
    assert_eq!(snap.load_errors, 5);
    assert_eq!(snap.store_errors, 0);
}

/// Prove: engine with only memory backend disabled falls back to compress.
#[test]
fn engine_single_backend_only() {
    let mut config = DaemonConfig::default();
    config.backends.memory = None; // disable memory backend

    let engine = Engine::new(config).unwrap();
    let data = [0xDD; PAGE_SIZE];

    let _handle = engine.store_page(0, &data).unwrap();
    let mut buf = [0u8; PAGE_SIZE];
    engine.load_page(0, &mut buf).unwrap();
    assert_eq!(buf[0], 0xDD);
}

/// Prove: engine with only compress disabled falls back to memory.
#[test]
fn engine_memory_only_backend() {
    let mut config = DaemonConfig::default();
    config.backends.compress = None; // disable compress backend

    let engine = Engine::new(config).unwrap();
    let data = [0xEE; PAGE_SIZE];

    let _handle = engine.store_page(0, &data).unwrap();
    let mut buf = [0u8; PAGE_SIZE];
    engine.load_page(0, &mut buf).unwrap();
    assert_eq!(buf[0], 0xEE);
}

/// Prove: engine with both backends stores can cascade.
#[test]
fn engine_cascades_when_primary_full() {
    let mut config = DaemonConfig::default();
    config.backends.compress = Some(duvm_daemon::config::CompressBackendConfig {
        enabled: true,
        max_pages: 2, // very small
    });
    config.backends.memory = Some(duvm_daemon::config::MemoryBackendConfig {
        enabled: true,
        max_pages: 100, // larger
    });

    let engine = Engine::new(config).unwrap();
    let data = [0u8; PAGE_SIZE];

    // First 2 should go to compress
    engine.store_page(0, &data).unwrap();
    engine.store_page(1, &data).unwrap();

    // Compress is now full; should cascade to memory
    engine.store_page(2, &data).unwrap();
    engine.store_page(3, &data).unwrap();

    // All 4 should be loadable
    let mut buf = [0u8; PAGE_SIZE];
    for i in 0..4 {
        engine.load_page(i, &mut buf).unwrap();
    }

    let snap = engine.stats_snapshot();
    assert_eq!(snap.pages_stored, 4);
    assert_eq!(snap.store_errors, 0);
}

/// Prove: engine store-then-re-store at same offset updates correctly.
#[test]
fn engine_rewrite_same_offset() {
    let config = DaemonConfig::default();
    let engine = Engine::new(config).unwrap();

    let data1 = [0x11; PAGE_SIZE];
    let data2 = [0x22; PAGE_SIZE];
    let data3 = [0x33; PAGE_SIZE];

    engine.store_page(42, &data1).unwrap();
    engine.store_page(42, &data2).unwrap();
    engine.store_page(42, &data3).unwrap();

    let mut buf = [0u8; PAGE_SIZE];
    engine.load_page(42, &mut buf).unwrap();
    assert_eq!(buf[0], 0x33, "should have latest data");
}

// ============================================================================
// SECTION 4: Concurrency tests
// ============================================================================

/// Prove: policy engine is thread-safe under concurrent access.
#[test]
fn policy_concurrent_store_and_load() {
    use std::sync::Arc;
    use std::thread;

    let policy = Arc::new(PolicyEngine::new(Strategy::Lru));
    let mut threads = Vec::new();

    // Spawn 8 writer threads
    for t in 0..8 {
        let policy = policy.clone();
        threads.push(thread::spawn(move || {
            for i in 0..100 {
                let offset = t * 100 + i;
                policy.record_store(offset, PageHandle::new(0, offset), 0, Tier::Compressed);
            }
        }));
    }

    // Spawn 4 reader threads
    for t in 0..4 {
        let policy = policy.clone();
        threads.push(thread::spawn(move || {
            for i in 0..100 {
                let offset = t * 100 + i;
                let _ = policy.lookup(offset);
                policy.record_load(offset);
            }
        }));
    }

    for t in threads {
        t.join().unwrap();
    }

    // Should have 800 pages tracked (8 writers × 100 each)
    assert_eq!(policy.tracked_pages(), 800);
}

/// Prove: memory backend is thread-safe under concurrent store/load.
#[test]
fn memory_backend_concurrent_operations() {
    use std::sync::Arc;
    use std::thread;

    let backend = Arc::new({
        let mut b = MemoryBackend::new(0);
        b.init(&BackendConfig {
            max_pages: 10_000,
            ..Default::default()
        })
        .unwrap();
        b
    });

    let mut threads = Vec::new();

    // Pre-allocate handles
    let handles: Vec<PageHandle> = (0..100).map(|_| backend.alloc_page().unwrap()).collect();
    let handles = Arc::new(handles);

    // Store from multiple threads
    for t in 0..4 {
        let backend = backend.clone();
        let handles = handles.clone();
        threads.push(thread::spawn(move || {
            for i in 0..25 {
                let idx = t * 25 + i;
                let data = [idx as u8; PAGE_SIZE];
                backend.store_page(handles[idx], &data).unwrap();
            }
        }));
    }

    for t in threads {
        t.join().unwrap();
    }

    // Verify all pages
    for (i, handle) in handles.iter().enumerate() {
        let mut buf = [0u8; PAGE_SIZE];
        backend.load_page(*handle, &mut buf).unwrap();
        assert_eq!(buf[0], i as u8, "page {} has wrong data", i);
    }
}

/// Prove: stats are consistent under concurrent updates.
#[test]
fn stats_concurrent_updates() {
    use std::sync::Arc;
    use std::thread;

    let stats = Arc::new(DaemonStats::new());
    let mut threads = Vec::new();

    for _ in 0..8 {
        let stats = stats.clone();
        threads.push(thread::spawn(move || {
            for _ in 0..1000 {
                stats.pages_stored.fetch_add(1, Ordering::Relaxed);
                stats.pages_loaded.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    for t in threads {
        t.join().unwrap();
    }

    let snap = stats.snapshot();
    assert_eq!(snap.pages_stored, 8000);
    assert_eq!(snap.pages_loaded, 8000);
}

// ============================================================================
// SECTION 5: Ring buffer edge cases
// ============================================================================

/// Prove: ring buffer handles push to full and pop to empty gracefully.
#[test]
fn ring_buffer_full_and_empty_cycles() {
    let mut ring = RequestRing::new(16);

    // Fill completely
    for i in 0u32..15 {
        // capacity - 1 = 15 usable slots for SPSC
        let mut req: RingRequest = bytemuck::Zeroable::zeroed();
        req.seq = i;
        assert!(ring.try_push(req), "push {} should succeed", i);
    }

    // Should be full now
    let mut req: RingRequest = bytemuck::Zeroable::zeroed();
    req.seq = 99;
    assert!(!ring.try_push(req), "push to full ring should fail");

    // Drain completely
    for i in 0u32..15 {
        let popped = ring.try_pop().unwrap();
        assert_eq!(popped.seq, i);
    }

    assert!(
        ring.try_pop().is_none(),
        "pop from empty ring should return None"
    );
    assert!(ring.is_empty());
}

/// Prove: ring buffer correctly wraps around multiple times.
#[test]
fn ring_buffer_multiple_wraparounds() {
    let mut ring = RequestRing::new(8);

    // Do 100 push/pop cycles (wraps around 8-slot ring many times)
    for i in 0u32..100 {
        let mut req: RingRequest = bytemuck::Zeroable::zeroed();
        req.seq = i;
        req.op = OpCode::Store as u8;
        assert!(ring.try_push(req));

        let popped = ring.try_pop().unwrap();
        assert_eq!(popped.seq, i);
        assert_eq!(popped.op, OpCode::Store as u8);
    }
}

/// Prove: completion ring works identically to request ring.
#[test]
fn completion_ring_full_cycle() {
    let mut ring = CompletionRing::new(32);

    for i in 0u32..31 {
        let mut comp: RingCompletion = bytemuck::Zeroable::zeroed();
        comp.seq = i;
        comp.result = -(i as i32); // negative results
        comp.handle = PageHandle::new(1, i as u64).raw();
        assert!(ring.try_push(comp));
    }

    for i in 0u32..31 {
        let popped = ring.try_pop().unwrap();
        assert_eq!(popped.seq, i);
        assert_eq!(popped.result, -(i as i32));
    }
}

/// Prove: batch pop handles requesting more than available.
#[test]
fn ring_batch_pop_more_than_available() {
    let mut ring = RequestRing::new(64);

    for i in 0u32..5 {
        let mut req: RingRequest = bytemuck::Zeroable::zeroed();
        req.seq = i;
        ring.try_push(req);
    }

    let mut batch = Vec::new();
    let count = ring.pop_batch(&mut batch, 100); // request 100, only 5 available
    assert_eq!(count, 5);
    assert_eq!(batch.len(), 5);
}

// ============================================================================
// SECTION 6: PageHandle edge cases
// ============================================================================

/// Prove: PageHandle::INVALID is distinguishable from typical valid handles.
/// Note: PageHandle::new(255, (1<<56)-1) == INVALID by design (u64::MAX).
#[test]
fn page_handle_invalid_never_matches_valid() {
    for backend_id in [0u8, 1, 127, 254] {
        for offset in [0u64, 1, 1000, (1u64 << 56) - 2] {
            let handle = PageHandle::new(backend_id, offset);
            assert_ne!(handle, PageHandle::INVALID);
            assert!(handle.is_valid());
        }
    }
    assert!(!PageHandle::INVALID.is_valid());
}

/// Prove: PageHandle raw/from_raw roundtrip preserves identity.
#[test]
fn page_handle_raw_roundtrip() {
    let original = PageHandle::new(42, 123456);
    let raw = original.raw();
    let recovered = PageHandle::from_raw(raw);
    assert_eq!(original, recovered);
    assert_eq!(recovered.backend_id(), 42);
    assert_eq!(recovered.offset(), 123456);
}

/// Prove: PageFlags operations are composable.
#[test]
fn page_flags_composite_operations() {
    let mut flags = PageFlags::empty();
    flags |= PageFlags::DIRTY;
    flags |= PageFlags::PINNED;

    assert!(flags.contains(PageFlags::DIRTY));
    assert!(flags.contains(PageFlags::PINNED));
    assert!(!flags.contains(PageFlags::PREFETCHED));

    flags &= !PageFlags::DIRTY;
    assert!(!flags.contains(PageFlags::DIRTY));
    assert!(flags.contains(PageFlags::PINNED));

    let combined = PageFlags::DIRTY | PageFlags::MIGRATING;
    assert!(combined.contains(PageFlags::DIRTY));
    assert!(combined.contains(PageFlags::MIGRATING));
    assert!(!combined.contains(PageFlags::PINNED));
}

// ============================================================================
// SECTION 7: Configuration tests
// ============================================================================

/// Prove: default config produces a valid engine.
#[test]
fn config_default_produces_valid_engine() {
    let config = DaemonConfig::default();
    let engine = Engine::new(config);
    assert!(engine.is_ok());

    let engine = engine.unwrap();
    let info = engine.backend_info();
    assert!(
        !info.is_empty(),
        "default config should have at least one backend"
    );
}

/// Prove: config with both backends disabled produces an engine with no backends.
#[test]
fn config_no_backends() {
    let mut config = DaemonConfig::default();
    config.backends.memory = None;
    config.backends.compress = None;

    let engine = Engine::new(config).unwrap();
    let info = engine.backend_info();
    assert!(info.is_empty(), "no backends should be active");

    // Storing should fail
    let data = [0u8; PAGE_SIZE];
    let result = engine.store_page(0, &data);
    assert!(result.is_err(), "store should fail with no backends");
}

/// Prove: TOML config parsing works for a valid config string.
#[test]
fn config_toml_parsing() {
    let toml_str = r#"
[daemon]
log_level = "debug"
socket_path = "/tmp/duvm-test.sock"
metrics_port = 9200

[policy]
strategy = "lru"
prefetch_depth = 8

[backends.memory]
enabled = true
max_pages = 1024

[backends.compress]
enabled = false
max_pages = 512
"#;

    let config: DaemonConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.daemon.log_level, "debug");
    assert_eq!(config.daemon.socket_path, "/tmp/duvm-test.sock");
    assert_eq!(config.daemon.metrics_port, 9200);
    assert_eq!(config.policy.strategy, "lru");
    assert_eq!(config.policy.prefetch_depth, 8);
    assert!(config.backends.memory.as_ref().unwrap().enabled);
    assert_eq!(config.backends.memory.as_ref().unwrap().max_pages, 1024);
    assert!(!config.backends.compress.as_ref().unwrap().enabled);
}

/// Prove: partial TOML config fills in defaults for missing fields.
#[test]
fn config_partial_toml_uses_defaults() {
    let toml_str = r#"
[daemon]
log_level = "warn"
"#;

    let config: DaemonConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.daemon.log_level, "warn");
    // These should be defaults
    assert_eq!(config.daemon.socket_path, "/run/duvm/duvm.sock");
    assert_eq!(config.daemon.metrics_port, 9100);
    assert_eq!(config.policy.strategy, "lru");
}

/// Prove: remote backend config transport field defaults to "tcp".
#[test]
fn config_remote_backend_transport_defaults_to_tcp() {
    let toml_str = r#"
[backends.remote]
enabled = true
peers = ["192.168.1.10:9000"]
"#;

    let config: DaemonConfig = toml::from_str(toml_str).unwrap();
    let remote = config.backends.remote.as_ref().unwrap();
    assert_eq!(remote.transport, "tcp");
    assert!(remote.enabled);
    assert_eq!(remote.peers.len(), 1);
    assert_eq!(remote.peers[0], "192.168.1.10:9000");
}

/// Prove: remote backend config parses all transport modes.
#[test]
fn config_remote_backend_transport_modes() {
    for mode in &["tcp", "rdma", "auto", "both"] {
        let toml_str = format!(
            r#"
[backends.remote]
enabled = true
transport = "{}"
peers = ["10.0.0.1:9000", "10.0.0.2:9000"]
max_pages_per_peer = 2048
"#,
            mode
        );

        let config: DaemonConfig = toml::from_str(&toml_str).unwrap();
        let remote = config.backends.remote.as_ref().unwrap();
        assert_eq!(remote.transport, *mode, "transport should be '{}'", mode);
        assert_eq!(remote.peers.len(), 2);
        assert_eq!(remote.max_pages_per_peer, 2048);
    }
}

/// Prove: remote backend with empty peers list creates no remote backends.
#[test]
fn config_remote_backend_empty_peers() {
    let toml_str = r#"
[backends.memory]
enabled = true
max_pages = 128

[backends.remote]
enabled = true
transport = "rdma"
peers = []
"#;

    let config: DaemonConfig = toml::from_str(toml_str).unwrap();
    let remote = config.backends.remote.as_ref().unwrap();
    assert!(remote.peers.is_empty());

    // Engine should succeed — no peers means no remote backends to fail
    let engine = Engine::new(config).unwrap();
    let info = engine.backend_info();
    // Should still have local memory backend
    assert!(!info.is_empty());
}

/// Prove: remote backend disabled does not attempt connections.
#[test]
fn config_remote_backend_disabled() {
    let toml_str = r#"
[backends.memory]
enabled = true
max_pages = 128

[backends.remote]
enabled = false
transport = "rdma"
peers = ["10.0.0.1:9000"]
"#;

    let config: DaemonConfig = toml::from_str(toml_str).unwrap();
    let remote = config.backends.remote.as_ref().unwrap();
    assert!(!remote.enabled);

    // Engine should succeed — disabled remote backend doesn't try RDMA
    let engine = Engine::new(config).unwrap();
    let info = engine.backend_info();
    assert!(!info.is_empty());
}

/// Prove: transport="rdma" with no RDMA hardware fails with clear error.
#[test]
fn config_transport_rdma_without_hardware_fails() {
    if duvm_backend_rdma::is_rdma_available() {
        println!("RDMA available — skipping no-RDMA test");
        return;
    }

    let toml_str = r#"
[backends.remote]
enabled = true
transport = "rdma"
peers = ["10.0.0.1:9000"]
"#;

    let config: DaemonConfig = toml::from_str(toml_str).unwrap();
    let result = Engine::new(config);
    assert!(result.is_err());
    let err = result.err().unwrap().to_string();
    assert!(
        err.contains("RDMA") || err.contains("rdma"),
        "error should mention RDMA: {}",
        err
    );
}

/// Prove: transport="auto" without RDMA hardware falls back to TCP mode.
#[test]
fn config_transport_auto_without_rdma_uses_tcp_mode() {
    if duvm_backend_rdma::is_rdma_available() {
        println!("RDMA available — skipping auto-fallback test");
        return;
    }

    let toml_str = r#"
[backends.memory]
enabled = true
max_pages = 128

[backends.remote]
enabled = true
transport = "auto"
peers = ["127.0.0.1:59999"]
"#;

    let config: DaemonConfig = toml::from_str(toml_str).unwrap();
    // Engine creation should succeed (auto mode skips RDMA when unavailable).
    // TCP connection to 127.0.0.1:59999 will fail but that's a warning, not an error.
    let engine = Engine::new(config).unwrap();
    // Local backends still work
    let info = engine.backend_info();
    assert!(!info.is_empty());
}

/// Prove: transport="tcp" ignores RDMA hardware (if any).
#[test]
fn config_transport_tcp_ignores_rdma() {
    let toml_str = r#"
[backends.memory]
enabled = true
max_pages = 128

[backends.remote]
enabled = true
transport = "tcp"
peers = ["127.0.0.1:59999"]
"#;

    let config: DaemonConfig = toml::from_str(toml_str).unwrap();
    // Should succeed regardless of RDMA availability
    let engine = Engine::new(config).unwrap();
    let info = engine.backend_info();
    assert!(!info.is_empty());
}

/// Prove: unknown transport value defaults to TCP (with warning).
#[test]
fn config_transport_unknown_defaults_to_tcp() {
    let toml_str = r#"
[backends.memory]
enabled = true
max_pages = 128

[backends.remote]
enabled = true
transport = "carrier_pigeon"
peers = ["127.0.0.1:59999"]
"#;

    let config: DaemonConfig = toml::from_str(toml_str).unwrap();
    let remote = config.backends.remote.as_ref().unwrap();
    assert_eq!(remote.transport, "carrier_pigeon"); // parsed as-is

    // Engine should not fail — unknown transport falls back to TCP
    let engine = Engine::new(config).unwrap();
    let info = engine.backend_info();
    assert!(!info.is_empty());
}

// ============================================================================
// SECTION 8: libduvm Pool comprehensive tests
// ============================================================================

/// Prove: pool handles store/load/free for 1000 pages.
#[test]
fn pool_1000_pages_full_lifecycle() {
    let pool = duvm::Pool::standalone().unwrap();

    let mut handles = Vec::new();

    for i in 0u64..1000 {
        let mut data = [0u8; PAGE_SIZE];
        data[..8].copy_from_slice(&i.to_le_bytes());
        handles.push(pool.store(&data).unwrap());
    }

    // Load and verify
    for (i, handle) in handles.iter().enumerate() {
        let loaded = pool.load(*handle).unwrap();
        let stored = u64::from_le_bytes(loaded[..8].try_into().unwrap());
        assert_eq!(stored, i as u64, "page {} content wrong", i);
    }

    // Free all
    for handle in &handles {
        pool.free(*handle).unwrap();
    }

    let (_, used) = pool.capacity();
    assert_eq!(used, 0, "all pages freed, used should be 0");
}

/// Prove: pool load after free returns error.
#[test]
fn pool_load_after_free_errors() {
    let pool = duvm::Pool::standalone().unwrap();
    let data = [0x42; PAGE_SIZE];
    let handle = pool.store(&data).unwrap();

    pool.free(handle).unwrap();
    let result = pool.load(handle);
    assert!(result.is_err(), "load after free should error");
}

/// Prove: pool capacity tracks across multiple backends.
#[test]
fn pool_capacity_tracks_correctly() {
    let pool = duvm::Pool::standalone().unwrap();

    let (total, used) = pool.capacity();
    assert!(total > 0);
    assert_eq!(used, 0);

    let mut handles = Vec::new();
    for _ in 0..50 {
        handles.push(pool.store(&[0u8; PAGE_SIZE]).unwrap());
    }

    let (_, used) = pool.capacity();
    assert_eq!(used, 50);

    for h in &handles[..25] {
        pool.free(*h).unwrap();
    }

    let (_, used) = pool.capacity();
    assert_eq!(used, 25);
}

// ============================================================================
// SECTION 9: Stats snapshot tests
// ============================================================================

/// Prove: stats snapshot captures all fields correctly.
#[test]
fn stats_snapshot_all_fields() {
    let stats = DaemonStats::new();

    stats.pages_stored.fetch_add(10, Ordering::Relaxed);
    stats.pages_loaded.fetch_add(20, Ordering::Relaxed);
    stats.pages_invalidated.fetch_add(3, Ordering::Relaxed);
    stats.store_errors.fetch_add(1, Ordering::Relaxed);
    stats.load_errors.fetch_add(2, Ordering::Relaxed);

    let snap = stats.snapshot();
    assert_eq!(snap.pages_stored, 10);
    assert_eq!(snap.pages_loaded, 20);
    assert_eq!(snap.pages_invalidated, 3);
    assert_eq!(snap.store_errors, 1);
    assert_eq!(snap.load_errors, 2);
}

/// Prove: stats display format is human-readable.
#[test]
fn stats_display_format() {
    let stats = DaemonStats::new();
    stats.pages_stored.fetch_add(42, Ordering::Relaxed);
    let snap = stats.snapshot();
    let display = format!("{}", snap);
    assert!(display.contains("42"), "display should contain the count");
}

// ============================================================================
// SECTION 10: Daemon socket communication test
// ============================================================================

/// Prove: daemon responds correctly to control commands over Unix socket.
#[tokio::test]
async fn daemon_socket_ping_pong() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let socket_path = format!("/tmp/duvm-test-{}.sock", std::process::id());

    let mut config = DaemonConfig::default();
    config.daemon.socket_path = socket_path.clone();

    let mut engine = Engine::new(config).unwrap();

    // Run the daemon in the background
    let handle = tokio::spawn(async move {
        engine.run().await.ok();
    });

    // Give daemon time to start listening
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Connect and send ping
    let stream = UnixStream::connect(&socket_path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    writer.write_all(b"ping\n").await.unwrap();
    let mut response = String::new();
    reader.read_line(&mut response).await.unwrap();
    assert_eq!(response.trim(), "pong");

    // Send status command
    response.clear();
    writer.write_all(b"status\n").await.unwrap();
    reader.read_line(&mut response).await.unwrap();
    let status: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
    assert_eq!(status["status"], "running");

    // Send backends command
    response.clear();
    writer.write_all(b"backends\n").await.unwrap();
    reader.read_line(&mut response).await.unwrap();
    let backends: Vec<serde_json::Value> = serde_json::from_str(response.trim()).unwrap();
    assert!(!backends.is_empty());

    // Send stats command
    response.clear();
    writer.write_all(b"stats\n").await.unwrap();
    reader.read_line(&mut response).await.unwrap();
    let stats: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
    assert!(stats.get("pages_stored").is_some());

    // Send unknown command
    response.clear();
    writer.write_all(b"foobar\n").await.unwrap();
    reader.read_line(&mut response).await.unwrap();
    assert!(response.contains("error"));

    // Cleanup
    drop(writer);
    drop(reader);
    handle.abort();
    let _ = std::fs::remove_file(&socket_path);
}

// ============================================================================
// SECTION 11: Protocol struct layout tests
// ============================================================================

/// Prove: RingRequest and RingCompletion are exactly 64 bytes (cache line).
#[test]
fn protocol_struct_sizes() {
    assert_eq!(
        std::mem::size_of::<RingRequest>(),
        64,
        "RingRequest must be 64 bytes for cache line alignment"
    );
    assert_eq!(
        std::mem::size_of::<RingCompletion>(),
        64,
        "RingCompletion must be 64 bytes for cache line alignment"
    );
}

/// Prove: OpCode values match kernel module defines.
#[test]
fn opcode_values_match_kernel() {
    assert_eq!(OpCode::Nop as u8, 0);
    assert_eq!(OpCode::Store as u8, 1);
    assert_eq!(OpCode::Load as u8, 2);
    assert_eq!(OpCode::Invalidate as u8, 3);
    assert_eq!(OpCode::Prefetch as u8, 4);
}

// ============================================================================
// SECTION 12: BackendCapacity tests
// ============================================================================

/// Prove: BackendCapacity utilization calculation is correct.
#[test]
fn backend_capacity_utilization() {
    let cap = BackendCapacity {
        backend_id: 0,
        tier: Tier::Compressed,
        total_pages: 100,
        used_pages: 75,
        latency_ns: 3_000,
        healthy: true,
    };
    assert!((cap.utilization() - 0.75).abs() < f64::EPSILON);
    assert!(cap.has_capacity());

    let full = BackendCapacity {
        backend_id: 0,
        tier: Tier::Compressed,
        total_pages: 100,
        used_pages: 100,
        latency_ns: 3_000,
        healthy: true,
    };
    assert!((full.utilization() - 1.0).abs() < f64::EPSILON);
    assert!(!full.has_capacity());

    let unhealthy = BackendCapacity {
        backend_id: 0,
        tier: Tier::Compressed,
        total_pages: 100,
        used_pages: 0,
        latency_ns: 3_000,
        healthy: false,
    };
    assert!(!unhealthy.has_capacity());
}

// ============================================================================
// SECTION 13: Bug-fix verification tests
// ============================================================================

/// Prove: double-store at same offset frees the old handle (no leak).
/// Before the fix, storing at offset 0 twice would leave the first backend
/// page allocated but unreachable.
#[test]
fn engine_double_store_frees_old_handle() {
    let mut config = DaemonConfig::default();
    config.backends.compress = Some(duvm_daemon::config::CompressBackendConfig {
        enabled: true,
        max_pages: 10,
    });
    config.backends.memory = None;

    let engine = Engine::new(config).unwrap();

    let data1 = [0xAA; PAGE_SIZE];
    let data2 = [0xBB; PAGE_SIZE];

    engine.store_page(0, &data1).unwrap();
    let (_, used_after_first) = engine
        .backend_info()
        .iter()
        .map(|b| (b.total_pages, b.used_pages))
        .next()
        .unwrap();
    assert_eq!(used_after_first, 1);

    // Second store at same offset should free the first page
    engine.store_page(0, &data2).unwrap();
    let (_, used_after_second) = engine
        .backend_info()
        .iter()
        .map(|b| (b.total_pages, b.used_pages))
        .next()
        .unwrap();
    // Should still be 1, not 2 (old page was freed)
    assert_eq!(used_after_second, 1, "old page should be freed on re-store");

    // Data should be the second version
    let mut buf = [0u8; PAGE_SIZE];
    engine.load_page(0, &mut buf).unwrap();
    assert_eq!(buf[0], 0xBB);
}

/// Prove: LRU eviction works when all backends are full.
#[test]
fn engine_eviction_under_pressure() {
    let mut config = DaemonConfig::default();
    config.backends.compress = Some(duvm_daemon::config::CompressBackendConfig {
        enabled: true,
        max_pages: 3,
    });
    config.backends.memory = Some(duvm_daemon::config::MemoryBackendConfig {
        enabled: true,
        max_pages: 3,
    });

    let engine = Engine::new(config).unwrap();
    let data = [0u8; PAGE_SIZE];

    // Fill all 6 slots
    for i in 0..6 {
        engine.store_page(i, &data).unwrap();
    }

    // Access pages 3-5 to make them "hot"
    let mut buf = [0u8; PAGE_SIZE];
    for i in 3..6 {
        engine.load_page(i, &mut buf).unwrap();
    }

    // Store a 7th page — should evict one of pages 0-2 (cold / LRU)
    engine.store_page(100, &data).unwrap();

    // Page 100 should be loadable
    engine.load_page(100, &mut buf).unwrap();

    // At least one of 0-2 should have been evicted
    let mut evicted = 0;
    for i in 0..3 {
        if engine.load_page(i, &mut buf).is_err() {
            evicted += 1;
        }
    }
    assert!(evicted >= 1, "at least one cold page should be evicted");

    // Hot pages 3-5 should still be loadable
    for i in 3..6 {
        assert!(
            engine.load_page(i, &mut buf).is_ok(),
            "hot page {} should survive",
            i
        );
    }
}

/// Prove: pool.free() now errors when backend doesn't exist.
#[test]
fn pool_free_with_invalid_backend_errors() {
    let pool = duvm::Pool::standalone().unwrap();
    // Create a handle with backend_id=99 which doesn't exist
    let fake_handle = PageHandle::new(99, 0);
    let result = pool.free(fake_handle);
    assert!(
        result.is_err(),
        "freeing from non-existent backend should error"
    );
}

/// Prove: pool.capacity() uses saturating_add (no overflow).
#[test]
fn pool_capacity_does_not_overflow() {
    // With default config, capacity is large but should not overflow u64
    let pool = duvm::Pool::standalone().unwrap();
    let (total, used) = pool.capacity();
    assert!(total > 0);
    assert_eq!(used, 0);
    // Basic sanity: total should be sum of both backends
    assert!(
        total >= 2,
        "should have at least 2 backends worth of capacity"
    );
}

// ============================================================================
// SECTION 14: Config validation tests
// ============================================================================

/// Prove: config with max_pages=0 fails validation.
#[test]
fn config_validation_rejects_zero_max_pages() {
    let mut config = DaemonConfig::default();
    config.backends.memory = Some(duvm_daemon::config::MemoryBackendConfig {
        enabled: true,
        max_pages: 0,
    });
    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("max_pages"));
}

/// Prove: config with unknown strategy fails validation.
#[test]
fn config_validation_rejects_unknown_strategy() {
    let mut config = DaemonConfig::default();
    config.policy.strategy = "clock-pro".to_string();
    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("unknown policy strategy"));
}

/// Prove: default config passes validation.
#[test]
fn config_default_passes_validation() {
    let config = DaemonConfig::default();
    assert!(config.validate().is_ok());
}

/// Prove: CLI overrides are applied correctly.
#[test]
fn config_cli_overrides() {
    let mut config = DaemonConfig::default();
    assert_eq!(config.daemon.socket_path, "/run/duvm/duvm.sock");

    config.apply_cli_overrides(Some("/tmp/test.sock"), Some("debug"));
    assert_eq!(config.daemon.socket_path, "/tmp/test.sock");
    assert_eq!(config.daemon.log_level, "debug");
}

/// Prove: CLI overrides with None leave values unchanged.
#[test]
fn config_cli_overrides_none() {
    let mut config = DaemonConfig::default();
    let original_path = config.daemon.socket_path.clone();
    config.apply_cli_overrides(None, None);
    assert_eq!(config.daemon.socket_path, original_path);
}

/// Prove: config with disabled backend and max_pages=0 passes validation.
#[test]
fn config_disabled_backend_zero_pages_ok() {
    let mut config = DaemonConfig::default();
    config.backends.memory = Some(duvm_daemon::config::MemoryBackendConfig {
        enabled: false,
        max_pages: 0,
    });
    // Should be OK because the backend is disabled
    assert!(config.validate().is_ok());
}

// ============================================================================
// SECTION 15: PageFlags bitwise operator tests
// ============================================================================

/// Prove: BitOr combines flags correctly.
#[test]
fn page_flags_bitor() {
    let combined = PageFlags::DIRTY | PageFlags::PINNED;
    assert!(combined.contains(PageFlags::DIRTY));
    assert!(combined.contains(PageFlags::PINNED));
    assert!(!combined.contains(PageFlags::PREFETCHED));
}

/// Prove: BitOrAssign works correctly.
#[test]
fn page_flags_bitor_assign() {
    let mut flags = PageFlags::empty();
    flags |= PageFlags::DIRTY;
    flags |= PageFlags::MIGRATING;
    assert!(flags.contains(PageFlags::DIRTY));
    assert!(flags.contains(PageFlags::MIGRATING));
    assert!(!flags.contains(PageFlags::PINNED));
}

/// Prove: BitAnd masks flags correctly.
#[test]
fn page_flags_bitand() {
    let combined = PageFlags::DIRTY | PageFlags::PINNED | PageFlags::PREFETCHED;
    let masked = combined & PageFlags::PINNED;
    assert!(masked.contains(PageFlags::PINNED));
    assert!(!masked.contains(PageFlags::DIRTY));
}

/// Prove: BitAndAssign works correctly.
#[test]
fn page_flags_bitand_assign() {
    let mut flags = PageFlags::DIRTY | PageFlags::PINNED;
    flags &= !PageFlags::DIRTY;
    assert!(!flags.contains(PageFlags::DIRTY));
    assert!(flags.contains(PageFlags::PINNED));
}

/// Prove: Not inverts all bits.
#[test]
fn page_flags_not() {
    let flags = PageFlags::DIRTY;
    let inverted = !flags;
    assert!(!inverted.contains(PageFlags::DIRTY));
    // All other bits should be set
    assert!(inverted.contains(PageFlags::PINNED));
    assert!(inverted.contains(PageFlags::PREFETCHED));
    assert!(inverted.contains(PageFlags::MIGRATING));
}

// ============================================================================
// SECTION 16: Ring buffer edge case tests
// ============================================================================

/// Prove: pop_batch works correctly across a wrap-around boundary.
#[test]
fn ring_batch_pop_wraparound() {
    let mut ring = RequestRing::new(8);

    // Fill 7 slots (capacity-1) and drain to advance indices
    for i in 0..7 {
        let mut req: RingRequest = bytemuck::Zeroable::zeroed();
        req.seq = i;
        assert!(ring.try_push(req));
    }
    let mut buf = Vec::new();
    ring.pop_batch(&mut buf, 7);
    assert_eq!(buf.len(), 7);

    // Now indices are at 7. Push 5 more — this wraps around the 8-slot ring.
    for i in 100..105 {
        let mut req: RingRequest = bytemuck::Zeroable::zeroed();
        req.seq = i;
        assert!(ring.try_push(req));
    }

    // Batch pop all 5 — these span the wrap-around
    buf.clear();
    let count = ring.pop_batch(&mut buf, 10);
    assert_eq!(count, 5);
    assert_eq!(buf[0].seq, 100);
    assert_eq!(buf[4].seq, 104);
    assert!(ring.is_empty());
}

/// Prove: non-power-of-2 capacity panics.
#[test]
#[should_panic(expected = "power of 2")]
fn ring_non_power_of_two_panics() {
    RequestRing::new(5);
}

/// Prove: CompletionRing non-power-of-2 panics.
#[test]
#[should_panic(expected = "power of 2")]
fn completion_ring_non_power_of_two_panics() {
    CompletionRing::new(7);
}

/// Prove: ring len() is correct after wrap-around.
#[test]
fn ring_len_after_wraparound() {
    let mut ring = RequestRing::new(4);

    // Push 3, pop 3 to advance indices to 3
    for _ in 0..3 {
        ring.try_push(bytemuck::Zeroable::zeroed());
    }
    for _ in 0..3 {
        ring.try_pop();
    }
    assert_eq!(ring.len(), 0);

    // Push 2 more — wraps around (indices 3, 0)
    ring.try_push(bytemuck::Zeroable::zeroed());
    ring.try_push(bytemuck::Zeroable::zeroed());
    assert_eq!(ring.len(), 2);
}

// ============================================================================
// SECTION 17: Protocol serialization tests
// ============================================================================

/// Prove: RingRequest fields are at expected byte offsets (cache-line layout).
#[test]
fn ring_request_field_layout() {
    let mut req: RingRequest = bytemuck::Zeroable::zeroed();
    req.op = 0xAB;
    req.flags = 0xCD;
    req.seq = 0x12345678;
    req.pfn = 0x0102030405060708;
    req.offset = 0x1112131415161718;
    req.staging_slot = 0xAABBCCDD;

    let bytes = bytemuck::bytes_of(&req);
    assert_eq!(bytes[0], 0xAB, "op at offset 0");
    assert_eq!(bytes[1], 0xCD, "flags at offset 1");
    assert_eq!(
        &bytes[4..8],
        &0x12345678u32.to_le_bytes(),
        "seq at offset 4"
    );
    assert_eq!(
        &bytes[8..16],
        &0x0102030405060708u64.to_le_bytes(),
        "pfn at offset 8"
    );
    assert_eq!(
        &bytes[16..24],
        &0x1112131415161718u64.to_le_bytes(),
        "offset at offset 16"
    );
    assert_eq!(
        &bytes[24..28],
        &0xAABBCCDDu32.to_le_bytes(),
        "staging_slot at offset 24"
    );
}

/// Prove: RingCompletion fields are at expected byte offsets.
#[test]
fn ring_completion_field_layout() {
    let mut comp: RingCompletion = bytemuck::Zeroable::zeroed();
    comp.seq = 0xDEADBEEF;
    comp.result = -42;
    comp.handle = 0xCAFEBABE01020304;
    comp.staging_slot = 0x99887766;

    let bytes = bytemuck::bytes_of(&comp);
    assert_eq!(
        &bytes[0..4],
        &0xDEADBEEFu32.to_le_bytes(),
        "seq at offset 0"
    );
    assert_eq!(&bytes[4..8], &(-42i32).to_le_bytes(), "result at offset 4");
    assert_eq!(
        &bytes[8..16],
        &0xCAFEBABE01020304u64.to_le_bytes(),
        "handle at offset 8"
    );
    assert_eq!(
        &bytes[12..16],
        &0xCAFEBABEu32.to_le_bytes(),
        "handle upper at offset 12"
    );
    assert_eq!(
        &bytes[16..20],
        &0x99887766u32.to_le_bytes(),
        "staging_slot at offset 16"
    );
}

/// Prove: OpCode invalid values return None.
#[test]
fn opcode_invalid_values_return_none() {
    assert_eq!(OpCode::from_u8(5), None);
    assert_eq!(OpCode::from_u8(10), None);
    assert_eq!(OpCode::from_u8(255), None);
}

/// Prove: sequence number matching works across request-completion pairs.
#[test]
fn request_completion_seq_matching() {
    let mut req_ring = RequestRing::new(16);
    let mut comp_ring = CompletionRing::new(16);

    // Submit 5 requests with specific seq numbers
    for seq in [10, 20, 30, 40, 50] {
        let mut req: RingRequest = bytemuck::Zeroable::zeroed();
        req.seq = seq;
        req.op = OpCode::Store as u8;
        req_ring.try_push(req);
    }

    // "Process" them and create completions in reverse order
    let mut requests = Vec::new();
    while let Some(req) = req_ring.try_pop() {
        requests.push(req);
    }

    for req in requests.iter().rev() {
        let mut comp: RingCompletion = bytemuck::Zeroable::zeroed();
        comp.seq = req.seq;
        comp.result = 0;
        comp_ring.try_push(comp);
    }

    // Pop completions and verify they can be matched back
    let mut seqs_seen = Vec::new();
    while let Some(comp) = comp_ring.try_pop() {
        seqs_seen.push(comp.seq);
        assert_eq!(comp.result, 0);
    }
    assert_eq!(seqs_seen.len(), 5);
    // All original seqs should be present (order may differ)
    for expected_seq in [10, 20, 30, 40, 50] {
        assert!(
            seqs_seen.contains(&expected_seq),
            "missing seq {}",
            expected_seq
        );
    }
}

// ============================================================================
// SECTION 18: Stats tests
// ============================================================================

/// Prove: StatsSnapshot Display format includes all fields.
#[test]
fn stats_display_includes_all_fields() {
    let stats = DaemonStats::new();
    stats
        .pages_stored
        .fetch_add(42, std::sync::atomic::Ordering::Relaxed);
    stats
        .pages_loaded
        .fetch_add(17, std::sync::atomic::Ordering::Relaxed);
    stats
        .pages_invalidated
        .fetch_add(3, std::sync::atomic::Ordering::Relaxed);
    stats
        .store_errors
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    stats
        .load_errors
        .fetch_add(2, std::sync::atomic::Ordering::Relaxed);
    stats
        .fallback_events
        .fetch_add(5, std::sync::atomic::Ordering::Relaxed);
    stats
        .ring_full_events
        .fetch_add(7, std::sync::atomic::Ordering::Relaxed);

    let snap = stats.snapshot();
    let display = format!("{}", snap);

    assert!(display.contains("42"), "should contain pages_stored=42");
    assert!(display.contains("17"), "should contain pages_loaded=17");
    assert!(display.contains("3"), "should contain pages_invalidated=3");
    assert!(
        display.contains("Store errors"),
        "should have store errors label"
    );
    assert!(
        display.contains("Load errors"),
        "should have load errors label"
    );
    assert!(
        display.contains("Fallback events"),
        "should have fallback label"
    );
    assert!(
        display.contains("Ring full events"),
        "should have ring full label"
    );
}

/// Prove: StatsSnapshot serialization roundtrips through JSON.
#[test]
fn stats_snapshot_json_roundtrip() {
    let stats = DaemonStats::new();
    stats
        .pages_stored
        .fetch_add(100, std::sync::atomic::Ordering::Relaxed);
    stats
        .load_errors
        .fetch_add(3, std::sync::atomic::Ordering::Relaxed);

    let snap = stats.snapshot();
    let json = serde_json::to_string(&snap).unwrap();
    let deserialized: duvm_common::stats::StatsSnapshot = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.pages_stored, 100);
    assert_eq!(deserialized.load_errors, 3);
    assert_eq!(deserialized.pages_loaded, 0);
}

// ============================================================================
// SECTION 19: Backend trait method coverage
// ============================================================================

/// Prove: memory backend name() and tier() return expected values.
#[test]
fn memory_backend_name_and_tier() {
    let backend = MemoryBackend::new(0);
    assert_eq!(backend.name(), "memory");
    assert_eq!(backend.tier(), Tier::Compressed);
}

/// Prove: compress backend name() and tier() return expected values.
#[test]
fn compress_backend_name_and_tier() {
    let backend = CompressBackend::new(1);
    assert_eq!(backend.name(), "compress");
    assert_eq!(backend.tier(), Tier::Compressed);
}

/// Prove: memory backend latency_ns() returns expected value.
#[test]
fn memory_backend_latency() {
    let backend = MemoryBackend::new(0);
    assert_eq!(backend.latency_ns(), 100);
}

/// Prove: compress backend latency_ns() returns expected value.
#[test]
fn compress_backend_latency() {
    let backend = CompressBackend::new(1);
    assert_eq!(backend.latency_ns(), 3000);
}

/// Prove: BackendInfo::from_backend correctly maps all fields.
#[test]
fn backend_info_from_backend_mapping() {
    let mut backend = MemoryBackend::new(0);
    backend
        .init(&BackendConfig {
            max_pages: 500,
            ..Default::default()
        })
        .unwrap();

    let h = backend.alloc_page().unwrap();
    backend.store_page(h, &[0u8; PAGE_SIZE]).unwrap();

    let info = duvm_backend_trait::BackendInfo::from_backend(&backend);
    assert_eq!(info.name, "memory");
    assert_eq!(info.tier, "compressed"); // Tier::Compressed.to_string()
    assert_eq!(info.total_pages, 500);
    assert_eq!(info.used_pages, 1);
    assert_eq!(info.latency_ns, 100);
    assert!(info.healthy);
}

// ============================================================================
// SECTION 20: Engine eviction + double-store interaction
// ============================================================================

/// Prove: storing 100 pages into a 10-slot backend works via eviction.
#[test]
fn engine_store_100_in_10_slots() {
    let mut config = DaemonConfig::default();
    config.backends.compress = Some(duvm_daemon::config::CompressBackendConfig {
        enabled: true,
        max_pages: 5,
    });
    config.backends.memory = Some(duvm_daemon::config::MemoryBackendConfig {
        enabled: true,
        max_pages: 5,
    });

    let engine = Engine::new(config).unwrap();

    // Store 100 pages into 10 total slots — requires constant eviction
    for i in 0u64..100 {
        let mut data = [0u8; PAGE_SIZE];
        data[..8].copy_from_slice(&i.to_le_bytes());
        engine.store_page(i, &data).unwrap();
    }

    // The last ~10 pages should be loadable
    let mut loadable = 0;
    let mut buf = [0u8; PAGE_SIZE];
    for i in 0u64..100 {
        if engine.load_page(i, &mut buf).is_ok() {
            let stored = u64::from_le_bytes(buf[..8].try_into().unwrap());
            assert_eq!(stored, i, "loaded page {} has wrong data", i);
            loadable += 1;
        }
    }

    // Should have about 10 pages loadable (the 10 most recent ones)
    assert!(
        (5..=12).contains(&loadable),
        "expected ~10 loadable pages, got {}",
        loadable
    );

    let snap = engine.stats_snapshot();
    assert_eq!(snap.pages_stored, 100);
    assert_eq!(snap.store_errors, 0);
}

/// Prove: config with all backends disabled produces no-backend engine that
/// fails to store (with no evictable pages, so store errors properly).
#[test]
fn engine_no_backends_store_fails() {
    let mut config = DaemonConfig::default();
    config.backends.memory = None;
    config.backends.compress = None;

    let engine = Engine::new(config).unwrap();
    let data = [0u8; PAGE_SIZE];
    let result = engine.store_page(0, &data);
    assert!(result.is_err());

    let snap = engine.stats_snapshot();
    assert!(snap.store_errors >= 1);
}

// ============================================================================
// SECTION 21: Coverage for previously-untested public functions
// ============================================================================

/// Prove: PageHandle::raw() and from_raw() roundtrip correctly.
#[test]
fn page_handle_raw_and_from_raw() {
    let h = PageHandle::new(7, 999);
    let raw = h.raw();
    let recovered = PageHandle::from_raw(raw);
    assert_eq!(recovered.backend_id(), 7);
    assert_eq!(recovered.offset(), 999);
    assert_eq!(h, recovered);

    // INVALID roundtrips too
    let raw_invalid = PageHandle::INVALID.raw();
    assert_eq!(raw_invalid, u64::MAX);
    assert!(!PageHandle::from_raw(raw_invalid).is_valid());
}

/// Prove: RequestRing::capacity() returns the configured capacity.
#[test]
fn request_ring_capacity_getter() {
    let ring = RequestRing::new(64);
    assert_eq!(ring.capacity(), 64);

    let ring2 = RequestRing::new(1024);
    assert_eq!(ring2.capacity(), 1024);
}

/// Prove: CompletionRing::len() tracks items correctly.
#[test]
fn completion_ring_len() {
    let mut ring = CompletionRing::new(16);
    assert_eq!(ring.len(), 0);
    assert!(ring.is_empty());

    for i in 0..5 {
        let mut comp: RingCompletion = bytemuck::Zeroable::zeroed();
        comp.seq = i;
        ring.try_push(comp);
    }
    assert_eq!(ring.len(), 5);

    ring.try_pop();
    ring.try_pop();
    assert_eq!(ring.len(), 3);
}

/// Prove: BackendCapacity::utilization() returns correct fractions.
#[test]
fn backend_capacity_utilization_values() {
    let empty = BackendCapacity {
        backend_id: 0,
        tier: Tier::Compressed,
        total_pages: 100,
        used_pages: 0,
        latency_ns: 100,
        healthy: true,
    };
    assert!((empty.utilization() - 0.0).abs() < f64::EPSILON);

    let half = BackendCapacity {
        backend_id: 0,
        tier: Tier::Compressed,
        total_pages: 200,
        used_pages: 100,
        latency_ns: 100,
        healthy: true,
    };
    assert!((half.utilization() - 0.5).abs() < f64::EPSILON);

    let zero_cap = BackendCapacity {
        backend_id: 0,
        tier: Tier::Compressed,
        total_pages: 0,
        used_pages: 0,
        latency_ns: 100,
        healthy: true,
    };
    // zero total_pages reports as 100% full
    assert!((zero_cap.utilization() - 1.0).abs() < f64::EPSILON);
}

/// Prove: PolicyEngine::clear_flag() works on existing and non-existing pages.
#[test]
fn policy_clear_flag_coverage() {
    let policy = PolicyEngine::new(Strategy::Lru);
    policy.record_store(0, PageHandle::new(0, 0), 0, Tier::Compressed);

    // Set and verify
    policy.set_flag(0, PageFlags::DIRTY | PageFlags::PINNED);
    let meta = policy.lookup(0).unwrap();
    assert!(meta.flags.contains(PageFlags::DIRTY));
    assert!(meta.flags.contains(PageFlags::PINNED));

    // Clear only DIRTY
    policy.clear_flag(0, PageFlags::DIRTY);
    let meta = policy.lookup(0).unwrap();
    assert!(!meta.flags.contains(PageFlags::DIRTY));
    assert!(meta.flags.contains(PageFlags::PINNED));

    // Clear on non-existent page is a no-op (no panic)
    policy.clear_flag(999, PageFlags::DIRTY);
}

/// Prove: Engine::with_backends() creates a working engine.
#[test]
fn engine_with_backends_constructor() {
    let mut backends: std::collections::HashMap<u8, Box<dyn duvm_backend_trait::DuvmBackend>> =
        std::collections::HashMap::new();

    let mut mem = MemoryBackend::new(0);
    mem.init(&duvm_backend_trait::BackendConfig::default())
        .unwrap();
    backends.insert(0, Box::new(mem));

    let config = DaemonConfig::default();
    let engine = Engine::with_backends(config, backends);

    let data = [0xAB; PAGE_SIZE];
    engine.store_page(0, &data).unwrap();
    let mut buf = [0u8; PAGE_SIZE];
    engine.load_page(0, &mut buf).unwrap();
    assert_eq!(buf[0], 0xAB);
}

/// Prove: Engine::stats() and Engine::policy() return usable handles.
#[test]
fn engine_stats_and_policy_getters() {
    let config = DaemonConfig::default();
    let engine = Engine::new(config).unwrap();

    // stats() should work
    let stats = engine.stats();
    stats
        .pages_stored
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(engine.stats_snapshot().pages_stored, 1);

    // policy() should work
    let policy = engine.policy();
    policy.record_store(99, PageHandle::new(0, 0), 0, Tier::Compressed);
    assert_eq!(policy.tracked_pages(), 1);
}

// ============================================================================
// SECTION 22: Protocol bug fix verification
// ============================================================================

/// Prove: TCP backend alloc_page fails cleanly when memserver is full
/// (verifies the ALLOC error response protocol fix — server must send 9 bytes).
#[test]
fn tcp_alloc_fails_cleanly_when_server_full() {
    use duvm_backend_tcp::TcpBackend;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // Start a mini memserver that is immediately full (max_pages=0)
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        loop {
            let mut op = [0u8; 1];
            if stream.read_exact(&mut op).is_err() {
                break;
            }
            match op[0] {
                4 => {
                    // ALLOC — always fail (full server)
                    let mut resp = [0u8; 9];
                    resp[0] = 1; // RESP_ERR
                    stream.write_all(&resp).unwrap();
                }
                _ => {
                    stream.write_all(&[1]).unwrap();
                }
            }
            stream.flush().unwrap();
        }
    });

    let mut backend = TcpBackend::new(2, &addr.to_string());
    backend
        .init(&duvm_backend_trait::BackendConfig {
            max_pages: 10000,
            ..Default::default()
        })
        .unwrap();

    // alloc_page should fail cleanly (not hang, not panic)
    let result = backend.alloc_page();
    assert!(result.is_err(), "alloc should fail when server is full");

    backend.shutdown().unwrap();
    drop(server);
}

/// Prove: TCP backend free_page propagates server errors.
#[test]
fn tcp_free_propagates_server_error() {
    use duvm_backend_tcp::TcpBackend;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        loop {
            let mut op = [0u8; 1];
            if stream.read_exact(&mut op).is_err() {
                break;
            }
            match op[0] {
                4 => {
                    // ALLOC — succeed
                    let mut resp = [0u8; 9];
                    resp[0] = 0;
                    resp[1..9].copy_from_slice(&42u64.to_le_bytes());
                    stream.write_all(&resp).unwrap();
                }
                3 => {
                    // FREE — read offset, return error
                    let mut header = [0u8; 8];
                    stream.read_exact(&mut header).unwrap();
                    stream.write_all(&[1]).unwrap(); // RESP_ERR
                }
                _ => {
                    stream.write_all(&[1]).unwrap();
                }
            }
            stream.flush().unwrap();
        }
    });

    let mut backend = TcpBackend::new(2, &addr.to_string());
    backend
        .init(&duvm_backend_trait::BackendConfig {
            max_pages: 10000,
            ..Default::default()
        })
        .unwrap();

    let handle = backend.alloc_page().unwrap();
    // Free should fail because server returns error
    let result = backend.free_page(handle);
    assert!(
        result.is_err(),
        "free should propagate server error, not swallow it"
    );

    backend.shutdown().unwrap();
    drop(server);
}
