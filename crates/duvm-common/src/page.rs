//! Page handle and metadata types shared across duvm components.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Opaque handle to a page stored in a backend.
/// Encodes backend_id (upper 8 bits) + internal offset (lower 56 bits).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct PageHandle(u64);

impl PageHandle {
    pub const INVALID: Self = Self(u64::MAX);

    /// Create a new page handle from backend ID and offset.
    pub fn new(backend_id: u8, offset: u64) -> Self {
        assert!(offset < (1 << 56), "offset exceeds 56-bit limit");
        Self((backend_id as u64) << 56 | offset)
    }

    pub fn backend_id(self) -> u8 {
        (self.0 >> 56) as u8
    }

    pub fn offset(self) -> u64 {
        self.0 & ((1 << 56) - 1)
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub fn is_valid(self) -> bool {
        self != Self::INVALID
    }
}

impl fmt::Display for PageHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PageHandle(backend={}, offset={})",
            self.backend_id(),
            self.offset()
        )
    }
}

/// Standard page size (4KB).
pub const PAGE_SIZE: usize = 4096;

/// A page-aligned buffer for transferring page data.
pub type PageBuffer = [u8; PAGE_SIZE];

/// Tier levels for page placement.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Tier {
    /// Local DRAM (kernel-managed, not duvm).
    Local = 0,
    /// Compressed local memory.
    Compressed = 1,
    /// CXL-attached memory.
    Cxl = 2,
    /// Remote DRAM via RDMA.
    Rdma = 3,
    /// GPU HBM.
    Gpu = 4,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tier::Local => write!(f, "local"),
            Tier::Compressed => write!(f, "compressed"),
            Tier::Cxl => write!(f, "cxl"),
            Tier::Rdma => write!(f, "rdma"),
            Tier::Gpu => write!(f, "gpu"),
        }
    }
}

/// Page flags used in metadata tracking.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageFlags(u8);

impl PageFlags {
    pub const DIRTY: Self = Self(1 << 0);
    pub const PINNED: Self = Self(1 << 1);
    pub const PREFETCHED: Self = Self(1 << 2);
    pub const MIGRATING: Self = Self(1 << 3);

    pub fn empty() -> Self {
        Self(0)
    }

    pub fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    pub fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }

    pub fn remove(&mut self, flag: Self) {
        self.0 &= !flag.0;
    }
}

impl std::ops::BitOr for PageFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_handle_roundtrip() {
        let h = PageHandle::new(3, 12345);
        assert_eq!(h.backend_id(), 3);
        assert_eq!(h.offset(), 12345);
        assert!(h.is_valid());
    }

    #[test]
    fn page_handle_invalid() {
        assert!(!PageHandle::INVALID.is_valid());
    }

    #[test]
    fn page_handle_max_offset() {
        let max = (1u64 << 56) - 1;
        let h = PageHandle::new(255, max);
        assert_eq!(h.backend_id(), 255);
        assert_eq!(h.offset(), max);
    }

    #[test]
    fn page_flags_operations() {
        let mut flags = PageFlags::empty();
        assert!(!flags.contains(PageFlags::DIRTY));
        flags.insert(PageFlags::DIRTY);
        assert!(flags.contains(PageFlags::DIRTY));
        flags.remove(PageFlags::DIRTY);
        assert!(!flags.contains(PageFlags::DIRTY));
    }
}
