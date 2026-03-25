//! RDMA server: listens for RDMA CM connections, registers a memory region,
//! and exposes it to clients via one-sided RDMA WRITE/READ.
//!
//! The server pre-allocates a contiguous buffer (max_pages * PAGE_SIZE),
//! registers it as an RDMA memory region, and hands out per-client slices
//! via RDMA CM private data during the accept handshake.
//!
//! After connection setup, the server has no data-path involvement — the NIC
//! handles RDMA WRITE/READ directly to the registered buffer.

use crate::{ffi, RdmaHandshake, HANDSHAKE_SIZE};
use anyhow::{Result, bail};
use duvm_common::page::PAGE_SIZE;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// RDMA memory server. Allocates a contiguous buffer, registers it with the NIC,
/// and accepts RDMA CM connections. Clients get the rkey and their slice address
/// via private data in the accept handshake.
pub struct RdmaMemServer {
    max_pages: u64,
    pages_per_client: u64,
    port: u16,
    running: Arc<AtomicBool>,
}

struct ServerState {
    ec: *mut ffi::rdma_event_channel,
    listen_id: *mut ffi::rdma_cm_id,
    pd: *mut ffi::ibv_pd,
    mr: *mut ffi::ibv_mr,
    buf: *mut u8,
    buf_size: usize,
    next_client_offset: AtomicU64,
    max_pages: u64,
    pages_per_client: u64,
}

// Safety: RDMA resources are only accessed from the server thread.
unsafe impl Send for ServerState {}
unsafe impl Sync for ServerState {}

impl RdmaMemServer {
    pub fn new(port: u16, max_pages: u64, pages_per_client: u64) -> Self {
        Self {
            max_pages,
            pages_per_client,
            port,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the RDMA server. Blocks the calling thread.
    pub fn run(&self) -> Result<()> {
        if !crate::is_rdma_available() {
            bail!("No RDMA devices available for memserver");
        }

        self.running.store(true, Ordering::SeqCst);

        // Create event channel
        let ec = unsafe { ffi::rdma_create_event_channel() };
        if ec.is_null() {
            bail!("rdma_create_event_channel failed");
        }

        // Create listen ID
        let mut listen_id: *mut ffi::rdma_cm_id = std::ptr::null_mut();
        let ret = unsafe {
            ffi::rdma_create_id(ec, &mut listen_id, std::ptr::null_mut(), ffi::RDMA_PS_TCP)
        };
        if ret != 0 {
            unsafe { ffi::rdma_destroy_event_channel(ec) };
            bail!("rdma_create_id failed: {}", ret);
        }

        // Bind to port
        let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        addr.sin_family = libc::AF_INET as u16;
        addr.sin_port = self.port.to_be();
        // sin_addr = 0 means INADDR_ANY

        let ret = unsafe {
            ffi::rdma_bind_addr(listen_id, &mut addr as *mut _ as *mut libc::sockaddr)
        };
        if ret != 0 {
            unsafe {
                ffi::rdma_destroy_id(listen_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("rdma_bind_addr failed: {}", ret);
        }

        // Listen
        let ret = unsafe { ffi::rdma_listen(listen_id, 16) };
        if ret != 0 {
            unsafe {
                ffi::rdma_destroy_id(listen_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("rdma_listen failed: {}", ret);
        }

        // Get the verbs context from the listen ID (available after bind)
        let verbs = unsafe { (*listen_id).verbs };
        if verbs.is_null() {
            unsafe {
                ffi::rdma_destroy_id(listen_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("rdma_bind_addr did not set verbs context");
        }

        // Allocate PD
        let pd = unsafe { ffi::ibv_alloc_pd(verbs) };
        if pd.is_null() {
            unsafe {
                ffi::rdma_destroy_id(listen_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("ibv_alloc_pd failed");
        }

        // Allocate contiguous buffer for all pages
        let buf_size = self.max_pages as usize * PAGE_SIZE;
        let buf = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                buf_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
                -1,
                0,
            )
        } as *mut u8;
        if buf.is_null() || buf as usize == usize::MAX {
            unsafe {
                ffi::ibv_dealloc_pd(pd);
                ffi::rdma_destroy_id(listen_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!(
                "mmap for RDMA buffer failed ({} pages, {} bytes)",
                self.max_pages,
                buf_size
            );
        }

        // Register the entire buffer as one MR
        let mr = unsafe {
            ffi::ibv_reg_mr(
                pd,
                buf as *mut libc::c_void,
                buf_size,
                ffi::IBV_ACCESS_LOCAL_WRITE
                    | ffi::IBV_ACCESS_REMOTE_WRITE
                    | ffi::IBV_ACCESS_REMOTE_READ,
            )
        };
        if mr.is_null() {
            unsafe {
                libc::munmap(buf as *mut libc::c_void, buf_size);
                ffi::ibv_dealloc_pd(pd);
                ffi::rdma_destroy_id(listen_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("ibv_reg_mr failed for {} byte buffer", buf_size);
        }

        let rkey = unsafe { (*mr).rkey };
        eprintln!(
            "  RDMA server: buffer={} pages ({:.1} MB), rkey=0x{:08x}, addr=0x{:x}",
            self.max_pages,
            buf_size as f64 / 1e6,
            rkey,
            buf as u64
        );
        eprintln!("  RDMA server: listening on port {}", self.port);

        let state = ServerState {
            ec,
            listen_id,
            pd,
            mr,
            buf,
            buf_size,
            next_client_offset: AtomicU64::new(0),
            max_pages: self.max_pages,
            pages_per_client: self.pages_per_client,
        };

        // Event loop
        self.event_loop(&state)?;

        // Cleanup
        unsafe {
            ffi::ibv_dereg_mr(state.mr);
            libc::munmap(state.buf as *mut libc::c_void, state.buf_size);
            ffi::ibv_dealloc_pd(state.pd);
            ffi::rdma_destroy_id(state.listen_id);
            ffi::rdma_destroy_event_channel(state.ec);
        }

        Ok(())
    }

    fn event_loop(&self, state: &ServerState) -> Result<()> {
        while self.running.load(Ordering::Relaxed) {
            let mut event: *mut ffi::rdma_cm_event = std::ptr::null_mut();
            let ret = unsafe { ffi::rdma_get_cm_event(state.ec, &mut event) };
            if ret != 0 {
                if !self.running.load(Ordering::Relaxed) {
                    break; // Shutting down
                }
                bail!("rdma_get_cm_event failed: {}", ret);
            }

            let event_type = unsafe { (*event).event };
            let conn_id = unsafe { (*event).id };

            match event_type {
                ffi::RDMA_CM_EVENT_CONNECT_REQUEST => {
                    if let Err(e) = self.handle_connect(state, conn_id) {
                        eprintln!("  RDMA: connect request failed: {}", e);
                    }
                }
                ffi::RDMA_CM_EVENT_ESTABLISHED => {
                    eprintln!("  RDMA: connection established");
                }
                ffi::RDMA_CM_EVENT_DISCONNECTED => {
                    eprintln!("  RDMA: client disconnected");
                    unsafe {
                        ffi::rdma_disconnect(conn_id);
                        ffi::rdma_destroy_id(conn_id);
                    }
                }
                other => {
                    eprintln!("  RDMA: unhandled event {}", other);
                }
            }

            unsafe { ffi::rdma_ack_cm_event(event) };
        }

        Ok(())
    }

    fn handle_connect(
        &self,
        state: &ServerState,
        conn_id: *mut ffi::rdma_cm_id,
    ) -> Result<()> {
        // Allocate a slice for this client
        let client_offset_pages = state
            .next_client_offset
            .fetch_add(state.pages_per_client, Ordering::Relaxed);
        if client_offset_pages + state.pages_per_client > state.max_pages {
            eprintln!(
                "  RDMA: refusing connection — out of capacity ({} used of {})",
                client_offset_pages, state.max_pages
            );
            unsafe { ffi::rdma_destroy_id(conn_id) };
            bail!("RDMA server out of capacity");
        }

        let client_addr = unsafe { state.buf.add(client_offset_pages as usize * PAGE_SIZE) } as u64;
        let client_size = state.pages_per_client * PAGE_SIZE as u64;

        // Create CQ for this connection
        let verbs = unsafe { (*conn_id).verbs };
        if verbs.is_null() {
            unsafe { ffi::rdma_destroy_id(conn_id) };
            bail!("conn_id has no verbs context");
        }

        let cq = unsafe {
            ffi::ibv_create_cq(verbs, 16, std::ptr::null_mut(), std::ptr::null_mut(), 0)
        };
        if cq.is_null() {
            unsafe { ffi::rdma_destroy_id(conn_id) };
            bail!("ibv_create_cq failed for connection");
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
            _pad: [0; 4],
        };

        let ret = unsafe { ffi::rdma_create_qp(conn_id, state.pd, &mut qp_attr) };
        if ret != 0 {
            unsafe {
                ffi::ibv_destroy_cq(cq);
                ffi::rdma_destroy_id(conn_id);
            }
            bail!("rdma_create_qp failed: {}", ret);
        }

        // Build handshake: send this client's rkey, base address, and size
        let handshake = RdmaHandshake {
            rkey: unsafe { (*state.mr).rkey },
            _pad: 0,
            addr: client_addr,
            size: client_size,
        };

        let mut conn_param: ffi::rdma_conn_param = unsafe { std::mem::zeroed() };
        conn_param.private_data = &handshake as *const _ as *const libc::c_void;
        conn_param.private_data_len = HANDSHAKE_SIZE as u8;
        conn_param.responder_resources = 1;
        conn_param.initiator_depth = 1;

        let ret = unsafe { ffi::rdma_accept(conn_id, &mut conn_param) };
        if ret != 0 {
            unsafe {
                ffi::ibv_destroy_cq(cq);
                ffi::rdma_destroy_id(conn_id);
            }
            bail!("rdma_accept failed: {}", ret);
        }

        eprintln!(
            "  RDMA: accepted client — offset={} pages, addr=0x{:x}, size={} pages, rkey=0x{:08x}",
            client_offset_pages,
            client_addr,
            state.pages_per_client,
            handshake.rkey
        );

        Ok(())
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}
