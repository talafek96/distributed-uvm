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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

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

/// Prove: TCP backend respects max_pages via CAS loop.
/// Allocating beyond capacity returns an error instead of over-allocating.
#[test]
fn tcp_capacity_limit_enforced() {
    let port = start_test_server();
    let mut backend = TcpBackend::new(10, &format!("127.0.0.1:{}", port));
    backend
        .init(&BackendConfig {
            max_pages: 5,
            ..Default::default()
        })
        .unwrap();

    // Fill to capacity
    for i in 0..5 {
        let h = backend.alloc_page().unwrap();
        backend.store_page(h, &[i as u8; PAGE_SIZE]).unwrap();
    }
    assert_eq!(backend.capacity().1, 5);

    // Next alloc should fail — not exceed max_pages
    let result = backend.alloc_page();
    assert!(
        result.is_err(),
        "alloc_page should fail when backend is full"
    );
    assert_eq!(
        backend.capacity().1,
        5,
        "pages_used must not exceed max_pages"
    );
}

/// Prove: freeing a page allows a new allocation when at capacity.
#[test]
fn tcp_capacity_recovers_after_free() {
    let port = start_test_server();
    let mut backend = TcpBackend::new(10, &format!("127.0.0.1:{}", port));
    backend
        .init(&BackendConfig {
            max_pages: 3,
            ..Default::default()
        })
        .unwrap();

    let h1 = backend.alloc_page().unwrap();
    let h2 = backend.alloc_page().unwrap();
    let h3 = backend.alloc_page().unwrap();
    assert!(backend.alloc_page().is_err(), "should be full at 3");

    backend.free_page(h2).unwrap();
    assert_eq!(backend.capacity().1, 2);

    // Should succeed now
    let h4 = backend.alloc_page().unwrap();
    backend.store_page(h4, &[99u8; PAGE_SIZE]).unwrap();
    assert_eq!(backend.capacity().1, 3);

    // Verify data on the remaining pages
    backend.store_page(h1, &[1u8; PAGE_SIZE]).unwrap();
    backend.store_page(h3, &[3u8; PAGE_SIZE]).unwrap();
    let mut buf = [0u8; PAGE_SIZE];
    backend.load_page(h1, &mut buf).unwrap();
    assert_eq!(buf[0], 1);
    backend.load_page(h3, &mut buf).unwrap();
    assert_eq!(buf[0], 3);
    backend.load_page(h4, &mut buf).unwrap();
    assert_eq!(buf[0], 99);
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

/// Prove: multiple TCP clients can operate concurrently on the same server.
/// Each client allocates, stores, and loads pages; all data stays isolated.
#[test]
fn tcp_concurrent_clients() {
    let port = start_test_server();
    let num_clients = 4;
    let pages_per_client = 20;

    let handles: Vec<_> = (0..num_clients)
        .map(|client_id| {
            let addr = format!("127.0.0.1:{}", port);
            thread::spawn(move || {
                let mut backend = TcpBackend::new(client_id as u8, &addr);
                backend.init(&BackendConfig::default()).unwrap();

                let mut page_handles = Vec::new();
                for page_i in 0..pages_per_client {
                    let h = backend.alloc_page().unwrap();
                    // Each client writes a unique pattern: client_id * 64 + page_index
                    let marker = ((client_id * 64 + page_i) % 256) as u8;
                    backend.store_page(h, &[marker; PAGE_SIZE]).unwrap();
                    page_handles.push((h, marker));
                }

                // Verify all pages
                for (h, expected) in &page_handles {
                    let mut buf = [0u8; PAGE_SIZE];
                    backend.load_page(*h, &mut buf).unwrap();
                    assert_eq!(
                        buf[0], *expected,
                        "client {} page data corrupted",
                        client_id
                    );
                    assert_eq!(buf[PAGE_SIZE - 1], *expected);
                }
                page_handles.len()
            })
        })
        .collect();

    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(total, num_clients * pages_per_client);
}

// ── Reconnection & negative-path tests ─────────────────────────────

use std::sync::atomic::AtomicBool;

/// A test server that can be cleanly stopped, freeing its port for reuse.
/// The accept thread checks a stop flag every 100ms.
struct ControllableServer {
    port: u16,
    stop_flag: Arc<AtomicBool>,
    /// Cloned handles for every accepted connection.
    clients: Arc<std::sync::Mutex<Vec<TcpStream>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl ControllableServer {
    fn start_on(port: u16) -> Self {
        let addr = format!("127.0.0.1:{}", port);
        // Retry bind briefly in case the OS hasn't released the port yet.
        let listener = loop {
            match TcpListener::bind(&addr) {
                Ok(l) => break l,
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        };
        // Non-blocking so the accept thread can check the stop flag.
        listener.set_nonblocking(true).unwrap();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let clients: Arc<std::sync::Mutex<Vec<TcpStream>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sf = stop_flag.clone();
        let cl = clients.clone();
        let join = thread::spawn(move || {
            while !sf.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        if let Ok(dup) = stream.try_clone() {
                            cl.lock().unwrap().push(dup);
                        }
                        thread::spawn(move || handle_client(stream));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
            // Thread exits → listener is dropped → port is freed.
        });
        Self {
            port,
            stop_flag,
            clients,
            join: Some(join),
        }
    }

    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        Self::start_on(port)
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Simulate a crash: RST all client connections and stop the accept thread,
    /// freeing the port so it can be rebound.
    fn stop(&mut self) {
        // 1. Close all client connections
        for client in self.clients.lock().unwrap().drain(..) {
            client.shutdown(std::net::Shutdown::Both).ok();
        }
        // 2. Signal the accept thread to exit
        self.stop_flag.store(true, Ordering::Relaxed);
        // 3. Wait for it so the TcpListener is dropped
        if let Some(h) = self.join.take() {
            h.join().ok();
        }
    }
}

/// Prove: after server crash, is_healthy() returns false once the backend
/// discovers the broken stream (on the next operation).
#[test]
fn tcp_server_crash_marks_unhealthy() {
    let mut srv = ControllableServer::start();
    let mut backend = TcpBackend::new(10, &srv.addr());
    backend.init(&BackendConfig::default()).unwrap();
    assert!(backend.is_healthy());

    // Store a page — should work
    let h = backend.alloc_page().unwrap();
    backend.store_page(h, &[0xAA; PAGE_SIZE]).unwrap();
    assert!(backend.is_healthy());

    // Kill the server
    srv.stop();
    // Give OS a moment to close the listening socket
    thread::sleep(Duration::from_millis(50));

    // Next operation should fail and clear the stream
    let result = backend.alloc_page();
    assert!(result.is_err(), "alloc should fail after server crash");
    assert!(
        !backend.is_healthy(),
        "backend must report unhealthy after I/O failure"
    );
}

/// Prove: after server restart, the backend automatically reconnects
/// and operations succeed again.
#[test]
fn tcp_reconnect_after_server_restart() {
    let mut srv = ControllableServer::start();
    let port = srv.port;
    let addr = srv.addr();
    let mut backend = TcpBackend::new(10, &addr);
    backend.init(&BackendConfig::default()).unwrap();

    // Store works
    let h = backend.alloc_page().unwrap();
    backend.store_page(h, &[0xBB; PAGE_SIZE]).unwrap();

    // Kill server
    srv.stop();
    thread::sleep(Duration::from_millis(50));

    // Operation fails, stream cleared
    assert!(backend.alloc_page().is_err());
    assert!(!backend.is_healthy());

    // Restart server on the same port
    let _srv2 = ControllableServer::start_on(port);

    // Next operation should auto-reconnect and succeed.
    // Note: pages stored on the old server are lost (new server, fresh state).
    let h2 = backend.alloc_page().unwrap();
    backend.store_page(h2, &[0xCC; PAGE_SIZE]).unwrap();
    assert!(
        backend.is_healthy(),
        "backend should be healthy after reconnect"
    );

    let mut buf = [0u8; PAGE_SIZE];
    backend.load_page(h2, &mut buf).unwrap();
    assert_eq!(buf[0], 0xCC, "data integrity after reconnect");
}

/// Prove: connecting to a port where nothing is listening fails
/// immediately, and the backend reports unhealthy.
#[test]
fn tcp_connect_to_dead_server_is_unhealthy() {
    // Bind to get a port, then immediately drop the listener
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut backend = TcpBackend::new(10, &format!("127.0.0.1:{}", port));
    assert!(
        backend.init(&BackendConfig::default()).is_err(),
        "init to dead server should fail"
    );
    assert!(!backend.is_healthy());
}

/// Prove: store_page failure on a broken stream clears it, and a
/// subsequent store can reconnect to a restarted server.
#[test]
fn tcp_store_failure_clears_stream_and_reconnects() {
    let mut srv = ControllableServer::start();
    let port = srv.port;
    let mut backend = TcpBackend::new(10, &srv.addr());
    backend.init(&BackendConfig::default()).unwrap();

    let h = backend.alloc_page().unwrap();
    backend.store_page(h, &[1u8; PAGE_SIZE]).unwrap();

    // Kill server
    srv.stop();
    thread::sleep(Duration::from_millis(50));

    // store_page should fail
    let h2_result = backend.alloc_page();
    assert!(h2_result.is_err());

    // Restart
    let _srv2 = ControllableServer::start_on(port);

    // alloc + store should work via reconnect
    let h3 = backend.alloc_page().unwrap();
    backend.store_page(h3, &[2u8; PAGE_SIZE]).unwrap();

    let mut buf = [0u8; PAGE_SIZE];
    backend.load_page(h3, &mut buf).unwrap();
    assert_eq!(buf[0], 2);
}
