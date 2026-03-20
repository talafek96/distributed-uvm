//! Page placement policy engine.
//!
//! Decides which backend tier should store each page based on access patterns.
//! Currently implements a simple LRU-based strategy. CLOCK-Pro is planned for Phase 2.

#![allow(dead_code)]

use duvm_common::page::{PageFlags, PageHandle, Tier};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::time::Instant;

/// Metadata tracked per page.
#[derive(Clone, Debug)]
pub struct PageMeta {
    pub handle: PageHandle,
    pub backend_id: u8,
    pub tier: Tier,
    pub access_count: u32,
    pub last_access: Instant,
    pub flags: PageFlags,
}

/// The policy engine tracks page metadata and makes placement decisions.
pub struct PolicyEngine {
    pages: RwLock<HashMap<u64, PageMeta>>,
    strategy: Strategy,
}

#[derive(Clone, Debug)]
pub enum Strategy {
    Lru,
}

impl PolicyEngine {
    pub fn new(strategy: Strategy) -> Self {
        Self {
            pages: RwLock::new(HashMap::new()),
            strategy,
        }
    }

    /// Record that a page was stored to a backend.
    pub fn record_store(&self, offset: u64, handle: PageHandle, backend_id: u8, tier: Tier) {
        let mut pages = self.pages.write();
        pages.insert(
            offset,
            PageMeta {
                handle,
                backend_id,
                tier,
                access_count: 0,
                last_access: Instant::now(),
                flags: PageFlags::empty(),
            },
        );
    }

    /// Record that a page was loaded (accessed).
    pub fn record_load(&self, offset: u64) {
        let mut pages = self.pages.write();
        if let Some(meta) = pages.get_mut(&offset) {
            meta.access_count = meta.access_count.saturating_add(1);
            meta.last_access = Instant::now();
        }
    }

    /// Look up which backend has a page.
    pub fn lookup(&self, offset: u64) -> Option<PageMeta> {
        let pages = self.pages.read();
        pages.get(&offset).cloned()
    }

    /// Remove a page from tracking.
    pub fn remove(&self, offset: u64) -> Option<PageMeta> {
        let mut pages = self.pages.write();
        pages.remove(&offset)
    }

    /// Select the best tier for a new page store.
    /// For LRU strategy: always use the first available backend.
    pub fn select_tier(&self) -> Tier {
        match self.strategy {
            Strategy::Lru => Tier::Compressed,
        }
    }

    /// Number of tracked pages.
    pub fn tracked_pages(&self) -> usize {
        let pages = self.pages.read();
        pages.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_lookup() {
        let engine = PolicyEngine::new(Strategy::Lru);
        let handle = PageHandle::new(1, 42);
        engine.record_store(100, handle, 1, Tier::Compressed);

        let meta = engine.lookup(100).unwrap();
        assert_eq!(meta.handle, handle);
        assert_eq!(meta.tier, Tier::Compressed);
        assert_eq!(meta.access_count, 0);
    }

    #[test]
    fn record_load_increments_count() {
        let engine = PolicyEngine::new(Strategy::Lru);
        let handle = PageHandle::new(1, 0);
        engine.record_store(0, handle, 1, Tier::Compressed);

        engine.record_load(0);
        engine.record_load(0);
        let meta = engine.lookup(0).unwrap();
        assert_eq!(meta.access_count, 2);
    }

    #[test]
    fn remove_page() {
        let engine = PolicyEngine::new(Strategy::Lru);
        engine.record_store(0, PageHandle::new(0, 0), 0, Tier::Compressed);
        assert!(engine.lookup(0).is_some());
        engine.remove(0);
        assert!(engine.lookup(0).is_none());
    }
}
