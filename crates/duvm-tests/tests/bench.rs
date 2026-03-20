//! Performance benchmarks for duvm components.
//!
//! Run: cargo test --test bench --release -- --nocapture
//!
//! These are not microbenchmarks (no Criterion) — just throughput measurements
//! to establish baselines and catch regressions.

use duvm_backend_compress::CompressBackend;
use duvm_backend_memory::MemoryBackend;
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::PAGE_SIZE;
use duvm_common::protocol::{OpCode, RingRequest};
use duvm_common::ring::RequestRing;
use std::time::Instant;

const BENCH_PAGES: u64 = 10_000;
const RING_OPS: u32 = 100_000;

fn make_test_page(i: u64) -> [u8; PAGE_SIZE] {
    let mut data = [0u8; PAGE_SIZE];
    data[..8].copy_from_slice(&i.to_le_bytes());
    // Fill with a compressible but non-trivial pattern
    for chunk in data[8..].chunks_exact_mut(8) {
        let val = i
            .wrapping_mul(0x517cc1b727220a95)
            .wrapping_add(0x6c62272e07bb0142);
        chunk.copy_from_slice(&val.to_le_bytes());
    }
    data
}

#[test]
fn bench_memory_backend_throughput() {
    let mut backend = MemoryBackend::new(0);
    backend
        .init(&BackendConfig {
            max_pages: BENCH_PAGES + 100,
            ..Default::default()
        })
        .unwrap();

    // Benchmark store
    let mut handles = Vec::with_capacity(BENCH_PAGES as usize);
    let start = Instant::now();
    for i in 0..BENCH_PAGES {
        let h = backend.alloc_page().unwrap();
        let data = make_test_page(i);
        backend.store_page(h, &data).unwrap();
        handles.push(h);
    }
    let store_elapsed = start.elapsed();

    // Benchmark load
    let start = Instant::now();
    let mut buf = [0u8; PAGE_SIZE];
    for h in &handles {
        backend.load_page(*h, &mut buf).unwrap();
    }
    let load_elapsed = start.elapsed();

    let store_rate = BENCH_PAGES as f64 / store_elapsed.as_secs_f64();
    let load_rate = BENCH_PAGES as f64 / load_elapsed.as_secs_f64();
    let store_bw = (BENCH_PAGES as f64 * PAGE_SIZE as f64) / store_elapsed.as_secs_f64() / 1e9;
    let load_bw = (BENCH_PAGES as f64 * PAGE_SIZE as f64) / load_elapsed.as_secs_f64() / 1e9;

    println!("\n=== Memory Backend Benchmark ({} pages) ===", BENCH_PAGES);
    println!(
        "  Store: {:.0} pages/sec ({:.2} GB/s), avg {:.1} us/page",
        store_rate,
        store_bw,
        store_elapsed.as_micros() as f64 / BENCH_PAGES as f64
    );
    println!(
        "  Load:  {:.0} pages/sec ({:.2} GB/s), avg {:.1} us/page",
        load_rate,
        load_bw,
        load_elapsed.as_micros() as f64 / BENCH_PAGES as f64
    );

    // Sanity: even in debug mode, in-memory should manage >10k pages/sec
    assert!(
        store_rate > 10_000.0,
        "store too slow: {:.0} pages/sec",
        store_rate
    );
    assert!(
        load_rate > 10_000.0,
        "load too slow: {:.0} pages/sec",
        load_rate
    );
}

#[test]
fn bench_compress_backend_throughput() {
    let mut backend = CompressBackend::new(1);
    backend
        .init(&BackendConfig {
            max_pages: BENCH_PAGES + 100,
            ..Default::default()
        })
        .unwrap();

    // Benchmark store (includes LZ4 compression)
    let mut handles = Vec::with_capacity(BENCH_PAGES as usize);
    let start = Instant::now();
    for i in 0..BENCH_PAGES {
        let h = backend.alloc_page().unwrap();
        let data = make_test_page(i);
        backend.store_page(h, &data).unwrap();
        handles.push(h);
    }
    let store_elapsed = start.elapsed();

    // Benchmark load (includes LZ4 decompression)
    let start = Instant::now();
    let mut buf = [0u8; PAGE_SIZE];
    for h in &handles {
        backend.load_page(*h, &mut buf).unwrap();
    }
    let load_elapsed = start.elapsed();

    let store_rate = BENCH_PAGES as f64 / store_elapsed.as_secs_f64();
    let load_rate = BENCH_PAGES as f64 / load_elapsed.as_secs_f64();
    let store_bw = (BENCH_PAGES as f64 * PAGE_SIZE as f64) / store_elapsed.as_secs_f64() / 1e9;
    let load_bw = (BENCH_PAGES as f64 * PAGE_SIZE as f64) / load_elapsed.as_secs_f64() / 1e9;

    println!(
        "\n=== Compress Backend Benchmark ({} pages) ===",
        BENCH_PAGES
    );
    println!(
        "  Store: {:.0} pages/sec ({:.2} GB/s), avg {:.1} us/page",
        store_rate,
        store_bw,
        store_elapsed.as_micros() as f64 / BENCH_PAGES as f64
    );
    println!(
        "  Load:  {:.0} pages/sec ({:.2} GB/s), avg {:.1} us/page",
        load_rate,
        load_bw,
        load_elapsed.as_micros() as f64 / BENCH_PAGES as f64
    );
    println!("  Compression ratio: {:.2}x", backend.compression_ratio());

    // Sanity: even in debug mode with LZ4, should manage >5k pages/sec
    assert!(
        store_rate > 5_000.0,
        "store too slow: {:.0} pages/sec",
        store_rate
    );
    assert!(
        load_rate > 5_000.0,
        "load too slow: {:.0} pages/sec",
        load_rate
    );
}

#[test]
fn bench_ring_buffer_throughput() {
    let mut ring = RequestRing::new(4096);

    // Benchmark push/pop cycles
    let start = Instant::now();
    for i in 0..RING_OPS {
        let mut req: RingRequest = bytemuck::Zeroable::zeroed();
        req.op = OpCode::Store as u8;
        req.seq = i;
        assert!(ring.try_push(req));
        ring.try_pop().unwrap();
    }
    let elapsed = start.elapsed();

    let rate = RING_OPS as f64 / elapsed.as_secs_f64();
    println!("\n=== Ring Buffer Benchmark ({} ops) ===", RING_OPS);
    println!(
        "  Push+Pop: {:.0} ops/sec, avg {:.0} ns/op",
        rate,
        elapsed.as_nanos() as f64 / RING_OPS as f64
    );

    // Ring buffer should be very fast: > 1M ops/sec
    assert!(rate > 1_000_000.0, "ring too slow: {:.0} ops/sec", rate);
}

#[test]
fn bench_ring_buffer_batch() {
    let mut ring = RequestRing::new(4096);

    let batch_size = 64;
    let batches = RING_OPS / batch_size;

    let start = Instant::now();
    for _ in 0..batches {
        // Push a batch
        for j in 0..batch_size {
            let mut req: RingRequest = bytemuck::Zeroable::zeroed();
            req.op = OpCode::Load as u8;
            req.seq = j;
            ring.try_push(req);
        }
        // Pop batch
        let mut buf = Vec::with_capacity(batch_size as usize);
        ring.pop_batch(&mut buf, batch_size as usize);
    }
    let elapsed = start.elapsed();

    let total_ops = (batches * batch_size) as f64;
    let rate = total_ops / elapsed.as_secs_f64();
    println!(
        "\n=== Ring Buffer Batch Benchmark (batch={}) ===",
        batch_size
    );
    println!(
        "  Batched: {:.0} ops/sec, avg {:.0} ns/op",
        rate,
        elapsed.as_nanos() as f64 / total_ops
    );
}

#[test]
fn bench_pool_throughput() {
    let pool = duvm::Pool::standalone().unwrap();

    // Store
    let mut handles = Vec::with_capacity(BENCH_PAGES as usize);
    let start = Instant::now();
    for i in 0..BENCH_PAGES {
        let data = make_test_page(i);
        handles.push(pool.store(&data).unwrap());
    }
    let store_elapsed = start.elapsed();

    // Load
    let start = Instant::now();
    for h in &handles {
        let _ = pool.load(*h).unwrap();
    }
    let load_elapsed = start.elapsed();

    let store_rate = BENCH_PAGES as f64 / store_elapsed.as_secs_f64();
    let load_rate = BENCH_PAGES as f64 / load_elapsed.as_secs_f64();

    println!("\n=== Pool (libduvm) Benchmark ({} pages) ===", BENCH_PAGES);
    println!(
        "  Store: {:.0} pages/sec, avg {:.1} us/page",
        store_rate,
        store_elapsed.as_micros() as f64 / BENCH_PAGES as f64
    );
    println!(
        "  Load:  {:.0} pages/sec, avg {:.1} us/page",
        load_rate,
        load_elapsed.as_micros() as f64 / BENCH_PAGES as f64
    );
}
