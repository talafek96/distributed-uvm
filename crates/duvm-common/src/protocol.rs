//! Ring buffer protocol messages between kernel module and daemon.

use bytemuck::{Pod, Zeroable};

/// Operation types for ring buffer requests.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    /// No operation (empty slot).
    Nop = 0,
    /// Store a page to a backend (page eviction / swap-out).
    Store = 1,
    /// Load a page from a backend (page fault / swap-in).
    Load = 2,
    /// Invalidate a page (no longer needed).
    Invalidate = 3,
    /// Prefetch hint from libduvm.
    Prefetch = 4,
}

impl OpCode {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Nop),
            1 => Some(Self::Store),
            2 => Some(Self::Load),
            3 => Some(Self::Invalidate),
            4 => Some(Self::Prefetch),
            _ => None,
        }
    }
}

/// A request in the ring buffer (kernel -> daemon).
/// Fixed 64 bytes for cache-line alignment.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct RingRequest {
    /// Operation code.
    pub op: u8,
    /// Flags (reserved).
    pub flags: u8,
    /// Padding.
    pub _pad: [u8; 2],
    /// Request sequence number.
    pub seq: u32,
    /// Page frame number.
    pub pfn: u64,
    /// Swap offset (identifies the page in swap space).
    pub offset: u64,
    /// Index into the staging buffer where page data resides.
    pub staging_slot: u32,
    /// Reserved for future use.
    pub _reserved: [u8; 28],
}

// Safety: RingRequest is repr(C) with only primitive fields, all of which are Zeroable and Pod.
// SAFETY: All fields are primitives; no padding holes due to explicit layout.
unsafe impl Zeroable for RingRequest {}
// SAFETY: repr(C, align(64)) with all-Pod fields, no padding issues.
unsafe impl Pod for RingRequest {}

/// A completion in the ring buffer (daemon -> kernel).
/// Fixed 64 bytes for cache-line alignment.
#[derive(Copy, Clone, Debug)]
#[repr(C, align(64))]
pub struct RingCompletion {
    /// Matches the request sequence number.
    pub seq: u32,
    /// Result code: 0 = success, negative = error.
    pub result: i32,
    /// The page handle assigned by the backend (for store ops).
    pub handle: u64,
    /// Staging slot where loaded page data resides (for load ops).
    pub staging_slot: u32,
    /// Reserved.
    pub _reserved: [u8; 40],
}

// SAFETY: RingCompletion is repr(C) with only primitive fields, all Zeroable.
unsafe impl Zeroable for RingCompletion {}
// SAFETY: repr(C, align(64)) with all-Pod fields, no padding issues.
unsafe impl Pod for RingCompletion {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn ring_request_is_64_bytes() {
        assert_eq!(mem::size_of::<RingRequest>(), 64);
    }

    #[test]
    fn ring_completion_is_64_bytes() {
        assert_eq!(mem::size_of::<RingCompletion>(), 64);
    }

    #[test]
    fn opcode_roundtrip() {
        assert_eq!(OpCode::from_u8(1), Some(OpCode::Store));
        assert_eq!(OpCode::from_u8(2), Some(OpCode::Load));
        assert_eq!(OpCode::from_u8(99), None);
    }
}
