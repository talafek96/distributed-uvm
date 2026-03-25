//! duvm-memserver: Remote memory server for distributed UVM.
//!
//! Listens on a TCP port and serves page store/load requests from duvm clients.
//! Deploy this on each machine that contributes memory to the distributed pool.
//!
//! **Critical design: refuse when full, never recurse.**
//!
//! When the memserver's pool is full, it returns RESP_ERR for STORE requests.
//! The requesting machine's daemon then tries other machines, and if none have
//! space, returns an error to the kernel module. The kernel falls through to
//! the next swap device (local SSD). This prevents the mutual-OOM deadlock:
//!
//!   A swaps to B → B is full → B returns ERR → A tries C → C is full →
//!   A falls back to local SSD swap. No recursion.
//!
//! The memserver allocates dynamically (no wasteful pre-allocation) but tracks
//! its usage against max_pages and refuses new pages before the machine runs
//! out of RAM.
//!
//! Usage: duvm-memserver --bind 0.0.0.0:9200 --max-pages 1000000
//!
//! Protocol:
//!   ALLOC:  client sends [4], server responds [status:1][offset:8]
//!   STORE:  client sends [1][offset:8][data:4096], server responds [status:1]
//!   LOAD:   client sends [2][offset:8], server responds [status:1][data:4096]
//!   FREE:   client sends [3][offset:8], server responds [status:1]
//!   STATUS: client sends [5], server responds [status:1][used:8][total:8]

use anyhow::Result;
use clap::Parser;
use duvm_common::page::PAGE_SIZE;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const OP_STORE: u8 = 1;
const OP_LOAD: u8 = 2;
const OP_FREE: u8 = 3;
const OP_ALLOC: u8 = 4;
const OP_STATUS: u8 = 5;
const RESP_OK: u8 = 0;
const RESP_ERR: u8 = 1;

#[derive(Parser)]
#[command(name = "duvm-memserver", about = "Remote memory server for duvm")]
struct Args {
    /// Address to bind to (for TCP)
    #[arg(short, long, default_value = "0.0.0.0:9200")]
    bind: String,

    /// Maximum number of pages to serve.
    /// Set this to the amount of RAM you want to dedicate to remote pages.
    /// The memserver refuses STORE requests when this limit is reached.
    #[arg(short, long, default_value = "1000000")]
    max_pages: u64,

    /// Enable RDMA listener (requires RDMA hardware or SoftRoCE).
    /// When enabled, clients can connect via RDMA for one-sided page transfers.
    /// TCP listener still runs alongside for non-RDMA clients.
    #[arg(long)]
    rdma: bool,

    /// Maximum pages per RDMA client (each client gets its own slice of the buffer).
    #[arg(long, default_value = "100000")]
    rdma_pages_per_client: u64,

    /// Port for RDMA listener (default: TCP port + 1). Must differ from TCP port
    /// because iWARP (SoftiWARP) uses TCP as transport.
    #[arg(long)]
    rdma_port: Option<u16>,
}

/// Shared page storage. All connections share one pool with one capacity limit.
/// Dynamic allocation — pages are heap-allocated when stored, freed when released.
/// When max_pages is reached, new STORE requests are refused (RESP_ERR).
struct PageStore {
    pages: Mutex<HashMap<u64, Box<[u8; PAGE_SIZE]>>>,
    max_pages: u64,
    used: AtomicU64,
    next_offset: AtomicU64,
}

impl PageStore {
    fn new(max_pages: u64) -> Self {
        Self {
            pages: Mutex::new(HashMap::new()),
            max_pages,
            used: AtomicU64::new(0),
            next_offset: AtomicU64::new(0),
        }
    }

    fn alloc_offset(&self) -> Option<u64> {
        if self.used.load(Ordering::Relaxed) >= self.max_pages {
            return None;
        }
        Some(self.next_offset.fetch_add(1, Ordering::Relaxed))
    }

    fn store(&self, offset: u64, data: Box<[u8; PAGE_SIZE]>) -> bool {
        let mut pages = self.pages.lock().unwrap();
        if pages.len() as u64 >= self.max_pages && !pages.contains_key(&offset) {
            return false; // Full and this is a new page (not an overwrite)
        }
        let is_new = !pages.contains_key(&offset);
        pages.insert(offset, data);
        if is_new {
            self.used.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    fn load(&self, offset: u64) -> Option<Box<[u8; PAGE_SIZE]>> {
        let pages = self.pages.lock().unwrap();
        pages.get(&offset).map(|data| {
            let mut copy = Box::new([0u8; PAGE_SIZE]);
            copy.copy_from_slice(data.as_ref());
            copy
        })
    }

    fn free(&self, offset: u64) -> bool {
        let mut pages = self.pages.lock().unwrap();
        if pages.remove(&offset).is_some() {
            self.used.fetch_sub(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    fn used_pages(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    eprintln!("duvm-memserver starting on {}", args.bind);
    eprintln!(
        "  Max pages: {} ({:.1} GB)",
        args.max_pages,
        args.max_pages as f64 * PAGE_SIZE as f64 / 1e9
    );
    eprintln!("  When full: refuses STORE → client falls back to next swap device");

    // Start RDMA listener in a background thread if requested
    if args.rdma {
        let tcp_port: u16 = args
            .bind
            .split(':')
            .last()
            .unwrap_or("9200")
            .parse()
            .unwrap_or(9200);
        let rdma_port = args.rdma_port.unwrap_or(tcp_port + 1);
        let rdma_max_pages = args.max_pages;
        let rdma_pages_per_client = args.rdma_pages_per_client;

        std::thread::spawn(move || {
            let server = duvm_backend_rdma::server::RdmaMemServer::new(
                rdma_port,
                rdma_max_pages,
                rdma_pages_per_client,
            );
            if let Err(e) = server.run() {
                eprintln!("  RDMA server error: {}", e);
            }
        });
        eprintln!("  RDMA listener: port={}, pages_per_client={}", rdma_port, args.rdma_pages_per_client);
    }

    let store = Arc::new(PageStore::new(args.max_pages));

    let listener = TcpListener::bind(&args.bind)?;
    eprintln!("  Listening for connections...");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let peer = stream.peer_addr().ok();
                eprintln!("  Client connected from {:?}", peer);
                stream.set_nodelay(true)?;
                let store = store.clone();
                if let Err(e) = handle_client(stream, &store) {
                    eprintln!("  Client disconnected: {}", e);
                }
            }
            Err(e) => eprintln!("  Accept error: {}", e),
        }
    }

    Ok(())
}

fn handle_client(mut stream: TcpStream, store: &PageStore) -> Result<()> {
    let mut ops: u64 = 0;

    loop {
        let mut op = [0u8; 1];
        if stream.read_exact(&mut op).is_err() {
            eprintln!(
                "  Session ended after {} ops, {} pages in store",
                ops,
                store.used_pages()
            );
            return Ok(());
        }
        ops += 1;

        match op[0] {
            OP_ALLOC => {
                match store.alloc_offset() {
                    Some(offset) => {
                        let mut resp = [0u8; 9];
                        resp[0] = RESP_OK;
                        resp[1..9].copy_from_slice(&offset.to_le_bytes());
                        stream.write_all(&resp)?;
                    }
                    None => {
                        // Full — client should try another machine or fall back to local swap
                        let mut resp = [0u8; 9];
                        resp[0] = RESP_ERR;
                        stream.write_all(&resp)?;
                    }
                }
            }
            OP_STORE => {
                let mut header = [0u8; 8];
                stream.read_exact(&mut header)?;
                let offset = u64::from_le_bytes(header);

                let mut data = Box::new([0u8; PAGE_SIZE]);
                stream.read_exact(data.as_mut())?;

                if store.store(offset, data) {
                    stream.write_all(&[RESP_OK])?;
                } else {
                    // Pool is full — refuse. Client will try another machine.
                    stream.write_all(&[RESP_ERR])?;
                }
            }
            OP_LOAD => {
                let mut header = [0u8; 8];
                stream.read_exact(&mut header)?;
                let offset = u64::from_le_bytes(header);

                match store.load(offset) {
                    Some(data) => {
                        stream.write_all(&[RESP_OK])?;
                        stream.write_all(data.as_ref())?;
                    }
                    None => {
                        stream.write_all(&[RESP_ERR])?;
                    }
                }
            }
            OP_FREE => {
                let mut header = [0u8; 8];
                stream.read_exact(&mut header)?;
                let offset = u64::from_le_bytes(header);

                if store.free(offset) {
                    stream.write_all(&[RESP_OK])?;
                } else {
                    stream.write_all(&[RESP_ERR])?;
                }
            }
            OP_STATUS => {
                let mut resp = [0u8; 17];
                resp[0] = RESP_OK;
                resp[1..9].copy_from_slice(&store.used_pages().to_le_bytes());
                resp[9..17].copy_from_slice(&store.max_pages.to_le_bytes());
                stream.write_all(&resp)?;
            }
            _ => {
                stream.write_all(&[RESP_ERR])?;
            }
        }
        stream.flush()?;
    }
}
