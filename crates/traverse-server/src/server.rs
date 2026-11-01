use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing::{error, info};

use traverse_core::graph::Graph;
use traverse_core::tx::MvccStore;

use crate::auth::AuthCatalog;
use crate::connection::Connection;
use crate::ip_filter::IpFilter;
use crate::registry::DatabaseRegistry;

/// Optional TLS configuration.
#[cfg(feature = "tls")]
pub struct TlsConfig {
    pub acceptor: tokio_rustls::TlsAcceptor,
}

/// Max concurrent connections.
const MAX_CONNECTIONS: usize = 256;

/// Traverse Bolt protocol server.
pub struct Server {
    registry: Arc<DatabaseRegistry>,
    listen_addr: String,
    auth_catalog: Arc<RwLock<AuthCatalog>>,
    max_connections: Arc<Semaphore>,
    ip_filter: Arc<IpFilter>,
    #[cfg(feature = "tls")]
    tls: Option<TlsConfig>,
}

impl Server {
    /// New server over an empty in-memory graph.
    pub fn new(listen_addr: String) -> Self {
        let store = Arc::new(MvccStore::new(Graph::new()));
        let registry = Arc::new(DatabaseRegistry::single("traverse".into(), store, None));
        Self {
            registry,
            listen_addr,
            auth_catalog: Arc::new(RwLock::new(AuthCatalog::new())),
            max_connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            ip_filter: Arc::new(IpFilter::allow_all()),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    /// New server over an existing MvccStore.
    pub fn with_store(store: Arc<MvccStore>, listen_addr: String) -> Self {
        let registry = Arc::new(DatabaseRegistry::single("traverse".into(), store, None));
        Self {
            registry,
            listen_addr,
            auth_catalog: Arc::new(RwLock::new(AuthCatalog::new())),
            max_connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            ip_filter: Arc::new(IpFilter::allow_all()),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    /// New server over a DatabaseRegistry.
    pub fn with_registry(registry: Arc<DatabaseRegistry>, listen_addr: String) -> Self {
        Self {
            registry,
            listen_addr,
            auth_catalog: Arc::new(RwLock::new(AuthCatalog::new())),
            max_connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            ip_filter: Arc::new(IpFilter::allow_all()),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    /// MVCC store of the default database.
    pub fn store(&self) -> Arc<MvccStore> {
        self.registry.default_entry().store.clone()
    }

    /// Database registry.
    pub fn registry(&self) -> &Arc<DatabaseRegistry> {
        &self.registry
    }

    /// Auth catalog.
    pub fn auth_catalog(&self) -> &Arc<RwLock<AuthCatalog>> {
        &self.auth_catalog
    }

    /// Enables authentication with an AuthCatalog.
    pub fn with_auth_catalog(mut self, catalog: AuthCatalog) -> Self {
        self.auth_catalog = Arc::new(RwLock::new(catalog));
        self
    }

    /// IP allowlist.
    pub fn with_ip_filter(mut self, filter: IpFilter) -> Self {
        self.ip_filter = Arc::new(filter);
        self
    }

    /// Enables TLS.
    #[cfg(feature = "tls")]
    pub fn with_tls(mut self, tls_config: TlsConfig) -> Self {
        self.tls = Some(tls_config);
        self
    }

    /// Binds the TCP listener without starting the accept loop; returns the listener and its local
    /// address.
    pub async fn bind(&self) -> crate::error::Result<(TcpListener, SocketAddr)> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Ok((listener, local_addr))
    }

    /// Runs the accept loop on a pre-bound listener (see `bind()`).
    pub async fn run_with_listener(&self, listener: TcpListener) -> crate::error::Result<()> {
        let local_addr = listener.local_addr()?;
        let server_addr = local_addr.to_string();

        // apoc.periodic.* background jobs run against the default database through this hook (one
        // write transaction per run)
        {
            let registry = Arc::clone(&self.registry);
            traverse_cypher::apoc_scheduler::install_executor(Box::new(move |stmt| {
                let entry = registry.default_entry();
                let mut tx = entry.store.begin_write();
                match traverse_cypher::execute_with_params(
                    &mut tx,
                    stmt,
                    &std::collections::HashMap::new(),
                ) {
                    Ok(_) => match tx.commit() {
                        Ok(_) => {
                            entry.mark_dirty();
                            Ok(())
                        }
                        Err(e) => Err(format!("commit failed: {e}")),
                    },
                    Err(e) => {
                        tx.abort();
                        Err(e.to_string())
                    }
                }
            }));
        }

        loop {
            let (stream, peer_addr) = listener.accept().await?;

            if !self.ip_filter.is_allowed(&peer_addr.ip()) {
                tracing::warn!("rejected connection from {peer_addr} (IP not in allowlist)");
                drop(stream);
                continue;
            }

            stream.set_nodelay(true).ok();
            info!("accepted connection from {peer_addr}");

            let permit = match self.max_connections.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    error!("max connections reached ({MAX_CONNECTIONS}), rejecting {peer_addr}");
                    drop(stream);
                    continue;
                }
            };

            let registry = Arc::clone(&self.registry);
            let addr = server_addr.clone();
            let auth_catalog = Arc::clone(&self.auth_catalog);

            #[cfg(feature = "tls")]
            if let Some(ref tls) = self.tls {
                let acceptor = tls.acceptor.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            let conn = Connection::with_registry(registry, addr)
                                .with_auth_catalog(auth_catalog)
                                .with_client_address(peer_addr.to_string());
                            if let Err(e) = conn.run(tls_stream).await {
                                error!("connection error from {peer_addr}: {e}");
                            }
                        }
                        Err(e) => {
                            error!("TLS handshake failed from {peer_addr}: {e}");
                        }
                    }
                    info!("connection closed: {peer_addr}");
                });
                continue;
            }

            tokio::spawn(async move {
                let _permit = permit;
                let conn = Connection::with_registry(registry, addr)
                    .with_auth_catalog(auth_catalog)
                    .with_client_address(peer_addr.to_string());
                if let Err(e) = conn.run(stream).await {
                    error!("connection error from {peer_addr}: {e}");
                }
                info!("connection closed: {peer_addr}");
            });
        }
    }

    /// Runs the server, accepting connections until shutdown.
    pub async fn run(&self) -> crate::error::Result<()> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        let local_addr = listener.local_addr()?;
        info!("Traverse listening on {local_addr}");

        let server_addr = local_addr.to_string();

        loop {
            let (stream, peer_addr) = listener.accept().await?;

            if !self.ip_filter.is_allowed(&peer_addr.ip()) {
                tracing::warn!("rejected connection from {peer_addr} (IP not in allowlist)");
                drop(stream);
                continue;
            }

            stream.set_nodelay(true).ok();
            info!("accepted connection from {peer_addr}");

            let permit = match self.max_connections.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    error!("max connections reached ({MAX_CONNECTIONS}), rejecting {peer_addr}");
                    drop(stream);
                    continue;
                }
            };

            let registry = Arc::clone(&self.registry);
            let addr = server_addr.clone();
            let auth_catalog = Arc::clone(&self.auth_catalog);

            #[cfg(feature = "tls")]
            if let Some(ref tls) = self.tls {
                let acceptor = tls.acceptor.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            let conn = Connection::with_registry(registry, addr)
                                .with_auth_catalog(auth_catalog)
                                .with_client_address(peer_addr.to_string());
                            if let Err(e) = conn.run(tls_stream).await {
                                error!("connection error from {peer_addr}: {e}");
                            }
                        }
                        Err(e) => {
                            error!("TLS handshake failed from {peer_addr}: {e}");
                        }
                    }
                    info!("connection closed: {peer_addr}");
                });
                continue;
            }

            tokio::spawn(async move {
                let _permit = permit;
                let conn = Connection::with_registry(registry, addr)
                    .with_auth_catalog(auth_catalog)
                    .with_client_address(peer_addr.to_string());
                if let Err(e) = conn.run(stream).await {
                    error!("connection error from {peer_addr}: {e}");
                }
                info!("connection closed: {peer_addr}");
            });
        }
    }

    /// Runs the server, returning the bound address (tests).
    pub async fn run_until_shutdown(
        self,
        shutdown: tokio::sync::oneshot::Receiver<()>,
    ) -> crate::error::Result<std::net::SocketAddr> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        let local_addr = listener.local_addr()?;
        info!("Traverse listening on {local_addr}");

        let server_addr = local_addr.to_string();
        let registry = self.registry;
        let auth_catalog = self.auth_catalog;
        let max_connections = self.max_connections;
        let ip_filter = self.ip_filter;

        tokio::spawn(async move {
            tokio::select! {
                _ = async {
                    loop {
                        match listener.accept().await {
                            Ok((stream, peer_addr)) => {
                                if !ip_filter.is_allowed(&peer_addr.ip()) {
                                    tracing::warn!("rejected connection from {peer_addr} (IP not in allowlist)");
                                    drop(stream);
                                    continue;
                                }
                                stream.set_nodelay(true).ok();
                                info!("accepted connection from {peer_addr}");
                                let permit = match max_connections.clone().try_acquire_owned() {
                                    Ok(p) => p,
                                    Err(_) => {
                                        error!("max connections reached, rejecting {peer_addr}");
                                        drop(stream);
                                        continue;
                                    }
                                };
                                let registry = Arc::clone(&registry);
                                let addr = server_addr.clone();
                                let auth_catalog = Arc::clone(&auth_catalog);
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let conn = Connection::with_registry(registry, addr)
                                        .with_auth_catalog(auth_catalog)
                .with_client_address(peer_addr.to_string());
                                    if let Err(e) = conn.run(stream).await {
                                        error!("connection error from {peer_addr}: {e}");
                                    }
                                });
                            }
                            Err(e) => {
                                error!("accept error: {e}");
                            }
                        }
                    }
                } => {}
                _ = shutdown => {
                    info!("shutting down");
                }
            }
        });

        Ok(local_addr)
    }

    /// Runs the server with auth, returning the bound address (tests).
    pub async fn run_until_shutdown_with_auth(
        mut self,
        catalog: AuthCatalog,
        shutdown: tokio::sync::oneshot::Receiver<()>,
    ) -> crate::error::Result<std::net::SocketAddr> {
        self.auth_catalog = Arc::new(RwLock::new(catalog));
        self.run_until_shutdown(shutdown).await
    }
}
