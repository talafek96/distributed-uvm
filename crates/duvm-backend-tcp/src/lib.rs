//! TCP remote memory backend for duvm.
//!
//! Connects to a duvm-memserver running on a remote machine and stores/loads
//! pages over TCP. Used for cross-machine distributed memory when RDMA verbs
//! are not available or as a simpler alternative.
//!
//! **Reconnection:** If the TCP connection drops (memserver restart, network
//! blip), the backend automatically clears the broken stream and attempts to
//! reconnect on the next operation. A circuit breaker prevents reconnect storms:
//! after `MAX_CONSECUTIVE_FAILURES` failures, the backend waits at least
//! `BACKOFF_SECS` before trying again.
//!
//! Protocol: simple request/response over TCP.
//!   Request:  [op: u8][handle: u64][data: 4096 bytes (for store)]
//!   Response: [status: u8][data: 4096 bytes (for load)]

use anyhow::{Result, bail};
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::{PageBuffer, PageHandle, Tier};
use parking_lot::Mutex;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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

/// Stop trying to reconnect after this many consecutive failures.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// Minimum seconds between reconnection attempts once the circuit breaker trips.
const BACKOFF_SECS: u64 = 5;
/// TCP connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub struct TcpBackend {
    name: String,
    /// Guarded connection state: stream + circuit breaker.
    conn: Mutex<ConnState>,
    addr: String,
    backend_id: u8,
    max_pages: u64,
    pages_used: AtomicU64,
    total_store_ns: AtomicU64,
    total_load_ns: AtomicU64,
    store_count: AtomicU64,
    load_count: AtomicU64,
}

/// Connection state behind the Mutex.
struct ConnState {
    stream: Option<TcpStream>,
    /// Consecutive I/O failures since the last successful operation.
    consecutive_failures: u32,
    /// When the last reconnection attempt happened (for backoff).
    last_attempt: Option<Instant>,
}

impl TcpBackend {
    pub fn new(backend_id: u8, addr: &str) -> Self {
        Self {
            name: format!("tcp({})", addr),
            conn: Mutex::new(ConnState {
                stream: None,
                consecutive_failures: 0,
                last_attempt: None,
            }),
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

    /// Get a connected stream, attempting reconnection if disconnected.
    ///
    /// If the stream is broken (None), tries to reconnect unless the circuit
    /// breaker has tripped (too many recent failures).
    fn with_stream<F, T>(&self, op: F) -> Result<T>
    where
        F: FnOnce(&mut TcpStream) -> Result<T>,
    {
        let mut guard = self.conn.lock();

        // If disconnected, try to reconnect
        if guard.stream.is_none() {
            self.try_reconnect(&mut guard)?;
        }

        let stream = guard
            .stream
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("TCP backend not connected to {}", self.addr))?;

        match op(stream) {
            Ok(val) => {
                guard.consecutive_failures = 0;
                Ok(val)
            }
            Err(e) => {
                // I/O failed — the stream is likely broken. Clear it so the
                // next call triggers a reconnect instead of retrying on a
                // dead socket.
                guard.stream = None;
                guard.consecutive_failures += 1;
                tracing::warn!(
                    addr = %self.addr,
                    failures = guard.consecutive_failures,
                    error = %e,
                    "TCP backend I/O failed, stream cleared"
                );
                Err(e)
            }
        }
    }

    /// Attempt to establish a new TCP connection, respecting the circuit breaker.
    fn try_reconnect(&self, state: &mut ConnState) -> Result<()> {
        // Circuit breaker: too many recent failures → back off
        if state.consecutive_failures >= MAX_CONSECUTIVE_FAILURES
            && state
                .last_attempt
                .is_some_and(|last| last.elapsed() < Duration::from_secs(BACKOFF_SECS))
        {
            bail!(
                "TCP backend {} circuit breaker open ({} consecutive failures, backoff)",
                self.addr,
                state.consecutive_failures
            );
        }

        state.last_attempt = Some(Instant::now());

        match TcpStream::connect_timeout(
            &self
                .addr
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid address {}: {}", self.addr, e))?,
            CONNECT_TIMEOUT,
        ) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                tracing::info!(addr = %self.addr, "TCP backend reconnected");
                state.stream = Some(stream);
                state.consecutive_failures = 0;
                Ok(())
            }
            Err(e) => {
                state.consecutive_failures += 1;
                tracing::warn!(
                    addr = %self.addr,
                    failures = state.consecutive_failures,
                    "TCP reconnect failed: {}", e
                );
                Err(e.into())
            }
        }
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
        let stream = TcpStream::connect_timeout(
            &self
                .addr
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid address {}: {}", self.addr, e))?,
            CONNECT_TIMEOUT,
        )?;
        stream.set_nodelay(true)?;
        tracing::info!(addr = %self.addr, "Connected to remote memory server");

        let mut guard = self.conn.lock();
        guard.stream = Some(stream);
        guard.consecutive_failures = 0;
        Ok(())
    }

    fn alloc_page(&self) -> Result<PageHandle> {
        // Reserve a slot atomically before talking to the remote server.
        loop {
            let used = self.pages_used.load(Ordering::Relaxed);
            if used >= self.max_pages {
                bail!("TCP backend full: {} pages", self.max_pages);
            }
            if self
                .pages_used
                .compare_exchange(used, used + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }

        let result = self.with_stream(|stream| {
            stream.write_all(&[OP_ALLOC])?;
            stream.flush()?;

            let mut resp = [0u8; 9];
            stream.read_exact(&mut resp)?;

            if resp[0] != RESP_OK {
                bail!("alloc failed on remote server");
            }

            let offset = u64::from_le_bytes(resp[1..9].try_into().unwrap());
            Ok(PageHandle::new(self.backend_id, offset))
        });

        if result.is_err() {
            // Give back the reserved slot on failure.
            self.pages_used.fetch_sub(1, Ordering::Relaxed);
        }
        result
    }

    fn free_page(&self, handle: PageHandle) -> Result<()> {
        self.with_stream(|stream| {
            let mut req = [0u8; 9];
            req[0] = OP_FREE;
            req[1..9].copy_from_slice(&handle.offset().to_le_bytes());
            stream.write_all(&req)?;
            stream.flush()?;

            let mut resp = [0u8; 1];
            stream.read_exact(&mut resp)?;

            if resp[0] != RESP_OK {
                bail!("free failed on remote server for handle {}", handle);
            }
            Ok(())
        })?;
        self.pages_used.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    fn store_page(&self, handle: PageHandle, data: &PageBuffer) -> Result<()> {
        let start = Instant::now();

        self.with_stream(|stream| {
            let mut header = [0u8; 9];
            header[0] = OP_STORE;
            header[1..9].copy_from_slice(&handle.offset().to_le_bytes());
            stream.write_all(&header)?;
            stream.write_all(data)?;
            stream.flush()?;

            let mut resp = [0u8; 1];
            stream.read_exact(&mut resp)?;

            if resp[0] != RESP_OK {
                bail!("store failed on remote server");
            }
            Ok(())
        })?;

        let elapsed = start.elapsed().as_nanos() as u64;
        self.total_store_ns.fetch_add(elapsed, Ordering::Relaxed);
        self.store_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn load_page(&self, handle: PageHandle, buf: &mut PageBuffer) -> Result<()> {
        let start = Instant::now();

        self.with_stream(|stream| {
            let mut header = [0u8; 9];
            header[0] = OP_LOAD;
            header[1..9].copy_from_slice(&handle.offset().to_le_bytes());
            stream.write_all(&header)?;
            stream.flush()?;

            let mut status = [0u8; 1];
            stream.read_exact(&mut status)?;

            if status[0] != RESP_OK {
                bail!("load failed: page not found on remote server");
            }

            stream.read_exact(buf)?;
            Ok(())
        })?;

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
        self.conn.lock().stream.is_some()
    }

    fn shutdown(&mut self) -> Result<()> {
        self.conn.lock().stream = None;
        tracing::info!(name = self.name, "TCP backend disconnected");
        Ok(())
    }
}
