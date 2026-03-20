//! Memory pool: the main interface for applications using libduvm.

use anyhow::Result;
use duvm_backend_compress::CompressBackend;
use duvm_backend_memory::MemoryBackend;
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::{PAGE_SIZE, PageBuffer, PageHandle};
use std::collections::HashMap;

/// A duvm memory pool that manages distributed page storage.
///
/// In standalone mode, the pool directly manages backends.
/// In daemon mode, it communicates with duvm-daemon via Unix socket.
pub struct Pool {
    backends: HashMap<u8, Box<dyn DuvmBackend>>,
}

impl Pool {
    /// Create a standalone pool with in-memory and compression backends.
    /// This mode doesn't require a running daemon.
    pub fn standalone() -> Result<Self> {
        let mut backends: HashMap<u8, Box<dyn DuvmBackend>> = HashMap::new();

        let mut mem = MemoryBackend::new(0);
        mem.init(&BackendConfig::default())?;
        backends.insert(0, Box::new(mem));

        let mut comp = CompressBackend::new(1);
        comp.init(&BackendConfig::default())?;
        backends.insert(1, Box::new(comp));

        Ok(Self { backends })
    }

    /// Store a page and return its handle.
    pub fn store(&self, data: &PageBuffer) -> Result<PageHandle> {
        let backend = self
            .backends
            .get(&1) // prefer compression
            .or_else(|| self.backends.get(&0))
            .ok_or_else(|| anyhow::anyhow!("no backends available"))?;

        let handle = backend.alloc_page()?;
        backend.store_page(handle, data)?;

        Ok(handle)
    }

    /// Load a page by its handle.
    pub fn load(&self, handle: PageHandle) -> Result<PageBuffer> {
        let backend_id = handle.backend_id();
        let backend = self
            .backends
            .get(&backend_id)
            .ok_or_else(|| anyhow::anyhow!("backend {} not found", backend_id))?;

        let mut buf = [0u8; PAGE_SIZE];
        backend.load_page(handle, &mut buf)?;
        Ok(buf)
    }

    /// Free a page.
    pub fn free(&self, handle: PageHandle) -> Result<()> {
        let backend_id = handle.backend_id();
        let backend = self
            .backends
            .get(&backend_id)
            .ok_or_else(|| anyhow::anyhow!("backend {} not found for free", backend_id))?;
        backend.free_page(handle)?;
        Ok(())
    }

    /// Get capacity info: (total_pages, used_pages) across all backends.
    pub fn capacity(&self) -> (u64, u64) {
        let mut total = 0u64;
        let mut used = 0u64;
        for backend in self.backends.values() {
            let (t, u) = backend.capacity();
            total = total.saturating_add(t);
            used = used.saturating_add(u);
        }
        (total, used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_standalone_store_load() {
        let pool = Pool::standalone().unwrap();

        let mut data = [0u8; PAGE_SIZE];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }

        let handle = pool.store(&data).unwrap();
        let loaded = pool.load(handle).unwrap();
        assert_eq!(data, loaded);
    }

    #[test]
    fn pool_free_page() {
        let pool = Pool::standalone().unwrap();
        let data = [42u8; PAGE_SIZE];
        let handle = pool.store(&data).unwrap();
        pool.free(handle).unwrap();
        // After free, load should fail
        assert!(pool.load(handle).is_err());
    }

    #[test]
    fn pool_capacity() {
        let pool = Pool::standalone().unwrap();
        let (total, used) = pool.capacity();
        assert!(total > 0);
        assert_eq!(used, 0);

        let data = [0u8; PAGE_SIZE];
        pool.store(&data).unwrap();
        let (_, used) = pool.capacity();
        assert_eq!(used, 1);
    }

    #[test]
    fn pool_multiple_pages() {
        let pool = Pool::standalone().unwrap();

        let mut handles = Vec::new();
        for i in 0u8..10 {
            let data = [i; PAGE_SIZE];
            handles.push(pool.store(&data).unwrap());
        }

        for (i, handle) in handles.iter().enumerate() {
            let loaded = pool.load(*handle).unwrap();
            assert_eq!(loaded[0], i as u8);
        }
    }
}
