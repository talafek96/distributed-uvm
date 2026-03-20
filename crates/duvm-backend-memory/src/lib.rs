//! In-memory backend for duvm.
//!
//! Stores pages in a simple HashMap in local memory. Used for testing and
//! development when no real remote memory hardware is available.
//! Also serves as a reference implementation for the backend trait.

use anyhow::{Result, bail};
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::{PAGE_SIZE, PageBuffer, PageHandle, Tier};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct MemoryBackend {
    name: String,
    pages: RwLock<HashMap<u64, Box<PageBuffer>>>,
    next_offset: AtomicU64,
    max_pages: u64,
    backend_id: u8,
}

impl MemoryBackend {
    pub fn new(backend_id: u8) -> Self {
        Self {
            name: "memory".to_string(),
            pages: RwLock::new(HashMap::new()),
            next_offset: AtomicU64::new(0),
            max_pages: 1024 * 1024,
            backend_id,
        }
    }
}

impl DuvmBackend for MemoryBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn tier(&self) -> Tier {
        Tier::Compressed // closest equivalent for in-memory
    }

    fn init(&mut self, config: &BackendConfig) -> Result<()> {
        self.max_pages = config.max_pages;
        tracing::info!(
            backend = self.name,
            max_pages = self.max_pages,
            "Memory backend initialized"
        );
        Ok(())
    }

    fn alloc_page(&self) -> Result<PageHandle> {
        let pages = self.pages.read();
        if pages.len() as u64 >= self.max_pages {
            bail!("memory backend full: {} pages", self.max_pages);
        }
        drop(pages);
        let offset = self.next_offset.fetch_add(1, Ordering::Relaxed);
        Ok(PageHandle::new(self.backend_id, offset))
    }

    fn free_page(&self, handle: PageHandle) -> Result<()> {
        let mut pages = self.pages.write();
        pages.remove(&handle.offset());
        Ok(())
    }

    fn store_page(&self, handle: PageHandle, data: &PageBuffer) -> Result<()> {
        let mut page_data = Box::new([0u8; PAGE_SIZE]);
        page_data.copy_from_slice(data);
        let mut pages = self.pages.write();
        pages.insert(handle.offset(), page_data);
        Ok(())
    }

    fn load_page(&self, handle: PageHandle, buf: &mut PageBuffer) -> Result<()> {
        let pages = self.pages.read();
        match pages.get(&handle.offset()) {
            Some(data) => {
                buf.copy_from_slice(data.as_ref());
                Ok(())
            }
            None => bail!("page not found: {}", handle),
        }
    }

    fn capacity(&self) -> (u64, u64) {
        let pages = self.pages.read();
        (self.max_pages, pages.len() as u64)
    }

    fn latency_ns(&self) -> u64 {
        100 // ~100ns for local memory access
    }

    fn is_healthy(&self) -> bool {
        true
    }

    fn shutdown(&mut self) -> Result<()> {
        let mut pages = self.pages.write();
        pages.clear();
        tracing::info!(backend = self.name, "Memory backend shut down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_load_page() {
        let mut backend = MemoryBackend::new(0);
        backend.init(&BackendConfig::default()).unwrap();

        let handle = backend.alloc_page().unwrap();
        let mut data = [0u8; PAGE_SIZE];
        data[0] = 0xDE;
        data[1] = 0xAD;
        data[4095] = 0xFF;
        backend.store_page(handle, &data).unwrap();

        let mut loaded = [0u8; PAGE_SIZE];
        backend.load_page(handle, &mut loaded).unwrap();
        assert_eq!(loaded[0], 0xDE);
        assert_eq!(loaded[1], 0xAD);
        assert_eq!(loaded[4095], 0xFF);
    }

    #[test]
    fn free_page_removes_data() {
        let mut backend = MemoryBackend::new(0);
        backend.init(&BackendConfig::default()).unwrap();

        let handle = backend.alloc_page().unwrap();
        let data = [42u8; PAGE_SIZE];
        backend.store_page(handle, &data).unwrap();
        backend.free_page(handle).unwrap();

        let mut buf = [0u8; PAGE_SIZE];
        assert!(backend.load_page(handle, &mut buf).is_err());
    }

    #[test]
    fn capacity_tracking() {
        let mut backend = MemoryBackend::new(0);
        backend
            .init(&BackendConfig {
                max_pages: 10,
                ..Default::default()
            })
            .unwrap();

        let (total, used) = backend.capacity();
        assert_eq!(total, 10);
        assert_eq!(used, 0);

        let h = backend.alloc_page().unwrap();
        backend.store_page(h, &[0u8; PAGE_SIZE]).unwrap();
        let (_, used) = backend.capacity();
        assert_eq!(used, 1);
    }

    #[test]
    fn batch_store_and_load() {
        let mut backend = MemoryBackend::new(0);
        backend.init(&BackendConfig::default()).unwrap();

        let h1 = backend.alloc_page().unwrap();
        let h2 = backend.alloc_page().unwrap();
        let data1 = [1u8; PAGE_SIZE];
        let data2 = [2u8; PAGE_SIZE];

        backend.store_pages(&[(h1, &data1), (h2, &data2)]).unwrap();

        let mut buf1 = [0u8; PAGE_SIZE];
        let mut buf2 = [0u8; PAGE_SIZE];
        backend
            .load_pages(&mut [(h1, &mut buf1), (h2, &mut buf2)])
            .unwrap();
        assert_eq!(buf1[0], 1);
        assert_eq!(buf2[0], 2);
    }

    #[test]
    fn health_check() {
        let backend = MemoryBackend::new(0);
        assert!(backend.is_healthy());
    }
}
