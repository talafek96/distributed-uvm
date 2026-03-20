//! TCP backend integration tests.
//!
//! Starts an in-process TCP memory server and tests the full
//! TCP backend store/load/free cycle without external processes.

use duvm_backend_tcp::TcpBackend;
use duvm_backend_trait::{BackendConfig, DuvmBackend};
use duvm_common::page::PAGE_SIZE;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

const OP_STORE: u8 = 1;
const OP_LOAD: u8 = 2;
const OP_FREE: u8 = 3;
const OP_ALLOC: u8 = 4;
const RESP_OK: u8 = 0;
const RESP_ERR: u8 = 1;

fn start_test_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || handle_client(stream));
        }
    });
    port
}

fn handle_client(mut stream: TcpStream) {
    let mut pages: HashMap<u64, Box<[u8; PAGE_SIZE]>> = HashMap::new();
    let next: AtomicU64 = AtomicU64::new(0);
    stream.set_nodelay(true).ok();
    loop {
        let mut op = [0u8; 1];
        if stream.read_exact(&mut op).is_err() {
            return;
        }
        match op[0] {
            OP_ALLOC => {
                let off = next.fetch_add(1, Ordering::Relaxed);
                let mut r = [0u8; 9];
                r[0] = RESP_OK;
                r[1..9].copy_from_slice(&off.to_le_bytes());
                stream.write_all(&r).ok();
            }
            OP_STORE => {
                let mut h = [0u8; 8];
                if stream.read_exact(&mut h).is_err() {
                    return;
                }
                let off = u64::from_le_bytes(h);
                let mut data = Box::new([0u8; PAGE_SIZE]);
                if stream.read_exact(data.as_mut()).is_err() {
                    return;
                }
                pages.insert(off, data);
                stream.write_all(&[RESP_OK]).ok();
            }
            OP_LOAD => {
                let mut h = [0u8; 8];
                if stream.read_exact(&mut h).is_err() {
                    return;
                }
                let off = u64::from_le_bytes(h);
                match pages.get(&off) {
                    Some(d) => {
                        stream.write_all(&[RESP_OK]).ok();
                        stream.write_all(d.as_ref()).ok();
                    }
                    None => {
                        stream.write_all(&[RESP_ERR]).ok();
                    }
                }
            }
            OP_FREE => {
                let mut h = [0u8; 8];
                if stream.read_exact(&mut h).is_err() {
                    return;
                }
                let off = u64::from_le_bytes(h);
                pages.remove(&off);
                stream.write_all(&[RESP_OK]).ok();
            }
            _ => {
                stream.write_all(&[RESP_ERR]).ok();
            }
        }
        stream.flush().ok();
    }
}

#[test]
fn tcp_store_load_roundtrip() {
    let port = start_test_server();
    let mut backend = TcpBackend::new(10, &format!("127.0.0.1:{}", port));
    backend.init(&BackendConfig::default()).unwrap();

    let handle = backend.alloc_page().unwrap();
    let mut data = [0u8; PAGE_SIZE];
    data[0] = 0xDE;
    data[1] = 0xAD;
    data[4095] = 0xFF;
    backend.store_page(handle, &data).unwrap();

    let mut loaded = [0u8; PAGE_SIZE];
    backend.load_page(handle, &mut loaded).unwrap();
    assert_eq!(data, loaded);
}

#[test]
fn tcp_50_pages() {
    let port = start_test_server();
    let mut backend = TcpBackend::new(10, &format!("127.0.0.1:{}", port));
    backend.init(&BackendConfig::default()).unwrap();

    let mut handles = Vec::new();
    for i in 0u8..50 {
        let h = backend.alloc_page().unwrap();
        backend.store_page(h, &[i; PAGE_SIZE]).unwrap();
        handles.push((h, i));
    }
    for (h, expected) in &handles {
        let mut buf = [0u8; PAGE_SIZE];
        backend.load_page(*h, &mut buf).unwrap();
        assert_eq!(buf[0], *expected);
        assert_eq!(buf[4095], *expected);
    }
    assert_eq!(backend.capacity().1, 50);
}

#[test]
fn tcp_free_page() {
    let port = start_test_server();
    let mut backend = TcpBackend::new(10, &format!("127.0.0.1:{}", port));
    backend.init(&BackendConfig::default()).unwrap();

    let h = backend.alloc_page().unwrap();
    backend.store_page(h, &[42u8; PAGE_SIZE]).unwrap();
    assert_eq!(backend.capacity().1, 1);
    backend.free_page(h).unwrap();
    assert_eq!(backend.capacity().1, 0);

    let mut buf = [0u8; PAGE_SIZE];
    assert!(backend.load_page(h, &mut buf).is_err());
}

#[test]
fn tcp_data_integrity_patterns() {
    let port = start_test_server();
    let mut backend = TcpBackend::new(10, &format!("127.0.0.1:{}", port));
    backend.init(&BackendConfig::default()).unwrap();

    // Zeros
    let h1 = backend.alloc_page().unwrap();
    let zeros = [0u8; PAGE_SIZE];
    backend.store_page(h1, &zeros).unwrap();

    // Sequential
    let h2 = backend.alloc_page().unwrap();
    let mut seq = [0u8; PAGE_SIZE];
    for (i, b) in seq.iter_mut().enumerate() {
        *b = (i % 256) as u8;
    }
    backend.store_page(h2, &seq).unwrap();

    // Pseudo-random
    let h3 = backend.alloc_page().unwrap();
    let mut rand_data = [0u8; PAGE_SIZE];
    let mut v: u32 = 0xDEADBEEF;
    for chunk in rand_data.chunks_exact_mut(4) {
        v = v.wrapping_mul(1103515245).wrapping_add(12345);
        chunk.copy_from_slice(&v.to_le_bytes());
    }
    backend.store_page(h3, &rand_data).unwrap();

    let mut buf = [0u8; PAGE_SIZE];
    backend.load_page(h1, &mut buf).unwrap();
    assert_eq!(buf, zeros);
    backend.load_page(h2, &mut buf).unwrap();
    assert_eq!(buf, seq);
    backend.load_page(h3, &mut buf).unwrap();
    assert_eq!(buf, rand_data);
}

#[test]
fn tcp_latency_tracking() {
    let port = start_test_server();
    let mut backend = TcpBackend::new(10, &format!("127.0.0.1:{}", port));
    backend.init(&BackendConfig::default()).unwrap();

    for _ in 0..100 {
        let h = backend.alloc_page().unwrap();
        backend.store_page(h, &[0u8; PAGE_SIZE]).unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        backend.load_page(h, &mut buf).unwrap();
    }
    assert!(backend.avg_store_ns() > 0, "store latency not tracked");
    assert!(backend.avg_load_ns() > 0, "load latency not tracked");
    assert!(backend.avg_store_ns() < 10_000_000, "store >10ms");
    assert!(backend.avg_load_ns() < 10_000_000, "load >10ms");
}

#[test]
fn tcp_health_and_shutdown() {
    let port = start_test_server();
    let mut backend = TcpBackend::new(10, &format!("127.0.0.1:{}", port));
    backend.init(&BackendConfig::default()).unwrap();
    assert!(backend.is_healthy());
    assert!(backend.name().contains("127.0.0.1"));
    assert_eq!(backend.tier(), duvm_common::page::Tier::Rdma);
    backend.shutdown().unwrap();
    assert!(!backend.is_healthy());
}
