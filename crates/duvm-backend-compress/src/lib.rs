//! LZ4 compression backend for duvm.
//!
//! Compresses pages using LZ4 and stores them in local memory.
//! Equivalent to zswap but managed by duvm for unified policy decisions.
//! Typical latency: ~1-5us (compression + memcpy).

use anyhow::{Result, bail};
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::{PAGE_SIZE, PageBuffer, PageHandle, Tier};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct CompressBackend {
    name: String,
    pages: RwLock<HashMap<u64, Vec<u8>>>,
    next_offset: AtomicU64,
    max_pages: u64,
    backend_id: u8,
    bytes_stored_raw: AtomicU64,
    bytes_stored_compressed: AtomicU64,
}

impl CompressBackend {
    pub fn new(backend_id: u8) -> Self {
        Self {
            name: "compress".to_string(),
            pages: RwLock::new(HashMap::new()),
            next_offset: AtomicU64::new(0),
            max_pages: 1024 * 1024,
            backend_id,
            bytes_stored_raw: AtomicU64::new(0),
            bytes_stored_compressed: AtomicU64::new(0),
        }
    }

    /// Get compression ratio (raw / compressed). Higher is better.
    pub fn compression_ratio(&self) -> f64 {
        let raw = self.bytes_stored_raw.load(Ordering::Relaxed) as f64;
        let compressed = self.bytes_stored_compressed.load(Ordering::Relaxed) as f64;
        if compressed == 0.0 {
            1.0
        } else {
            raw / compressed
        }
    }
}

impl DuvmBackend for CompressBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn tier(&self) -> Tier {
        Tier::Compressed
    }

    fn init(&mut self, config: &BackendConfig) -> Result<()> {
        self.max_pages = config.max_pages;
        tracing::info!(
            backend = self.name,
            max_pages = self.max_pages,
            "Compression backend initialized (LZ4)"
        );
        Ok(())
    }

    fn alloc_page(&self) -> Result<PageHandle> {
        let pages = self.pages.read();
        if pages.len() as u64 >= self.max_pages {
            bail!("compress backend full: {} pages", self.max_pages);
        }
        drop(pages);
        let offset = self.next_offset.fetch_add(1, Ordering::Relaxed);
        Ok(PageHandle::new(self.backend_id, offset))
    }

    fn free_page(&self, handle: PageHandle) -> Result<()> {
        let mut pages = self.pages.write();
        if let Some(data) = pages.remove(&handle.offset()) {
            self.bytes_stored_raw
                .fetch_sub(PAGE_SIZE as u64, Ordering::Relaxed);
            self.bytes_stored_compressed
                .fetch_sub(data.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    fn store_page(&self, handle: PageHandle, data: &PageBuffer) -> Result<()> {
        let compressed = lz4_flex::compress_prepend_size(data);
        let compressed_len = compressed.len() as u64;

        let mut pages = self.pages.write();
        pages.insert(handle.offset(), compressed);

        self.bytes_stored_raw
            .fetch_add(PAGE_SIZE as u64, Ordering::Relaxed);
        self.bytes_stored_compressed
            .fetch_add(compressed_len, Ordering::Relaxed);
        Ok(())
    }

    fn load_page(&self, handle: PageHandle, buf: &mut PageBuffer) -> Result<()> {
        let pages = self.pages.read();
        match pages.get(&handle.offset()) {
            Some(compressed) => {
                let decompressed = lz4_flex::decompress_size_prepended(compressed)
                    .map_err(|e| anyhow::anyhow!("LZ4 decompression failed: {}", e))?;
                if decompressed.len() != PAGE_SIZE {
                    bail!(
                        "decompressed size mismatch: expected {}, got {}",
                        PAGE_SIZE,
                        decompressed.len()
                    );
                }
                buf.copy_from_slice(&decompressed);
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
        3000 // ~3us for LZ4 compress + decompress
    }

    fn is_healthy(&self) -> bool {
        true
    }

    fn shutdown(&mut self) -> Result<()> {
        let mut pages = self.pages.write();
        pages.clear();
        self.bytes_stored_raw.store(0, Ordering::Relaxed);
        self.bytes_stored_compressed.store(0, Ordering::Relaxed);
        tracing::info!(backend = self.name, "Compression backend shut down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_store_load_roundtrip() {
        let mut backend = CompressBackend::new(1);
        backend.init(&BackendConfig::default()).unwrap();

        let handle = backend.alloc_page().unwrap();

        // Create a page with compressible data (repeated pattern)
        let mut data = [0u8; PAGE_SIZE];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }

        backend.store_page(handle, &data).unwrap();

        let mut loaded = [0u8; PAGE_SIZE];
        backend.load_page(handle, &mut loaded).unwrap();
        assert_eq!(data, loaded);
    }

    #[test]
    fn compress_random_data() {
        let mut backend = CompressBackend::new(1);
        backend.init(&BackendConfig::default()).unwrap();

        let handle = backend.alloc_page().unwrap();

        // Random-ish data (less compressible)
        let mut data = [0u8; PAGE_SIZE];
        let mut val: u32 = 0xDEADBEEF;
        for chunk in data.chunks_exact_mut(4) {
            val = val.wrapping_mul(1103515245).wrapping_add(12345);
            chunk.copy_from_slice(&val.to_le_bytes());
        }

        backend.store_page(handle, &data).unwrap();

        let mut loaded = [0u8; PAGE_SIZE];
        backend.load_page(handle, &mut loaded).unwrap();
        assert_eq!(data, loaded);
    }

    #[test]
    fn compress_zeros_high_ratio() {
        let mut backend = CompressBackend::new(1);
        backend.init(&BackendConfig::default()).unwrap();

        let handle = backend.alloc_page().unwrap();
        let data = [0u8; PAGE_SIZE]; // all zeros = very compressible

        backend.store_page(handle, &data).unwrap();

        let ratio = backend.compression_ratio();
        // All zeros should compress very well (ratio > 10x)
        assert!(ratio > 10.0, "compression ratio for zeros: {}", ratio);
    }

    #[test]
    fn compress_free_updates_stats() {
        let mut backend = CompressBackend::new(1);
        backend.init(&BackendConfig::default()).unwrap();

        let handle = backend.alloc_page().unwrap();
        backend.store_page(handle, &[0u8; PAGE_SIZE]).unwrap();
        let (_, used_before) = backend.capacity();
        assert_eq!(used_before, 1);

        backend.free_page(handle).unwrap();
        let (_, used_after) = backend.capacity();
        assert_eq!(used_after, 0);
    }
}
