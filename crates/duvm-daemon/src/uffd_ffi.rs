//! FFI bindings to the C userfaultfd helper.
//!
//! All userfaultfd operations go through C to avoid aarch64 ABI issues.

use std::ffi::c_void;

/// Callback type for page resolution: (page_idx, out_buf, ctx) -> 0 on success
pub type PageResolverFn =
    unsafe extern "C" fn(page_idx: i32, out_buf: *mut u8, ctx: *mut c_void) -> i32;

unsafe extern "C" {
    pub fn duvm_uffd_create() -> i32;
    pub fn duvm_uffd_api(uffd: i32) -> i32;
    pub fn duvm_uffd_register(uffd: i32, start: u64, len: u64) -> i32;
    pub fn duvm_uffd_copy(uffd: i32, dst: u64, src: *const u8, len: u64) -> i32;
    pub fn duvm_uffd_read_fault(uffd: i32) -> u64;
    pub fn duvm_uffd_run(
        num_pages: i32,
        resolver: Option<PageResolverFn>,
        ctx: *mut c_void,
        out_faults: *mut u64,
        out_errors: *mut u64,
        out_elapsed_us: *mut u64,
        out_base: *mut *mut c_void,
    ) -> i32;
    pub fn duvm_uffd_run_demo(
        num_pages: i32,
        out_faults: *mut u64,
        out_errors: *mut u64,
        out_elapsed_us: *mut u64,
    ) -> i32;
    pub fn duvm_cache_init(max_pages: i32) -> i32;
    pub fn duvm_cache_set_page(idx: i32, data: *const u8);
    pub fn duvm_uffd_run_cached(
        num_pages: i32,
        out_faults: *mut u64,
        out_errors: *mut u64,
        out_elapsed_us: *mut u64,
    ) -> i32;
}
