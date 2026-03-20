//! duvm-backend-trait: Defines the interface that all duvm storage backends must implement.
//!
//! Each backend is a shared library (.so) that provides a way to store and retrieve
//! 4KB pages. Backends can represent different memory tiers: compressed local memory,
//! CXL-attached memory, RDMA remote memory, or GPU HBM.

use anyhow::Result;
use duvm_common::page::{PageBuffer, PageHandle, Tier};

/// Trait that all duvm backends must implement.
///
/// Backends are responsible for storing and retrieving 4KB pages. Each backend
/// represents one tier in the memory hierarchy.
pub trait DuvmBackend: Send + Sync {
    /// Human-readable name for logging and metrics.
    fn name(&self) -> &str;

    /// The memory tier this backend represents.
    fn tier(&self) -> Tier;

    /// Initialize the backend with configuration.
    fn init(&mut self, config: &BackendConfig) -> Result<()>;

    /// Allocate a slot for a page. Returns an opaque handle.
    fn alloc_page(&self) -> Result<PageHandle>;

    /// Free a previously allocated page slot.
    fn free_page(&self, handle: PageHandle) -> Result<()>;

    /// Store a 4KB page at the given handle.
    fn store_page(&self, handle: PageHandle, data: &PageBuffer) -> Result<()>;

    /// Load a 4KB page from the given handle into the buffer.
    fn load_page(&self, handle: PageHandle, buf: &mut PageBuffer) -> Result<()>;

    /// Batch store: store multiple pages. Default calls store_page in a loop.
    fn store_pages(&self, pages: &[(PageHandle, &PageBuffer)]) -> Result<()> {
        for (handle, data) in pages {
            self.store_page(*handle, data)?;
        }
        Ok(())
    }

    /// Batch load: load multiple pages. Default calls load_page in a loop.
    fn load_pages(&self, pages: &mut [(PageHandle, &mut PageBuffer)]) -> Result<()> {
        for (handle, buf) in pages.iter_mut() {
            self.load_page(*handle, buf)?;
        }
        Ok(())
    }

    /// Report current capacity: (total_pages, used_pages).
    fn capacity(&self) -> (u64, u64);

    /// Report average access latency in nanoseconds.
    fn latency_ns(&self) -> u64;

    /// Health check. Returns false if backend is degraded/unreachable.
    fn is_healthy(&self) -> bool;

    /// Shutdown and release all resources.
    fn shutdown(&mut self) -> Result<()>;
}

/// Configuration passed to a backend during initialization.
#[derive(Clone, Debug)]
pub struct BackendConfig {
    /// Backend-specific configuration as a TOML table.
    pub settings: toml::Table,
    /// Maximum number of pages this backend should manage.
    pub max_pages: u64,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            settings: toml::Table::new(),
            max_pages: 1024 * 1024, // 4GB default
        }
    }
}

/// Information about a backend's current state (for reporting).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BackendInfo {
    pub name: String,
    pub tier: String,
    pub total_pages: u64,
    pub used_pages: u64,
    pub latency_ns: u64,
    pub healthy: bool,
}

impl BackendInfo {
    pub fn from_backend(backend: &dyn DuvmBackend) -> Self {
        let (total, used) = backend.capacity();
        Self {
            name: backend.name().to_string(),
            tier: backend.tier().to_string(),
            total_pages: total,
            used_pages: used,
            latency_ns: backend.latency_ns(),
            healthy: backend.is_healthy(),
        }
    }
}
