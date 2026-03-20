//! TCP remote memory backend for duvm.
//!
//! Connects to a duvm-memserver running on a remote machine and stores/loads
//! pages over TCP. Used for cross-machine distributed memory when RDMA verbs
//! are not available or as a simpler alternative.
//!
//! Protocol: simple request/response over TCP.
//!   Request:  [op: u8][handle: u64][data: 4096 bytes (for store)]
//!   Response: [status: u8][data: 4096 bytes (for load)]

use anyhow::{Result, bail};
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::{PageBuffer, PageHandle, Tier};
use parking_lot::{Mutex, MutexGuard};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

// Wire protocol opcodes
const OP_STORE: u8 = 1;
const OP_LOAD: u8 = 2;
const OP_FREE: u8 = 3;
const OP_ALLOC: u8 = 4;
#[allow(dead_code)]
const OP_STATUS: u8 = 5;

const RESP_OK: u8 = 0;
#[allow(dead_code)]
const RESP_ERR: u8 = 1;

pub struct TcpBackend {
    name: String,
    stream: Mutex<Option<TcpStream>>,
    addr: String,
    backend_id: u8,
    max_pages: u64,
    pages_used: AtomicU64,
    total_store_ns: AtomicU64,
    total_load_ns: AtomicU64,
    store_count: AtomicU64,
    load_count: AtomicU64,
}

impl TcpBackend {
    pub fn new(backend_id: u8, addr: &str) -> Self {
        Self {
            name: format!("tcp({})", addr),
            stream: Mutex::new(None),
            addr: addr.to_string(),
            backend_id,
            max_pages: 1024 * 1024,
            pages_used: AtomicU64::new(0),
            total_store_ns: AtomicU64::new(0),
            total_load_ns: AtomicU64::new(0),
            store_count: AtomicU64::new(0),
            load_count: AtomicU64::new(0),
        }
    }

    fn get_stream(&self) -> Result<MutexGuard<'_, Option<TcpStream>>> {
        let guard = self.stream.lock();
        if guard.is_none() {
            bail!("TCP backend not connected to {}", self.addr);
        }
        Ok(guard)
    }

    /// Average store latency in nanoseconds.
    pub fn avg_store_ns(&self) -> u64 {
        let count = self.store_count.load(Ordering::Relaxed);
        if count == 0 {
            return 0;
        }
        self.total_store_ns.load(Ordering::Relaxed) / count
    }

    /// Average load latency in nanoseconds.
    pub fn avg_load_ns(&self) -> u64 {
        let count = self.load_count.load(Ordering::Relaxed);
        if count == 0 {
            return 0;
        }
        self.total_load_ns.load(Ordering::Relaxed) / count
    }
}

impl DuvmBackend for TcpBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn tier(&self) -> Tier {
        Tier::Rdma // TCP is the closest equivalent tier
    }

    fn init(&mut self, config: &BackendConfig) -> Result<()> {
        self.max_pages = config.max_pages;

        // Connect to remote memory server
        tracing::info!(addr = %self.addr, "Connecting to remote memory server...");
        let stream = TcpStream::connect(&self.addr)?;
        stream.set_nodelay(true)?;
        tracing::info!(addr = %self.addr, "Connected to remote memory server");

        *self.stream.lock() = Some(stream);
        Ok(())
    }

    fn alloc_page(&self) -> Result<PageHandle> {
        let mut guard = self.get_stream()?;
        let stream = guard.as_mut().unwrap();

        // Send alloc request
        stream.write_all(&[OP_ALLOC])?;
        stream.flush()?;

        // Read response: [status: u8][offset: u64]
        let mut resp = [0u8; 9];
        stream.read_exact(&mut resp)?;

        if resp[0] != RESP_OK {
            bail!("alloc failed on remote server");
        }

        let offset = u64::from_le_bytes(resp[1..9].try_into().unwrap());
        self.pages_used.fetch_add(1, Ordering::Relaxed);
        Ok(PageHandle::new(self.backend_id, offset))
    }

    fn free_page(&self, handle: PageHandle) -> Result<()> {
        let mut guard = self.get_stream()?;
        let stream = guard.as_mut().unwrap();

        let mut req = [0u8; 9];
        req[0] = OP_FREE;
        req[1..9].copy_from_slice(&handle.offset().to_le_bytes());
        stream.write_all(&req)?;
        stream.flush()?;

        let mut resp = [0u8; 1];
        stream.read_exact(&mut resp)?;

        if resp[0] == RESP_OK {
            self.pages_used.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn store_page(&self, handle: PageHandle, data: &PageBuffer) -> Result<()> {
        let start = Instant::now();

        let mut guard = self.get_stream()?;
        let stream = guard.as_mut().unwrap();

        // Send: [OP_STORE][offset: u64][data: 4096]
        let mut header = [0u8; 9];
        header[0] = OP_STORE;
        header[1..9].copy_from_slice(&handle.offset().to_le_bytes());
        stream.write_all(&header)?;
        stream.write_all(data)?;
        stream.flush()?;

        // Read response
        let mut resp = [0u8; 1];
        stream.read_exact(&mut resp)?;

        let elapsed = start.elapsed().as_nanos() as u64;
        self.total_store_ns.fetch_add(elapsed, Ordering::Relaxed);
        self.store_count.fetch_add(1, Ordering::Relaxed);

        if resp[0] != RESP_OK {
            bail!("store failed on remote server");
        }
        Ok(())
    }

    fn load_page(&self, handle: PageHandle, buf: &mut PageBuffer) -> Result<()> {
        let start = Instant::now();

        let mut guard = self.get_stream()?;
        let stream = guard.as_mut().unwrap();

        // Send: [OP_LOAD][offset: u64]
        let mut header = [0u8; 9];
        header[0] = OP_LOAD;
        header[1..9].copy_from_slice(&handle.offset().to_le_bytes());
        stream.write_all(&header)?;
        stream.flush()?;

        // Read response: [status: u8][data: 4096]
        let mut status = [0u8; 1];
        stream.read_exact(&mut status)?;

        if status[0] != RESP_OK {
            bail!("load failed: page not found on remote server");
        }

        stream.read_exact(buf)?;

        let elapsed = start.elapsed().as_nanos() as u64;
        self.total_load_ns.fetch_add(elapsed, Ordering::Relaxed);
        self.load_count.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    fn capacity(&self) -> (u64, u64) {
        (self.max_pages, self.pages_used.load(Ordering::Relaxed))
    }

    fn latency_ns(&self) -> u64 {
        let avg = self.avg_load_ns();
        if avg > 0 { avg } else { 250_000 } // default estimate: 250us for TCP
    }

    fn is_healthy(&self) -> bool {
        self.stream.lock().is_some()
    }

    fn shutdown(&mut self) -> Result<()> {
        *self.stream.lock() = None;
        tracing::info!(name = self.name, "TCP backend disconnected");
        Ok(())
    }
}
