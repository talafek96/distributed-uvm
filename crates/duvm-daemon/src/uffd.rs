//! userfaultfd-based page fault interception (user-space-only fallback mode).
//!
//! When the kernel module is not available, we use Linux userfaultfd to intercept
//! page faults on managed memory regions. This adds ~5-8us overhead per fault
//! compared to the kernel module path, but works without root or kernel changes.

#![allow(dead_code)]

use anyhow::{Result, bail};
use std::os::unix::io::RawFd;

// userfaultfd constants from <linux/userfaultfd.h>
const UFFD_API: u64 = 0xAA;
const UFFDIO_API: libc::c_ulong = 0xC018AA3F;
const UFFDIO_REGISTER: libc::c_ulong = 0xC020AA00;
const UFFDIO_COPY: libc::c_ulong = 0xC028AA03;
const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;

#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdMsg {
    event: u8,
    _reserved1: u8,
    _reserved2: u16,
    _reserved3: u32,
    arg: UffdMsgArg,
}

#[repr(C)]
union UffdMsgArg {
    pagefault: UffdMsgPagefault,
    _pad: [u64; 4],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct UffdMsgPagefault {
    flags: u64,
    address: u64,
    _feat: UffdMsgPfFeat,
}

#[repr(C)]
#[derive(Copy, Clone)]
union UffdMsgPfFeat {
    ptid: u32,
    _pad: u32,
}

#[repr(C)]
struct UffdCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

/// A userfaultfd-based memory region that intercepts page faults.
pub struct UffdRegion {
    uffd: RawFd,
    base: *mut libc::c_void,
    len: usize,
}

// Safety: UffdRegion is safe to send between threads.
// The uffd file descriptor and mmap'd region are process-global resources.
unsafe impl Send for UffdRegion {}
unsafe impl Sync for UffdRegion {}

impl UffdRegion {
    /// Create a new userfaultfd-managed memory region.
    ///
    /// Allocates `size` bytes of anonymous memory and registers it with userfaultfd
    /// for missing-page fault interception.
    pub fn new(size: usize) -> Result<Self> {
        let page_size = 4096usize;
        let size = (size + page_size - 1) & !(page_size - 1); // align up

        // Create userfaultfd
        let uffd =
            unsafe { libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if uffd < 0 {
            bail!(
                "userfaultfd syscall failed (errno: {}). Try: sudo sysctl vm.unprivileged_userfaultfd=1",
                std::io::Error::last_os_error()
            );
        }
        let uffd = uffd as RawFd;

        // Initialize API
        let mut api = UffdioApi {
            api: UFFD_API,
            features: 0,
            ioctls: 0,
        };
        let ret = unsafe { libc::ioctl(uffd, UFFDIO_API, &mut api as *mut _) };
        if ret < 0 {
            unsafe {
                libc::close(uffd);
            }
            bail!("UFFDIO_API failed: {}", std::io::Error::last_os_error());
        }

        // Map anonymous memory
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            unsafe {
                libc::close(uffd);
            }
            bail!("mmap failed: {}", std::io::Error::last_os_error());
        }

        // Register region with userfaultfd
        let mut reg = UffdioRegister {
            range: UffdioRange {
                start: base as u64,
                len: size as u64,
            },
            mode: UFFDIO_REGISTER_MODE_MISSING,
            ioctls: 0,
        };
        let ret = unsafe { libc::ioctl(uffd, UFFDIO_REGISTER, &mut reg as *mut _) };
        if ret < 0 {
            unsafe {
                libc::munmap(base, size);
                libc::close(uffd);
            }
            bail!(
                "UFFDIO_REGISTER failed: {}",
                std::io::Error::last_os_error()
            );
        }

        tracing::info!(addr = ?base, size, "userfaultfd region created");
        Ok(Self {
            uffd,
            base,
            len: size,
        })
    }

    /// Get the base address of the managed region.
    pub fn base(&self) -> *mut u8 {
        self.base as *mut u8
    }

    /// Get the length of the managed region.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Get the userfaultfd file descriptor (for polling).
    pub fn fd(&self) -> RawFd {
        self.uffd
    }

    /// Read a fault event from the userfaultfd. Returns the faulting address.
    /// Returns None if no fault is pending (non-blocking).
    pub fn read_event(&self) -> Result<Option<u64>> {
        let mut msg = std::mem::MaybeUninit::<UffdMsg>::zeroed();
        let ret = unsafe {
            libc::read(
                self.uffd,
                msg.as_mut_ptr() as *mut libc::c_void,
                std::mem::size_of::<UffdMsg>(),
            )
        };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            bail!("userfaultfd read failed: {}", err);
        }
        let msg = unsafe { msg.assume_init() };
        // Event type 0 = UFFD_EVENT_PAGEFAULT
        if msg.event == 0 {
            let addr = unsafe { msg.arg.pagefault.address };
            Ok(Some(addr))
        } else {
            Ok(None)
        }
    }

    /// Resolve a page fault by copying data into the faulting page.
    pub fn resolve_fault(&self, fault_addr: u64, data: &[u8; 4096]) -> Result<()> {
        let page_addr = fault_addr & !0xFFF; // align to page boundary
        let mut copy = UffdCopy {
            dst: page_addr,
            src: data.as_ptr() as u64,
            len: 4096,
            mode: 0,
            copy: 0,
        };
        let ret = unsafe { libc::ioctl(self.uffd, UFFDIO_COPY, &mut copy as *mut _) };
        if ret < 0 {
            bail!("UFFDIO_COPY failed: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for UffdRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base, self.len);
            libc::close(self.uffd);
        }
    }
}
