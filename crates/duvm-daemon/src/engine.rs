//! Core daemon engine: manages backends, processes page requests.

#![allow(dead_code)]

use crate::config::DaemonConfig;
use crate::policy::{PolicyEngine, Strategy};
use anyhow::Result;
use duvm_backend_compress::CompressBackend;
use duvm_backend_memory::MemoryBackend;
use duvm_backend_trait::{BackendConfig, BackendInfo, DuvmBackend};
use duvm_common::page::{PageBuffer, PageHandle, Tier};
use duvm_common::stats::DaemonStats;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// The daemon engine coordinates backends, policy, and request processing.
pub struct Engine {
    config: DaemonConfig,
    backends: HashMap<u8, Box<dyn DuvmBackend>>,
    policy: Arc<PolicyEngine>,
    stats: Arc<DaemonStats>,
}

impl Engine {
    pub fn new(config: DaemonConfig) -> Result<Self> {
        let mut backends: HashMap<u8, Box<dyn DuvmBackend>> = HashMap::new();

        // Initialize memory backend
        if let Some(ref mem_cfg) = config.backends.memory
            && mem_cfg.enabled
        {
            let mut backend = MemoryBackend::new(0);
            backend.init(&BackendConfig {
                max_pages: mem_cfg.max_pages,
                ..Default::default()
            })?;
            backends.insert(0, Box::new(backend));
        }

        // Initialize compression backend
        if let Some(ref comp_cfg) = config.backends.compress
            && comp_cfg.enabled
        {
            let mut backend = CompressBackend::new(1);
            backend.init(&BackendConfig {
                max_pages: comp_cfg.max_pages,
                ..Default::default()
            })?;
            backends.insert(1, Box::new(backend));
        }

        let policy = Arc::new(PolicyEngine::new(Strategy::Lru));
        let stats = Arc::new(DaemonStats::new());

        Ok(Self {
            config,
            backends,
            policy,
            stats,
        })
    }

    /// Store a page to the appropriate backend.
    pub fn store_page(&self, offset: u64, data: &PageBuffer) -> Result<PageHandle> {
        let tier = self.policy.select_tier();
        let backend_id = self.tier_to_backend_id(tier);

        let backend = self
            .backends
            .get(&backend_id)
            .ok_or_else(|| anyhow::anyhow!("no backend for tier {:?}", tier))?;

        let handle = backend.alloc_page()?;
        backend.store_page(handle, data)?;
        self.policy.record_store(offset, handle, backend_id, tier);
        self.stats.pages_stored.fetch_add(1, Ordering::Relaxed);

        tracing::debug!(offset, %handle, %tier, "page stored");
        Ok(handle)
    }

    /// Load a page from whichever backend has it.
    pub fn load_page(&self, offset: u64, buf: &mut PageBuffer) -> Result<()> {
        let meta = self
            .policy
            .lookup(offset)
            .ok_or_else(|| anyhow::anyhow!("page not found for offset {}", offset))?;

        let backend = self
            .backends
            .get(&meta.backend_id)
            .ok_or_else(|| anyhow::anyhow!("backend {} not found", meta.backend_id))?;

        backend.load_page(meta.handle, buf)?;
        self.policy.record_load(offset);
        self.stats.pages_loaded.fetch_add(1, Ordering::Relaxed);

        tracing::debug!(offset, %meta.handle, "page loaded");
        Ok(())
    }

    /// Invalidate a page (remove from backend and tracking).
    pub fn invalidate_page(&self, offset: u64) -> Result<()> {
        if let Some(meta) = self.policy.remove(offset) {
            if let Some(backend) = self.backends.get(&meta.backend_id) {
                backend.free_page(meta.handle)?;
            }
            self.stats.pages_invalidated.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Get backend info for all active backends.
    pub fn backend_info(&self) -> Vec<BackendInfo> {
        self.backends
            .values()
            .map(|b| BackendInfo::from_backend(b.as_ref()))
            .collect()
    }

    /// Get stats snapshot.
    pub fn stats_snapshot(&self) -> duvm_common::stats::StatsSnapshot {
        self.stats.snapshot()
    }

    fn tier_to_backend_id(&self, tier: Tier) -> u8 {
        match tier {
            Tier::Compressed => {
                if self.backends.contains_key(&1) {
                    1
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// Run the daemon: listen for control commands on a Unix socket.
    pub async fn run(&mut self) -> Result<()> {
        let socket_path = &self.config.daemon.socket_path;

        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(socket_path).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // Remove stale socket
        std::fs::remove_file(socket_path).ok();

        let listener = UnixListener::bind(socket_path)?;
        tracing::info!(
            socket = socket_path,
            "Daemon listening for control commands"
        );

        // Handle graceful shutdown
        let stats = self.stats.clone();
        let policy = self.policy.clone();

        // Build a shared reference to backends info for the handler
        let backends_info: Vec<BackendInfo> = self.backend_info();

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            let stats = stats.clone();
                            let policy = policy.clone();
                            let backends_info = backends_info.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_client(stream, &stats, &policy, &backends_info).await {
                                    tracing::warn!("Client error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Accept error: {}", e);
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Shutting down");
                    break;
                }
            }
        }

        // Shutdown backends
        for (_, backend) in self.backends.iter_mut() {
            backend.shutdown()?;
        }

        std::fs::remove_file(socket_path).ok();
        Ok(())
    }
}

async fn handle_client(
    stream: tokio::net::UnixStream,
    stats: &DaemonStats,
    policy: &PolicyEngine,
    backends_info: &[BackendInfo],
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let cmd = line.trim();
        let response = match cmd {
            "status" => {
                let snap = stats.snapshot();
                serde_json::to_string(&serde_json::json!({
                    "status": "running",
                    "tracked_pages": policy.tracked_pages(),
                    "stats": snap,
                }))?
            }
            "backends" => serde_json::to_string(backends_info)?,
            "stats" => serde_json::to_string(&stats.snapshot())?,
            "ping" => "pong".to_string(),
            _ => format!("{{\"error\": \"unknown command: {}\"}}", cmd),
        };
        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        line.clear();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use duvm_common::page::PAGE_SIZE;

    #[test]
    fn engine_store_and_load() {
        let config = DaemonConfig::default();
        let engine = Engine::new(config).unwrap();

        let mut data = [0u8; PAGE_SIZE];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }

        let _handle = engine.store_page(42, &data).unwrap();

        let mut loaded = [0u8; PAGE_SIZE];
        engine.load_page(42, &mut loaded).unwrap();
        assert_eq!(data, loaded);
    }

    #[test]
    fn engine_invalidate() {
        let config = DaemonConfig::default();
        let engine = Engine::new(config).unwrap();

        let data = [0xAB; PAGE_SIZE];
        engine.store_page(0, &data).unwrap();
        engine.invalidate_page(0).unwrap();

        let mut buf = [0u8; PAGE_SIZE];
        assert!(engine.load_page(0, &mut buf).is_err());
    }

    #[test]
    fn engine_stats_tracking() {
        let config = DaemonConfig::default();
        let engine = Engine::new(config).unwrap();

        let data = [0u8; PAGE_SIZE];
        engine.store_page(0, &data).unwrap();
        engine.store_page(1, &data).unwrap();

        let mut buf = [0u8; PAGE_SIZE];
        engine.load_page(0, &mut buf).unwrap();

        let snap = engine.stats_snapshot();
        assert_eq!(snap.pages_stored, 2);
        assert_eq!(snap.pages_loaded, 1);
    }

    #[test]
    fn engine_backend_info() {
        let config = DaemonConfig::default();
        let engine = Engine::new(config).unwrap();
        let info = engine.backend_info();
        assert!(!info.is_empty());
        assert!(info.iter().all(|b| b.healthy));
    }
}
