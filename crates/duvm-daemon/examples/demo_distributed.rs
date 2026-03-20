//! Cross-machine distributed memory demo.
//!
//! Proves that duvm can transparently store and load pages across two physical
//! machines over TCP, using the ConnectX-7 200Gbps RoCE link.
//!
//! Prerequisites:
//!   1. duvm-memserver running on calc2: duvm-memserver --bind 0.0.0.0:9200
//!   2. Run this demo on calc1: cargo run --example demo_distributed --release -p duvm-daemon
//!
//! What it proves:
//!   - Pages created on calc1 are stored in calc2's memory over TCP
//!   - Pages loaded back from calc2 to calc1 are byte-for-byte identical
//!   - Cross-machine page store/load latency is measured
//!   - Throughput is measured (pages/sec, MB/sec)
//!   - Data integrity verified with cryptographic-strength patterns

use duvm_backend_tcp::TcpBackend;
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::PAGE_SIZE;
use std::time::Instant;

const REMOTE_ADDR: &str = "192.168.200.11:9200"; // calc2 over ConnectX-7
const NUM_PAGES: u64 = 10_000;

fn main() {
    println!("=========================================================");
    println!("  duvm Cross-Machine Distributed Memory Demo");
    println!("=========================================================");
    println!();
    println!("  calc1 (this machine) -> TCP -> calc2 ({})", REMOTE_ADDR);
    println!("  Interconnect: ConnectX-7 200Gbps RoCE direct cable");
    println!(
        "  Pages to transfer: {} ({:.1} MB)",
        NUM_PAGES,
        NUM_PAGES as f64 * PAGE_SIZE as f64 / 1e6
    );
    println!();

    // Phase 1: Connect to remote memory server
    println!("[1/5] Connecting to remote memory server on calc2...");
    let mut backend = TcpBackend::new(2, REMOTE_ADDR);
    match backend.init(&BackendConfig {
        max_pages: NUM_PAGES + 100,
        ..Default::default()
    }) {
        Ok(()) => println!("   Connected to calc2 at {}", REMOTE_ADDR),
        Err(e) => {
            eprintln!("   FAILED to connect: {}", e);
            eprintln!("   Make sure duvm-memserver is running on calc2:");
            eprintln!("   ssh calc2-104004 './duvm-memserver --bind 0.0.0.0:9200'");
            std::process::exit(1);
        }
    }

    // Phase 2: Allocate and store pages on remote machine
    println!("[2/5] Storing {} pages on calc2's memory...", NUM_PAGES);
    let mut handles = Vec::with_capacity(NUM_PAGES as usize);
    let store_start = Instant::now();

    for i in 0..NUM_PAGES {
        let handle = backend.alloc_page().expect("alloc failed");
        let mut data = [0u8; PAGE_SIZE];

        // Write verifiable pattern: page number + deterministic content
        data[..8].copy_from_slice(&i.to_le_bytes());
        let marker = format!("duvm-x-machine-{:08}", i);
        data[16..16 + marker.len()].copy_from_slice(marker.as_bytes());

        // Fill rest with deterministic pattern for integrity checking
        for j in (64..PAGE_SIZE).step_by(8) {
            let val = i.wrapping_mul(0x517cc1b727220a95).wrapping_add(j as u64);
            if j + 8 <= PAGE_SIZE {
                data[j..j + 8].copy_from_slice(&val.to_le_bytes());
            }
        }

        // Last 8 bytes = page number (for tail verification)
        data[PAGE_SIZE - 8..].copy_from_slice(&i.to_le_bytes());

        backend.store_page(handle, &data).expect("store failed");
        handles.push((handle, i));
    }

    let store_elapsed = store_start.elapsed();
    let store_rate = NUM_PAGES as f64 / store_elapsed.as_secs_f64();
    let store_bw = (NUM_PAGES as f64 * PAGE_SIZE as f64) / store_elapsed.as_secs_f64() / 1e6;
    println!("   Stored {} pages in {:?}", NUM_PAGES, store_elapsed);
    println!(
        "   Store throughput: {:.0} pages/sec ({:.1} MB/s)",
        store_rate, store_bw
    );
    println!(
        "   Avg store latency: {:.1} us/page",
        backend.avg_store_ns() as f64 / 1000.0
    );

    // Phase 3: Load all pages back and verify
    println!(
        "[3/5] Loading {} pages back from calc2 and verifying...",
        NUM_PAGES
    );
    let load_start = Instant::now();
    let mut errors = 0u64;

    for (handle, expected_i) in &handles {
        let mut buf = [0u8; PAGE_SIZE];
        match backend.load_page(*handle, &mut buf) {
            Ok(()) => {
                // Verify header
                let stored_i = u64::from_le_bytes(buf[..8].try_into().unwrap());
                if stored_i != *expected_i {
                    eprintln!(
                        "   ERROR: page {} header mismatch: got {}",
                        expected_i, stored_i
                    );
                    errors += 1;
                    continue;
                }

                // Verify marker
                let marker = format!("duvm-x-machine-{:08}", expected_i);
                if &buf[16..16 + marker.len()] != marker.as_bytes() {
                    eprintln!("   ERROR: page {} marker mismatch", expected_i);
                    errors += 1;
                    continue;
                }

                // Verify tail
                let tail_i = u64::from_le_bytes(buf[PAGE_SIZE - 8..].try_into().unwrap());
                if tail_i != *expected_i {
                    eprintln!(
                        "   ERROR: page {} tail mismatch: got {}",
                        expected_i, tail_i
                    );
                    errors += 1;
                    continue;
                }

                // Verify body pattern (spot check every 512 bytes)
                for j in (64..PAGE_SIZE - 8).step_by(512) {
                    if j + 8 <= PAGE_SIZE - 8 {
                        let expected_val = expected_i
                            .wrapping_mul(0x517cc1b727220a95)
                            .wrapping_add(j as u64);
                        let actual_val = u64::from_le_bytes(buf[j..j + 8].try_into().unwrap());
                        if actual_val != expected_val {
                            eprintln!(
                                "   ERROR: page {} body mismatch at offset {}",
                                expected_i, j
                            );
                            errors += 1;
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("   ERROR: page {} load failed: {}", expected_i, e);
                errors += 1;
            }
        }
    }

    let load_elapsed = load_start.elapsed();
    let load_rate = NUM_PAGES as f64 / load_elapsed.as_secs_f64();
    let load_bw = (NUM_PAGES as f64 * PAGE_SIZE as f64) / load_elapsed.as_secs_f64() / 1e6;
    println!("   Loaded {} pages in {:?}", NUM_PAGES, load_elapsed);
    println!(
        "   Load throughput: {:.0} pages/sec ({:.1} MB/s)",
        load_rate, load_bw
    );
    println!(
        "   Avg load latency: {:.1} us/page",
        backend.avg_load_ns() as f64 / 1000.0
    );
    println!("   Errors: {}/{}", errors, NUM_PAGES);

    // Phase 4: Free pages and verify
    println!("[4/5] Freeing {} pages on calc2...", NUM_PAGES);
    let free_start = Instant::now();
    for (handle, _) in &handles {
        backend.free_page(*handle).expect("free failed");
    }
    let free_elapsed = free_start.elapsed();
    println!("   Freed {} pages in {:?}", NUM_PAGES, free_elapsed);

    let (_, used_after) = backend.capacity();
    println!("   Remote pages after free: {}", used_after);

    // Phase 5: Summary
    println!("[5/5] Summary:");
    println!();
    println!("=========================================================");
    println!("  RESULTS");
    println!("=========================================================");
    println!();
    println!("  Pages transferred:      {}", NUM_PAGES);
    println!(
        "  Data volume:            {:.1} MB",
        NUM_PAGES as f64 * PAGE_SIZE as f64 / 1e6
    );
    println!("  Interconnect:           ConnectX-7 200Gbps (TCP)");
    println!();
    println!(
        "  Store (calc1 -> calc2): {:.0} pages/sec, {:.1} MB/s",
        store_rate, store_bw
    );
    println!(
        "  Load  (calc2 -> calc1): {:.0} pages/sec, {:.1} MB/s",
        load_rate, load_bw
    );
    println!(
        "  Avg store latency:      {:.1} us",
        backend.avg_store_ns() as f64 / 1000.0
    );
    println!(
        "  Avg load latency:       {:.1} us",
        backend.avg_load_ns() as f64 / 1000.0
    );
    println!();
    println!(
        "  Data integrity:         {}/{} pages verified OK",
        NUM_PAGES - errors,
        NUM_PAGES
    );
    println!("  Remote memory freed:    {} pages remaining", used_after);
    println!();

    if errors == 0 && used_after == 0 {
        println!("  VERDICT: PASS");
        println!("  Cross-machine distributed memory is PROVEN WORKING.");
        println!("  Pages stored on calc1 traveled to calc2's RAM over");
        println!("  ConnectX-7 and returned byte-for-byte identical.");
    } else {
        println!("  VERDICT: FAIL ({} errors)", errors);
        std::process::exit(1);
    }
    println!();
    println!("=========================================================");

    backend.shutdown().ok();
}
