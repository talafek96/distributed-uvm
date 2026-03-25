//! Core daemon engine: manages backends, processes page requests.

use crate::config::DaemonConfig;
use crate::policy::{BackendCapacity, PolicyEngine, Strategy};
use anyhow::Result;
use duvm_backend_compress::CompressBackend;
use duvm_backend_memory::MemoryBackend;
use duvm_backend_rdma::RdmaBackend;
use duvm_backend_tcp::TcpBackend;
use duvm_backend_trait::{BackendConfig, BackendInfo, DuvmBackend};
use duvm_common::page::{PageBuffer, PageHandle, Tier};
use duvm_common::stats::DaemonStats;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// The daemon engine coordinates backends, policy, and request processing.
pub struct Engine {
    config: DaemonConfig,
    backends: HashMap<u8, Box<dyn DuvmBackend>>,
    policy: Arc<PolicyEngine>,
    stats: Arc<DaemonStats>,
    /// Round-robin counter for distributing pages across same-tier backends.
    round_robin: AtomicUsize,
}

impl Engine {
    /// Create a default config.
    pub fn default_config() -> DaemonConfig {
        DaemonConfig::default()
    }

    pub fn new(config: DaemonConfig) -> Result<Self> {
        let mut backends: HashMap<u8, Box<dyn DuvmBackend>> = HashMap::new();
        let mut next_id: u8 = 0;

        // Initialize memory backend
        if let Some(ref mem_cfg) = config.backends.memory
            && mem_cfg.enabled
        {
            let mut backend = MemoryBackend::new(next_id);
            backend.init(&BackendConfig {
                max_pages: mem_cfg.max_pages,
                ..Default::default()
            })?;
            backends.insert(next_id, Box::new(backend));
            next_id += 1;
        }

        // Initialize compression backend
        if let Some(ref comp_cfg) = config.backends.compress
            && comp_cfg.enabled
        {
            let mut backend = CompressBackend::new(next_id);
            backend.init(&BackendConfig {
                max_pages: comp_cfg.max_pages,
                ..Default::default()
            })?;
            backends.insert(next_id, Box::new(backend));
            next_id += 1;
        }

        // Initialize remote backends (one per peer)
        // Transport modes:
        //   "tcp"  — only TCP backends, ignore RDMA even if available
        //   "rdma" — only RDMA backends, fail if RDMA not available
        //   "auto" — try RDMA first, fall back to TCP per peer
        //   "both" — create both RDMA and TCP backends for each peer
        if let Some(ref remote_cfg) = config.backends.remote
            && remote_cfg.enabled
        {
            let transport = remote_cfg.transport.as_str();
            let rdma_available = duvm_backend_rdma::is_rdma_available();

            if transport == "rdma" && !rdma_available {
                tracing::error!("transport=rdma but no RDMA devices found");
                anyhow::bail!("transport=rdma requires RDMA hardware or SoftRoCE");
            }

            let use_rdma = match transport {
                "rdma" => true,
                "tcp" => false,
                "auto" => rdma_available,
                "both" => true, // always attempt RDMA in "both" mode
                _ => {
                    tracing::warn!(transport, "unknown transport, defaulting to tcp");
                    false
                }
            };
            let use_tcp = match transport {
                "rdma" => false,
                "tcp" => true,
                "auto" => !rdma_available, // TCP only if RDMA not available
                "both" => true,            // always
                _ => true,
            };

            for addr in &remote_cfg.peers {
                let mut tcp_fallback_created = false;

                if use_rdma {
                    let mut backend = RdmaBackend::new(next_id, addr);
                    match backend.init(&BackendConfig {
                        max_pages: remote_cfg.max_pages_per_peer,
                        ..Default::default()
                    }) {
                        Ok(()) => {
                            tracing::info!(
                                id = next_id,
                                addr,
                                transport = "rdma",
                                "Remote RDMA backend connected"
                            );
                            backends.insert(next_id, Box::new(backend));
                            next_id += 1;
                        }
                        Err(e) => {
                            tracing::warn!(addr, error = %e, "RDMA backend failed");
                            // If transport=auto or both, fall through to TCP
                            if matches!(transport, "auto" | "both") {
                                let mut tcp_backend = TcpBackend::new(next_id, addr);
                                match tcp_backend.init(&BackendConfig {
                                    max_pages: remote_cfg.max_pages_per_peer,
                                    ..Default::default()
                                }) {
                                    Ok(()) => {
                                        tracing::info!(
                                            id = next_id,
                                            addr,
                                            transport = "tcp",
                                            "Fell back to TCP after RDMA failure"
                                        );
                                        backends.insert(next_id, Box::new(tcp_backend));
                                        next_id += 1;
                                        tcp_fallback_created = true;
                                    }
                                    Err(e2) => {
                                        tracing::warn!(addr, error = %e2, "TCP fallback also failed — skipping peer");
                                    }
                                }
                            }
                        }
                    }
                }

                if use_tcp && !tcp_fallback_created {
                    let mut backend = TcpBackend::new(next_id, addr);
                    match backend.init(&BackendConfig {
                        max_pages: remote_cfg.max_pages_per_peer,
                        ..Default::default()
                    }) {
                        Ok(()) => {
                            tracing::info!(
                                id = next_id,
                                addr,
                                transport = "tcp",
                                "Remote TCP backend connected"
                            );
                            backends.insert(next_id, Box::new(backend));
                            next_id += 1;
                        }
                        Err(e) => {
                            tracing::warn!(addr, error = %e, "TCP backend failed — skipping peer");
                        }
                    }
                }
            }
        }

        let policy = Arc::new(PolicyEngine::new(Strategy::Lru));
        let stats = Arc::new(DaemonStats::new());

        Ok(Self {
            config,
            backends,
            policy,
            stats,
            round_robin: AtomicUsize::new(0),
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
            round_robin: AtomicUsize::new(0),
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
    ///
    /// If the offset already has a page stored, the old page is freed first
    /// to prevent handle leaks.
    ///
    /// If all backends are full, attempts LRU eviction before failing.
    pub fn store_page(&self, offset: u64, data: &PageBuffer) -> Result<PageHandle> {
        // Free the old page if this offset was already stored (prevent handle leak)
        if let Some(old_meta) = self.policy.remove(offset)
            && let Some(old_backend) = self.backends.get(&old_meta.backend_id)
        {
            let _ = old_backend.free_page(old_meta.handle);
        }

        let capacities = self.backend_capacities();
        let tier = match self.policy.select_tier(&capacities) {
            Some(t) => t,
            None => {
                // All backends full — try LRU eviction
                if let Some(evicted) = self.try_evict_one() {
                    tracing::debug!(evicted_offset = evicted, "evicted LRU page to make room");
                    // Re-check after eviction
                    let caps = self.backend_capacities();
                    self.policy.select_tier(&caps).ok_or_else(|| {
                        self.stats.store_errors.fetch_add(1, Ordering::Relaxed);
                        anyhow::anyhow!("all backends full even after eviction")
                    })?
                } else {
                    self.stats.store_errors.fetch_add(1, Ordering::Relaxed);
                    anyhow::bail!(
                        "all backends are full — no tier available and no evictable pages"
                    );
                }
            }
        };

        let backend_id = self.tier_to_backend_id(tier);
        let backend = self.backends.get(&backend_id).ok_or_else(|| {
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

    /// Attempt to evict one LRU page from any backend. Returns the evicted offset.
    fn try_evict_one(&self) -> Option<u64> {
        // Try each backend for an eviction candidate
        for (&backend_id, backend) in &self.backends {
            if let Some(offset) = self.policy.find_eviction_candidate(backend_id)
                && let Some(meta) = self.policy.remove(offset)
                && backend.free_page(meta.handle).is_ok()
            {
                self.stats.pages_invalidated.fetch_add(1, Ordering::Relaxed);
                return Some(offset);
            }
        }
        None
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
    /// Uses least-loaded selection to distribute pages fairly across backends
    /// in the same tier. This ensures balanced utilization even when peers
    /// have different capacities or different current load.
    fn tier_to_backend_id(&self, tier: Tier) -> u8 {
        // Collect all healthy backends with capacity for this tier
        let mut candidates: Vec<(u8, f64)> = self
            .backends
            .iter()
            .filter(|(_, backend)| {
                backend.tier() == tier && backend.is_healthy() && {
                    let (total, used) = backend.capacity();
                    used < total
                }
            })
            .map(|(&id, backend)| {
                let (total, used) = backend.capacity();
                let utilization = if total == 0 {
                    1.0
                } else {
                    used as f64 / total as f64
                };
                (id, utilization)
            })
            .collect();

        if !candidates.is_empty() {
            // Pick the least loaded backend (lowest utilization).
            // On ties, use round-robin to break the tie fairly.
            candidates.sort_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.cmp(&b.0))
            });

            let min_util = candidates[0].1;
            // Count how many are tied at the minimum utilization
            let tied: Vec<u8> = candidates
                .iter()
                .filter(|(_, u)| (*u - min_util).abs() < 0.001)
                .map(|(id, _)| *id)
                .collect();

            if tied.len() > 1 {
                // Break ties with round-robin
                let idx = self.round_robin.fetch_add(1, Ordering::Relaxed) % tied.len();
                return tied[idx];
            }
            return candidates[0].0;
        }

        // No exact tier match — fall back to any healthy backend with capacity
        // (same least-loaded logic)
        let mut fallback: Vec<(u8, f64)> = self
            .backends
            .iter()
            .filter(|(_, backend)| {
                backend.is_healthy() && {
                    let (total, used) = backend.capacity();
                    used < total
                }
            })
            .map(|(&id, backend)| {
                let (total, used) = backend.capacity();
                let utilization = if total == 0 {
                    1.0
                } else {
                    used as f64 / total as f64
                };
                (id, utilization)
            })
            .collect();

        if !fallback.is_empty() {
            fallback.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            return fallback[0].0;
        }

        // Last resort: return first backend id that exists
        self.backends.keys().next().copied().unwrap_or(0)
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
        // NOTE: This is rebuilt per-request cycle via backend_info_fn closure below
        let backends = &self.backends;

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            let stats = stats.clone();
                            let policy = policy.clone();
                            let backends_info: Vec<BackendInfo> = backends
                                .values()
                                .map(|b| BackendInfo::from_backend(b.as_ref()))
                                .collect();
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
            _ => serde_json::to_string(
                &serde_json::json!({"error": format!("unknown command: {}", cmd)}),
            )?,
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
    fn engine_capacity_overflow_triggers_eviction() {
        // Create config with tiny backend (2 pages max each)
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

        // 5th store should succeed via LRU eviction
        let result = engine.store_page(4, &data);
        assert!(result.is_ok(), "store should succeed via eviction");

        // At least one of the original 4 pages should have been evicted
        let mut buf = [0u8; PAGE_SIZE];
        let mut evicted = 0;
        for i in 0..4 {
            if engine.load_page(i, &mut buf).is_err() {
                evicted += 1;
            }
        }
        assert!(evicted >= 1, "at least one old page should be evicted");

        // The newly stored page should be loadable
        engine.load_page(4, &mut buf).unwrap();
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
