//! FFI bindings to the C userfaultfd helper.
//!
//! All userfaultfd operations go through C to avoid aarch64 ABI issues.

unsafe extern "C" {
    pub fn duvm_uffd_create() -> i32;
    pub fn duvm_uffd_api(uffd: i32) -> i32;
    pub fn duvm_uffd_register(uffd: i32, start: u64, len: u64) -> i32;
    pub fn duvm_uffd_copy(uffd: i32, dst: u64, src: *const u8, len: u64) -> i32;
    pub fn duvm_uffd_read_fault(uffd: i32) -> u64;
    pub fn duvm_uffd_run_demo(
        num_pages: i32,
        out_faults: *mut u64,
        out_errors: *mut u64,
        out_elapsed_us: *mut u64,
    ) -> i32;
}
