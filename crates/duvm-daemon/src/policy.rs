//! Page placement policy engine.
//!
//! Decides which backend tier should store each page based on access patterns.
//! Implements LRU-based strategy with tier-aware placement.
//! CLOCK-Pro is planned for Phase 2.

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

/// Backend capacity snapshot for tier selection.
#[derive(Clone, Debug)]
pub struct BackendCapacity {
    pub backend_id: u8,
    pub tier: Tier,
    pub total_pages: u64,
    pub used_pages: u64,
    pub latency_ns: u64,
    pub healthy: bool,
}

impl BackendCapacity {
    /// Returns true if this backend has room for at least one more page.
    pub fn has_capacity(&self) -> bool {
        self.healthy && self.used_pages < self.total_pages
    }

    /// Fraction of capacity used (0.0 to 1.0).
    pub fn utilization(&self) -> f64 {
        if self.total_pages == 0 {
            1.0
        } else {
            self.used_pages as f64 / self.total_pages as f64
        }
    }
}

/// The policy engine tracks page metadata and makes placement decisions.
pub struct PolicyEngine {
    pages: RwLock<HashMap<u64, PageMeta>>,
    strategy: Strategy,
}

#[derive(Clone, Debug, PartialEq)]
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

    /// Select the best tier for a new page store, given current backend capacities.
    ///
    /// Tier preference order (lowest latency first):
    ///   Compressed → Cxl → Rdma → Gpu → Local
    ///
    /// If the preferred tier is full, cascade to the next available tier.
    pub fn select_tier(&self, capacities: &[BackendCapacity]) -> Option<Tier> {
        match self.strategy {
            Strategy::Lru => self.select_tier_lru(capacities),
        }
    }

    fn select_tier_lru(&self, capacities: &[BackendCapacity]) -> Option<Tier> {
        // Preferred order: lowest-latency first, then by available capacity
        let tier_preference = [
            Tier::Compressed,
            Tier::Cxl,
            Tier::Rdma,
            Tier::Gpu,
            Tier::Local,
        ];

        for &tier in &tier_preference {
            // Find backends for this tier that have capacity
            let available: Vec<&BackendCapacity> = capacities
                .iter()
                .filter(|c| c.tier == tier && c.has_capacity())
                .collect();

            if !available.is_empty() {
                return Some(tier);
            }
        }

        None // all backends are full
    }

    /// Find the LRU (least recently used) eviction candidate in a given backend.
    /// Returns the offset of the page to evict.
    pub fn find_eviction_candidate(&self, backend_id: u8) -> Option<u64> {
        let pages = self.pages.read();
        pages
            .iter()
            .filter(|(_, meta)| {
                meta.backend_id == backend_id && !meta.flags.contains(PageFlags::PINNED)
            })
            .min_by_key(|(_, meta)| (meta.last_access, meta.access_count))
            .map(|(offset, _)| *offset)
    }

    /// Find up to `count` LRU eviction candidates in a given backend.
    pub fn find_eviction_candidates(&self, backend_id: u8, count: usize) -> Vec<u64> {
        let pages = self.pages.read();
        let mut candidates: Vec<(u64, &PageMeta)> = pages
            .iter()
            .filter(|(_, meta)| {
                meta.backend_id == backend_id && !meta.flags.contains(PageFlags::PINNED)
            })
            .map(|(offset, meta)| (*offset, meta))
            .collect();

        candidates.sort_by_key(|(_, meta)| (meta.last_access, meta.access_count));
        candidates.into_iter().take(count).map(|(off, _)| off).collect()
    }

    /// Number of tracked pages.
    pub fn tracked_pages(&self) -> usize {
        let pages = self.pages.read();
        pages.len()
    }

    /// Number of tracked pages in a specific backend.
    pub fn pages_in_backend(&self, backend_id: u8) -> usize {
        let pages = self.pages.read();
        pages.values().filter(|m| m.backend_id == backend_id).count()
    }

    /// Set a flag on a page.
    pub fn set_flag(&self, offset: u64, flag: PageFlags) {
        let mut pages = self.pages.write();
        if let Some(meta) = pages.get_mut(&offset) {
            meta.flags |= flag;
        }
    }

    /// Clear a flag on a page.
    pub fn clear_flag(&self, offset: u64, flag: PageFlags) {
        let mut pages = self.pages.write();
        if let Some(meta) = pages.get_mut(&offset) {
            meta.flags &= !flag;
        }
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

    #[test]
    fn select_tier_prefers_lowest_latency() {
        let engine = PolicyEngine::new(Strategy::Lru);
        let caps = vec![
            BackendCapacity {
                backend_id: 0,
                tier: Tier::Rdma,
                total_pages: 100,
                used_pages: 0,
                latency_ns: 250_000,
                healthy: true,
            },
            BackendCapacity {
                backend_id: 1,
                tier: Tier::Compressed,
                total_pages: 100,
                used_pages: 0,
                latency_ns: 3_000,
                healthy: true,
            },
        ];
        assert_eq!(engine.select_tier(&caps), Some(Tier::Compressed));
    }

    #[test]
    fn select_tier_cascades_when_full() {
        let engine = PolicyEngine::new(Strategy::Lru);
        let caps = vec![
            BackendCapacity {
                backend_id: 1,
                tier: Tier::Compressed,
                total_pages: 100,
                used_pages: 100, // full
                latency_ns: 3_000,
                healthy: true,
            },
            BackendCapacity {
                backend_id: 2,
                tier: Tier::Rdma,
                total_pages: 100,
                used_pages: 50,
                latency_ns: 250_000,
                healthy: true,
            },
        ];
        assert_eq!(engine.select_tier(&caps), Some(Tier::Rdma));
    }

    #[test]
    fn select_tier_returns_none_when_all_full() {
        let engine = PolicyEngine::new(Strategy::Lru);
        let caps = vec![BackendCapacity {
            backend_id: 1,
            tier: Tier::Compressed,
            total_pages: 100,
            used_pages: 100,
            latency_ns: 3_000,
            healthy: true,
        }];
        assert_eq!(engine.select_tier(&caps), None);
    }

    #[test]
    fn select_tier_skips_unhealthy() {
        let engine = PolicyEngine::new(Strategy::Lru);
        let caps = vec![
            BackendCapacity {
                backend_id: 1,
                tier: Tier::Compressed,
                total_pages: 100,
                used_pages: 0,
                latency_ns: 3_000,
                healthy: false, // unhealthy
            },
            BackendCapacity {
                backend_id: 2,
                tier: Tier::Rdma,
                total_pages: 100,
                used_pages: 0,
                latency_ns: 250_000,
                healthy: true,
            },
        ];
        assert_eq!(engine.select_tier(&caps), Some(Tier::Rdma));
    }

    #[test]
    fn eviction_candidate_picks_lru() {
        let engine = PolicyEngine::new(Strategy::Lru);

        // Store three pages; middle one was accessed most recently
        engine.record_store(0, PageHandle::new(1, 0), 1, Tier::Compressed);
        std::thread::sleep(std::time::Duration::from_millis(5));
        engine.record_store(1, PageHandle::new(1, 1), 1, Tier::Compressed);
        std::thread::sleep(std::time::Duration::from_millis(5));
        engine.record_store(2, PageHandle::new(1, 2), 1, Tier::Compressed);

        // Access page 0 again — it should no longer be the LRU
        engine.record_load(0);

        // LRU should be page 1 (oldest last_access among 1 and 2, but page 1 was stored before page 2)
        let candidate = engine.find_eviction_candidate(1).unwrap();
        assert_eq!(candidate, 1);
    }

    #[test]
    fn eviction_skips_pinned_pages() {
        let engine = PolicyEngine::new(Strategy::Lru);

        engine.record_store(0, PageHandle::new(1, 0), 1, Tier::Compressed);
        std::thread::sleep(std::time::Duration::from_millis(5));
        engine.record_store(1, PageHandle::new(1, 1), 1, Tier::Compressed);

        // Pin page 0 — it should be skipped
        engine.set_flag(0, PageFlags::PINNED);

        let candidate = engine.find_eviction_candidate(1).unwrap();
        assert_eq!(candidate, 1);
    }

    #[test]
    fn pages_in_backend_count() {
        let engine = PolicyEngine::new(Strategy::Lru);
        engine.record_store(0, PageHandle::new(0, 0), 0, Tier::Compressed);
        engine.record_store(1, PageHandle::new(0, 1), 0, Tier::Compressed);
        engine.record_store(2, PageHandle::new(1, 0), 1, Tier::Rdma);

        assert_eq!(engine.pages_in_backend(0), 2);
        assert_eq!(engine.pages_in_backend(1), 1);
        assert_eq!(engine.pages_in_backend(99), 0);
    }

    #[test]
    fn flag_set_and_clear() {
        let engine = PolicyEngine::new(Strategy::Lru);
        engine.record_store(0, PageHandle::new(0, 0), 0, Tier::Compressed);

        engine.set_flag(0, PageFlags::DIRTY);
        let meta = engine.lookup(0).unwrap();
        assert!(meta.flags.contains(PageFlags::DIRTY));

        engine.clear_flag(0, PageFlags::DIRTY);
        let meta = engine.lookup(0).unwrap();
        assert!(!meta.flags.contains(PageFlags::DIRTY));
    }
}
