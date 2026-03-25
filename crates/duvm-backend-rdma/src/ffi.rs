//! Minimal FFI bindings for libibverbs and librdmacm.
//! Only the types and functions needed for one-sided RDMA READ/WRITE.

#![allow(non_camel_case_types)]
#![allow(dead_code)]

use libc::{c_char, c_int, c_void, size_t};

// ── ibverbs types ──────────────────────────────────────────────────

pub const IBV_ACCESS_LOCAL_WRITE: c_int = 1;
pub const IBV_ACCESS_REMOTE_WRITE: c_int = 2;
pub const IBV_ACCESS_REMOTE_READ: c_int = 4;

pub const IBV_QPT_RC: c_int = 2; // Reliable Connection

pub const IBV_QPS_RESET: c_int = 0;
pub const IBV_QPS_INIT: c_int = 1;
pub const IBV_QPS_RTR: c_int = 2;
pub const IBV_QPS_RTS: c_int = 3;

pub const IBV_WR_RDMA_WRITE: c_int = 0;
pub const IBV_WR_RDMA_READ: c_int = 3;

pub const IBV_SEND_SIGNALED: c_int = 1 << 2;

pub const IBV_WC_SUCCESS: c_int = 0;

// Opaque structs — we only use pointers
#[repr(C)]
pub struct ibv_context {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct ibv_pd {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct ibv_cq {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct ibv_device {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct ibv_mr {
    pub context: *mut ibv_context,
    pub pd: *mut ibv_pd,
    pub addr: *mut c_void,
    pub length: size_t,
    pub handle: u32,
    pub lkey: u32,
    pub rkey: u32,
}

#[repr(C)]
pub struct ibv_qp {
    pub context: *mut ibv_context,
    pub qp_context: *mut c_void,
    pub pd: *mut ibv_pd,
    pub send_cq: *mut ibv_cq,
    pub recv_cq: *mut ibv_cq,
    // ... more fields we don't use
    pub qp_num: u32,
    pub qp_type: c_int,
}

#[repr(C)]
pub struct ibv_sge {
    pub addr: u64,
    pub length: u32,
    pub lkey: u32,
}

#[repr(C)]
pub struct ibv_send_wr {
    pub wr_id: u64,
    pub next: *mut ibv_send_wr,
    pub sg_list: *mut ibv_sge,
    pub num_sge: c_int,
    pub opcode: c_int,
    pub send_flags: c_int,
    // (4 bytes implicit padding for u64 alignment — occupies imm_data slot)
    // RDMA-specific fields (union in C, we use the rdma variant)
    pub rdma_remote_addr: u64,
    pub rdma_rkey: u32,
    // Pad to match C sizeof(ibv_send_wr) = 128 bytes. Offset here = 52.
    pub(crate) _pad: [u8; 76],
}

#[repr(C)]
pub struct ibv_wc {
    pub wr_id: u64,
    pub status: c_int,
    pub opcode: c_int,
    pub vendor_err: u32,
    pub byte_len: u32,
    // Pad to match C sizeof(ibv_wc) = 48 bytes. Offset here = 24.
    pub(crate) _pad: [u8; 24],
}

// ── rdma_cm types ──────────────────────────────────────────────────

#[repr(C)]
pub struct rdma_event_channel {
    pub fd: c_int,
}

#[repr(C)]
pub struct rdma_cm_id {
    pub verbs: *mut ibv_context,
    pub channel: *mut rdma_event_channel,
    pub context: *mut c_void,
    pub qp: *mut ibv_qp,
    pub pd: *mut ibv_pd,
    // ... more fields
    _pad: [u8; 128],
}

pub const RDMA_PS_TCP: c_int = 0x0106;

pub const RDMA_CM_EVENT_ADDR_RESOLVED: c_int = 0;
pub const RDMA_CM_EVENT_ADDR_ERROR: c_int = 1;
pub const RDMA_CM_EVENT_ROUTE_RESOLVED: c_int = 2;
pub const RDMA_CM_EVENT_ROUTE_ERROR: c_int = 3;
pub const RDMA_CM_EVENT_CONNECT_REQUEST: c_int = 4;
pub const RDMA_CM_EVENT_CONNECT_RESPONSE: c_int = 5;
pub const RDMA_CM_EVENT_CONNECT_ERROR: c_int = 6;
pub const RDMA_CM_EVENT_UNREACHABLE: c_int = 7;
pub const RDMA_CM_EVENT_REJECTED: c_int = 8;
pub const RDMA_CM_EVENT_ESTABLISHED: c_int = 9;
pub const RDMA_CM_EVENT_DISCONNECTED: c_int = 10;

#[repr(C)]
pub struct rdma_cm_event {
    pub id: *mut rdma_cm_id,
    pub listen_id: *mut rdma_cm_id,
    pub event: c_int,
    pub status: c_int,
    // param union starts at offset 24 — first variant is rdma_conn_param
    pub param_private_data: *const c_void,
    pub param_private_data_len: u8,
    pub param_responder_resources: u8,
    pub param_initiator_depth: u8,
    pub param_flow_control: u8,
    pub param_retry_count: u8,
    pub param_rnr_retry_count: u8,
    pub param_srq: u8,
    _param_pad: u8,
    pub param_qp_num: u32,
    // Pad to match C sizeof(rdma_cm_event) = 80 bytes. Offset here = 52.
    _pad: [u8; 28],
}

#[repr(C)]
pub struct rdma_conn_param {
    pub private_data: *const c_void,
    pub private_data_len: u8,
    pub responder_resources: u8,
    pub initiator_depth: u8,
    pub flow_control: u8,
    pub retry_count: u8,
    pub rnr_retry_count: u8,
    pub srq: u8,
    pub(crate) _pad: u8, // align qp_num to offset 16
    pub qp_num: u32,
    pub(crate) _pad2: [u8; 4], // pad to sizeof = 24
}

#[repr(C)]
pub struct ibv_qp_init_attr {
    pub qp_context: *mut c_void,
    pub send_cq: *mut ibv_cq,
    pub recv_cq: *mut ibv_cq,
    pub srq: *mut c_void,
    pub cap: ibv_qp_cap,
    pub qp_type: c_int,
    pub sq_sig_all: c_int,
    pub(crate) _pad: [u8; 4], // match C sizeof = 64
}

#[repr(C)]
pub struct ibv_qp_cap {
    pub max_send_wr: u32,
    pub max_recv_wr: u32,
    pub max_send_sge: u32,
    pub max_recv_sge: u32,
    pub max_inline_data: u32,
}

// ── ibverbs functions ──────────────────────────────────────────────

unsafe extern "C" {
    pub fn ibv_get_device_list(num_devices: *mut c_int) -> *mut *mut ibv_device;
    pub fn ibv_free_device_list(list: *mut *mut ibv_device);
    pub fn ibv_get_device_name(device: *mut ibv_device) -> *const c_char;
    pub fn ibv_open_device(device: *mut ibv_device) -> *mut ibv_context;
    pub fn ibv_close_device(context: *mut ibv_context) -> c_int;
    pub fn ibv_alloc_pd(context: *mut ibv_context) -> *mut ibv_pd;
    pub fn ibv_dealloc_pd(pd: *mut ibv_pd) -> c_int;
    pub fn ibv_reg_mr(
        pd: *mut ibv_pd,
        addr: *mut c_void,
        length: size_t,
        access: c_int,
    ) -> *mut ibv_mr;
    pub fn ibv_dereg_mr(mr: *mut ibv_mr) -> c_int;
    pub fn ibv_create_cq(
        context: *mut ibv_context,
        cqe: c_int,
        cq_context: *mut c_void,
        channel: *mut c_void,
        comp_vector: c_int,
    ) -> *mut ibv_cq;
    pub fn ibv_destroy_cq(cq: *mut ibv_cq) -> c_int;
    // These are inline functions in libibverbs headers — our C shim wraps them.
    #[link_name = "duvm_ibv_post_send"]
    pub fn ibv_post_send(
        qp: *mut ibv_qp,
        wr: *mut ibv_send_wr,
        bad_wr: *mut *mut ibv_send_wr,
    ) -> c_int;
    #[link_name = "duvm_ibv_poll_cq"]
    pub fn ibv_poll_cq(cq: *mut ibv_cq, num_entries: c_int, wc: *mut ibv_wc) -> c_int;
}

// ── rdma_cm functions ──────────────────────────────────────────────

unsafe extern "C" {
    pub fn rdma_create_event_channel() -> *mut rdma_event_channel;
    pub fn rdma_destroy_event_channel(channel: *mut rdma_event_channel);
    pub fn rdma_create_id(
        channel: *mut rdma_event_channel,
        id: *mut *mut rdma_cm_id,
        context: *mut c_void,
        ps: c_int,
    ) -> c_int;
    pub fn rdma_destroy_id(id: *mut rdma_cm_id) -> c_int;
    pub fn rdma_resolve_addr(
        id: *mut rdma_cm_id,
        src_addr: *mut libc::sockaddr,
        dst_addr: *mut libc::sockaddr,
        timeout_ms: c_int,
    ) -> c_int;
    pub fn rdma_resolve_route(id: *mut rdma_cm_id, timeout_ms: c_int) -> c_int;
    pub fn rdma_create_qp(
        id: *mut rdma_cm_id,
        pd: *mut ibv_pd,
        qp_init_attr: *mut ibv_qp_init_attr,
    ) -> c_int;
    pub fn rdma_connect(id: *mut rdma_cm_id, conn_param: *mut rdma_conn_param) -> c_int;
    pub fn rdma_disconnect(id: *mut rdma_cm_id) -> c_int;
    pub fn rdma_get_cm_event(
        channel: *mut rdma_event_channel,
        event: *mut *mut rdma_cm_event,
    ) -> c_int;
    pub fn rdma_ack_cm_event(event: *mut rdma_cm_event) -> c_int;
    pub fn rdma_bind_addr(id: *mut rdma_cm_id, addr: *mut libc::sockaddr) -> c_int;
    pub fn rdma_listen(id: *mut rdma_cm_id, backlog: c_int) -> c_int;
    pub fn rdma_accept(id: *mut rdma_cm_id, conn_param: *mut rdma_conn_param) -> c_int;
}

/// Check if any RDMA devices are available on this system.
pub fn rdma_available() -> bool {
    unsafe {
        let mut num_devices: c_int = 0;
        let list = ibv_get_device_list(&mut num_devices);
        if list.is_null() {
            return false;
        }
        ibv_free_device_list(list);
        num_devices > 0
    }
}
