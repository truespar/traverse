use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Forwards every formatted tracing line to the in-process ring buffer behind `apoc.log.stream`.
#[derive(Clone, Default)]
struct ApocLogWriter;

impl std::io::Write for ApocLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for line in String::from_utf8_lossy(buf).lines() {
            traverse_cypher::apoc_log::push_line(line);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ApocLogWriter {
    type Writer = ApocLogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ApocLogWriter
    }
}

use traverse_core::graph::Graph;
use traverse_core::storage::database::Database;
use traverse_core::tx::MvccStore;

use crate::auth::{AuthCatalog, Role, UserChange};
use crate::config::ServerConfig;
use crate::registry::DatabaseRegistry;
use crate::server::Server;
use crate::sync::Unpoisoned as _;

pub(crate) const COPYRIGHT: &str =
    "Copyright (c) 2026 The Intelligence Company AB (publ). MIT OR Apache-2.0.";

#[derive(Parser)]
#[command(
    name = "traverse-server",
    about = "Traverse graph database server",
    version = concat!(env!("CARGO_PKG_VERSION"), "\n", "Copyright (c) 2026 The Intelligence Company AB (publ). MIT OR Apache-2.0."),
    before_help = concat!("traverse-server ", env!("CARGO_PKG_VERSION")),
)]
pub(crate) struct Cli {
    /// Path to configuration file (TOML)
    #[arg(long, short)]
    config: Option<PathBuf>,

    /// Bolt protocol listen address [default: 0.0.0.0:7690]
    #[arg(long)]
    listen: Option<String>,

    /// HTTP API and WebSocket listen address [default: 0.0.0.0:7691, use "none" to disable]
    #[arg(long)]
    http_listen: Option<String>,

    /// Path to .tvdb file or directory of .tvdb files
    #[arg(long)]
    data: Option<String>,

    /// Default database name [default: traverse]
    #[arg(long)]
    default_db: Option<String>,

    /// Memory limit (e.g. 4GB, 512MB)
    #[arg(long)]
    memory_limit: Option<String>,

    /// Per-query memory budget (e.g. 8GB, 512MB, 0 = unlimited) [default: 25% of RAM]
    #[arg(long)]
    query_memory_limit: Option<String>,

    /// Seed admin credentials (user:pass)
    #[arg(long)]
    auth: Option<String>,

    /// Query timeout in milliseconds
    #[arg(long)]
    query_timeout: Option<u64>,

    /// Seconds an explicit transaction may sit idle before it's rolled back (0 = no limit)
    /// [default: 60]
    #[arg(long, value_name = "SECONDS")]
    tx_idle_timeout: Option<u64>,

    /// Slow query logging threshold in milliseconds
    #[arg(long)]
    slow_query_threshold: Option<u64>,

    /// IP allowlist (comma-separated CIDR ranges)
    #[arg(long)]
    allowed_ips: Option<String>,

    /// Allow DROP DATABASE via Studio and HTTP API
    #[arg(long)]
    allow_drop_database: bool,

    /// API key for HTTP API authentication (auto-generated if not set)
    #[arg(long)]
    api_key: Option<String>,

    /// Studio's Graph Assistant: anthropic (the Claude API) or openai (any
    /// OpenAI-compatible endpoint); its key comes from ai_api_key /
    /// TRAVERSE_AI_API_KEY [default: off]
    #[arg(long, value_name = "PROVIDER")]
    ai_provider: Option<String>,

    /// Graph Assistant model [default: claude-opus-5-5 on anthropic]
    #[arg(long, value_name = "MODEL")]
    ai_model: Option<String>,

    /// Graph Assistant API base URL (a gateway, Azure OpenAI, Ollama, ...)
    #[arg(long, value_name = "URL")]
    ai_base_url: Option<String>,

    /// Output tokens per Graph Assistant answer [default: 64000 on anthropic]
    #[arg(long, value_name = "N")]
    ai_max_tokens: Option<u32>,

    /// Graph Assistant effort on the Claude API: low, medium, high, xhigh or max
    #[arg(long, value_name = "LEVEL")]
    ai_effort: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long)]
    log_level: Option<String>,

    /// Directory for log files (daily rotation, e.g. traverse.log.2026-03-13)
    #[arg(long)]
    log_dir: Option<PathBuf>,

    /// Durability mode: snapshot (default), wal-async (per-commit rlog
    /// append, background fsync - crash window bounded by --wal-fsync-ms)
    /// or wal (fdatasync before every ack)
    #[arg(long, value_name = "MODE")]
    durability: Option<String>,

    /// Background rlog fsync interval in milliseconds (wal-async mode).
    #[arg(long, value_name = "MS")]
    wal_fsync_ms: Option<u64>,

    /// Periodic checkpoint interval in seconds: persist dirty databases in
    /// the background. Bounds crash loss in snapshot mode and rlog replay
    /// length in the WAL modes. 0 disables.
    #[arg(long, value_name = "SECONDS")]
    checkpoint_interval: Option<u64>,

    /// Where ML calls without a `device` setting run: auto (the first usable
    /// GPU, else the CPU), cpu, gpu or gpu:<n> [default: auto]
    #[arg(long, value_name = "DEVICE")]
    ml_device: Option<String>,

    /// Training jobs that may run at once; the rest queue [default: 1]
    #[arg(long, value_name = "N")]
    ml_max_jobs: Option<usize>,

    /// Override the Bolt server agent advertised to clients (e.g.
    /// "Neo4j/5.26.0" for libraries that verify a genuine Neo4j
    /// instance). Compatibility/debug aid; default: Traverse/<version>
    #[arg(long, value_name = "AGENT")]
    server_agent: Option<String>,

    /// TLS certificate path
    #[cfg(feature = "tls")]
    #[arg(long)]
    tls_cert: Option<String>,

    /// TLS private key path
    #[cfg(feature = "tls")]
    #[arg(long)]
    tls_key: Option<String>,

    /// gRPC listen address [default: 0.0.0.0:50051]
    #[cfg(feature = "grpc")]
    #[arg(long)]
    grpc_listen: Option<String>,

    /// Install as a Windows service
    #[cfg(windows)]
    #[arg(long)]
    service_install: bool,

    /// Uninstall the Windows service
    #[cfg(windows)]
    #[arg(long)]
    service_uninstall: bool,

    /// Run as a Windows service (used by the Service Control Manager)
    #[cfg(windows)]
    #[arg(long, hide = true)]
    service: bool,
}

impl Cli {
    /// Overlays CLI args onto a ServerConfig; CLI wins over everything.
    fn apply_to(&self, cfg: &mut ServerConfig) {
        if self.listen.is_some() {
            cfg.listen = self.listen.clone();
        }
        if self.http_listen.is_some() {
            cfg.http_listen = self.http_listen.clone();
        }
        if self.data.is_some() {
            cfg.data = self.data.clone();
        }
        if self.default_db.is_some() {
            cfg.default_db = self.default_db.clone();
        }
        if self.memory_limit.is_some() {
            cfg.memory_limit = self.memory_limit.clone();
        }
        if self.query_memory_limit.is_some() {
            cfg.query_memory_limit = self.query_memory_limit.clone();
        }
        if self.auth.is_some() {
            cfg.auth = self.auth.clone();
        }
        if self.query_timeout.is_some() {
            cfg.query_timeout = self.query_timeout;
        }
        if self.tx_idle_timeout.is_some() {
            cfg.tx_idle_timeout = self.tx_idle_timeout;
        }
        if self.slow_query_threshold.is_some() {
            cfg.slow_query_threshold = self.slow_query_threshold;
        }
        if self.allowed_ips.is_some() {
            cfg.allowed_ips = self
                .allowed_ips
                .as_ref()
                .map(|v| v.split(',').map(|s| s.trim().to_string()).collect());
        }
        if self.allow_drop_database {
            cfg.allow_drop_database = Some(true);
        }
        if self.api_key.is_some() {
            cfg.api_key = self.api_key.clone();
        }
        if self.ai_provider.is_some() {
            cfg.ai_provider = self.ai_provider.clone();
        }
        if self.ai_model.is_some() {
            cfg.ai_model = self.ai_model.clone();
        }
        if self.ai_base_url.is_some() {
            cfg.ai_base_url = self.ai_base_url.clone();
        }
        if self.ai_max_tokens.is_some() {
            cfg.ai_max_tokens = self.ai_max_tokens;
        }
        if self.ai_effort.is_some() {
            cfg.ai_effort = self.ai_effort.clone();
        }
        if self.log_level.is_some() {
            cfg.log_level = self.log_level.clone();
        }
        if self.log_dir.is_some() {
            cfg.log_dir = self.log_dir.as_ref().map(|p| p.display().to_string());
        }
        if self.durability.is_some() {
            cfg.durability = self.durability.clone();
        }
        if self.wal_fsync_ms.is_some() {
            cfg.wal_fsync_ms = self.wal_fsync_ms;
        }
        if self.checkpoint_interval.is_some() {
            cfg.checkpoint_interval = self.checkpoint_interval;
        }
        if self.ml_device.is_some() {
            cfg.ml_device = self.ml_device.clone();
        }
        if self.ml_max_jobs.is_some() {
            cfg.ml_max_jobs = self.ml_max_jobs;
        }

        #[cfg(feature = "tls")]
        {
            if self.tls_cert.is_some() {
                cfg.tls_cert = self.tls_cert.clone();
            }
            if self.tls_key.is_some() {
                cfg.tls_key = self.tls_key.clone();
            }
        }

        #[cfg(feature = "grpc")]
        {
            if self.grpc_listen.is_some() {
                cfg.grpc_listen = self.grpc_listen.clone();
            }
        }
    }
}

/// Where a config value came from (startup banner).
#[derive(Clone, Copy)]
enum ConfigSource {
    Default,
    Toml,
    Env,
    Arg,
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigSource::Default => write!(f, "default"),
            ConfigSource::Toml => write!(f, "toml"),
            ConfigSource::Env => write!(f, "env"),
            ConfigSource::Arg => write!(f, "arg"),
        }
    }
}

/// Key config values with their sources, for the banner.
pub(crate) struct ConfigSources {
    listen: ConfigSource,
    http_listen: ConfigSource,
    data: ConfigSource,
    auth: ConfigSource,
    memory_limit: Option<ConfigSource>,
}

/// Publishes the resolved server config to the engine's settings registry behind `SHOW SETTINGS`.
/// Secrets (the API key) are redacted to a presence marker.
fn register_show_settings(cfg: &ServerConfig) {
    use traverse_cypher::settings::register_setting;
    let redact = |v: &Option<String>| -> String {
        match v {
            Some(s) if !s.is_empty() => "<redacted>".to_string(),
            _ => String::new(),
        }
    };
    let opt = |v: &Option<String>| v.clone().unwrap_or_default();

    register_setting(
        "server.bolt.listen_address",
        &opt(&cfg.listen),
        "0.0.0.0:7690",
        "Bolt protocol listen address.",
        false,
    );
    register_setting(
        "server.http.listen_address",
        &opt(&cfg.http_listen),
        "0.0.0.0:7691",
        "HTTP API / WebSocket listen address (\"none\" disables).",
        false,
    );
    register_setting(
        "server.directories.data",
        &opt(&cfg.data),
        "",
        "Data directory or .tvdb file the databases load from.",
        false,
    );
    register_setting(
        "server.default_database",
        &opt(&cfg.default_db),
        "traverse",
        "Database selected when a session names none.",
        false,
    );
    register_setting(
        "server.memory.limit",
        &opt(&cfg.memory_limit),
        "90% of system RAM",
        "Memory budget for graph data and query execution.",
        false,
    );
    register_setting(
        "db.memory.transaction.max",
        &opt(&cfg.query_memory_limit),
        "25% of system RAM",
        "Per-query memory budget: a query whose buffers exceed it fails instead of \
         exhausting the server (TRAVERSE_QUERY_MEMORY_LIMIT; 0 = unlimited).",
        false,
    );
    register_setting(
        "db.transaction.idle_timeout",
        &cfg.tx_idle_timeout
            .unwrap_or(crate::config::DEFAULT_TX_IDLE_SECS)
            .to_string(),
        &crate::config::DEFAULT_TX_IDLE_SECS.to_string(),
        "Seconds an explicit transaction may sit idle before it's rolled back \
         (TRAVERSE_TX_IDLE_TIMEOUT; 0 = no limit).",
        false,
    );
    register_setting(
        "db.transaction.timeout",
        &cfg.query_timeout.map(|v| v.to_string()).unwrap_or_default(),
        "",
        "Per-query timeout in milliseconds (empty = unlimited).",
        false,
    );
    register_setting(
        "db.logs.query.threshold",
        &cfg.slow_query_threshold
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "",
        "Slow-query log threshold in milliseconds.",
        false,
    );
    register_setting(
        "server.databases.allow_drop",
        &cfg.allow_drop_database
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "true",
        "Whether DROP DATABASE (and the Studio delete button) is permitted.",
        false,
    );
    register_setting(
        "server.ml.device",
        &opt(&cfg.ml_device),
        "auto",
        "Where ML calls without a `device` setting run: auto, cpu, gpu or gpu:<n> \
         (TRAVERSE_ML_DEVICE).",
        false,
    );
    register_setting(
        "server.ml.max_jobs",
        &cfg.ml_max_jobs.map(|v| v.to_string()).unwrap_or_default(),
        "1",
        "Training jobs that may run at once; the rest queue (TRAVERSE_ML_MAX_JOBS).",
        false,
    );
    register_setting(
        "server.auth.enabled",
        &opt(&cfg.auth),
        "",
        "Authentication mode.",
        false,
    );
    register_setting(
        "server.api_key",
        &redact(&cfg.api_key),
        "",
        "HTTP API key (redacted).",
        false,
    );
    register_setting(
        "server.ai.provider",
        &opt(&cfg.ai_provider),
        "",
        "Studio Graph Assistant provider: anthropic, openai or empty (off) (TRAVERSE_AI_PROVIDER).",
        false,
    );
    register_setting(
        "server.ai.model",
        &opt(&cfg.ai_model),
        "",
        "Graph Assistant model; empty = the provider's default (TRAVERSE_AI_MODEL).",
        false,
    );
}

/// Resolved ServerConfig: TOML (if any) -> env vars -> CLI args. Returns (config,
/// config file path if one was loaded, config sources).
pub(crate) fn resolve_config(
    cli: &Cli,
) -> Result<(ServerConfig, Option<String>, ConfigSources), Box<dyn std::error::Error>> {
    // 1. TOML base config
    let (mut cfg, config_path) = if let Some(ref path) = cli.config {
        let p = path.display().to_string();
        (ServerConfig::load(path)?, Some(p))
    } else {
        // auto-detect traverse.toml in the CWD
        let default_path = PathBuf::from("traverse.toml");
        if default_path.exists() {
            tracing::info!("using config file: traverse.toml");
            (
                ServerConfig::load(&default_path)?,
                Some("traverse.toml".into()),
            )
        } else {
            (ServerConfig::default(), None)
        }
    };

    // sources start as toml (if loaded) or default
    let base = if config_path.is_some() {
        ConfigSource::Toml
    } else {
        ConfigSource::Default
    };
    let mut src = ConfigSources {
        listen: if cfg.listen.is_some() {
            base
        } else {
            ConfigSource::Default
        },
        http_listen: if cfg.http_listen.is_some() {
            base
        } else {
            ConfigSource::Default
        },
        data: if cfg.data.is_some() {
            base
        } else {
            ConfigSource::Default
        },
        auth: if cfg.auth.is_some() {
            base
        } else {
            ConfigSource::Default
        },
        memory_limit: cfg.memory_limit.as_ref().map(|_| base),
    };

    // 2. environment variables
    if std::env::var("TRAVERSE_LISTEN").is_ok() {
        src.listen = ConfigSource::Env;
    }
    if std::env::var("TRAVERSE_HTTP_LISTEN").is_ok() {
        src.http_listen = ConfigSource::Env;
    }
    if std::env::var("TRAVERSE_DATA").is_ok() {
        src.data = ConfigSource::Env;
    }
    if std::env::var("TRAVERSE_AUTH").is_ok() {
        src.auth = ConfigSource::Env;
    }
    if std::env::var("TRAVERSE_MEMORY_LIMIT").is_ok() {
        src.memory_limit = Some(ConfigSource::Env);
    }
    cfg.merge_env();

    // 3. CLI args (highest priority)
    if cli.listen.is_some() {
        src.listen = ConfigSource::Arg;
    }
    if cli.http_listen.is_some() {
        src.http_listen = ConfigSource::Arg;
    }
    if cli.data.is_some() {
        src.data = ConfigSource::Arg;
    }
    if cli.auth.is_some() {
        src.auth = ConfigSource::Arg;
    }
    if cli.memory_limit.is_some() {
        src.memory_limit = Some(ConfigSource::Arg);
    }
    cli.apply_to(&mut cfg);

    // resolve durability and pass the FINAL value on through the env: the writer-attachment gate
    // (registry::wal_durability_enabled) reads it deep in store construction, where there's no
    // config handle
    let durability = match cfg.durability.as_deref() {
        None | Some("snapshot") => "snapshot",
        Some("wal") | Some("WAL") | Some("full") | Some("1") | Some("true") => "wal",
        Some("wal-async") | Some("wal_async") | Some("walasync") | Some("async") => "wal-async",
        Some(other) => {
            eprintln!(
                "warning: unknown durability mode `{other}` - using `snapshot` \
                 (valid: snapshot, wal-async, wal)"
            );
            "snapshot"
        }
    };
    cfg.durability = Some(durability.to_string());
    // SAFETY: called during single-threaded startup, before the runtime
    // and any worker threads exist.
    unsafe {
        std::env::set_var("TRAVERSE_DURABILITY", durability);
    }

    // --server-agent goes through the env too: the Bolt HELLO handler reads TRAVERSE_SERVER_AGENT
    // per connection (connection::session); the CLI wins over a pre-set env var, like the rest of the
    // config
    if let Some(ref agent) = cli.server_agent {
        // SAFETY: single-threaded startup, same as TRAVERSE_DURABILITY.
        unsafe {
            std::env::set_var("TRAVERSE_SERVER_AGENT", agent);
        }
    }

    Ok((cfg, config_path, src))
}

/// Entry point: creates a tokio runtime and runs the server.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // mimalloc: purge abandoned pages on thread exit so RSS drops promptly after
    // rayon workers shut down. Hot-swap RSS release is explicit, via
    // `purge_allocator()` (which fans `mi_collect(true)` across all rayon workers)
    // at drop/unload/import sites; `purge_delay` keeps its 10ms default, since
    // lowering it taxes every alloc/free on the hot query path.
    //
    // Option values from the bundled mimalloc v2 header
    // (libmimalloc-sys 0.1.44, c_src/mimalloc/v2/include/mimalloc.h):
    //   12 = mi_option_abandoned_page_purge  (purge on thread exit, default=0)
    //
    // Cross-checked against a known constant (mi_option_eager_commit_delay=14,
    // exposed by the Rust bindings) in case the enum ever shifts.
    #[cfg(feature = "native-alloc")]
    {
        const MI_OPTION_ABANDONED_PAGE_PURGE: i32 = 12;

        debug_assert_eq!(
            libmimalloc_sys::mi_option_eager_commit_delay,
            14,
            "mimalloc option enum layout changed - update purge option constants"
        );

        // SAFETY: a plain global-option write at startup; no pointers involved
        unsafe {
            libmimalloc_sys::mi_option_set(MI_OPTION_ABANDONED_PAGE_PURGE, 1);
        }
    }

    // crypto provider before any TLS config (Bolt or HTTP)
    #[cfg(feature = "tls")]
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    rayon::ThreadPoolBuilder::new()
        .stack_size(2 * 1024 * 1024)
        .build_global()
        .expect("failed to configure rayon thread pool");

    traverse_core::memory::activate_tracking();

    let cli = Cli::parse();

    // Windows service commands before any server setup
    #[cfg(windows)]
    {
        if cli.service_install {
            return crate::win_service::install().map_err(Into::into);
        }
        if cli.service_uninstall {
            return crate::win_service::uninstall().map_err(Into::into);
        }
        if cli.service {
            // running as a Windows service: hand off to the SCM; blocks until the service stops
            // (the service entry point makes its own tokio runtime)
            return crate::win_service::run_as_service().map_err(Into::into);
        }
    }

    // log level and log dir from CLI/env, before config resolution
    let log_level = cli
        .log_level
        .clone()
        .or_else(|| std::env::var("TRAVERSE_LOG_LEVEL").ok())
        .unwrap_or_else(|| "info".into());
    let log_dir = cli
        .log_dir
        .clone()
        .or_else(|| std::env::var("TRAVERSE_LOG_DIR").ok().map(PathBuf::from));

    // tracing: file + stdout with --log-dir, else stdout only
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&log_level));

    // guard lives for the whole process; tracing-appender's non-blocking writer flushes when it
    // drops
    #[cfg(feature = "native-alloc")]
    let _log_guard: Option<tracing_appender::non_blocking::WorkerGuard>;

    if let Some(ref dir) = log_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("Failed to create log directory `{}`: {e}", dir.display());
            std::process::exit(1);
        }

        #[cfg(feature = "native-alloc")]
        {
            let file_appender = tracing_appender::rolling::daily(dir, "traverse.log");
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            _log_guard = Some(guard);

            use tracing_subscriber::layer::SubscriberExt;
            use tracing_subscriber::util::SubscriberInitExt;

            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(non_blocking);
            let stdout_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

            let ring_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(ApocLogWriter);
            tracing_subscriber::registry()
                .with(env_filter)
                .with(file_layer)
                .with(stdout_layer)
                .with(ring_layer)
                .init();
        }

        #[cfg(not(feature = "native-alloc"))]
        {
            {
                use tracing_subscriber::layer::SubscriberExt;
                use tracing_subscriber::util::SubscriberInitExt;
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_ansi(false)
                            .with_writer(ApocLogWriter),
                    )
                    .init();
            }
            tracing::warn!("--log-dir ignored (file logging requires native-alloc feature)");
        }
    } else {
        #[cfg(feature = "native-alloc")]
        {
            _log_guard = None;
        }

        {
            use tracing_subscriber::layer::SubscriberExt;
            use tracing_subscriber::util::SubscriberInitExt;
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(ApocLogWriter),
                )
                .init();
        }
    };

    tracing::debug!("RSS-based memory tracking active");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(cli, shutdown_signal(), || Ok(())))
}

/// SIGINT (Ctrl+C) or SIGTERM (what process managers send).
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => tracing::info!("SIGINT received, shutting down gracefully"),
            _ = sigterm.recv() => tracing::info!("SIGTERM received, shutting down gracefully"),
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
        tracing::info!("Ctrl+C received, shutting down gracefully");
    }
}

/// The server, interactive or as a Windows service: runs until `shutdown` resolves, then saves
/// every database. `ready` runs once every listener is bound.
pub(crate) async fn serve(
    cli: Cli,
    shutdown: impl std::future::Future<Output = ()>,
    ready: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cfg, config_path, config_sources) = resolve_config(&cli)?;

    // resolved config for `SHOW SETTINGS` (Neo4j-style setting names); secrets are redacted here,
    // the registry shows values verbatim
    register_show_settings(&cfg);

    let listen_addr = cfg.listen();

    // memory budget: explicit limit, else 90% of system RAM
    let memory_budget = match cfg.memory_limit {
        Some(ref val) => {
            let budget =
                parse_memory_limit(val).ok_or_else(|| format!("invalid memory_limit: {val}"))?;
            tracing::info!("memory limit set to {} bytes ({val})", budget);
            Some(budget)
        }
        None => {
            if let Some(total) = total_system_memory() {
                let budget = total / 10 * 9; // 90% of system RAM
                tracing::debug!(
                    "no memory limit set, defaulting to 90% of system RAM ({} / {} bytes)",
                    format_bytes(budget),
                    format_bytes(total),
                );
                Some(budget)
            } else {
                tracing::warn!("could not detect system memory, running without memory limit");
                None
            }
        }
    };

    if let Some(budget) = memory_budget {
        traverse_core::memory::set_memory_ceiling(budget);
    }

    // per-query budget: explicit value, else the engine default (a quarter of system RAM); `0` /
    // `unlimited` disables it
    if let Some(ref val) = cfg.query_memory_limit {
        let limit = traverse_cypher::memory::parse_byte_size(val)
            .ok_or_else(|| format!("invalid query_memory_limit: {val}"))?;
        traverse_cypher::memory::set_query_limit(limit);
        if limit == 0 {
            tracing::info!("per-query memory limit disabled");
        } else {
            tracing::info!(
                "per-query memory limit set to {} ({val})",
                format_bytes(limit)
            );
        }
    } else {
        tracing::debug!(
            "per-query memory limit defaulting to {}",
            format_bytes(traverse_cypher::memory::query_limit()),
        );
    }

    let registry = open_registry(&cfg, memory_budget)?;

    // where ML runs, and how many training jobs at once
    #[cfg(feature = "ml")]
    {
        let wanted = cfg.ml_device.as_deref().unwrap_or("auto");
        let request: traverse_cypher::ml::device::Request = wanted
            .parse()
            .map_err(|e| format!("TRAVERSE_ML_DEVICE: {e}"))?;
        for line in traverse_cypher::ml::device::gpu_report() {
            tracing::info!("ML: {line}");
        }
        match traverse_cypher::ml::device::set_default_device(request) {
            Ok(device) => tracing::info!("ML: calls run on {device} unless they name a device"),
            Err(e) => tracing::error!(
                "ML: TRAVERSE_ML_DEVICE={request} can't be used ({e}); ML calls without a device \
                 setting fail until it can"
            ),
        }
        traverse_cypher::ml::set_max_training_jobs(cfg.ml_max_jobs.unwrap_or(1));
    }
    #[cfg(not(feature = "ml"))]
    if cfg.ml_device.is_some() || cfg.ml_max_jobs.is_some() {
        tracing::warn!("TRAVERSE_ML_DEVICE / TRAVERSE_ML_MAX_JOBS ignored: built without ML");
    }

    // ML state lives next to the `.tvdb`s: the pipeline catalog (every build) and trained models
    // as `<name>.tvmodel` (ml builds); both reload here
    if let Some(dir) = registry.data_dir() {
        match traverse_cypher::ml::registry::open_data_dir(dir) {
            Ok(0) => {}
            Ok(n) => tracing::info!("loaded {n} ML pipeline(s) from {}", dir.display()),
            Err(e) => tracing::error!("ML pipeline catalog: {e}"),
        }
        #[cfg(feature = "ml")]
        {
            let (loaded, failed) = traverse_cypher::ml::registry::load_models_from_dir(dir);
            for f in &failed {
                tracing::warn!("ML model not loaded: {f}");
            }
            if loaded > 0 {
                tracing::info!("loaded {loaded} ML model(s) from {}", dir.display());
            }
        }
    }

    // background rlog fsync (wal-async), bounding crash loss to the interval; a failed sync poisons
    // the sync control and every later commit fails, so nothing keeps acking onto a broken log
    if crate::registry::durability_mode() == crate::registry::DurabilityMode::WalAsync {
        let reg = Arc::clone(&registry);
        let interval_ms = cfg.wal_fsync_ms.unwrap_or(1000).max(10);
        tracing::info!("wal-async: background rlog fsync every {interval_ms} ms");
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                for (name, _, _, _) in reg.list_databases() {
                    let Some(entry) = reg.get(&name) else {
                        continue;
                    };
                    let Some(w) = entry.store.log_writer() else {
                        continue;
                    };
                    // only the handle under the writer mutex: commits keep appending during the sync
                    let sync = w.lock().unwrap_or_else(|e| e.into_inner()).sync_handle();
                    let res = tokio::task::spawn_blocking(move || sync.sync_now()).await;
                    if let Ok(Err(e)) = res {
                        tracing::error!(
                            db = name.as_str(),
                            "background rlog fsync failed - log poisoned, writes will fail: {e}"
                        );
                    }
                }
            }
        });
    }

    // periodic checkpoint: persist dirty databases. In snapshot mode this bounds crash loss; in WAL
    // modes it bounds restart replay and lets rlog segments be reclaimed. The registry serializes
    // it with every other save.
    let checkpoint_secs = cfg.checkpoint_interval.unwrap_or(300);
    if checkpoint_secs > 0 {
        let reg = Arc::clone(&registry);
        tracing::info!("checkpoint: persisting dirty databases every {checkpoint_secs} s");
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(checkpoint_secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // the first tick fires immediately: skip it
            loop {
                tick.tick().await;
                let r = Arc::clone(&reg);
                let res =
                    tokio::task::spawn_blocking(move || r.persist_all().map_err(|e| e.to_string()))
                        .await;
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("checkpoint save failed: {e}"),
                    Err(e) => tracing::warn!("checkpoint task panicked: {e}"),
                }
            }
        });
    }

    let auth_catalog = open_user_catalog(&cfg, &registry)?;

    // query timeout
    if let Some(ms) = cfg.query_timeout {
        crate::config::set_query_timeout(Some(Duration::from_millis(ms)));
        tracing::info!("query timeout set to {ms}ms");
    } else {
        crate::config::set_query_timeout(None);
    }
    crate::config::set_tx_idle_timeout(tx_idle_limit(cfg.tx_idle_timeout));

    if let Some(ms) = cfg.slow_query_threshold {
        crate::config::set_slow_query_threshold(Some(Duration::from_millis(ms)));
        tracing::info!("slow query logging threshold set to {ms}ms");
    } else {
        crate::config::set_slow_query_threshold(None);
    }

    // IP allowlist
    let ip_filter = match cfg.allowed_ips_str() {
        Some(val) => {
            let filter = crate::ip_filter::IpFilter::parse(&val)
                .map_err(|e| format!("invalid allowed_ips: {e}"))?;
            tracing::info!("IP allowlist active: {} ranges configured", filter.len());
            Arc::new(filter)
        }
        None => Arc::new(crate::ip_filter::IpFilter::allow_all()),
    };

    // a provider setting that can't work stops startup instead of failing every question
    let ai = crate::ai::AiConfig::resolve(cfg.ai_settings())?;

    #[allow(unused_mut)]
    let mut server = Server::with_registry(Arc::clone(&registry), listen_addr)
        .with_auth_catalog(auth_catalog)
        .with_ip_filter((*ip_filter).clone());

    // shared TLS config (if enabled)
    #[cfg(feature = "tls")]
    let tls_server_config: Option<Arc<tokio_rustls::rustls::ServerConfig>> = {
        let tls_cert = cfg.tls_cert.clone();
        let tls_key = cfg.tls_key.clone();

        if let (Some(cert_path), Some(key_path)) = (tls_cert, tls_key) {
            use rustls_pki_types::pem::PemObject;
            use rustls_pki_types::{CertificateDer, PrivateKeyDer};
            use tokio_rustls::rustls;

            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert_path)
                .map_err(|e| format!("failed to open TLS cert {cert_path}: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("failed to parse TLS cert {cert_path}: {e}"))?;
            let key = PrivateKeyDer::from_pem_file(&key_path)
                .map_err(|e| format!("failed to read TLS key {key_path}: {e}"))?;

            let config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)?;

            let config = Arc::new(config);

            let tls_config = crate::server::TlsConfig {
                acceptor: tokio_rustls::TlsAcceptor::from(Arc::clone(&config)),
            };
            server = server.with_tls(tls_config);
            tracing::info!("TLS enabled with cert={cert_path} key={key_path}");

            Some(config)
        } else {
            None
        }
    };

    // auth catalog handle for shutdown persistence, HTTP and gRPC
    let auth_catalog_ref = server.auth_catalog().clone();

    // --- bind every listener before printing the banner ---

    let (bolt_listener, bolt_addr) = server.bind().await?;

    let allow_drop_db = cfg.allow_drop_database();

    // HTTP is on by default (port 7691); --http-listen none disables it
    let http_setup = if let Some(http_addr_str) = cfg.http_listen() {
        let http_addr: std::net::SocketAddr = http_addr_str
            .parse()
            .map_err(|e| format!("invalid http_listen: {e}"))?;

        // API key: explicit, else generated
        let api_key = match cfg.api_key {
            Some(ref key) => {
                if !key.starts_with("tvs_") {
                    return Err("API key must start with 'tvs_' prefix".into());
                }
                key.clone()
            }
            None => generate_api_key(),
        };

        #[allow(unused_mut)]
        let mut http_server = crate::http::HttpServer::new(
            Arc::clone(&registry),
            Arc::clone(&auth_catalog_ref),
            http_addr,
        )
        .with_ip_filter(Arc::clone(&ip_filter))
        .with_allow_drop_database(allow_drop_db)
        .with_api_key(Some(api_key.clone()))
        .with_ai(ai.clone());

        #[cfg(feature = "tls")]
        if let Some(ref config) = tls_server_config {
            http_server = http_server.with_tls(tokio_rustls::TlsAcceptor::from(Arc::clone(config)));
        }

        let (http_listener, http_bound_addr) = http_server.bind().await?;
        Some((http_server, http_listener, http_bound_addr, api_key))
    } else {
        None
    };

    #[cfg(feature = "grpc")]
    let grpc_addr_str = cfg.grpc_listen();
    #[cfg(feature = "grpc")]
    let grpc_sock_addr: std::net::SocketAddr = grpc_addr_str
        .parse()
        .map_err(|e| format!("invalid grpc_listen: {e}"))?;
    #[cfg(feature = "grpc")]
    let grpc_server = crate::grpc::GrpcServer::new(Arc::clone(&registry), grpc_sock_addr)
        .with_auth_catalog(auth_catalog_ref.clone());
    #[cfg(feature = "grpc")]
    let (grpc_listener, grpc_bound_addr) = grpc_server.bind().await?;

    // --- startup banner ---
    {
        let db_summary = registry.database_summary();
        let auth_user_count = auth_catalog_ref.read_unpoisoned().list_users().len();

        #[cfg(feature = "grpc")]
        let grpc_display = Some(grpc_bound_addr);
        #[cfg(not(feature = "grpc"))]
        let grpc_display: Option<std::net::SocketAddr> = None;

        let (http_display, api_key_display) = match &http_setup {
            Some((_, _, addr, key)) => (Some(*addr), Some(key.as_str())),
            None => (None, None),
        };

        if !cfg.quiet() {
            print_startup_banner(
                env!("CARGO_PKG_VERSION"),
                bolt_addr,
                http_display,
                grpc_display,
                &db_summary,
                auth_user_count,
                config_path.as_deref(),
                api_key_display,
                &config_sources,
                cfg.data.as_deref(),
                cfg.memory_limit.as_deref(),
                cfg.log_dir.as_deref(),
                ai.as_ref(),
            );
        }
    }

    ready()?;

    // --- start servers ---

    #[cfg(feature = "grpc")]
    let grpc_handle = tokio::spawn(async move {
        if let Err(e) = grpc_server.run_with_listener(grpc_listener).await {
            tracing::error!("gRPC server error: {e}");
        }
    });

    let http_handle = if let Some((http_server, http_listener, _, _)) = http_setup {
        Some(tokio::spawn(async move {
            if let Err(e) = http_server.run_with_listener(http_listener).await {
                tracing::error!("HTTP server error: {e}");
            }
        }))
    } else {
        None
    };

    tokio::select! {
        result = server.run_with_listener(bolt_listener) => { result?; }
        _ = shutdown => {}
    }

    #[cfg(feature = "grpc")]
    grpc_handle.abort();
    if let Some(handle) = http_handle {
        handle.abort();
    }

    // user changes are saved as they happen; this saves the data
    registry.persist_all()?;

    Ok(())
}

/// Opens the databases `cfg.data` names: one `.tvdb` (single-file mode), a directory of them, or
/// nothing (in memory). The default database's metadata, the user catalog, comes along.
pub(crate) fn open_registry(
    cfg: &ServerConfig,
    memory_budget: Option<usize>,
) -> Result<Arc<DatabaseRegistry>, Box<dyn std::error::Error>> {
    let default_db = cfg.default_db();
    let Some(path_str) = &cfg.data else {
        // no persistence: in memory only
        let store = Arc::new(MvccStore::with_budget(Graph::new(), memory_budget));
        return Ok(Arc::new(DatabaseRegistry::single(default_db, store, None)));
    };
    let path = PathBuf::from(path_str);
    if path.is_file() || path.extension().is_some_and(|e| e == "tvdb") {
        // single-file mode: the "traverse" (or configured default) database
        tracing::info!("opening database at {}", path.display());
        let existed = path.is_file();
        let db = Database::open_with_durability(&path, crate::registry::wal_durability_enabled())?;
        // a new database has no file yet, and commits replayed from the log aren't in the file: a
        // save must write them
        let unsaved = !existed || db.replayed_transactions() > 0;
        let (graph, db_path, metadata, log_writer, recovery_floor) = db.into_parts();
        let node_count = graph.node_count();
        let edge_count = graph.edge_count();
        let mem = graph.estimated_memory();
        tracing::info!(
            "loaded {node_count} nodes, {edge_count} edges from disk (estimated {mem} bytes)"
        );
        if let Some(budget) = memory_budget
            && mem > budget
        {
            tracing::warn!(
                "loaded graph uses {mem} bytes, exceeding memory limit of {budget} bytes"
            );
        }
        // the writerless-replay floor too, so a snapshot-mode boot that replayed a prior WAL run's
        // tail stamps it into the next snapshot
        let store = Arc::new(crate::registry::install_writer_with_floor(
            MvccStore::with_budget(graph, memory_budget),
            log_writer,
            recovery_floor,
        ));
        Ok(Arc::new(
            DatabaseRegistry::single(default_db, store, Some(db_path))
                .loaded(unsaved)
                .with_metadata(metadata),
        ))
    } else if path.is_dir() {
        // directory mode: each *.tvdb becomes a named database
        tracing::info!("scanning directory {} for databases", path.display());
        Ok(Arc::new(DatabaseRegistry::from_directory(
            path,
            default_db,
            memory_budget,
        )?))
    } else {
        // path doesn't exist yet: single-file, created by the first save
        tracing::info!(
            "database path {} does not exist, will create on first save",
            path.display()
        );
        let store = Arc::new(MvccStore::with_budget(Graph::new(), memory_budget));
        Ok(Arc::new(DatabaseRegistry::single(
            default_db,
            store,
            Some(path),
        )))
    }
}

/// The user catalog from the default database's file, seeded with `cfg.auth`'s admin when it has
/// no users. One that's there but can't be read stops startup, since starting without it would
/// mean open access, unless `cfg.auth` names an admin to start over with.
pub(crate) fn open_user_catalog(
    cfg: &ServerConfig,
    registry: &DatabaseRegistry,
) -> Result<AuthCatalog, Box<dyn std::error::Error>> {
    let seed = match &cfg.auth {
        Some(auth) => Some(
            auth.split_once(':')
                .ok_or("auth must be in user:pass format")?,
        ),
        None => None,
    };
    let metadata = registry.metadata();
    let catalog = if metadata.is_empty() {
        AuthCatalog::new()
    } else {
        match AuthCatalog::deserialize(&metadata) {
            Ok(catalog) => {
                let user_count = catalog.list_users().len();
                tracing::info!("loaded auth catalog with {user_count} users from disk");
                catalog
            }
            Err(e) if seed.is_some() => {
                tracing::error!(
                    "the user catalog can't be read ({e}); replacing it with the admin from the \
                     auth setting"
                );
                AuthCatalog::new()
            }
            Err(e) => {
                return Err(format!(
                    "the user catalog can't be read ({e}); refusing to start without it (set \
                     TRAVERSE_AUTH=user:pass to replace it with that admin)"
                )
                .into());
            }
        }
    };

    // empty catalog and auth set: seed the first admin, saved right away
    let users = std::sync::RwLock::new(catalog);
    if let Some((user, pass)) = seed {
        if users.read_unpoisoned().is_empty() {
            let admin = UserChange::add(user, pass, Role::Admin)
                .map_err(|e| format!("failed to seed admin user: {e}"))?;
            registry
                .change_users(&users, &admin)
                .map_err(|e| format!("failed to seed admin user: {e}"))?;
            tracing::info!("seeded admin user '{user}' from auth config");
        } else {
            tracing::info!("auth catalog already has users, ignoring auth seed");
        }
    }
    let users = users
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if users.is_empty() {
        tracing::debug!("no users configured - authentication disabled (open access)");
    }
    Ok(users)
}

/// Random API key: `tvs_` prefix + 32 hex chars (128 bits).
pub(crate) fn generate_api_key() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("failed to generate random API key");
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("tvs_{hex}")
}

/// Startup banner with every connection URL and status.
fn print_startup_banner(
    version: &str,
    bolt_addr: std::net::SocketAddr,
    http_addr: Option<std::net::SocketAddr>,
    grpc_addr: Option<std::net::SocketAddr>,
    db_summary: &[(String, usize, usize, bool)],
    auth_user_count: usize,
    config_loaded: Option<&str>,
    api_key: Option<&str>,
    sources: &ConfigSources,
    data_path: Option<&str>,
    memory_limit: Option<&str>,
    log_dir: Option<&str>,
    ai: Option<&crate::ai::AiConfig>,
) {
    fn display_host(addr: std::net::SocketAddr) -> String {
        if addr.ip().is_unspecified() {
            format!("localhost:{}", addr.port())
        } else {
            addr.to_string()
        }
    }

    fn format_count(n: usize) -> String {
        if n >= 1_000_000 {
            let m = n as f64 / 1_000_000.0;
            if m >= 100.0 {
                format!("{:.0}M", m)
            } else {
                format!("{:.1}M", m)
            }
        } else if n >= 1_000 {
            let k = n as f64 / 1_000.0;
            if k >= 100.0 {
                format!("{:.0}K", k)
            } else {
                format!("{:.1}K", k)
            }
        } else {
            n.to_string()
        }
    }

    const DIM: &str = "\x1b[2m";
    const CYAN: &str = "\x1b[36m";
    const YELLOW: &str = "\x1b[33m";
    const RESET: &str = "\x1b[0m";

    println!();
    println!("  {CYAN}▀█▀ █▀▄ ▄▀▄ █ █ █▀▀ █▀▄ ▄▀▀ █▀▀{RESET}   {DIM}v{version}{RESET}");
    println!("  {CYAN} █  █▄▀ █▀█ ▀▄▀ █▀▀ █▄▀ ▀▀▄ █▀▀{RESET}   {DIM}{COPYRIGHT}{RESET}");
    println!("  {CYAN} ▀  ▀ ▀ ▀ ▀  ▀  ▀▀▀ ▀ ▀ ▀▀▀ ▀▀▀{RESET}");
    println!();
    if let Some(http) = http_addr {
        println!(
            "  {DIM}Studio{RESET}     {CYAN}http://{}{RESET}",
            display_host(http)
        );
    }
    println!(
        "  {DIM}Bolt{RESET}       {CYAN}bolt://{}{RESET}  {DIM}(protocol v5.1-6.0){RESET}",
        display_host(bolt_addr)
    );
    if let Some(http) = http_addr {
        let h = display_host(http);
        println!("  {DIM}HTTP API{RESET}   {CYAN}http://{h}/api{RESET}");
        println!("  {DIM}API docs{RESET}   {CYAN}http://{h}/api/docs{RESET}");
    }
    if let Some(grpc) = grpc_addr {
        println!(
            "  {DIM}gRPC{RESET}       {CYAN}{}{RESET}",
            display_host(grpc)
        );
    }
    if let Some(key) = api_key {
        println!("  {DIM}API key{RESET}    {YELLOW}{key}{RESET}");
    }
    println!();

    let multi = db_summary.len() > 1;
    for (i, (name, nodes, edges, is_default)) in db_summary.iter().enumerate() {
        let label = if i == 0 {
            format!("  {DIM}Databases{RESET}  ")
        } else {
            "             ".to_string()
        };
        let suffix = if multi && *is_default {
            " (default)"
        } else {
            ""
        };
        println!(
            "{}{} ({} nodes, {} edges){}",
            label,
            name,
            format_count(*nodes),
            format_count(*edges),
            suffix,
        );
    }

    if http_addr.is_some() {
        match ai {
            Some(ai) => println!("  {DIM}Assistant{RESET}  {} {}", ai.provider, ai.model),
            None => println!("  {DIM}Assistant{RESET}  {DIM}off (set TRAVERSE_AI_PROVIDER){RESET}"),
        }
    }
    if auth_user_count == 0 {
        println!("  {DIM}Auth{RESET}       {YELLOW}disabled (open access){RESET}");
    } else {
        println!(
            "  {DIM}Auth{RESET}       {} user{}",
            auth_user_count,
            if auth_user_count == 1 { "" } else { "s" }
        );
    }

    // -- config sources --
    println!();
    fn source_tag(s: ConfigSource) -> String {
        let dim = "\x1b[2m";
        let reset = "\x1b[0m";
        format!("{dim}({s}){reset}")
    }

    println!(
        "  {DIM}Config{RESET}     {}",
        match config_loaded {
            Some(path) => format!("loaded {path}"),
            None => "see traverse.example.toml".into(),
        }
    );
    println!(
        "  {DIM}Bolt{RESET}       {} {}",
        bolt_addr,
        source_tag(sources.listen)
    );
    if let Some(http) = http_addr {
        println!(
            "  {DIM}HTTP{RESET}       {} {}",
            http,
            source_tag(sources.http_listen)
        );
    }
    println!(
        "  {DIM}Data{RESET}       {} {}",
        data_path.unwrap_or("(in-memory, no persistence)"),
        source_tag(sources.data)
    );
    if let Some(limit) = memory_limit {
        println!(
            "  {DIM}Memory{RESET}     {} {}",
            limit,
            source_tag(sources.memory_limit.unwrap_or(ConfigSource::Default))
        );
    }
    if let Some(dir) = log_dir {
        println!("  {DIM}Log dir{RESET}    {CYAN}{dir}{RESET}");
    }
    if auth_user_count > 0 {
        println!(
            "  {DIM}Auth{RESET}       {} user{} {}",
            auth_user_count,
            if auth_user_count == 1 { "" } else { "s" },
            source_tag(sources.auth)
        );
    }

    println!();
    println!("  {DIM}Docs{RESET}       {CYAN}https://truespar.com/traverse/docs{RESET}");
    println!();
    println!("  {DIM}Press Ctrl+C to stop the server{RESET}");
    println!("  {DIM}Run with --help for all options{RESET}");
    println!();
}

/// Total physical memory in bytes; `None` if detection fails.
pub(crate) fn total_system_memory() -> Option<usize> {
    traverse_core::memory::total_memory()
}

/// Bytes as a human-readable string (e.g. "1.50 GB").
fn format_bytes(bytes: usize) -> String {
    const GB: usize = 1024 * 1024 * 1024;
    const MB: usize = 1024 * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    }
}

/// Parses a memory size ("4GB", "512MB", "1073741824").
pub(crate) fn parse_memory_limit(s: &str) -> Option<usize> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let s_upper = s.to_uppercase();
    if let Some(num) = s_upper.strip_suffix("TB") {
        num.trim()
            .parse::<usize>()
            .ok()
            .map(|n| n * 1024 * 1024 * 1024 * 1024)
    } else if let Some(num) = s_upper.strip_suffix("GB") {
        num.trim()
            .parse::<usize>()
            .ok()
            .map(|n| n * 1024 * 1024 * 1024)
    } else if let Some(num) = s_upper.strip_suffix("MB") {
        num.trim().parse::<usize>().ok().map(|n| n * 1024 * 1024)
    } else if let Some(num) = s_upper.strip_suffix("KB") {
        num.trim().parse::<usize>().ok().map(|n| n * 1024)
    } else {
        s.parse::<usize>().ok()
    }
}

/// Configured idle limit (seconds; 0 = none) -> the runtime setting; unset is the default.
pub(crate) fn tx_idle_limit(secs: Option<u64>) -> Option<Duration> {
    match secs.unwrap_or(crate::config::DEFAULT_TX_IDLE_SECS) {
        0 => None,
        s => Some(Duration::from_secs(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry over `traverse.tvdb` in `dir`, saved with `metadata` and read back as startup
    /// does.
    fn registry_with(dir: &std::path::Path, metadata: &[u8]) -> DatabaseRegistry {
        let path = dir.join("traverse.tvdb");
        if !metadata.is_empty() || !path.exists() {
            Database::save(&path, &Graph::new(), metadata).unwrap();
        }
        let stored = Database::open(&path).unwrap().into_parts().2;
        let store = Arc::new(MvccStore::new(Graph::new()));
        DatabaseRegistry::single("traverse".into(), store, Some(path)).with_metadata(stored)
    }

    fn stored_users(dir: &std::path::Path) -> Vec<(String, Role)> {
        let meta = Database::open(dir.join("traverse.tvdb"))
            .unwrap()
            .into_parts()
            .2;
        let users = AuthCatalog::deserialize(&meta).unwrap();
        users
            .list_users()
            .into_iter()
            .map(|(name, role)| (name.to_string(), role))
            .collect()
    }

    /// A user catalog that can't be read doesn't turn into open access: startup stops, unless an
    /// admin is named to start over with.
    #[test]
    fn an_unreadable_user_catalog_needs_an_admin_to_start_over() {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = registry_with(dir.path(), b"not a user catalog");
        let mut cfg = ServerConfig::default();
        let err = open_user_catalog(&cfg, &registry).expect_err("startup stops");
        assert!(err.to_string().contains("can't be read"), "{err}");

        cfg.auth = Some("root:rootpw".into());
        let users = open_user_catalog(&cfg, &registry).unwrap();
        assert_eq!(users.list_users(), vec![("root", Role::Admin)]);
        assert_eq!(stored_users(dir.path()), vec![("root".into(), Role::Admin)]);
    }

    /// A database file opened in single-file mode is saved already: nothing for the periodic
    /// checkpoint to rewrite until a commit marks it, as in directory mode. A path with no file
    /// yet is created by the first save.
    #[test]
    fn a_single_file_opens_clean() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("one.tvdb");
        let mut g = Graph::new();
        g.create_node(&["N"], Default::default());
        Database::save(&path, &g, &[]).unwrap();
        let cfg = ServerConfig {
            data: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let registry = open_registry(&cfg, None).unwrap();
        assert!(
            !registry
                .default_entry()
                .dirty
                .load(std::sync::atomic::Ordering::Acquire),
            "an opened file starts dirty"
        );

        let cfg = ServerConfig {
            data: Some(dir.path().join("new.tvdb").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let registry = open_registry(&cfg, None).unwrap();
        assert!(
            registry
                .default_entry()
                .dirty
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    /// The seed admin is saved when it's created, and only seeds an empty catalog.
    #[test]
    fn the_seed_admin_is_saved_and_only_seeds_an_empty_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = ServerConfig {
            auth: Some("admin:pw".into()),
            ..Default::default()
        };
        open_user_catalog(&cfg, &registry_with(dir.path(), &[])).unwrap();
        assert_eq!(
            stored_users(dir.path()),
            vec![("admin".into(), Role::Admin)]
        );

        let cfg = ServerConfig {
            auth: Some("other:pw".into()),
            ..Default::default()
        };
        let users = open_user_catalog(&cfg, &registry_with(dir.path(), &[])).unwrap();
        assert_eq!(users.list_users(), vec![("admin", Role::Admin)]);

        let cfg = ServerConfig {
            auth: Some("no-colon".into()),
            ..Default::default()
        };
        assert!(open_user_catalog(&cfg, &registry_with(dir.path(), &[])).is_err());
    }
}
