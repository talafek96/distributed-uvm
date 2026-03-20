//! duvm-memserver: Remote memory server for distributed UVM.
//!
//! Listens on a TCP port and serves page store/load requests from duvm clients.
//! Deploy this on each machine that contributes memory to the distributed pool.
//!
//! Usage: duvm-memserver --bind 0.0.0.0:9200 --max-pages 1000000
//!
//! Protocol (binary, big-endian):
//!   ALLOC:  client sends [4], server responds [0][offset:8]
//!   STORE:  client sends [1][offset:8][data:4096], server responds [0]
//!   LOAD:   client sends [2][offset:8], server responds [0][data:4096]
//!   FREE:   client sends [3][offset:8], server responds [0]
//!   STATUS: client sends [5], server responds [0][used:8][total:8]

use anyhow::Result;
use clap::Parser;
use duvm_common::page::PAGE_SIZE;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// Address to bind to
    #[arg(short, long, default_value = "0.0.0.0:9200")]
    bind: String,

    /// Maximum number of pages to serve
    #[arg(short, long, default_value = "1000000")]
    max_pages: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();

    eprintln!("duvm-memserver starting on {}", args.bind);
    eprintln!(
        "  Max pages: {} ({:.1} GB)",
        args.max_pages,
        args.max_pages as f64 * PAGE_SIZE as f64 / 1e9
    );

    let listener = TcpListener::bind(&args.bind)?;
    eprintln!("  Listening for connections...");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let peer = stream.peer_addr().ok();
                eprintln!("  Client connected from {:?}", peer);
                stream.set_nodelay(true)?;
                if let Err(e) = handle_client(stream, args.max_pages) {
                    eprintln!("  Client disconnected: {}", e);
                }
            }
            Err(e) => eprintln!("  Accept error: {}", e),
        }
    }

    Ok(())
}

fn handle_client(mut stream: TcpStream, max_pages: u64) -> Result<()> {
    let mut pages: HashMap<u64, Box<[u8; PAGE_SIZE]>> = HashMap::new();
    let next_offset = AtomicU64::new(0);
    let mut ops: u64 = 0;

    loop {
        // Read opcode
        let mut op = [0u8; 1];
        if stream.read_exact(&mut op).is_err() {
            eprintln!(
                "  Session ended after {} ops, {} pages stored",
                ops,
                pages.len()
            );
            return Ok(());
        }
        ops += 1;

        match op[0] {
            OP_ALLOC => {
                if pages.len() as u64 >= max_pages {
                    // Must send full 9-byte response — client always reads 9 bytes
                    let mut resp = [0u8; 9];
                    resp[0] = RESP_ERR;
                    stream.write_all(&resp)?;
                } else {
                    let offset = next_offset.fetch_add(1, Ordering::Relaxed);
                    let mut resp = [0u8; 9];
                    resp[0] = RESP_OK;
                    resp[1..9].copy_from_slice(&offset.to_le_bytes());
                    stream.write_all(&resp)?;
                }
            }
            OP_STORE => {
                // Read offset + data
                let mut header = [0u8; 8];
                stream.read_exact(&mut header)?;
                let offset = u64::from_le_bytes(header);

                let mut data = Box::new([0u8; PAGE_SIZE]);
                stream.read_exact(data.as_mut())?;
                pages.insert(offset, data);
                stream.write_all(&[RESP_OK])?;
            }
            OP_LOAD => {
                let mut header = [0u8; 8];
                stream.read_exact(&mut header)?;
                let offset = u64::from_le_bytes(header);

                match pages.get(&offset) {
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
                pages.remove(&offset);
                stream.write_all(&[RESP_OK])?;
            }
            OP_STATUS => {
                let mut resp = [0u8; 17];
                resp[0] = RESP_OK;
                resp[1..9].copy_from_slice(&(pages.len() as u64).to_le_bytes());
                resp[9..17].copy_from_slice(&max_pages.to_le_bytes());
                stream.write_all(&resp)?;
            }
            _ => {
                stream.write_all(&[RESP_ERR])?;
            }
        }
        stream.flush()?;
    }
}
