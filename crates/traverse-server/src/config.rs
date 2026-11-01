//! Global server configuration: server-wide settings parsed once at startup and read from
//! connection handlers without threading them through every struct. Resolution: CLI args >
//! environment variables > TOML file > defaults.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

static QUERY_TIMEOUT: OnceLock<Option<Duration>> = OnceLock::new();
static SLOW_QUERY_THRESHOLD: OnceLock<Option<Duration>> = OnceLock::new();
static TX_IDLE_TIMEOUT: OnceLock<Option<Duration>> = OnceLock::new();

/// Default idle limit of an explicit transaction.
pub const DEFAULT_TX_IDLE_SECS: u64 = 60;

/// Sets the global default query timeout (once, at startup).
pub fn set_query_timeout(timeout: Option<Duration>) {
    QUERY_TIMEOUT.set(timeout).ok();
}

/// Global default query timeout.
pub fn query_timeout() -> Option<Duration> {
    QUERY_TIMEOUT.get().copied().flatten()
}

/// Sets the slow query logging threshold (once, at startup).
pub fn set_slow_query_threshold(threshold: Option<Duration>) {
    SLOW_QUERY_THRESHOLD.set(threshold).ok();
}

/// Slow query logging threshold.
pub fn slow_query_threshold() -> Option<Duration> {
    SLOW_QUERY_THRESHOLD.get().copied().flatten()
}

/// Sets the explicit-transaction idle limit (once, at startup); `None` disables it.
pub fn set_tx_idle_timeout(timeout: Option<Duration>) {
    TX_IDLE_TIMEOUT.set(timeout).ok();
}

/// How long an explicit transaction may sit between requests before it's rolled back: an open
/// transaction pins its snapshot, so the versions it can see can't be collected.
pub fn tx_idle_timeout() -> Option<Duration> {
    TX_IDLE_TIMEOUT
        .get()
        .copied()
        .unwrap_or(Some(Duration::from_secs(DEFAULT_TX_IDLE_SECS)))
}

/// Server configuration from the TOML file.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: Option<String>,
    pub http_listen: Option<String>,
    pub data: Option<String>,
    pub default_db: Option<String>,
    pub memory_limit: Option<String>,
    /// Per-query memory budget (`8GB`, `512MB`, `0` = unlimited); default a quarter of system RAM.
    /// See `traverse_cypher::memory`.
    pub query_memory_limit: Option<String>,
    pub auth: Option<String>,
    pub query_timeout: Option<u64>,
    /// Explicit-transaction idle limit in seconds (0 = none); default 60.
    pub tx_idle_timeout: Option<u64>,
    pub slow_query_threshold: Option<u64>,
    pub allowed_ips: Option<Vec<String>>,
    pub allow_drop_database: Option<bool>,
    pub quiet: Option<bool>,
    pub api_key: Option<String>,

    /// Studio's Graph Assistant: `anthropic` (the Claude API) or `openai` (any OpenAI-compatible
    /// endpoint); unset turns it off. See [`crate::ai`].
    pub ai_provider: Option<String>,
    /// The provider's API key (not needed by local OpenAI-compatible servers).
    pub ai_api_key: Option<String>,
    /// Model; default `claude-opus-5-5` on anthropic, required on openai.
    pub ai_model: Option<String>,
    /// API base URL; default `https://api.anthropic.com` / `https://api.openai.com/v1`.
    pub ai_base_url: Option<String>,
    /// Output tokens per answer; default 64000 on anthropic, the endpoint's own on openai.
    pub ai_max_tokens: Option<u32>,
    /// Claude API effort (`low` .. `max`); default the model's.
    pub ai_effort: Option<String>,

    pub log_level: Option<String>,
    pub log_dir: Option<String>,
    /// Durability mode: "snapshot" (default: in-memory writes, persisted on save/shutdown),
    /// "wal-async" (per-commit rlog append, background fsync, crash window bounded by
    /// `wal_fsync_ms`) or "wal" (per-commit crash durability via the redo log + group commit).
    pub durability: Option<String>,

    /// Background rlog fsync interval in ms for wal-async (default 1000).
    pub wal_fsync_ms: Option<u64>,

    /// Periodic checkpoint interval in seconds (default 300, 0 disables): persist dirty databases
    /// in the background.
    pub checkpoint_interval: Option<u64>,

    /// Where ML calls without a `device` setting run: `auto` (default: the first usable GPU, else
    /// the CPU), `cpu`, `gpu` or `gpu:<n>`.
    pub ml_device: Option<String>,

    /// Training jobs that may run at once (default 1); the rest queue.
    pub ml_max_jobs: Option<usize>,

    #[cfg(feature = "tls")]
    pub tls_cert: Option<String>,
    #[cfg(feature = "tls")]
    pub tls_key: Option<String>,

    #[cfg(feature = "grpc")]
    pub grpc_listen: Option<String>,
}

impl ServerConfig {
    /// Loads a TOML config file; `Default` if it doesn't exist.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read config file {}: {e}", path.display()))?;
        let config: ServerConfig = toml::from_str(&contents)
            .map_err(|e| format!("failed to parse config file {}: {e}", path.display()))?;
        Ok(config)
    }

    /// Overlays environment variables; they win over TOML values.
    pub fn merge_env(&mut self) {
        if let Ok(v) = std::env::var("TRAVERSE_LISTEN") {
            self.listen = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_HTTP_LISTEN") {
            self.http_listen = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_DATA") {
            self.data = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_DEFAULT_DB") {
            self.default_db = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_MEMORY_LIMIT") {
            self.memory_limit = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_QUERY_MEMORY_LIMIT") {
            self.query_memory_limit = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_AUTH") {
            self.auth = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_QUERY_TIMEOUT") {
            self.query_timeout = Some(
                v.parse()
                    .unwrap_or_else(|_| panic!("invalid TRAVERSE_QUERY_TIMEOUT: {v}")),
            );
        }
        if let Ok(v) = std::env::var("TRAVERSE_TX_IDLE_TIMEOUT") {
            self.tx_idle_timeout = Some(
                v.parse()
                    .unwrap_or_else(|_| panic!("invalid TRAVERSE_TX_IDLE_TIMEOUT: {v}")),
            );
        }
        if let Ok(v) = std::env::var("TRAVERSE_DURABILITY") {
            self.durability = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_WAL_FSYNC_MS") {
            self.wal_fsync_ms = Some(
                v.parse()
                    .unwrap_or_else(|_| panic!("invalid TRAVERSE_WAL_FSYNC_MS: {v}")),
            );
        }
        if let Ok(v) = std::env::var("TRAVERSE_ML_DEVICE") {
            self.ml_device = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_ML_MAX_JOBS") {
            self.ml_max_jobs = Some(
                v.parse()
                    .unwrap_or_else(|_| panic!("invalid TRAVERSE_ML_MAX_JOBS: {v}")),
            );
        }
        if let Ok(v) = std::env::var("TRAVERSE_CHECKPOINT_INTERVAL") {
            self.checkpoint_interval = Some(
                v.parse()
                    .unwrap_or_else(|_| panic!("invalid TRAVERSE_CHECKPOINT_INTERVAL: {v}")),
            );
        }
        if let Ok(v) = std::env::var("TRAVERSE_SLOW_QUERY_THRESHOLD") {
            self.slow_query_threshold = Some(
                v.parse()
                    .unwrap_or_else(|_| panic!("invalid TRAVERSE_SLOW_QUERY_THRESHOLD: {v}")),
            );
        }
        if let Ok(v) = std::env::var("TRAVERSE_ALLOWED_IPS") {
            self.allowed_ips = Some(v.split(',').map(|s| s.trim().to_string()).collect());
        }
        if let Ok(v) = std::env::var("TRAVERSE_ALLOW_DROP_DATABASE") {
            self.allow_drop_database = Some(v == "true" || v == "1");
        }
        if let Ok(v) = std::env::var("TRAVERSE_QUIET") {
            self.quiet = Some(v == "true" || v == "1");
        }
        if let Ok(v) = std::env::var("TRAVERSE_API_KEY") {
            self.api_key = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_AI_PROVIDER") {
            self.ai_provider = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_AI_API_KEY") {
            self.ai_api_key = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_AI_MODEL") {
            self.ai_model = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_AI_BASE_URL") {
            self.ai_base_url = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_AI_MAX_TOKENS") {
            self.ai_max_tokens = Some(
                v.parse()
                    .unwrap_or_else(|_| panic!("invalid TRAVERSE_AI_MAX_TOKENS: {v}")),
            );
        }
        if let Ok(v) = std::env::var("TRAVERSE_AI_EFFORT") {
            self.ai_effort = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_LOG_LEVEL") {
            self.log_level = Some(v);
        }
        if let Ok(v) = std::env::var("TRAVERSE_LOG_DIR") {
            self.log_dir = Some(v);
        }

        #[cfg(feature = "tls")]
        {
            if let Ok(v) = std::env::var("TRAVERSE_TLS_CERT") {
                self.tls_cert = Some(v);
            }
            if let Ok(v) = std::env::var("TRAVERSE_TLS_KEY") {
                self.tls_key = Some(v);
            }
        }

        #[cfg(feature = "grpc")]
        {
            if let Ok(v) = std::env::var("TRAVERSE_GRPC_LISTEN") {
                self.grpc_listen = Some(v);
            }
        }
    }

    /// Bolt listen address.
    pub fn listen(&self) -> String {
        self.listen.clone().unwrap_or_else(|| "0.0.0.0:7690".into())
    }

    /// HTTP listen address: `0.0.0.0:7691` by default, `"none"` or `"off"` disables it.
    pub fn http_listen(&self) -> Option<String> {
        let val = self
            .http_listen
            .clone()
            .unwrap_or_else(|| "0.0.0.0:7691".into());
        match val.as_str() {
            "none" | "off" | "false" | "disabled" => None,
            _ => Some(val),
        }
    }

    /// Default database name.
    pub fn default_db(&self) -> String {
        self.default_db.clone().unwrap_or_else(|| "traverse".into())
    }

    /// The Graph Assistant settings as configured.
    pub fn ai_settings(&self) -> crate::ai::AiSettings {
        crate::ai::AiSettings {
            provider: self.ai_provider.clone(),
            api_key: self.ai_api_key.clone(),
            model: self.ai_model.clone(),
            base_url: self.ai_base_url.clone(),
            max_tokens: self.ai_max_tokens,
            effort: self.ai_effort.clone(),
        }
    }

    /// Is DROP DATABASE allowed?
    pub fn allow_drop_database(&self) -> bool {
        self.allow_drop_database.unwrap_or(true)
    }

    /// Suppress the startup banner? It prints the API key; turn on for unattended/cloud deployments
    /// that ship stdout.
    pub fn quiet(&self) -> bool {
        self.quiet.unwrap_or(false)
    }

    /// Allowed IPs as a comma-separated string (for IpFilter::parse).
    pub fn allowed_ips_str(&self) -> Option<String> {
        self.allowed_ips.as_ref().map(|v| v.join(","))
    }

    /// gRPC listen address.
    #[cfg(feature = "grpc")]
    pub fn grpc_listen(&self) -> String {
        self.grpc_listen
            .clone()
            .unwrap_or_else(|| "0.0.0.0:50051".into())
    }
}
