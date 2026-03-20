//! Lock-free SPSC ring buffer for kernel-daemon communication.
//!
//! This module implements the user-space side of the shared ring buffer.
//! In production, the kernel side writes requests and the daemon reads them.
//! For user-space-only mode (userfaultfd fallback), both sides are in user-space.
//!
//! Design: io_uring-style single-producer/single-consumer with memory barriers.

use crate::protocol::{RingCompletion, RingRequest};
use bytemuck::Zeroable;
use crossbeam_utils::CachePadded;
use std::sync::atomic::AtomicU32;

/// Ring buffer header stored in shared memory.
/// Each index is on its own cache line to avoid false sharing.
#[repr(C)]
pub struct RingHeader {
    /// Written by producer, read by consumer.
    pub write_idx: CachePadded<AtomicU32>,
    /// Written by consumer, read by producer.
    pub read_idx: CachePadded<AtomicU32>,
    /// Ring capacity (must be power of 2).
    pub capacity: u32,
    /// Version tag for protocol compatibility.
    pub version: u32,
}

/// User-space view of the request ring (kernel writes, daemon reads).
pub struct RequestRing {
    entries: Vec<RingRequest>,
    write_idx: usize,
    read_idx: usize,
    capacity: usize,
    mask: usize,
}

impl RequestRing {
    /// Create a new request ring with the given capacity (must be power of 2).
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two(), "capacity must be power of 2");
        Self {
            entries: vec![RingRequest::zeroed(); capacity],
            write_idx: 0,
            read_idx: 0,
            capacity,
            mask: capacity - 1,
        }
    }

    /// Try to push a request onto the ring. Returns false if full.
    pub fn try_push(&mut self, req: RingRequest) -> bool {
        let next_write = (self.write_idx + 1) & self.mask;
        if next_write == self.read_idx {
            return false; // ring full
        }
        self.entries[self.write_idx] = req;
        self.write_idx = (self.write_idx + 1) & self.mask;
        true
    }

    /// Try to pop a request from the ring. Returns None if empty.
    pub fn try_pop(&mut self) -> Option<RingRequest> {
        if self.read_idx == self.write_idx {
            return None; // ring empty
        }
        let req = self.entries[self.read_idx];
        self.read_idx = (self.read_idx + 1) & self.mask;
        Some(req)
    }

    /// Pop up to `max` requests into the provided buffer. Returns count.
    pub fn pop_batch(&mut self, buf: &mut Vec<RingRequest>, max: usize) -> usize {
        let mut count = 0;
        while count < max {
            match self.try_pop() {
                Some(req) => {
                    buf.push(req);
                    count += 1;
                }
                None => break,
            }
        }
        count
    }

    /// Number of entries currently in the ring.
    pub fn len(&self) -> usize {
        if self.write_idx >= self.read_idx {
            self.write_idx - self.read_idx
        } else {
            self.capacity - self.read_idx + self.write_idx
        }
    }

    pub fn is_empty(&self) -> bool {
        self.write_idx == self.read_idx
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// User-space view of the completion ring (daemon writes, kernel reads).
pub struct CompletionRing {
    entries: Vec<RingCompletion>,
    write_idx: usize,
    read_idx: usize,
    capacity: usize,
    mask: usize,
}

impl CompletionRing {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two(), "capacity must be power of 2");
        Self {
            entries: vec![RingCompletion::zeroed(); capacity],
            write_idx: 0,
            read_idx: 0,
            capacity,
            mask: capacity - 1,
        }
    }

    pub fn try_push(&mut self, comp: RingCompletion) -> bool {
        let next_write = (self.write_idx + 1) & self.mask;
        if next_write == self.read_idx {
            return false;
        }
        self.entries[self.write_idx] = comp;
        self.write_idx = (self.write_idx + 1) & self.mask;
        true
    }

    pub fn try_pop(&mut self) -> Option<RingCompletion> {
        if self.read_idx == self.write_idx {
            return None;
        }
        let comp = self.entries[self.read_idx];
        self.read_idx = (self.read_idx + 1) & self.mask;
        Some(comp)
    }

    pub fn len(&self) -> usize {
        if self.write_idx >= self.read_idx {
            self.write_idx - self.read_idx
        } else {
            self.capacity - self.read_idx + self.write_idx
        }
    }

    pub fn is_empty(&self) -> bool {
        self.write_idx == self.read_idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn request_ring_push_pop() {
        let mut ring = RequestRing::new(4);
        assert!(ring.is_empty());

        let mut req = RingRequest::zeroed();
        req.op = 1;
        req.seq = 42;
        assert!(ring.try_push(req));
        assert_eq!(ring.len(), 1);

        let popped = ring.try_pop().unwrap();
        assert_eq!(popped.op, 1);
        assert_eq!(popped.seq, 42);
        assert!(ring.is_empty());
    }

    #[test]
    fn request_ring_full() {
        let mut ring = RequestRing::new(4);
        // Can push capacity-1 = 3 items (one slot wasted for full detection)
        assert!(ring.try_push(RingRequest::zeroed()));
        assert!(ring.try_push(RingRequest::zeroed()));
        assert!(ring.try_push(RingRequest::zeroed()));
        assert!(!ring.try_push(RingRequest::zeroed())); // full
    }

    #[test]
    fn request_ring_batch_pop() {
        let mut ring = RequestRing::new(8);
        for i in 0..5 {
            let mut req = RingRequest::zeroed();
            req.seq = i;
            ring.try_push(req);
        }
        let mut buf = Vec::new();
        let count = ring.pop_batch(&mut buf, 3);
        assert_eq!(count, 3);
        assert_eq!(buf[0].seq, 0);
        assert_eq!(buf[2].seq, 2);
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn completion_ring_push_pop() {
        let mut ring = CompletionRing::new(4);
        let mut comp = RingCompletion::zeroed();
        comp.seq = 99;
        comp.result = 0;
        assert!(ring.try_push(comp));
        let popped = ring.try_pop().unwrap();
        assert_eq!(popped.seq, 99);
        assert_eq!(popped.result, 0);
    }

    #[test]
    fn ring_wraps_around() {
        let mut ring = RequestRing::new(4);
        // Fill and drain multiple times to test wrap-around
        for round in 0..10 {
            let mut req = RingRequest::zeroed();
            req.seq = round;
            assert!(ring.try_push(req));
            let popped = ring.try_pop().unwrap();
            assert_eq!(popped.seq, round);
        }
    }
}
