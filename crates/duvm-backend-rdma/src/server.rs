//! RDMA server: listens for RDMA CM connections, registers a memory region,
//! and exposes it to clients via one-sided RDMA WRITE/READ.
//!
//! The server pre-allocates a contiguous buffer (max_pages * PAGE_SIZE),
//! registers it as an RDMA memory region, and hands out per-client slices
//! via RDMA CM private data during the accept handshake.
//!
//! After connection setup, the server has no data-path involvement — the NIC
//! handles RDMA WRITE/READ directly to the registered buffer.

use crate::{HANDSHAKE_SIZE, RdmaHandshake, ffi};
use anyhow::{Result, bail};
use duvm_common::page::PAGE_SIZE;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// RDMA memory server. Allocates a contiguous buffer, registers it with the NIC,
/// and accepts RDMA CM connections. Clients get the rkey and their slice address
/// via private data in the accept handshake.
pub struct RdmaMemServer {
    max_pages: u64,
    pages_per_client: u64,
    port: u16,
    running: Arc<AtomicBool>,
}

/// Resources allocated lazily on first connection (need verbs context from CM).
struct ServerResources {
    pd: *mut ffi::ibv_pd,
    mr: *mut ffi::ibv_mr,
    buf: *mut u8,
    buf_size: usize,
    rkey: u32,
}

// Safety: RDMA resources are only accessed from the server thread.
unsafe impl Send for ServerResources {}

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

        let ret =
            unsafe { ffi::rdma_bind_addr(listen_id, &mut addr as *mut _ as *mut libc::sockaddr) };
        if ret != 0 {
            unsafe {
                ffi::rdma_destroy_id(listen_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("rdma_bind_addr failed: {}", ret);
        }

        let ret = unsafe { ffi::rdma_listen(listen_id, 16) };
        if ret != 0 {
            unsafe {
                ffi::rdma_destroy_id(listen_id);
                ffi::rdma_destroy_event_channel(ec);
            }
            bail!("rdma_listen failed: {}", ret);
        }

        eprintln!("  RDMA server: listening on port {}", self.port);

        // Resources are allocated lazily on first connection
        let mut resources: Option<ServerResources> = None;
        let next_client_offset = AtomicU64::new(0);

        // Event loop
        while self.running.load(Ordering::Relaxed) {
            let mut event: *mut ffi::rdma_cm_event = std::ptr::null_mut();
            let ret = unsafe { ffi::rdma_get_cm_event(ec, &mut event) };
            if ret != 0 {
                if !self.running.load(Ordering::Relaxed) {
                    break;
                }
                eprintln!("  RDMA: rdma_get_cm_event failed: {}", ret);
                break;
            }

            let event_type = unsafe { (*event).event };
            let conn_id = unsafe { (*event).id };

            match event_type {
                ffi::RDMA_CM_EVENT_CONNECT_REQUEST => {
                    // Lazily initialize resources using the verbs context from the first connection
                    if resources.is_none() {
                        match self.init_resources(conn_id) {
                            Ok(res) => {
                                resources = Some(res);
                            }
                            Err(e) => {
                                eprintln!("  RDMA: failed to init resources: {}", e);
                                unsafe {
                                    ffi::rdma_ack_cm_event(event);
                                    ffi::rdma_destroy_id(conn_id);
                                }
                                continue;
                            }
                        }
                    }

                    if let Some(ref res) = resources
                        && let Err(e) = self.handle_connect(res, conn_id, &next_client_offset)
                    {
                        eprintln!("  RDMA: connect failed: {}", e);
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
                    eprintln!("  RDMA: event {}", other);
                }
            }

            unsafe { ffi::rdma_ack_cm_event(event) };
        }

        // Cleanup
        if let Some(res) = resources {
            unsafe {
                ffi::ibv_dereg_mr(res.mr);
                libc::munmap(res.buf as *mut libc::c_void, res.buf_size);
                ffi::ibv_dealloc_pd(res.pd);
            }
        }
        unsafe {
            ffi::rdma_destroy_id(listen_id);
            ffi::rdma_destroy_event_channel(ec);
        }

        Ok(())
    }

    /// Initialize PD, buffer, and MR using the verbs context from the first connection.
    fn init_resources(&self, conn_id: *mut ffi::rdma_cm_id) -> Result<ServerResources> {
        let verbs = unsafe { (*conn_id).verbs };
        if verbs.is_null() {
            bail!("connection has no verbs context");
        }

        let pd = unsafe { ffi::ibv_alloc_pd(verbs) };
        if pd.is_null() {
            bail!("ibv_alloc_pd failed");
        }

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
            unsafe { ffi::ibv_dealloc_pd(pd) };
            bail!("mmap for RDMA buffer failed ({} bytes)", buf_size);
        }

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
            }
            bail!("ibv_reg_mr failed");
        }

        let rkey = unsafe { (*mr).rkey };
        eprintln!(
            "  RDMA server: buffer={} pages ({:.1} MB), rkey=0x{:08x}, addr=0x{:x}",
            self.max_pages,
            buf_size as f64 / 1e6,
            rkey,
            buf as u64
        );

        Ok(ServerResources {
            pd,
            mr,
            buf,
            buf_size,
            rkey,
        })
    }

    fn handle_connect(
        &self,
        res: &ServerResources,
        conn_id: *mut ffi::rdma_cm_id,
        next_client_offset: &AtomicU64,
    ) -> Result<()> {
        let client_offset_pages =
            next_client_offset.fetch_add(self.pages_per_client, Ordering::Relaxed);
        if client_offset_pages + self.pages_per_client > self.max_pages {
            eprintln!(
                "  RDMA: refusing connection — out of capacity ({} of {})",
                client_offset_pages, self.max_pages
            );
            unsafe { ffi::rdma_destroy_id(conn_id) };
            bail!("RDMA server out of capacity");
        }

        let client_addr = unsafe { res.buf.add(client_offset_pages as usize * PAGE_SIZE) } as u64;
        let client_size = self.pages_per_client * PAGE_SIZE as u64;

        // Create CQ for this connection
        let verbs = unsafe { (*conn_id).verbs };
        let cq =
            unsafe { ffi::ibv_create_cq(verbs, 16, std::ptr::null_mut(), std::ptr::null_mut(), 0) };
        if cq.is_null() {
            unsafe { ffi::rdma_destroy_id(conn_id) };
            bail!("ibv_create_cq failed");
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

        let ret = unsafe { ffi::rdma_create_qp(conn_id, res.pd, &mut qp_attr) };
        if ret != 0 {
            unsafe {
                ffi::ibv_destroy_cq(cq);
                ffi::rdma_destroy_id(conn_id);
            }
            bail!("rdma_create_qp failed: {}", ret);
        }

        // Send rkey + base address for this client's slice
        let handshake = RdmaHandshake {
            rkey: res.rkey,
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
            "  RDMA: accepted client — pages {}-{}, addr=0x{:x}, rkey=0x{:08x}",
            client_offset_pages,
            client_offset_pages + self.pages_per_client,
            client_addr,
            res.rkey
        );

        Ok(())
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}
