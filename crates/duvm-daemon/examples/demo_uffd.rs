//! End-to-end userfaultfd demo: transparent page fault handling.
//!
//! Proves that page faults can be intercepted and resolved transparently.
//! The entire mmap/register/handle/verify cycle runs in C (via uffd_helper.c)
//! to avoid Rust runtime interference with userfaultfd on aarch64.
//!
//! Run: cargo run --example demo_uffd --release -p duvm-daemon

use duvm_daemon::uffd_ffi::duvm_uffd_run_demo;

const NUM_PAGES: i32 = 256;

fn main() {
    println!("=== duvm Userfaultfd Transparent Page Fault Demo ===\n");
    println!("  Pages: {}", NUM_PAGES);
    println!("  Region: {} KB\n", NUM_PAGES as u64 * 4);

    let mut faults: u64 = 0;
    let mut errors: u64 = 0;
    let mut elapsed_us: u64 = 0;

    println!("Running userfaultfd demo (mmap + register + fault handler + verify)...\n");

    let ret = unsafe { duvm_uffd_run_demo(NUM_PAGES, &mut faults, &mut errors, &mut elapsed_us) };

    println!("=========================================================");
    println!("  RESULTS");
    println!("=========================================================");

    if ret < 0 {
        println!(
            "  FAILED at step {} (create={} api={} mmap={} register={})",
            -ret,
            ret == -1,
            ret == -2,
            ret == -3,
            ret == -4
        );
        if ret == -1 {
            println!("  Run: sudo sysctl -w vm.unprivileged_userfaultfd=1");
        }
        std::process::exit(1);
    }

    println!("  Pages:        {}", NUM_PAGES);
    println!("  Faults:       {}", faults);
    println!("  Errors:       {}", errors);
    println!(
        "  Time:         {} us ({:.1} us/fault)",
        elapsed_us,
        elapsed_us as f64 / NUM_PAGES as f64
    );
    println!();

    if errors == 0 && faults == NUM_PAGES as u64 {
        println!("  VERDICT: PASS");
        println!("  Transparent page fault handling WORKS.");
        println!(
            "  {} pages faulted in by handler, app never called any API.",
            NUM_PAGES
        );
    } else {
        println!("  VERDICT: FAIL");
        std::process::exit(1);
    }
    println!("=========================================================");
}
