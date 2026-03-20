//! Runtime statistics for duvm components.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Atomic counters for daemon statistics.
#[derive(Debug, Default)]
pub struct DaemonStats {
    pub pages_stored: AtomicU64,
    pub pages_loaded: AtomicU64,
    pub pages_invalidated: AtomicU64,
    pub pages_prefetched: AtomicU64,
    pub store_errors: AtomicU64,
    pub load_errors: AtomicU64,
    pub fallback_events: AtomicU64,
    pub ring_full_events: AtomicU64,
}

impl DaemonStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            pages_stored: self.pages_stored.load(Ordering::Relaxed),
            pages_loaded: self.pages_loaded.load(Ordering::Relaxed),
            pages_invalidated: self.pages_invalidated.load(Ordering::Relaxed),
            pages_prefetched: self.pages_prefetched.load(Ordering::Relaxed),
            store_errors: self.store_errors.load(Ordering::Relaxed),
            load_errors: self.load_errors.load(Ordering::Relaxed),
            fallback_events: self.fallback_events.load(Ordering::Relaxed),
            ring_full_events: self.ring_full_events.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of daemon statistics (serializable).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub pages_stored: u64,
    pub pages_loaded: u64,
    pub pages_invalidated: u64,
    pub pages_prefetched: u64,
    pub store_errors: u64,
    pub load_errors: u64,
    pub fallback_events: u64,
    pub ring_full_events: u64,
}

impl std::fmt::Display for StatsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Pages stored:      {}", self.pages_stored)?;
        writeln!(f, "Pages loaded:      {}", self.pages_loaded)?;
        writeln!(f, "Pages invalidated: {}", self.pages_invalidated)?;
        writeln!(f, "Pages prefetched:  {}", self.pages_prefetched)?;
        writeln!(f, "Store errors:      {}", self.store_errors)?;
        writeln!(f, "Load errors:       {}", self.load_errors)?;
        writeln!(f, "Fallback events:   {}", self.fallback_events)?;
        write!(f, "Ring full events:  {}", self.ring_full_events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_snapshot_roundtrip() {
        let stats = DaemonStats::new();
        stats.pages_stored.store(100, Ordering::Relaxed);
        stats.pages_loaded.store(50, Ordering::Relaxed);
        let snap = stats.snapshot();
        assert_eq!(snap.pages_stored, 100);
        assert_eq!(snap.pages_loaded, 50);
    }
}
