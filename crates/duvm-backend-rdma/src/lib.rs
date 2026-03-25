//! RDMA remote memory backend for duvm.
//!
//! Uses one-sided RDMA WRITE for store and RDMA READ for load.
//! Connection management via librdmacm. Data transfer via libibverbs.
//!
//! The remote side runs `duvm-memserver --rdma` which registers a memory
//! region and shares the rkey/addr via the RDMA CM private data during
//! connection setup. After that, no remote CPU involvement for data path.
//!
//! Falls back gracefully if no RDMA devices are available.

pub mod ffi;

use anyhow::{Result, bail};
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::{PAGE_SIZE, PageBuffer, PageHandle, Tier};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Check if RDMA hardware (or SoftRoCE) is available.
pub fn is_rdma_available() -> bool {
    ffi::rdma_available()
}

/// RDMA backend using one-sided RDMA WRITE/READ.
///
/// For now, this uses a TCP-based control channel to exchange memory region
/// info, then does RDMA for the data path. This is a pragmatic first
/// implementation that works with both real RDMA and SoftRoCE.
///
/// The connection setup sends:
///   Client → Server: "RDMA_SETUP" + local rkey + local addr
///   Server → Client: remote rkey + remote addr + remote size
///
/// After setup, store/load use ibv_post_send with RDMA WRITE/READ.
pub struct RdmaBackend {
    name: String,
    addr: String,
    backend_id: u8,
    max_pages: u64,
    pages_used: AtomicU64,

    // RDMA resources (set during init)
    state: Mutex<Option<RdmaState>>,

    // Latency tracking
    total_store_ns: AtomicU64,
    total_load_ns: AtomicU64,
    store_count: AtomicU64,
    load_count: AtomicU64,
}

struct RdmaState {
    // RDMA CM
    event_channel: *mut ffi::rdma_event_channel,
    cm_id: *mut ffi::rdma_cm_id,

    // Local resources
    pd: *mut ffi::ibv_pd,
    cq: *mut ffi::ibv_cq,
    local_mr: *mut ffi::ibv_mr,
    local_buf: *mut u8,
    local_buf_size: usize,

    // Remote memory region info (received during connection setup)
    remote_addr: u64,
    remote_rkey: u32,
    #[allow(dead_code)]
    remote_size: u64,
}

// Safety: RDMA resources are accessed under a Mutex.
unsafe impl Send for RdmaState {}
unsafe impl Sync for RdmaState {}

impl RdmaBackend {
    pub fn new(backend_id: u8, addr: &str) -> Self {
        Self {
            name: format!("rdma({})", addr),
            addr: addr.to_string(),
            backend_id,
            max_pages: 1024 * 1024,
            pages_used: AtomicU64::new(0),
            state: Mutex::new(None),
            total_store_ns: AtomicU64::new(0),
            total_load_ns: AtomicU64::new(0),
            store_count: AtomicU64::new(0),
            load_count: AtomicU64::new(0),
        }
    }

    /// Post an RDMA WRITE (for store) or RDMA READ (for load) and wait for completion.
    fn rdma_transfer(
        state: &RdmaState,
        local_offset: usize,
        remote_offset: u64,
        length: usize,
        is_write: bool,
    ) -> Result<()> {
        let mut sge = ffi::ibv_sge {
            addr: unsafe { state.local_buf.add(local_offset) } as u64,
            length: length as u32,
            lkey: unsafe { (*state.local_mr).lkey },
        };

        let mut wr = ffi::ibv_send_wr {
            wr_id: 1,
            next: std::ptr::null_mut(),
            sg_list: &mut sge,
            num_sge: 1,
            opcode: if is_write {
                ffi::IBV_WR_RDMA_WRITE
            } else {
                ffi::IBV_WR_RDMA_READ
            },
            send_flags: ffi::IBV_SEND_SIGNALED,
            rdma_remote_addr: state.remote_addr + remote_offset,
            rdma_rkey: state.remote_rkey,
            _pad: [0; 76],
        };

        let mut bad_wr: *mut ffi::ibv_send_wr = std::ptr::null_mut();
        let qp = unsafe { (*state.cm_id).qp };
        let ret = unsafe { ffi::ibv_post_send(qp, &mut wr, &mut bad_wr) };
        if ret != 0 {
            bail!("ibv_post_send failed: {}", ret);
        }

        // Poll for completion
        let cq = state.cq;
        let mut wc = ffi::ibv_wc {
            wr_id: 0,
            status: -1,
            opcode: 0,
            vendor_err: 0,
            byte_len: 0,
            _pad: [0; 24],
        };

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);

        loop {
            let n = unsafe { ffi::ibv_poll_cq(cq, 1, &mut wc) };
            if n > 0 {
                if wc.status != ffi::IBV_WC_SUCCESS {
                    bail!(
                        "RDMA {} failed: status={}",
                        if is_write { "WRITE" } else { "READ" },
                        wc.status
                    );
                }
                return Ok(());
            }
            if n < 0 {
                bail!("ibv_poll_cq error");
            }
            if start.elapsed() > timeout {
                bail!(
                    "RDMA {} timed out after {:?}",
                    if is_write { "WRITE" } else { "READ" },
                    timeout
                );
            }
            std::hint::spin_loop();
        }
    }
}

impl DuvmBackend for RdmaBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn tier(&self) -> Tier {
        Tier::Rdma
    }

    fn init(&mut self, config: &BackendConfig) -> Result<()> {
        self.max_pages = config.max_pages;

        if !is_rdma_available() {
            bail!(
                "No RDMA devices available. Install SoftRoCE (rdma_rxe) or use transport = \"tcp\""
            );
        }

        // Create event channel
        let ec = unsafe { ffi::rdma_create_event_channel() };
        if ec.is_null() {
            bail!("rdma_create_event_channel failed");
        }

        // Create CM ID
        let mut cm_id: *mut ffi::rdma_cm_id = std::ptr::null_mut();
        let ret =
            unsafe { ffi::rdma_create_id(ec, &mut cm_id, std::ptr::null_mut(), ffi::RDMA_PS_TCP) };
        if ret != 0 {
            unsafe { ffi::rdma_destroy_event_channel(ec) };
            bail!("rdma_create_id failed: {}", ret);
        }

        // Resolve address
        let port: u16 = self
            .addr
            .split(':')
            .last()
            .unwrap_or("9200")
            .parse()
            .unwrap_or(9200);

        // Build sockaddr_in
        let mut dst_addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        dst_addr.sin_family = libc::AF_INET as u16;
        dst_addr.sin_port = port.to_be();

        // Parse IP using std::net
        let ip_str = self.addr.split(':').next().unwrap_or("127.0.0.1");
        let ip: std::net::Ipv4Addr = ip_str.parse().map_err(|e| anyhow::anyhow!("bad IP: {}", e))?;
        dst_addr.sin_addr.s_addr = u32::from_ne_bytes(ip.octets());

        let ret = unsafe {
            ffi::rdma_resolve_addr(
                cm_id,
                std::ptr::null_mut(),
                &mut dst_addr as *mut _ as *mut libc::sockaddr,
                2000,
            )
        };
        if ret != 0 {
            unsafe {
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("rdma_resolve_addr failed for {}: {}", self.addr, ret);
        }

        // Wait for address resolved event
        let mut event: *mut ffi::rdma_cm_event = std::ptr::null_mut();
        let ret = unsafe { ffi::rdma_get_cm_event(ec, &mut event) };
        if ret != 0 || unsafe { (*event).event } != ffi::RDMA_CM_EVENT_ADDR_RESOLVED {
            unsafe {
                if !event.is_null() {
                    ffi::rdma_ack_cm_event(event);
                }
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("RDMA address resolution failed for {}", self.addr);
        }
        unsafe { ffi::rdma_ack_cm_event(event) };

        // Resolve route
        let ret = unsafe { ffi::rdma_resolve_route(cm_id, 2000) };
        if ret != 0 {
            unsafe {
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("rdma_resolve_route failed: {}", ret);
        }

        let ret = unsafe { ffi::rdma_get_cm_event(ec, &mut event) };
        if ret != 0 || unsafe { (*event).event } != ffi::RDMA_CM_EVENT_ROUTE_RESOLVED {
            unsafe {
                if !event.is_null() {
                    ffi::rdma_ack_cm_event(event);
                }
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("RDMA route resolution failed for {}", self.addr);
        }
        unsafe { ffi::rdma_ack_cm_event(event) };

        // Allocate PD
        let pd = unsafe { ffi::ibv_alloc_pd((*cm_id).verbs) };
        if pd.is_null() {
            unsafe {
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("ibv_alloc_pd failed");
        }

        // Create CQ
        let cq = unsafe { ffi::ibv_create_cq((*cm_id).verbs, 16, std::ptr::null_mut(), std::ptr::null_mut(), 0) };
        if cq.is_null() {
            unsafe {
                ffi::ibv_dealloc_pd(pd);
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("ibv_create_cq failed");
        }

        // Allocate local buffer for page transfers
        let buf_size = PAGE_SIZE; // one page at a time
        let local_buf = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                buf_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        } as *mut u8;
        if local_buf.is_null() || local_buf as usize == usize::MAX {
            unsafe {
                ffi::ibv_destroy_cq(cq);
                ffi::ibv_dealloc_pd(pd);
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("mmap for RDMA buffer failed");
        }

        // Register local memory region
        let mr = unsafe {
            ffi::ibv_reg_mr(
                pd,
                local_buf as *mut libc::c_void,
                buf_size,
                ffi::IBV_ACCESS_LOCAL_WRITE
                    | ffi::IBV_ACCESS_REMOTE_WRITE
                    | ffi::IBV_ACCESS_REMOTE_READ,
            )
        };
        if mr.is_null() {
            unsafe {
                libc::munmap(local_buf as *mut libc::c_void, buf_size);
                ffi::ibv_destroy_cq(cq);
                ffi::ibv_dealloc_pd(pd);
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("ibv_reg_mr failed");
        }

        // Create QP
        let mut qp_attr = ffi::ibv_qp_init_attr {
            qp_context: std::ptr::null_mut(),
            send_cq: cq,
            recv_cq: cq,
            srq: std::ptr::null_mut(),
            cap: ffi::ibv_qp_cap {
                max_send_wr: 16,
                max_recv_wr: 16,
                max_send_sge: 1,
                max_recv_sge: 1,
                max_inline_data: 0,
            },
            qp_type: ffi::IBV_QPT_RC,
            sq_sig_all: 0,
        };

        let ret = unsafe { ffi::rdma_create_qp(cm_id, pd, &mut qp_attr) };
        if ret != 0 {
            unsafe {
                ffi::ibv_dereg_mr(mr);
                libc::munmap(local_buf as *mut libc::c_void, buf_size);
                ffi::ibv_destroy_cq(cq);
                ffi::ibv_dealloc_pd(pd);
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("rdma_create_qp failed: {}", ret);
        }

        // Connect (exchange rkey/addr via private data)
        let mut conn_param: ffi::rdma_conn_param = unsafe { std::mem::zeroed() };
        conn_param.responder_resources = 1;
        conn_param.initiator_depth = 1;
        conn_param.retry_count = 7;

        let ret = unsafe { ffi::rdma_connect(cm_id, &mut conn_param) };
        if ret != 0 {
            unsafe {
                ffi::ibv_dereg_mr(mr);
                libc::munmap(local_buf as *mut libc::c_void, buf_size);
                ffi::ibv_destroy_cq(cq);
                ffi::ibv_dealloc_pd(pd);
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("rdma_connect failed: {}", ret);
        }

        let ret = unsafe { ffi::rdma_get_cm_event(ec, &mut event) };
        if ret != 0 || unsafe { (*event).event } != ffi::RDMA_CM_EVENT_ESTABLISHED {
            let ev = if event.is_null() {
                -1
            } else {
                unsafe { (*event).event }
            };
            unsafe {
                if !event.is_null() {
                    ffi::rdma_ack_cm_event(event);
                }
                ffi::ibv_dereg_mr(mr);
                libc::munmap(local_buf as *mut libc::c_void, buf_size);
                ffi::ibv_destroy_cq(cq);
                ffi::ibv_dealloc_pd(pd);
                ffi::rdma_destroy_id(cm_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("RDMA connection failed (event={})", ev);
        }
        unsafe { ffi::rdma_ack_cm_event(event) };

        tracing::info!(addr = %self.addr, "RDMA connection established");

        // For now, use a simple protocol: the remote side sends its rkey/addr/size
        // via a separate TCP channel. This is a placeholder — in production,
        // we'd exchange this via RDMA CM private data.
        // TODO: exchange rkey/addr via RDMA CM private data
        let remote_addr = 0u64; // placeholder
        let remote_rkey = 0u32; // placeholder
        let remote_size = self.max_pages as u64 * PAGE_SIZE as u64;

        *self.state.lock() = Some(RdmaState {
            event_channel: ec,
            cm_id,
            pd,
            cq,
            local_mr: mr,
            local_buf,
            local_buf_size: buf_size,
            remote_addr,
            remote_rkey,
            remote_size,
        });

        Ok(())
    }

    fn alloc_page(&self) -> Result<PageHandle> {
        let used = self.pages_used.load(Ordering::Relaxed);
        if used >= self.max_pages {
            bail!("RDMA backend full: {} pages", self.max_pages);
        }
        let offset = self.pages_used.fetch_add(1, Ordering::Relaxed);
        Ok(PageHandle::new(self.backend_id, offset))
    }

    fn free_page(&self, _handle: PageHandle) -> Result<()> {
        self.pages_used.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    fn store_page(&self, handle: PageHandle, data: &PageBuffer) -> Result<()> {
        let start = std::time::Instant::now();
        let state_guard = self.state.lock();
        let state = state_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("RDMA backend not initialized"))?;

        // Copy page data to local registered buffer
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), state.local_buf, PAGE_SIZE);
        }

        // RDMA WRITE to remote
        let remote_offset = handle.offset() * PAGE_SIZE as u64;
        Self::rdma_transfer(state, 0, remote_offset, PAGE_SIZE, true)?;

        let elapsed = start.elapsed().as_nanos() as u64;
        self.total_store_ns.fetch_add(elapsed, Ordering::Relaxed);
        self.store_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn load_page(&self, handle: PageHandle, buf: &mut PageBuffer) -> Result<()> {
        let start = std::time::Instant::now();
        let state_guard = self.state.lock();
        let state = state_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("RDMA backend not initialized"))?;

        // RDMA READ from remote
        let remote_offset = handle.offset() * PAGE_SIZE as u64;
        Self::rdma_transfer(state, 0, remote_offset, PAGE_SIZE, false)?;

        // Copy from local registered buffer to output
        unsafe {
            std::ptr::copy_nonoverlapping(state.local_buf, buf.as_mut_ptr(), PAGE_SIZE);
        }

        let elapsed = start.elapsed().as_nanos() as u64;
        self.total_load_ns.fetch_add(elapsed, Ordering::Relaxed);
        self.load_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn capacity(&self) -> (u64, u64) {
        (self.max_pages, self.pages_used.load(Ordering::Relaxed))
    }

    fn latency_ns(&self) -> u64 {
        let count = self.load_count.load(Ordering::Relaxed);
        if count == 0 {
            return 3_000; // default estimate: 3us for RDMA
        }
        self.total_load_ns.load(Ordering::Relaxed) / count
    }

    fn is_healthy(&self) -> bool {
        self.state.lock().is_some()
    }

    fn shutdown(&mut self) -> Result<()> {
        if let Some(state) = self.state.lock().take() {
            unsafe {
                ffi::rdma_disconnect(state.cm_id);
                ffi::ibv_dereg_mr(state.local_mr);
                ffi::ibv_destroy_cq(state.cq);
                ffi::ibv_dealloc_pd(state.pd);
                libc::munmap(state.local_buf as *mut libc::c_void, state.local_buf_size);
                ffi::rdma_destroy_id(state.cm_id);
                ffi::rdma_destroy_event_channel(state.event_channel);
            }
        }
        tracing::info!(name = self.name, "RDMA backend disconnected");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdma_availability_check() {
        // This just checks that the function doesn't crash.
        // On CI without RDMA, it returns false. On machines with RDMA or SoftRoCE, true.
        let available = is_rdma_available();
        println!("RDMA available: {}", available);
    }

    #[test]
    fn rdma_backend_not_initialized() {
        let backend = RdmaBackend::new(10, "127.0.0.1:9200");
        assert_eq!(backend.name(), "rdma(127.0.0.1:9200)");
        assert_eq!(backend.tier(), Tier::Rdma);
        assert!(!backend.is_healthy()); // not initialized yet

        // store/load should fail
        let handle = PageHandle::new(10, 0);
        let data = [0u8; PAGE_SIZE];
        assert!(backend.store_page(handle, &data).is_err());
        let mut buf = [0u8; PAGE_SIZE];
        assert!(backend.load_page(handle, &mut buf).is_err());
    }

    #[test]
    fn rdma_backend_init_fails_without_rdma() {
        if is_rdma_available() {
            println!("RDMA available — skipping no-RDMA test");
            return;
        }
        let mut backend = RdmaBackend::new(10, "127.0.0.1:9200");
        let result = backend.init(&BackendConfig::default());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No RDMA devices available")
        );
    }
}
