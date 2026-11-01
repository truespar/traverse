#[cfg(feature = "mcp")]
pub mod mcp;
pub mod routes;
pub mod tasks;

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use axum::Router;
use tokio::net::TcpListener;

use crate::auth::AuthCatalog;
use crate::ip_filter::IpFilter;
use crate::registry::DatabaseRegistry;

use self::routes::AppState;
use self::tasks::TaskManager;

/// Traverse HTTP/WebSocket server.
pub struct HttpServer {
    registry: Arc<DatabaseRegistry>,
    auth_catalog: Arc<RwLock<AuthCatalog>>,
    addr: SocketAddr,
    ip_filter: Arc<IpFilter>,
    allow_drop_database: bool,
    api_key: Option<String>,
    ai: Option<Arc<crate::ai::AiConfig>>,
    task_manager: Arc<TaskManager>,
    #[cfg(feature = "tls")]
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
}

impl HttpServer {
    /// New HTTP server.
    pub fn new(
        registry: Arc<DatabaseRegistry>,
        auth_catalog: Arc<RwLock<AuthCatalog>>,
        addr: SocketAddr,
    ) -> Self {
        Self {
            registry,
            auth_catalog,
            addr,
            ip_filter: Arc::new(IpFilter::allow_all()),
            allow_drop_database: false,
            api_key: None,
            ai: None,
            task_manager: Arc::new(TaskManager::new()),
            #[cfg(feature = "tls")]
            tls_acceptor: None,
        }
    }

    /// IP allowlist.
    pub fn with_ip_filter(mut self, filter: Arc<IpFilter>) -> Self {
        self.ip_filter = filter;
        self
    }

    /// Allows DROP DATABASE via the HTTP API.
    pub fn with_allow_drop_database(mut self, allow: bool) -> Self {
        self.allow_drop_database = allow;
        self
    }

    /// Turns on Studio's Graph Assistant (`/api/ai/chat`) with this model.
    pub fn with_ai(mut self, ai: Option<crate::ai::AiConfig>) -> Self {
        self.ai = ai.map(Arc::new);
        self
    }

    /// API key for HTTP auth.
    pub fn with_api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key;
        self
    }

    /// Enables TLS.
    #[cfg(feature = "tls")]
    pub fn with_tls(mut self, acceptor: tokio_rustls::TlsAcceptor) -> Self {
        self.tls_acceptor = Some(acceptor);
        self
    }

    fn build_router(&self) -> Router {
        let state = AppState {
            registry: Arc::clone(&self.registry),
            auth_catalog: Arc::clone(&self.auth_catalog),
            start_time: std::time::Instant::now(),
            allow_drop_database: self.allow_drop_database,
            api_key: self.api_key.clone(),
            ai: self.ai.clone(),
            task_manager: Arc::clone(&self.task_manager),
            db_lifecycle_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        };
        routes::build_router(state)
    }

    /// Binds the TCP listener without starting the serve loop; returns the listener and its local
    /// address.
    pub async fn bind(&self) -> std::io::Result<(TcpListener, SocketAddr)> {
        let listener = TcpListener::bind(self.addr).await?;
        let local_addr = listener.local_addr()?;
        Ok((listener, local_addr))
    }

    /// Runs on a pre-bound listener (see `bind()`).
    pub async fn run_with_listener(
        self,
        listener: TcpListener,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let router = self.build_router();
        let ip_filter = self.ip_filter;

        #[cfg(feature = "tls")]
        if let Some(acceptor) = self.tls_acceptor {
            loop {
                let (stream, peer_addr) = listener.accept().await?;
                if !ip_filter.is_allowed(&peer_addr.ip()) {
                    tracing::warn!(
                        "HTTP: rejected connection from {peer_addr} (IP not in allowlist)"
                    );
                    drop(stream);
                    continue;
                }
                stream.set_nodelay(true).ok();
                let acceptor = acceptor.clone();
                // handlers read the client's address from the request (SHOW TRANSACTIONS)
                let router = router
                    .clone()
                    .layer(axum::Extension(axum::extract::ConnectInfo(peer_addr)));
                tokio::spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            let io = hyper_util::rt::TokioIo::new(tls_stream);
                            let service = hyper_util::service::TowerToHyperService::new(
                                router.into_service(),
                            );
                            let _ = hyper_util::server::conn::auto::Builder::new(
                                hyper_util::rt::TokioExecutor::new(),
                            )
                            .serve_connection(io, service)
                            .await;
                        }
                        Err(e) => {
                            tracing::error!("HTTP TLS handshake failed: {e}");
                        }
                    }
                });
            }
        }

        if ip_filter.is_empty() {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await?;
        } else {
            loop {
                let (stream, peer_addr) = listener.accept().await?;
                if !ip_filter.is_allowed(&peer_addr.ip()) {
                    tracing::warn!(
                        "HTTP: rejected connection from {peer_addr} (IP not in allowlist)"
                    );
                    drop(stream);
                    continue;
                }
                stream.set_nodelay(true).ok();
                let router = router
                    .clone()
                    .layer(axum::Extension(axum::extract::ConnectInfo(peer_addr)));
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let service =
                        hyper_util::service::TowerToHyperService::new(router.into_service());
                    let _ = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(io, service)
                    .await;
                });
            }
        }
        Ok(())
    }
}
