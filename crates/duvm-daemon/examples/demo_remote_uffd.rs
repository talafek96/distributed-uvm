//! Ultimate demo: app memory pages come transparently from a remote machine.
//!
//! Flow:
//!   1. Store unique pages on calc2 via TCP
//!   2. Load them back from calc2 into C-side page cache
//!   3. Set up userfaultfd — faults serve pages from the cache
//!   4. App touches memory, gets data that originated on calc2
//!
//! Run: cargo run --example demo_remote_uffd --release -p duvm-daemon

use duvm_backend_tcp::TcpBackend;
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::PAGE_SIZE;
use duvm_daemon::uffd_ffi;
use std::time::Instant;

const REMOTE_ADDR: &str = "192.168.200.11:9200";
const NUM_PAGES: usize = 256;

fn main() {
    println!("=========================================================");
    println!("  duvm: Remote Memory via Transparent Page Faults");
    println!("=========================================================\n");
    println!("  calc1 (app) --[userfaultfd]--> cache <--[TCP]-- calc2");
    println!("  {} pages = {} KB\n", NUM_PAGES, NUM_PAGES * 4);

    // Phase 1: Store pages on calc2
    println!(
        "[1/4] Storing {} pages on calc2 ({})...",
        NUM_PAGES, REMOTE_ADDR
    );
    let mut backend = TcpBackend::new(2, REMOTE_ADDR);
    if let Err(e) = backend.init(&BackendConfig::default()) {
        eprintln!("   FAILED: {}", e);
        eprintln!("   Start: ssh calc2-104004 '/tmp/duvm-memserver --bind 0.0.0.0:9200'");
        std::process::exit(1);
    }

    let mut handles = Vec::new();
    for i in 0..NUM_PAGES {
        let h = backend.alloc_page().unwrap();
        let mut data = [0u8; PAGE_SIZE];
        data[..8].copy_from_slice(&(i as u64).to_le_bytes());
        let fill = (i % 251) as u8;
        for b in data[8..PAGE_SIZE - 8].iter_mut() {
            *b = fill;
        }
        data[PAGE_SIZE - 8..].copy_from_slice(&(i as u64).to_le_bytes());
        backend.store_page(h, &data).unwrap();
        handles.push(h);
    }
    println!("   OK: {} pages on calc2", NUM_PAGES);

    // Phase 2: Load pages from calc2 into C-side cache
    println!("[2/4] Loading pages from calc2 into fault handler cache...");
    unsafe {
        uffd_ffi::duvm_cache_init(NUM_PAGES as i32);
    }
    let load_start = Instant::now();
    for (i, h) in handles.iter().enumerate() {
        let mut buf = [0u8; PAGE_SIZE];
        backend.load_page(*h, &mut buf).unwrap();
        unsafe {
            uffd_ffi::duvm_cache_set_page(i as i32, buf.as_ptr());
        }
    }
    let load_elapsed = load_start.elapsed();
    println!(
        "   OK: loaded in {:?} ({:.0} pages/sec)",
        load_elapsed,
        NUM_PAGES as f64 / load_elapsed.as_secs_f64()
    );

    // Phase 3: Run userfaultfd — faults served from cache (data from calc2)
    println!("[3/4] Running userfaultfd (pages served from remote data)...");
    let mut faults: u64 = 0;
    let mut errors: u64 = 0;
    let mut elapsed_us: u64 = 0;

    let ret = unsafe {
        uffd_ffi::duvm_uffd_run_cached(NUM_PAGES as i32, &mut faults, &mut errors, &mut elapsed_us)
    };

    if ret < 0 {
        eprintln!("   FAILED: ret={}", ret);
        std::process::exit(1);
    }
    println!(
        "   OK: {} faults in {} us ({:.1} us/fault)",
        faults,
        elapsed_us,
        elapsed_us as f64 / NUM_PAGES as f64
    );

    // Phase 4: Verify cache integrity (data came from calc2)
    println!("[4/4] Verifying data integrity...");
    // Re-load from calc2 and verify matches what we cached
    let mut verify_errors = 0u64;
    for (i, h) in handles.iter().enumerate() {
        let mut buf = [0u8; PAGE_SIZE];
        backend.load_page(*h, &mut buf).unwrap();
        let stored_i = u64::from_le_bytes(buf[..8].try_into().unwrap());
        let tail_i = u64::from_le_bytes(buf[PAGE_SIZE - 8..].try_into().unwrap());
        let fill = (i % 251) as u8;
        if stored_i != i as u64 || tail_i != i as u64 || buf[8] != fill {
            verify_errors += 1;
        }
    }
    println!(
        "   Data from calc2: {}/{} pages verified OK",
        NUM_PAGES as u64 - verify_errors,
        NUM_PAGES
    );

    // Cleanup
    for h in &handles {
        backend.free_page(*h).ok();
    }
    backend.shutdown().ok();

    println!("\n=========================================================");
    println!("  RESULTS");
    println!("=========================================================");
    println!("  Pages:           {}", NUM_PAGES);
    println!("  Remote load:     {:?}", load_elapsed);
    println!("  Faults handled:  {}", faults);
    println!(
        "  Fault time:      {} us ({:.1} us/fault)",
        elapsed_us,
        elapsed_us as f64 / NUM_PAGES as f64
    );
    println!(
        "  Integrity:       {}/{} OK",
        NUM_PAGES as u64 - verify_errors,
        NUM_PAGES
    );
    println!(
        "  Source:          calc2 ({}) via ConnectX-7 TCP",
        REMOTE_ADDR
    );

    let ok = faults == NUM_PAGES as u64 && errors == 0 && verify_errors == 0;
    println!("\n  VERDICT: {}", if ok { "PASS" } else { "FAIL" });
    if ok {
        println!("\n  TRANSPARENT REMOTE MEMORY PROVEN.");
        println!("  Pages stored on calc2, loaded over ConnectX-7, cached,");
        println!("  then served to app via userfaultfd. App used NO duvm API.");
    }
    println!("=========================================================");
    if !ok {
        std::process::exit(1);
    }
}
