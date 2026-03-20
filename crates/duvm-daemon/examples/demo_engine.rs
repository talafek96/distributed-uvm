//! End-to-end demo: proves the full duvm engine data path works.
//!
//! This demo exercises:
//!   Application data → Engine → Policy → Backend (compress/memory) → Load back
//!
//! Run: cargo run --example demo_engine

use duvm_common::page::PAGE_SIZE;
use std::time::Instant;

fn main() {
    println!("=== duvm Engine Demo ===\n");

    // Create engine with default config (memory + compress backends)
    let config = duvm_daemon::config::DaemonConfig::default();
    let engine = duvm_daemon::engine::Engine::new(config).expect("Failed to create engine");

    let num_pages = 1000u64;

    // Phase 1: Store pages with unique patterns
    println!("[1/4] Storing {} pages...", num_pages);
    let start = Instant::now();

    let mut offsets = Vec::new();
    for i in 0..num_pages {
        let mut data = [0u8; PAGE_SIZE];
        // Write page number at start and end for verification
        data[..8].copy_from_slice(&i.to_le_bytes());
        let marker = format!("duvm-page-{:06}", i);
        data[100..100 + marker.len()].copy_from_slice(marker.as_bytes());
        data[4088..4096].copy_from_slice(&i.to_le_bytes());

        engine.store_page(i, &data).expect("store failed");
        offsets.push(i);
    }

    let store_elapsed = start.elapsed();
    println!(
        "   Stored {} pages in {:?} ({:.0} pages/sec)",
        num_pages,
        store_elapsed,
        num_pages as f64 / store_elapsed.as_secs_f64()
    );

    // Phase 2: Load all pages and verify
    println!("[2/4] Loading and verifying {} pages...", num_pages);
    let start = Instant::now();

    let mut errors = 0;
    for i in 0..num_pages {
        let mut buf = [0u8; PAGE_SIZE];
        engine.load_page(i, &mut buf).expect("load failed");

        let stored_i = u64::from_le_bytes(buf[..8].try_into().unwrap());
        let tail_i = u64::from_le_bytes(buf[4088..4096].try_into().unwrap());
        let marker = format!("duvm-page-{:06}", i);

        if stored_i != i || tail_i != i || &buf[100..100 + marker.len()] != marker.as_bytes() {
            eprintln!("   ERROR: page {} content mismatch!", i);
            errors += 1;
        }
    }

    let load_elapsed = start.elapsed();
    if errors == 0 {
        println!(
            "   All {} pages verified OK in {:?} ({:.0} pages/sec)",
            num_pages,
            load_elapsed,
            num_pages as f64 / load_elapsed.as_secs_f64()
        );
    } else {
        println!("   FAILED: {} pages had content errors", errors);
    }

    // Phase 3: Invalidate half the pages
    let invalidate_count = num_pages / 2;
    println!("[3/4] Invalidating {} pages...", invalidate_count);
    let start = Instant::now();

    for i in 0..invalidate_count {
        engine.invalidate_page(i).expect("invalidate failed");
    }

    let inv_elapsed = start.elapsed();
    println!(
        "   Invalidated {} pages in {:?}",
        invalidate_count, inv_elapsed
    );

    // Verify invalidated pages are gone
    let mut gone_count = 0;
    for i in 0..invalidate_count {
        let mut buf = [0u8; PAGE_SIZE];
        if engine.load_page(i, &mut buf).is_err() {
            gone_count += 1;
        }
    }
    println!(
        "   Verified: {}/{} invalidated pages correctly return errors",
        gone_count, invalidate_count
    );

    // Verify remaining pages still work
    let mut remaining_ok = 0;
    for i in invalidate_count..num_pages {
        let mut buf = [0u8; PAGE_SIZE];
        if engine.load_page(i, &mut buf).is_ok() {
            let stored_i = u64::from_le_bytes(buf[..8].try_into().unwrap());
            if stored_i == i {
                remaining_ok += 1;
            }
        }
    }
    println!(
        "   Verified: {}/{} remaining pages still accessible and correct",
        remaining_ok,
        num_pages - invalidate_count
    );

    // Phase 4: Print stats
    println!("[4/4] Statistics:");
    let snap = engine.stats_snapshot();
    println!("   {}", snap);

    // Backend info
    println!("\n   Backends:");
    for info in engine.backend_info() {
        println!(
            "     {} ({}): {}/{} pages, latency={}ns, healthy={}",
            info.name, info.tier, info.used_pages, info.total_pages, info.latency_ns, info.healthy
        );
    }

    // Final verdict
    println!("\n=== Result ===");
    let success = errors == 0
        && gone_count == invalidate_count
        && remaining_ok == num_pages - invalidate_count;

    if success {
        println!("PASS: All engine operations verified successfully.");
        println!(
            "  - {} pages stored and loaded through compression backend",
            num_pages
        );
        println!(
            "  - {} pages invalidated and confirmed removed",
            invalidate_count
        );
        println!(
            "  - {} pages still accessible after partial invalidation",
            remaining_ok
        );
    } else {
        println!("FAIL: Some operations did not verify correctly.");
        std::process::exit(1);
    }
}
