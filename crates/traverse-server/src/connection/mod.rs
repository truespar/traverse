//! One Bolt connection: its state machine, the message loop and the open transaction. Handlers
//! live by message kind in the submodules.

mod admin;
mod errors;
mod pull;
mod run;
mod session;
mod tx;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use traverse_core::tx::{MvccStore, OwnedOptimisticTx};
use traverse_cypher::PreparedQuery;
use traverse_cypher::executor::{QueryStats, QueryType};
use traverse_cypher::query::{Interrupt, QueryContext};
use traverse_cypher::transactions::Transaction;
use traverse_cypher::types::CypherValue;

use crate::auth::{AuthCatalog, Role};
use crate::error::{Result, ServerError};
use crate::framing;
use crate::handshake;
use crate::handshake::BoltVersion;
use crate::message::{self, ClientMessage, ServerMessage};
use crate::notification::{Notification, Severity};
use crate::packstream::{self, Value};
use crate::registry::{DatabaseEntry, DatabaseRegistry};
use crate::sync::LockUnpoisoned as _;

use errors::{
    Failure, classification_from_neo4j_code, failure_of, gql_status_from_neo4j_code,
    severity_from_neo4j_code,
};
use traverse_cypher::status::GqlStatus;

/// Global counter for unique connection ids.
static CONNECTION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `TRAVERSE_PERF_TRACE=1`: `handle_run` prints per-phase timings to stderr. Off by default.
fn perf_trace_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("TRAVERSE_PERF_TRACE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Bolt connection states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Authentication,
    Ready,
    Streaming,
    TxReady,
    TxStreaming,
    Failed,
}

/// Messages the socket reader may queue ahead of the message loop. Past this the reader waits,
/// and TCP pushes back on the client.
const MAX_QUEUED_MESSAGES: usize = 256;

/// What the socket reader reaches while the message loop runs a message.
#[derive(Default)]
struct Interrupts {
    /// RESETs read but not processed yet. Until the loop reaches them every message is IGNORED,
    /// Bolt's interrupt semantics.
    pending_resets: AtomicUsize,
    /// The statement running now.
    running: Mutex<Option<Arc<QueryContext>>>,
}

impl Interrupts {
    /// Stops the running statement, if any.
    fn cancel_running(&self) {
        if let Some(ctx) = self.running.lock_unpoisoned().as_ref() {
            ctx.cancel(Interrupt::Cancelled);
        }
    }

    fn set_running(&self, ctx: Option<Arc<QueryContext>>) {
        *self.running.lock_unpoisoned() = ctx;
    }

    /// Is a RESET waiting?
    fn reset_pending(&self) -> bool {
        self.pending_resets.load(Ordering::Acquire) > 0
    }
}

/// From the socket reader to the message loop.
enum Inbound {
    Message(ClientMessage),
    /// The client closed the connection.
    Closed,
    /// Reading or decoding failed; the connection ends.
    Failed(String),
}

/// Reads and decodes messages until the connection ends, queueing them for the message loop. A
/// RESET, GOODBYE or closed socket stops the running statement at once, while the loop is still
/// busy with it.
async fn read_messages<R: AsyncRead + Unpin>(
    mut reader: R,
    inbound: mpsc::Sender<Inbound>,
    interrupts: Arc<Interrupts>,
) {
    loop {
        let bytes = match framing::read_message(&mut reader).await {
            Ok(bytes) => bytes,
            Err(e) => {
                interrupts.cancel_running();
                let event = match e {
                    ServerError::ConnectionClosed => Inbound::Closed,
                    other => Inbound::Failed(format!("read error: {other}")),
                };
                let _ = inbound.send(event).await;
                return;
            }
        };
        // NOOP keep-alive: an empty message
        if bytes.is_empty() {
            continue;
        }
        let t_pack_start = perf_trace_enabled().then(Instant::now);
        let mut pos = 0;
        let value = match packstream::decode_value(&bytes, &mut pos) {
            Ok(v) => v,
            Err(e) => {
                interrupts.cancel_running();
                let _ = inbound
                    .send(Inbound::Failed(format!("PackStream decode error: {e}")))
                    .await;
                return;
            }
        };
        let t_pack = t_pack_start.map(|t| t.elapsed());
        let t_msg_start = perf_trace_enabled().then(Instant::now);
        let msg = match message::decode_client_message(value) {
            Ok(m) => m,
            Err(e) => {
                interrupts.cancel_running();
                let _ = inbound
                    .send(Inbound::Failed(format!("message decode error: {e}")))
                    .await;
                return;
            }
        };
        if let (Some(p), Some(m)) = (t_pack, t_msg_start.map(|t| t.elapsed())) {
            eprintln!(
                "[perf] {} decode: pack={}ns msg={}ns bytes={}",
                message_kind(&msg),
                p.as_nanos(),
                m.as_nanos(),
                bytes.len(),
            );
        }
        match msg {
            ClientMessage::Reset => {
                interrupts.pending_resets.fetch_add(1, Ordering::AcqRel);
                interrupts.cancel_running();
            }
            ClientMessage::Goodbye => interrupts.cancel_running(),
            _ => {}
        }
        if inbound.send(Inbound::Message(msg)).await.is_err() {
            // the message loop is gone
            return;
        }
    }
}

/// Message name for traces.
fn message_kind(msg: &ClientMessage) -> &'static str {
    match msg {
        ClientMessage::Run { .. } => "RUN",
        ClientMessage::Pull { .. } => "PULL",
        ClientMessage::Begin { .. } => "BEGIN",
        ClientMessage::Commit => "COMMIT",
        ClientMessage::Rollback => "ROLLBACK",
        ClientMessage::Reset => "RESET",
        ClientMessage::Hello { .. } => "HELLO",
        ClientMessage::Logon { .. } => "LOGON",
        ClientMessage::Logoff => "LOGOFF",
        ClientMessage::Goodbye => "GOODBYE",
        _ => "OTHER",
    }
}

// -- pending results -------------------------------------------------

/// Query result not fully streamed yet.
enum PendingResultKind {
    /// Fully materialized rows plus a streaming cursor.
    Materialized {
        rows: Vec<Vec<CypherValue>>,
        cursor: usize,
    },
    /// Deferred: prepared query, run on PULL.
    Deferred {
        prepared: Arc<PreparedQuery>,
        params: HashMap<String, CypherValue>,
        statement: DeferredStatement,
    },
}

/// A deferred auto-commit read: its transaction (listed from RUN until its rows are out) and what
/// its statement runs with.
struct DeferredStatement {
    transaction: Transaction,
    /// Time limit, and whether it's the client's.
    time_limit: (Option<Duration>, bool),
    text: String,
    /// The client's parameters, for SHOW TRANSACTIONS.
    params: Arc<HashMap<String, CypherValue>>,
}

struct PendingResult {
    kind: PendingResultKind,
    stats: QueryStats,
    query_type: QueryType,
    /// Serialized plan (EXPLAIN/PROFILE).
    plan: Option<CypherValue>,
    /// When RUN started (for t_first).
    run_start: Instant,
    /// Parse time (Memgraph Lab compatibility).
    parsing_time: Duration,
    /// Planning time (Memgraph Lab compatibility).
    planning_time: Duration,
    /// Execution time, measured during PULL (Memgraph Lab compatibility).
    execution_time: Duration,
}

/// Explicit transaction: an optimistic transaction on the connection's database. It reads a
/// snapshot plus its own writes, applies them at COMMIT if nothing it changed was changed
/// concurrently, and blocks no one while open. Dropping it rolls back.
struct ExplicitTx {
    tx: OwnedOptimisticTx,
    /// Its entry in the transaction registry (`SHOW` / `TERMINATE TRANSACTIONS`).
    registration: Transaction,
    /// Last client message in the transaction: the idle limit counts from here.
    last_active: Instant,
    /// When the client's `tx_timeout` ends it.
    deadline: Option<Instant>,
}

impl ExplicitTx {
    fn begin(
        store: &Arc<MvccStore>,
        read_only: bool,
        registration: Transaction,
        timeout: Option<Duration>,
    ) -> Result<Self> {
        let now = Instant::now();
        Ok(Self {
            tx: OwnedOptimisticTx::begin(store, read_only)?,
            registration,
            last_active: now,
            deadline: timeout.map(|t| now + t),
        })
    }

    /// Commits; the bookmark of the commit.
    fn commit(self) -> Result<String> {
        let ts = self.tx.commit()?;
        Ok(format!("FB:{}:{}", ts.physical, ts.logical))
    }

    /// Past the idle limit, if there is one?
    fn idle_expired(&self) -> bool {
        crate::config::tx_idle_timeout().is_some_and(|limit| self.last_active.elapsed() >= limit)
    }

    /// Past the client's `tx_timeout`?
    fn timed_out(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// Time left before the client's `tx_timeout`.
    fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// How long the message loop may wait for the next message before one of the limits ends
    /// this transaction.
    fn wait_limit(&self) -> Option<Duration> {
        let idle = crate::config::tx_idle_timeout()
            .map(|limit| limit.saturating_sub(self.last_active.elapsed()));
        match (idle, self.remaining()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Shared Notification -> Bolt packstream Value.
fn notification_to_packstream(n: &Notification, bolt_major: u8, bolt_minor: u8) -> Value {
    let mut fields = vec![
        ("code".into(), Value::String(n.code.into())),
        ("title".into(), Value::String(n.title.into())),
        ("description".into(), Value::String(n.description.clone())),
        ("severity".into(), Value::String(n.severity.as_str().into())),
        ("category".into(), Value::String(n.category.into())),
    ];
    if let Some((offset, line, col)) = n.position {
        fields.push((
            "position".into(),
            Value::Map(vec![
                ("offset".into(), Value::Integer(offset as i64)),
                ("line".into(), Value::Integer(line as i64)),
                ("column".into(), Value::Integer(col as i64)),
            ]),
        ));
    }
    // Bolt 5.6+: diagnostic_record in notifications
    if bolt_major > 5 || (bolt_major == 5 && bolt_minor >= 6) {
        fields.push((
            "diagnostic_record".into(),
            Value::Map(vec![
                ("OPERATION".into(), Value::String(String::new())),
                ("OPERATION_CODE".into(), Value::String("0".into())),
                ("CURRENT_SCHEMA".into(), Value::String("/".into())),
                ("_status_parameters".into(), Value::Map(vec![])),
                (
                    "_severity".into(),
                    Value::String(n.severity.as_str().into()),
                ),
                ("_classification".into(), Value::String(n.category.into())),
            ]),
        ));
    }
    Value::Map(fields)
}

/// Per-connection handler.
pub struct Connection {
    registry: Arc<DatabaseRegistry>,
    current_db_name: String,
    current_entry: Arc<DatabaseEntry>,
    state: State,
    version: BoltVersion,
    /// Unique connection id.
    connection_id: String,
    /// Pipelining: pending results by qid.
    pending_results: HashMap<i64, PendingResult>,
    /// Next qid to assign.
    next_qid: i64,
    /// Last assigned qid (the qid=-1 default).
    last_qid: i64,
    explicit_tx: Option<ExplicitTx>,
    server_addr: String,
    /// The client's address, `host:port`.
    client_address: String,
    /// Shared with the socket reader: pending RESETs and the running statement.
    interrupts: Arc<Interrupts>,
    /// Client metadata for the next transaction (BEGIN or auto-commit RUN `tx_metadata`).
    tx_metadata: BTreeMap<String, CypherValue>,
    /// The running statement's time limit is the client's (`tx_timeout`), not the server's:
    /// decides the error a timeout reports.
    client_time_limit: bool,
    /// Last bookmark from a commit/auto-commit.
    last_bookmark: Option<String>,
    /// Transaction timeout from BEGIN/RUN extras.
    tx_timeout: Option<Duration>,
    /// Shared auth catalog (multi-user RBAC); changes go through the registry, which saves them.
    auth_catalog: Arc<std::sync::RwLock<AuthCatalog>>,
    /// Authenticated user's role (after LOGON).
    role: Option<Role>,
    /// Authenticated username (after LOGON).
    username: Option<String>,
    /// Minimum notification severity from HELLO (None = OFF).
    notification_min_severity: Option<Severity>,
    /// Disabled notification categories from HELLO.
    notification_disabled_categories: Vec<String>,
    /// Notifications for the current query (filled during RUN, sent during PULL).
    pending_notifications: Vec<Notification>,
    /// Connection-level min severity from HELLO, restored after per-query overrides.
    hello_notification_min_severity: Option<Severity>,
    /// Connection-level disabled categories from HELLO.
    hello_notification_disabled_categories: Vec<String>,
    /// State before entering Failed (where RESET returns to).
    pre_failure_state: State,
    /// Access mode: true if BEGIN said mode:"r".
    read_only_tx: bool,
    /// The open transaction was rolled back by the server (idle limit, `tx_timeout`): the next
    /// message in it fails with this code and message, then the client RESETs or ROLLs BACK.
    terminated_tx: Option<(&'static str, String)>,
    /// Reusable encode buffer for outgoing messages.
    encode_buf: Vec<u8>,
    /// Reusable buffer for batched record framing.
    batch_buf: Vec<u8>,
    /// Auto-commit write counter for periodic GC.
    write_count: u64,
}

impl Connection {
    /// New connection over a single-database registry.
    pub fn new(store: Arc<MvccStore>, server_addr: String) -> Self {
        let registry = Arc::new(DatabaseRegistry::single("traverse".into(), store, None));
        Self::with_registry(registry, server_addr)
    }

    /// New connection backed by a DatabaseRegistry.
    pub fn with_registry(registry: Arc<DatabaseRegistry>, server_addr: String) -> Self {
        let id = CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let default_name = registry.default_name().to_string();
        let default_entry = registry.default_entry();
        Self {
            registry,
            current_db_name: default_name,
            current_entry: default_entry,
            state: State::Authentication,
            version: BoltVersion::new(0, 0),
            connection_id: format!("traverse-{id}"),
            pending_results: HashMap::new(),
            next_qid: 0,
            last_qid: -1,
            explicit_tx: None,
            server_addr,
            client_address: String::new(),
            interrupts: Arc::new(Interrupts::default()),
            tx_metadata: BTreeMap::new(),
            client_time_limit: false,
            last_bookmark: None,
            tx_timeout: None,
            auth_catalog: Arc::new(std::sync::RwLock::new(AuthCatalog::new())),
            role: None,
            username: None,
            notification_min_severity: Some(Severity::Warning),
            notification_disabled_categories: Vec::new(),
            pending_notifications: Vec::new(),
            hello_notification_min_severity: Some(Severity::Warning),
            hello_notification_disabled_categories: Vec::new(),
            pre_failure_state: State::Authentication,
            read_only_tx: false,
            terminated_tx: None,
            encode_buf: Vec::with_capacity(256),
            batch_buf: Vec::new(),
            write_count: 0,
        }
    }

    /// Sets the shared auth catalog.
    pub fn with_auth_catalog(mut self, catalog: Arc<std::sync::RwLock<AuthCatalog>>) -> Self {
        self.auth_catalog = catalog;
        self
    }

    /// Sets the client's address, as `SHOW TRANSACTIONS` reports it.
    pub fn with_client_address(mut self, address: String) -> Self {
        self.client_address = address;
        self
    }

    /// Who's running statements, for `SHOW` / `TERMINATE TRANSACTIONS`.
    fn principal(&self) -> Option<traverse_cypher::query::Principal> {
        crate::statement::principal(self.username.as_deref(), self.role)
    }

    /// Registers a transaction run by this connection, taking the pending `tx_metadata`.
    fn register_transaction(&mut self) -> Transaction {
        crate::statement::register(
            &self.current_db_name,
            self.username.as_deref(),
            "bolt",
            &self.connection_id,
            &self.client_address,
            &format!("bolt://{}", self.server_addr),
            std::mem::take(&mut self.tx_metadata),
        )
    }

    /// Neo4j code and message for a failed statement. A timeout reports the client's
    /// `tx_timeout` or the server's limit, whichever governed it.
    fn error_failure(&self, e: &ServerError) -> Failure {
        match e {
            ServerError::Interrupted {
                reason: Interrupt::TimedOut,
                message,
            } if self.client_time_limit => Failure {
                code: GqlStatus::TransactionTimedOutClient.neo4j_code(),
                status: Some(GqlStatus::TransactionTimedOutClient),
                message: format!(
                    "{message}: the transaction has not completed within the timeout the client \
                     specified at its start; retry with a longer timeout"
                ),
            },
            other => failure_of(other),
        }
    }

    /// The FAILURE for `e`.
    fn error_message(&self, e: &ServerError) -> ServerMessage {
        let f = self.error_failure(e);
        self.failure_with(f.code, f.status, &f.message)
    }

    /// Fails the next use of an explicit transaction that another session terminated or a limit
    /// ended: rolls it back and returns the FAILURE, or `None` if it's live.
    fn take_ended_transaction(&mut self) -> Option<ServerMessage> {
        if let Some(tx) = &self.explicit_tx
            && let Some(reason) = tx.registration.terminated()
        {
            self.explicit_tx = None;
            self.pending_results.clear();
            self.enter_failed();
            let message = match reason {
                Interrupt::Terminated => {
                    "The transaction has been terminated. Retry your operation in a new \
                     transaction, and you should see a successful result. Explicitly terminated \
                     by the user."
                }
                Interrupt::Cancelled => {
                    "The transaction has been terminated: the client cancelled it."
                }
                Interrupt::TimedOut => "The transaction has been terminated: it timed out.",
            };
            return Some(self.failure("Neo.TransientError.Transaction.Terminated", message));
        }
        if let Some((code, reason)) = self.terminated_tx.take() {
            self.enter_failed();
            return Some(self.failure(code, &reason));
        }
        None
    }

    /// Resolves a database name from extras; `Some(failure)` on error. Empty/missing db keeps the
    /// current one, a known name switches, unknown names and switches mid-transaction are rejected.
    fn resolve_database(&mut self, v: &Value) -> Option<Vec<ServerMessage>> {
        if let Some(db_name) = v.as_str() {
            // alias -> real database name
            let resolved = self.registry.resolve_alias(db_name);
            if !resolved.is_empty() && resolved != self.current_db_name {
                // no database switch mid-transaction
                if self.state == State::TxReady || self.state == State::TxStreaming {
                    self.enter_failed();
                    return Some(vec![self.failure(
                        "Neo.ClientError.Transaction.ForbiddenDueToTransactionType",
                        "cannot switch databases within an open transaction",
                    )]);
                }
                match self.registry.get(&resolved) {
                    Some(entry) => {
                        self.current_db_name = resolved;
                        self.current_entry = entry;
                    }
                    None => {
                        self.enter_failed();
                        return Some(vec![self.failure(
                            "Neo.ClientError.Database.DatabaseNotFound",
                            &format!("database '{db_name}' not found"),
                        )]);
                    }
                }
            }
        }
        None
    }

    /// Enters Failed, remembering the previous state for RESET.
    fn enter_failed(&mut self) {
        self.pre_failure_state = self.state;
        self.state = State::Failed;
    }

    /// Runs the connection to completion over any `AsyncRead + AsyncWrite + Unpin` stream (plain
    /// TCP or TLS).
    pub async fn run<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        mut self,
        stream: S,
    ) -> Result<()> {
        let (reader, writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        let mut writer = BufWriter::with_capacity(65536, writer);

        // 1. handshake
        self.version = handshake::perform_handshake(&mut CombinedStream {
            reader: &mut reader,
            writer: &mut writer,
        })
        .await?;

        info!(
            "Bolt {}.{} negotiated",
            self.version.major, self.version.minor
        );

        // a reader task reads ahead, so a RESET or a closed socket reaches a running statement
        let (inbound_tx, mut inbound) = mpsc::channel(MAX_QUEUED_MESSAGES);
        let reader_task = tokio::spawn(read_messages(
            reader,
            inbound_tx,
            Arc::clone(&self.interrupts),
        ));

        // close after 120 s idle
        let idle_timeout = Duration::from_secs(120);

        // 2. message loop
        loop {
            // an open transaction also has its own limits (it pins its snapshot)
            let tx_wait = self.explicit_tx.as_ref().and_then(ExplicitTx::wait_limit);
            let wait = tx_wait.map_or(idle_timeout, |t| t.min(idle_timeout));
            let client_msg = match tokio::time::timeout(wait, inbound.recv()).await {
                Ok(Some(Inbound::Message(msg))) => msg,
                Ok(Some(Inbound::Closed) | None) => {
                    debug!("client disconnected");
                    break;
                }
                Ok(Some(Inbound::Failed(e))) => {
                    error!("{e}");
                    break;
                }
                Err(_) => {
                    if let Some(tx) = &self.explicit_tx {
                        if tx.timed_out() {
                            info!("transaction passed its tx_timeout, rolled back");
                            self.explicit_tx = None;
                            self.terminated_tx = Some((
                                "Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration",
                                "the transaction has not completed within the timeout the client \
                                 specified at its start; it was rolled back"
                                    .to_string(),
                            ));
                            continue;
                        }
                        if tx.idle_expired() {
                            let limit = crate::config::tx_idle_timeout().unwrap_or_default();
                            info!("transaction idle for {}s, rolled back", limit.as_secs());
                            self.explicit_tx = None;
                            self.terminated_tx = Some((
                                "Neo.TransientError.Transaction.Terminated",
                                format!(
                                    "the transaction was rolled back after {}s idle \
                                     (TRAVERSE_TX_IDLE_TIMEOUT)",
                                    limit.as_secs()
                                ),
                            ));
                            continue;
                        }
                    }
                    // a transaction limit woke the loop before the connection's own: look again
                    if tx_wait.is_some_and(|t| t < idle_timeout) {
                        continue;
                    }
                    info!(
                        "idle timeout ({}s), closing connection",
                        idle_timeout.as_secs()
                    );
                    break;
                }
            };

            debug!("received: {client_msg:?}");

            // Goodbye: handle right away
            if matches!(client_msg, ClientMessage::Goodbye) {
                debug!("client sent Goodbye");
                break;
            }

            // a RESET is on its way: everything before it is IGNORED (the reader already stopped
            // whatever was running)
            if matches!(client_msg, ClientMessage::Reset) {
                self.interrupts
                    .pending_resets
                    .fetch_sub(1, Ordering::AcqRel);
            } else if self.interrupts.reset_pending() {
                self.write_msg(&mut writer, ServerMessage::Ignored).await?;
                if inbound.is_empty() {
                    writer.flush().await?;
                }
                continue;
            }

            // dispatch; PULL streams records straight to the wire (no bulk
            // Vec<ServerMessage>).
            //
            // Coalescing for pipelined RUN+PULL:
            // * after RUN: if the next message (likely PULL) is already buffered, skip the
            //   flush so the PULL response shares the same kernel send
            // * after PULL: ALWAYS flush; PULL carries the result the client waits on, and
            //   deferring it until the BufWriter fills causes head-of-line blocking under
            //   concurrency. Only the RUN/PULL pair is fused.
            let send_start = Instant::now();
            let is_pull = matches!(client_msg, ClientMessage::Pull { .. });
            if let ClientMessage::Pull { extra } = client_msg {
                self.handle_pull_streaming(extra, &mut writer).await?;
            } else {
                // messages are synchronous (planning, executing, committing): off the async worker
                let responses = crate::statement::blocking(|| self.handle_message(client_msg));
                for msg in responses {
                    let value = message::encode_server_message(msg);
                    self.encode_buf.clear();
                    packstream::encode_value(&mut self.encode_buf, &value);
                    framing::write_message(&mut writer, &self.encode_buf).await?;
                }
            }
            if is_pull || inbound.is_empty() {
                writer.flush().await?;
            }
            debug!(send_us = send_start.elapsed().as_micros() as u64, "send");
        }

        // clean up: dropping an open transaction rolls it back
        self.explicit_tx = None;
        reader_task.abort();

        Ok(())
    }

    fn handle_message(&mut self, msg: ClientMessage) -> Vec<ServerMessage> {
        // in Failed only Reset is allowed; everything else is Ignored
        if self.state == State::Failed && !matches!(msg, ClientMessage::Reset) {
            return vec![ServerMessage::Ignored];
        }

        // a transaction the server rolled back, or another session terminated: report it once,
        // at the next message in it
        if matches!(
            msg,
            ClientMessage::Run { .. } | ClientMessage::Commit | ClientMessage::Discard { .. }
        ) && let Some(failure) = self.take_ended_transaction()
        {
            return vec![failure];
        }
        if let Some(tx) = self.explicit_tx.as_mut() {
            tx.last_active = Instant::now();
        }

        match msg {
            ClientMessage::Hello { extra } => self.handle_hello(extra),
            ClientMessage::Logon { auth } => self.handle_logon(auth),
            ClientMessage::Logoff => self.handle_logoff(),
            ClientMessage::Run {
                query,
                parameters,
                extra,
            } => self.handle_run(query, parameters, extra),
            ClientMessage::Pull { .. } => unreachable!("PULL handled in main loop"),
            ClientMessage::Discard { extra } => self.handle_discard(extra),
            ClientMessage::Begin { extra } => self.handle_begin(extra),
            ClientMessage::Commit => self.handle_commit(),
            ClientMessage::Rollback => self.handle_rollback(),
            ClientMessage::Reset => self.handle_reset(),
            ClientMessage::Route {
                routing,
                bookmarks,
                db,
            } => self.handle_route(routing, bookmarks, db),
            ClientMessage::Telemetry { api } => {
                debug!("telemetry api: {api}");
                vec![ServerMessage::Success { metadata: vec![] }]
            }
            ClientMessage::Goodbye => unreachable!("handled above"),
        }
    }

    /// Encodes and writes one server message.
    async fn write_msg<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut BufWriter<W>,
        msg: ServerMessage,
    ) -> Result<()> {
        let value = message::encode_server_message(msg);
        self.encode_buf.clear();
        packstream::encode_value(&mut self.encode_buf, &value);
        framing::write_message(writer, &self.encode_buf).await?;
        Ok(())
    }

    fn failure(&self, code: &str, message: &str) -> ServerMessage {
        self.failure_with(code, None, message)
    }

    /// A FAILURE with Neo4j `code`; its GQLSTATUS is `status`, or derived from `code`.
    fn failure_with(&self, code: &str, status: Option<GqlStatus>, message: &str) -> ServerMessage {
        let mut metadata = vec![
            ("code".into(), Value::String(code.into())),
            ("message".into(), Value::String(message.into())),
        ];
        // Bolt 5.7 renamed the FAILURE fields: drivers read `neo4j_code` + `gql_status` +
        // `description` and ignore the legacy `code`, so without these every error (even plain
        // syntax errors) shows client-side as "Neo.DatabaseError.General.UnknownError / An unknown
        // error occurred". The legacy keys above stay for pre-5.7 connections.
        if self.version.major > 5 || (self.version.major == 5 && self.version.minor >= 7) {
            let (gql_status, gql_description) = match status {
                Some(status) => (status.code(), status.description()),
                None => gql_status_from_neo4j_code(code),
            };
            metadata.push(("neo4j_code".into(), Value::String(code.into())));
            metadata.push(("gql_status".into(), Value::String(gql_status.into())));
            metadata.push((
                "description".into(),
                Value::String(format!("{gql_description}. {message}")),
            ));
        }
        // Bolt 5.6+: diagnostic_record in FAILURE
        if self.version.major > 5 || (self.version.major == 5 && self.version.minor >= 6) {
            metadata.push((
                "diagnostic_record".into(),
                Value::Map(vec![
                    ("OPERATION".into(), Value::String(String::new())),
                    ("OPERATION_CODE".into(), Value::String("0".into())),
                    ("CURRENT_SCHEMA".into(), Value::String("/".into())),
                    ("_status_parameters".into(), Value::Map(vec![])),
                    (
                        "_severity".into(),
                        Value::String(severity_from_neo4j_code(code).into()),
                    ),
                    (
                        "_classification".into(),
                        Value::String(classification_from_neo4j_code(code).into()),
                    ),
                ]),
            ));
        }
        ServerMessage::Failure { metadata }
    }
}

/// Split reader + writer combined, for the handshake.
struct CombinedStream<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for CombinedStream<'_, R, W> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.reader).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for CombinedStream<'_, R, W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut *self.writer).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.writer).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.writer).poll_shutdown(cx)
    }
}
