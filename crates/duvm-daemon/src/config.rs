//! Daemon configuration.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub daemon: DaemonSection,
    #[serde(default)]
    pub policy: PolicySection,
    #[serde(default)]
    pub backends: BackendsSection,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonSection {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_socket_path")]
    pub socket_path: String,
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicySection {
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default = "default_prefetch_depth")]
    pub prefetch_depth: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendsSection {
    #[serde(default)]
    pub memory: Option<MemoryBackendConfig>,
    #[serde(default)]
    pub compress: Option<CompressBackendConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryBackendConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_pages")]
    pub max_pages: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompressBackendConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_pages")]
    pub max_pages: u64,
}

fn default_log_level() -> String {
    "info".to_string()
}
fn default_socket_path() -> String {
    "/run/duvm/duvm.sock".to_string()
}
fn default_metrics_port() -> u16 {
    9100
}
fn default_strategy() -> String {
    "lru".to_string()
}
fn default_prefetch_depth() -> usize {
    4
}
fn default_true() -> bool {
    true
}
fn default_max_pages() -> u64 {
    262144
} // 1GB worth of 4KB pages

impl Default for DaemonSection {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            socket_path: default_socket_path(),
            metrics_port: default_metrics_port(),
        }
    }
}

impl Default for PolicySection {
    fn default() -> Self {
        Self {
            strategy: default_strategy(),
            prefetch_depth: default_prefetch_depth(),
        }
    }
}

impl Default for BackendsSection {
    fn default() -> Self {
        Self {
            memory: Some(MemoryBackendConfig {
                enabled: true,
                max_pages: default_max_pages(),
            }),
            compress: Some(CompressBackendConfig {
                enabled: true,
                max_pages: default_max_pages(),
            }),
        }
    }
}

impl DaemonConfig {
    pub fn load_or_default(path: &str) -> Self {
        if Path::new(path).exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => match toml::from_str(&content) {
                    Ok(config) => {
                        let config: DaemonConfig = config;
                        if let Err(e) = config.validate() {
                            tracing::warn!("Config validation failed: {}, using defaults", e);
                            return Self::default();
                        }
                        return config;
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse config {}: {}, using defaults", path, e);
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read config {}: {}, using defaults", path, e);
                }
            }
        } else {
            tracing::info!("Config file {} not found, using defaults", path);
        }
        Self::default()
    }

    /// Apply CLI argument overrides to the loaded config.
    pub fn apply_cli_overrides(&mut self, socket_path: Option<&str>, log_level: Option<&str>) {
        if let Some(path) = socket_path {
            self.daemon.socket_path = path.to_string();
        }
        if let Some(level) = log_level {
            self.daemon.log_level = level.to_string();
        }
    }

    /// Validate configuration values.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(ref mem) = self.backends.memory
            && mem.enabled
            && mem.max_pages == 0
        {
            return Err("backends.memory.max_pages must be > 0".to_string());
        }
        if let Some(ref comp) = self.backends.compress
            && comp.enabled
            && comp.max_pages == 0
        {
            return Err("backends.compress.max_pages must be > 0".to_string());
        }
        // Validate strategy is a known value
        match self.policy.strategy.as_str() {
            "lru" => {}
            other => {
                return Err(format!(
                    "unknown policy strategy: '{}' (expected 'lru')",
                    other
                ));
            }
        }
        Ok(())
    }
}
