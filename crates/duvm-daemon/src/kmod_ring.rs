//! Kernel module ring buffer consumer.
//!
//! Opens `/dev/duvm_ctl`, mmaps the shared ring buffer, and processes
//! page store/load requests from the kernel module by routing them
//! through the daemon's engine (which sends them to backends).
//!
//! Memory layout (must match duvm-kmod/src/ring.c):
//!   [ring_header]      - 1 page (256 bytes used)
//!   [request entries]  - ceil(capacity * 64 / PAGE_SIZE) pages
//!   [completion entries] - ceil(capacity * 64 / PAGE_SIZE) pages
//!   [staging buffer]   - staging_pages * PAGE_SIZE

use crate::engine::Engine;
use anyhow::{Context, Result, bail};
use duvm_common::page::PAGE_SIZE;
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Ring header layout — must match struct duvm_ring_header in duvm_kmod.h.
/// Each index is padded to its own cache line (64 bytes).
/// We access fields via raw pointer offsets to avoid packed struct alignment issues.
///
/// Layout (byte offsets):
///   0:   req_write_idx  (u32)  + 60 bytes padding
///   64:  req_read_idx   (u32)  + 60 bytes padding
///   128: comp_write_idx (u32)  + 60 bytes padding
///   192: comp_read_idx  (u32)  + 60 bytes padding
///   256: capacity       (u32)
///   260: version        (u32)
///   264: staging_pages  (u32)
///   268: reserved       (u32)
const HDR_REQ_WRITE_IDX: usize = 0;
const HDR_REQ_READ_IDX: usize = 64;
const HDR_COMP_WRITE_IDX: usize = 128;
#[allow(dead_code)]
const HDR_COMP_READ_IDX: usize = 192;
const HDR_CAPACITY: usize = 256;
const HDR_VERSION: usize = 260;
const HDR_STAGING_PAGES: usize = 264;

/// Ring request — must match struct duvm_request in duvm_kmod.h.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct KmodRequest {
    op: u8,
    flags: u8,
    _pad: [u8; 2],
    seq: u32,
    pfn: u64,
    offset: u64,
    staging_slot: u32,
    _reserved: [u8; 28],
}

/// Ring completion — must match struct duvm_completion in duvm_kmod.h.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct KmodCompletion {
    seq: u32,
    result: i32,
    handle: u64,
    staging_slot: u32,
    _reserved: [u8; 40],
}

const OP_STORE: u8 = 1;
const OP_LOAD: u8 = 2;

/// Consumer that connects to the kernel module via /dev/duvm_ctl.
pub struct KmodRingConsumer {
    // mmap'd region base pointer
    base: *mut u8,
    mmap_len: usize,

    // Parsed pointers into the mmap'd region
    header: *mut u8, // raw pointer to header (access via offsets)
    requests: *const KmodRequest,
    completions: *mut KmodCompletion,
    staging: *mut u8,

    // Ring parameters
    #[allow(dead_code)]
    capacity: u32,
    mask: u32,

    // File handle (keeps /dev/duvm_ctl open)
    _file: std::fs::File,
}

// Safety: the mmap'd region is shared memory with the kernel module.
// Access is synchronized via the ring buffer protocol (SPSC with memory barriers).
// Only one KmodRingConsumer should exist at a time (enforced by /dev/duvm_ctl
// returning EBUSY on double-open).
unsafe impl Send for KmodRingConsumer {}
unsafe impl Sync for KmodRingConsumer {}

/// Read a u32 from a raw pointer at a byte offset, volatile.
unsafe fn read_u32(base: *const u8, offset: usize) -> u32 {
    unsafe { (base.add(offset) as *const u32).read_volatile() }
}

/// Write a u32 to a raw pointer at a byte offset, volatile.
unsafe fn write_u32(base: *mut u8, offset: usize, val: u32) {
    unsafe { (base.add(offset) as *mut u32).write_volatile(val) }
}

impl KmodRingConsumer {
    /// Open /dev/duvm_ctl and mmap the shared ring buffer.
    pub fn open(ctl_path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(ctl_path)
            .with_context(|| format!("Failed to open {}. Is duvm-kmod loaded?", ctl_path))?;

        // Probe mmap: read header to get ring parameters
        let header_map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE_SIZE,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if header_map == libc::MAP_FAILED {
            bail!("mmap header failed: {}", std::io::Error::last_os_error());
        }

        let hdr = header_map as *const u8;
        let capacity = unsafe { read_u32(hdr, HDR_CAPACITY) };
        let version = unsafe { read_u32(hdr, HDR_VERSION) };
        let staging_pages = unsafe { read_u32(hdr, HDR_STAGING_PAGES) };

        unsafe { libc::munmap(header_map, PAGE_SIZE) };

        if version != 2 {
            bail!("Unsupported ring buffer version: {} (expected 2)", version);
        }

        // Calculate total size (must match ring.c layout)
        let header_sz = PAGE_SIZE;
        let req_sz = align_up(capacity as usize * 64, PAGE_SIZE);
        let comp_sz = align_up(capacity as usize * 64, PAGE_SIZE);
        let staging_sz = staging_pages as usize * PAGE_SIZE;
        let total_sz = header_sz + req_sz + comp_sz + staging_sz;

        // Full mmap
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total_sz,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            bail!("mmap full ring failed: {}", std::io::Error::last_os_error());
        }

        let base_ptr = base as *mut u8;

        tracing::info!(
            capacity,
            staging_pages,
            total_bytes = total_sz,
            "Connected to kernel module ring buffer"
        );

        Ok(Self {
            base: base_ptr,
            mmap_len: total_sz,
            header: base_ptr,
            requests: unsafe { base_ptr.add(header_sz) } as *const KmodRequest,
            completions: unsafe { base_ptr.add(header_sz + req_sz) } as *mut KmodCompletion,
            staging: unsafe { base_ptr.add(header_sz + req_sz + comp_sz) },
            capacity,
            mask: capacity - 1,
            _file: file,
        })
    }

    /// Poll the request ring and process one request. Returns true if a request was processed.
    pub fn poll_once(&self, engine: &Engine) -> bool {
        let req_write = unsafe { read_u32(self.header, HDR_REQ_WRITE_IDX) };
        let req_read = unsafe { read_u32(self.header, HDR_REQ_READ_IDX) };

        if req_read == req_write {
            return false;
        }

        // Read the request
        std::sync::atomic::fence(Ordering::Acquire);
        let req = unsafe { *self.requests.add(req_read as usize) };

        let staging_offset = req.staging_slot as usize * PAGE_SIZE;

        // Process the request
        let result: i32 = match req.op {
            OP_STORE => {
                let mut page_data = [0u8; PAGE_SIZE];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.staging.add(staging_offset),
                        page_data.as_mut_ptr(),
                        PAGE_SIZE,
                    );
                }
                match engine.store_page(req.offset, &page_data) {
                    Ok(_) => 0,
                    Err(e) => {
                        tracing::warn!(offset = req.offset, error = %e, "kmod store failed");
                        -1
                    }
                }
            }
            OP_LOAD => {
                let mut page_data = [0u8; PAGE_SIZE];
                match engine.load_page(req.offset, &mut page_data) {
                    Ok(()) => {
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                page_data.as_ptr(),
                                self.staging.add(staging_offset),
                                PAGE_SIZE,
                            );
                        }
                        0
                    }
                    Err(e) => {
                        tracing::warn!(offset = req.offset, error = %e, "kmod load failed");
                        -1
                    }
                }
            }
            other => {
                tracing::warn!(op = other, "unknown kmod request opcode");
                -1
            }
        };

        // Advance request read index
        let next_read = (req_read + 1) & self.mask;
        unsafe { write_u32(self.header, HDR_REQ_READ_IDX, next_read) };

        // Write completion
        let comp_write = unsafe { read_u32(self.header, HDR_COMP_WRITE_IDX) };
        let comp = KmodCompletion {
            seq: req.seq,
            result,
            handle: 0,
            staging_slot: req.staging_slot,
            _reserved: [0; 40],
        };
        unsafe {
            std::ptr::write(self.completions.add(comp_write as usize), comp);
        }

        std::sync::atomic::fence(Ordering::Release);
        let next_comp_write = (comp_write + 1) & self.mask;
        unsafe { write_u32(self.header, HDR_COMP_WRITE_IDX, next_comp_write) };

        true
    }

    /// Run the consumer loop using poll() on /dev/duvm_ctl for instant wake-up.
    ///
    /// The kernel module implements poll() on /dev/duvm_ctl that returns POLLIN
    /// when there are pending requests. This means the daemon wakes up within
    /// ~1-5us of a new request, instead of the old 0-100us polling delay.
    ///
    /// Blocks until `stop` is set to true.
    pub fn run_loop(&self, engine: &Engine, stop: &Arc<AtomicBool>) {
        tracing::info!("kmod ring consumer started (event-driven via poll)");

        let fd = self._file.as_raw_fd();

        while !stop.load(Ordering::Relaxed) {
            // Process all pending requests first (drain the ring)
            while self.poll_once(engine) {
                // Keep processing — more requests may have arrived
            }

            // No more pending requests. Block on poll() until kernel wakes us.
            // Timeout of 100ms so we can check the stop flag periodically.
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            unsafe {
                libc::poll(&mut pollfd, 1, 100); // 100ms timeout
            }
            // poll returned — either POLLIN (new request) or timeout (check stop flag)
        }

        tracing::info!("kmod ring consumer stopped");
    }
}

impl Drop for KmodRingConsumer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.mmap_len);
        }
        tracing::info!("kmod ring consumer disconnected");
    }
}

fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_struct_sizes_match_kernel() {
        assert_eq!(std::mem::size_of::<KmodRequest>(), 64);
        assert_eq!(std::mem::size_of::<KmodCompletion>(), 64);
        // Header is 256 bytes: 4 * (4 + 60) + 4*4 = 272... actually
        // it's packed with specific padding, total = 4*64 + 16 = 272
        // But it lives in a full page (4096), so the size doesn't matter
        // as long as field offsets are correct.
    }

    #[test]
    fn align_up_works() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
    }
}
