//! Core daemon engine: manages backends, processes page requests.

use crate::config::DaemonConfig;
use crate::policy::{BackendCapacity, PolicyEngine, Strategy};
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

    /// Create an engine with explicit backends (for testing).
    pub fn with_backends(
        config: DaemonConfig,
        backends: HashMap<u8, Box<dyn DuvmBackend>>,
    ) -> Self {
        Self {
            config,
            backends,
            policy: Arc::new(PolicyEngine::new(Strategy::Lru)),
            stats: Arc::new(DaemonStats::new()),
        }
    }

    /// Build capacity snapshots from all active backends.
    fn backend_capacities(&self) -> Vec<BackendCapacity> {
        self.backends
            .iter()
            .map(|(&id, backend)| {
                let (total, used) = backend.capacity();
                BackendCapacity {
                    backend_id: id,
                    tier: backend.tier(),
                    total_pages: total,
                    used_pages: used,
                    latency_ns: backend.latency_ns(),
                    healthy: backend.is_healthy(),
                }
            })
            .collect()
    }

    /// Store a page to the appropriate backend.
    pub fn store_page(&self, offset: u64, data: &PageBuffer) -> Result<PageHandle> {
        let capacities = self.backend_capacities();
        let tier = self.policy.select_tier(&capacities).ok_or_else(|| {
            self.stats.store_errors.fetch_add(1, Ordering::Relaxed);
            anyhow::anyhow!("all backends are full — no tier available for storage")
        })?;

        let backend_id = self.tier_to_backend_id(tier);
        let backend = self
            .backends
            .get(&backend_id)
            .ok_or_else(|| {
                self.stats.store_errors.fetch_add(1, Ordering::Relaxed);
                anyhow::anyhow!("no backend for tier {:?} (id {})", tier, backend_id)
            })?;

        let handle = match backend.alloc_page() {
            Ok(h) => h,
            Err(e) => {
                self.stats.store_errors.fetch_add(1, Ordering::Relaxed);
                return Err(e);
            }
        };

        if let Err(e) = backend.store_page(handle, data) {
            self.stats.store_errors.fetch_add(1, Ordering::Relaxed);
            // Try to free the allocated page on failure
            let _ = backend.free_page(handle);
            return Err(e);
        }

        self.policy.record_store(offset, handle, backend_id, tier);
        self.stats.pages_stored.fetch_add(1, Ordering::Relaxed);

        tracing::debug!(offset, %handle, %tier, "page stored");
        Ok(handle)
    }

    /// Load a page from whichever backend has it.
    pub fn load_page(&self, offset: u64, buf: &mut PageBuffer) -> Result<()> {
        let meta = self.policy.lookup(offset).ok_or_else(|| {
            self.stats.load_errors.fetch_add(1, Ordering::Relaxed);
            anyhow::anyhow!("page not found for offset {}", offset)
        })?;

        let backend = self.backends.get(&meta.backend_id).ok_or_else(|| {
            self.stats.load_errors.fetch_add(1, Ordering::Relaxed);
            anyhow::anyhow!("backend {} not found", meta.backend_id)
        })?;

        if let Err(e) = backend.load_page(meta.handle, buf) {
            self.stats.load_errors.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }

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

    /// Get the raw stats handle (for testing).
    pub fn stats(&self) -> &Arc<DaemonStats> {
        &self.stats
    }

    /// Get the policy engine handle (for testing).
    pub fn policy(&self) -> &Arc<PolicyEngine> {
        &self.policy
    }

    /// Map a tier to the best available backend ID.
    fn tier_to_backend_id(&self, tier: Tier) -> u8 {
        // First, try to find a backend that matches the requested tier
        for (&id, backend) in &self.backends {
            if backend.tier() == tier && backend.is_healthy() {
                let (total, used) = backend.capacity();
                if used < total {
                    return id;
                }
            }
        }

        // If no exact match with capacity, fall back to any healthy backend with capacity
        for (&id, backend) in &self.backends {
            if backend.is_healthy() {
                let (total, used) = backend.capacity();
                if used < total {
                    return id;
                }
            }
        }

        // Last resort: return backend 0 (memory) or first available
        if self.backends.contains_key(&1) { 1 } else { 0 }
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

    #[test]
    fn engine_error_stats_on_invalid_load() {
        let config = DaemonConfig::default();
        let engine = Engine::new(config).unwrap();

        let mut buf = [0u8; PAGE_SIZE];
        // Loading a page that doesn't exist should fail and increment error counter
        assert!(engine.load_page(9999, &mut buf).is_err());

        let snap = engine.stats_snapshot();
        assert_eq!(snap.load_errors, 1);
    }

    #[test]
    fn engine_capacity_overflow_detected() {
        // Create config with tiny backend (2 pages max)
        let mut config = DaemonConfig::default();
        config.backends.memory = Some(crate::config::MemoryBackendConfig {
            enabled: true,
            max_pages: 2,
        });
        config.backends.compress = Some(crate::config::CompressBackendConfig {
            enabled: true,
            max_pages: 2,
        });

        let engine = Engine::new(config).unwrap();

        let data = [0u8; PAGE_SIZE];
        // Fill all backends (2 compress + 2 memory = 4 pages)
        engine.store_page(0, &data).unwrap();
        engine.store_page(1, &data).unwrap();
        engine.store_page(2, &data).unwrap();
        engine.store_page(3, &data).unwrap();

        // 5th store should fail — all backends full
        let result = engine.store_page(4, &data);
        assert!(result.is_err());

        let snap = engine.stats_snapshot();
        assert!(snap.store_errors >= 1, "store_errors should be incremented");
    }

    #[test]
    fn engine_invalidate_nonexistent_is_ok() {
        let config = DaemonConfig::default();
        let engine = Engine::new(config).unwrap();
        // Invalidating a page that doesn't exist should not error
        assert!(engine.invalidate_page(999).is_ok());
    }

    #[test]
    fn engine_double_store_overwrites() {
        let config = DaemonConfig::default();
        let engine = Engine::new(config).unwrap();

        let data1 = [0xAA; PAGE_SIZE];
        let data2 = [0xBB; PAGE_SIZE];

        engine.store_page(0, &data1).unwrap();
        engine.store_page(0, &data2).unwrap();

        let mut buf = [0u8; PAGE_SIZE];
        engine.load_page(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0xBB);
    }

    #[test]
    fn engine_tier_to_backend_id_fallback() {
        let config = DaemonConfig::default();
        let engine = Engine::new(config).unwrap();

        // Should find a backend for Compressed tier
        let id = engine.tier_to_backend_id(Tier::Compressed);
        assert!(engine.backends.contains_key(&id));

        // Rdma tier has no backend — should fall back to something available
        let id = engine.tier_to_backend_id(Tier::Rdma);
        assert!(engine.backends.contains_key(&id));
    }
}
