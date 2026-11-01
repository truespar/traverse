//! A statement the server runs: its query context (limits, stop flag) and its place in the
//! transaction registry, so `SHOW TRANSACTIONS` lists it and `TERMINATE TRANSACTIONS`, a client
//! reset, a closed connection or a cancelled request can stop it.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use traverse_core::tx::{MvccStore, WriteTx};
use traverse_cypher::error::CypherError;
use traverse_cypher::query::{ChildOptions, Principal, QueryContext, QueryOptions};
use traverse_cypher::transactions::{
    self, Parameters, RunningStatement, Transaction, TransactionInfo,
};
use traverse_cypher::types::CypherValue;

use crate::auth::Role;

/// Who's calling, from the session's login; `None` when authentication is off (then everyone is
/// trusted, as in an unsecured Neo4j).
pub(crate) fn principal(user: Option<&str>, role: Option<Role>) -> Option<Principal> {
    role.map(|role| Principal {
        user: user.unwrap_or_default().to_string(),
        admin: role == Role::Admin,
    })
}

/// The limit a statement runs under: the client's (`tx_timeout`, a gRPC deadline) or the server's
/// `TRAVERSE_QUERY_TIMEOUT`, whichever ends first. `true` when the client's governs, which changes
/// the error a timeout reports.
pub(crate) fn time_limit(client: Option<Duration>) -> (Option<Duration>, bool) {
    match (client, crate::config::query_timeout()) {
        (Some(c), Some(s)) if s < c => (Some(s), false),
        (Some(c), _) => (Some(c), true),
        (None, s) => (s, false),
    }
}

/// Registers a transaction with the server's details.
pub(crate) fn register(
    database: &str,
    user: Option<&str>,
    protocol: &'static str,
    connection_id: &str,
    client_address: &str,
    request_uri: &str,
    metadata: BTreeMap<String, CypherValue>,
) -> Transaction {
    transactions::register(TransactionInfo {
        database: database.to_string(),
        user: user.unwrap_or_default().to_string(),
        protocol,
        connection_id: connection_id.to_string(),
        client_address: client_address.to_string(),
        request_uri: request_uri.to_string(),
        metadata,
    })
}

/// A statement running in a transaction: its context, and its entry as the transaction's current
/// statement until dropped.
pub(crate) struct Running {
    /// The statement's context; enter it on the thread that runs the statement.
    pub(crate) ctx: Arc<QueryContext>,
    _statement: RunningStatement,
}

/// Starts a statement in `tx` with `timeout` (see [`time_limit`]).
pub(crate) fn start(
    tx: &Transaction,
    timeout: Option<Duration>,
    principal: Option<Principal>,
    text: &str,
    parameters: Option<Parameters>,
) -> Running {
    let ctx = QueryContext::new(tx.query_options(QueryOptions {
        timeout,
        memory_limit: None,
        principal,
        transaction: None,
    }));
    let statement = tx.run(&ctx, text, parameters);
    Running {
        ctx,
        _statement: statement,
    }
}

/// Runs synchronous work (planning, executing, committing a statement) from an async task without
/// starving the runtime. On the multi-threaded runtime the worker first hands its queued tasks to
/// another thread (`block_in_place`), so a long statement can't hold up other connections, the
/// socket reader that would cancel it, or timers; on a single-threaded runtime (some tests) it runs
/// inline.
pub(crate) fn blocking<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Takes the writer lock for an auto-commit write: waits while another writer holds it, but gives
/// up as soon as the statement is stopped (its deadline, a cancel, a terminate). The transaction
/// is the statement's own, so `CALL { ... } IN TRANSACTIONS` may commit batches in it.
pub(crate) fn begin_write<'a>(
    store: &'a MvccStore,
    ctx: &QueryContext,
) -> Result<WriteTx<'a>, CypherError> {
    const POLL: Duration = Duration::from_millis(20);
    loop {
        match store.try_begin_write(POLL) {
            Ok(mut tx) => {
                tx.allow_batch_commits();
                return Ok(tx);
            }
            Err(traverse_core::error::Error::Busy(_)) => ctx.check_interrupt()?,
            Err(e) => return Err(e.into()),
        }
    }
}

// -- HTTP requests ---------------------------------------------------

static CONNECTIONS: AtomicU64 = AtomicU64::new(0);

/// A connection id for one request or session of `protocol` (`http-12`, `grpc-7`).
pub(crate) fn next_connection_id(protocol: &str) -> String {
    format!("{protocol}-{}", CONNECTIONS.fetch_add(1, Ordering::Relaxed))
}

/// The client's address on an HTTP request, `host:port` (empty when the server can't tell).
#[derive(Debug, Clone, Default)]
pub(crate) struct ClientAddress(pub(crate) String);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for ClientAddress {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<SocketAddr>>()
                .map(|info| info.0.to_string())
                .unwrap_or_default(),
        ))
    }
}

/// One HTTP request's statements, run on a blocking thread. Each statement is an auto-commit
/// transaction of its own and a child of the request's context, so the client going away (the
/// handler's future is dropped) stops whichever one is running.
pub(crate) struct HttpRequest {
    ctx: Arc<QueryContext>,
    connection_id: String,
    client_address: String,
    user: Option<String>,
    protocol: &'static str,
}

/// A statement of an [`HttpRequest`]: registered while it runs.
pub(crate) struct HttpStatement {
    /// The statement's context, entered for the scope of [`HttpRequest::statement`]'s caller.
    pub(crate) ctx: Arc<QueryContext>,
    _statement: RunningStatement,
    _transaction: Transaction,
}

impl HttpRequest {
    /// Starts a statement against `database`: listed as a transaction until dropped, limited by
    /// the server's statement timeout, stopped with the request.
    pub(crate) fn statement(
        &self,
        database: &str,
        text: &str,
        parameters: Option<Parameters>,
    ) -> HttpStatement {
        let transaction = register(
            database,
            self.user.as_deref(),
            self.protocol,
            &self.connection_id,
            &self.client_address,
            "",
            BTreeMap::new(),
        );
        let ctx = self.ctx.child(ChildOptions {
            timeout: time_limit(None).0,
            transaction: Some(transaction.id()),
            ..ChildOptions::default()
        });
        let statement = transaction.run(&ctx, text, parameters);
        HttpStatement {
            ctx,
            _statement: statement,
            _transaction: transaction,
        }
    }

    /// The request's context: stopped when the client goes away.
    pub(crate) fn ctx(&self) -> &Arc<QueryContext> {
        &self.ctx
    }
}

/// Runs `work` on a blocking thread as one request from `client_address`; cancels it if the
/// returned future is dropped before it finishes. `Err` if `work` panicked.
pub(crate) async fn run_request<T: Send + 'static>(
    protocol: &'static str,
    client_address: String,
    user: Option<String>,
    work: impl FnOnce(&HttpRequest) -> T + Send + 'static,
) -> Result<T, String> {
    let request = HttpRequest {
        ctx: QueryContext::new(QueryOptions::default()),
        connection_id: next_connection_id(protocol),
        client_address,
        user,
        protocol,
    };
    let guard = CancelOnDrop::new(&request.ctx);
    let joined = tokio::task::spawn_blocking(move || work(&request)).await;
    guard.disarm();
    joined.map_err(|e| format!("the request failed: {e}"))
}

/// Cancels a statement when dropped, unless [`disarm`](Self::disarm)ed first. An async handler
/// holds one while a blocking task runs the statement: if the client goes away the handler's
/// future is dropped, and the statement stops at its next check.
pub(crate) struct CancelOnDrop(Option<Arc<QueryContext>>);

impl CancelOnDrop {
    pub(crate) fn new(ctx: &Arc<QueryContext>) -> Self {
        Self(Some(Arc::clone(ctx)))
    }

    /// The statement finished; dropping no longer cancels it.
    pub(crate) fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(ctx) = self.0.take() {
            ctx.cancel(traverse_cypher::query::Interrupt::Cancelled);
        }
    }
}
