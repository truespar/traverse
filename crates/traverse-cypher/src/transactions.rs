//! Live transactions, for `SHOW TRANSACTIONS` and `TERMINATE TRANSACTIONS`.
//!
//! Hosts register each transaction while it's open: an explicit one for its life, an auto-commit
//! one for its statement. The registry is process-wide, like Neo4j's per-DBMS listing, so one
//! session sees, and with the right to stops, another's work. A host marks the statement a
//! transaction is running with [`Transaction::run`]. Terminating a transaction cancels that
//! statement and flags the transaction; the host fails its next use with
//! `Neo.TransientError.Transaction.Terminated` and rolls it back.
//!
//! Costs: a sharded mutex insert at register, a remove at drop, and one uncontended lock per
//! statement to publish it. Listing walks the shards.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
// `web_time` is `std::time` natively and a performance.now() / Date.now() shim on wasm32, where the
// std clocks panic
use web_time::{Instant, SystemTime, UNIX_EPOCH};

use crate::query::{Interrupt, Principal, QueryContext, QueryOptions};
use crate::types::CypherValue;

/// What a host knows about a transaction it registers.
#[derive(Debug, Clone, Default)]
pub struct TransactionInfo {
    /// Database it runs against.
    pub database: String,
    /// User running it; empty when anonymous or embedded.
    pub user: String,
    /// `bolt`, `http`, `grpc`, `embedded`, ...
    pub protocol: &'static str,
    /// The host's id for the connection or session.
    pub connection_id: String,
    /// Client address, `host:port`.
    pub client_address: String,
    /// Server endpoint the client reached.
    pub request_uri: String,
    /// Client-supplied metadata (Bolt `tx_metadata`).
    pub metadata: BTreeMap<String, CypherValue>,
}

/// Parameters of a running statement, shared rather than copied.
pub type Parameters = Arc<HashMap<String, CypherValue>>;

struct Running {
    query: Arc<QueryContext>,
    text: Arc<str>,
    parameters: Option<Parameters>,
    started_at: SystemTime,
}

struct State {
    current: Option<Running>,
    /// When the last statement ended, or the transaction began.
    idle_since: Instant,
}

struct Entry {
    id: u64,
    display_id: String,
    info: TransactionInfo,
    started: Instant,
    started_at: SystemTime,
    state: Mutex<State>,
    /// 0 = live, else an [`Interrupt`] code.
    terminated: AtomicU8,
}

impl Entry {
    fn state(&self) -> MutexGuard<'_, State> {
        // a few field writes per update: a holder that panicked left it consistent
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn terminate(&self, reason: Interrupt) {
        let code = match reason {
            Interrupt::TimedOut => 1,
            Interrupt::Cancelled => 2,
            Interrupt::Terminated => 3,
        };
        let _ = self
            .terminated
            .compare_exchange(0, code, Ordering::Relaxed, Ordering::Relaxed);
        if let Some(running) = &self.state().current {
            running.query.cancel(reason);
        }
    }

    fn terminated(&self) -> Option<Interrupt> {
        match self.terminated.load(Ordering::Relaxed) {
            1 => Some(Interrupt::TimedOut),
            2 => Some(Interrupt::Cancelled),
            3 => Some(Interrupt::Terminated),
            _ => None,
        }
    }

    fn visible_to(&self, caller: Option<&Principal>) -> bool {
        caller.is_none_or(|p| p.admin || p.user == self.info.user)
    }
}

const SHARDS: usize = 16;

static REGISTRY: LazyLock<[Mutex<HashMap<u64, Arc<Entry>>>; SHARDS]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(HashMap::new())));

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn shard(id: u64) -> MutexGuard<'static, HashMap<u64, Arc<Entry>>> {
    REGISTRY[(id as usize) % SHARDS]
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Registers a transaction; it's listed until the returned value drops.
pub fn register(info: TransactionInfo) -> Transaction {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    let entry = Arc::new(Entry {
        id,
        display_id: display_id(&info.database, id),
        info,
        started,
        started_at: SystemTime::now(),
        state: Mutex::new(State {
            current: None,
            idle_since: started,
        }),
        terminated: AtomicU8::new(0),
    });
    shard(id).insert(id, Arc::clone(&entry));
    Transaction { entry }
}

/// Neo4j's form: `<database>-transaction-<n>`.
fn display_id(database: &str, id: u64) -> String {
    let db = if database.is_empty() {
        "traverse"
    } else {
        database
    };
    format!("{db}-transaction-{id}")
}

/// A registered transaction; unregisters on drop. Owned by the host running it.
pub struct Transaction {
    entry: Arc<Entry>,
}

impl Transaction {
    /// Registry id.
    pub fn id(&self) -> u64 {
        self.entry.id
    }

    /// The id `SHOW TRANSACTIONS` shows and `TERMINATE TRANSACTIONS` takes.
    pub fn display_id(&self) -> &str {
        &self.entry.display_id
    }

    /// Database it runs against.
    pub fn database(&self) -> &str {
        &self.entry.info.database
    }

    /// Settings for a statement in this transaction: `options` with the transaction filled in.
    pub fn query_options(&self, options: QueryOptions) -> QueryOptions {
        QueryOptions {
            transaction: Some(self.entry.id),
            ..options
        }
    }

    /// Publishes `query` as the statement this transaction is running until the returned guard
    /// drops. A transaction terminated before this cancels the query at once.
    pub fn run(
        &self,
        query: &Arc<QueryContext>,
        text: &str,
        parameters: Option<Parameters>,
    ) -> RunningStatement {
        self.entry.state().current = Some(Running {
            query: Arc::clone(query),
            text: Arc::from(text),
            parameters,
            started_at: SystemTime::now(),
        });
        // a terminate racing the line above saw no statement to cancel
        if let Some(reason) = self.entry.terminated() {
            query.cancel(reason);
        }
        RunningStatement {
            entry: Arc::clone(&self.entry),
        }
    }

    /// Why it was terminated, if it was; the host fails the next use and rolls back.
    pub fn terminated(&self) -> Option<Interrupt> {
        self.entry.terminated()
    }

    /// A cloneable handle for stopping this transaction from elsewhere (a socket reader, a
    /// timer).
    pub fn handle(&self) -> TransactionHandle {
        TransactionHandle {
            entry: Arc::clone(&self.entry),
        }
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        shard(self.entry.id).remove(&self.entry.id);
    }
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("id", &self.entry.display_id)
            .finish()
    }
}

/// Clears the running statement when dropped; see [`Transaction::run`].
pub struct RunningStatement {
    entry: Arc<Entry>,
}

impl Drop for RunningStatement {
    fn drop(&mut self) {
        let mut state = self.entry.state();
        state.current = None;
        state.idle_since = Instant::now();
    }
}

/// Stops a transaction from another thread or task.
#[derive(Clone)]
pub struct TransactionHandle {
    entry: Arc<Entry>,
}

impl TransactionHandle {
    /// Cancels the running statement only (a Bolt `RESET`, a closed connection); the transaction
    /// itself is the host's to end.
    pub fn cancel_statement(&self, reason: Interrupt) {
        if let Some(running) = &self.entry.state().current {
            running.query.cancel(reason);
        }
    }

    /// Terminates the transaction: cancels the running statement and fails its next use.
    pub fn terminate(&self, reason: Interrupt) {
        self.entry.terminate(reason);
    }

    /// The id `SHOW TRANSACTIONS` shows.
    pub fn display_id(&self) -> &str {
        &self.entry.display_id
    }
}

impl std::fmt::Debug for TransactionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransactionHandle")
            .field("id", &self.entry.display_id)
            .finish()
    }
}

/// Outcome of terminating one id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Termination {
    /// Terminated; carries the transaction's user.
    Terminated {
        /// Its user.
        user: String,
    },
    /// No such transaction the caller may see.
    NotFound,
}

/// Terminates the transaction shown as `display_id`, if the caller may see it (everyone's for
/// admins and trusted callers, else their own).
pub fn terminate(display_id: &str, caller: Option<&Principal>) -> Termination {
    let Some(id) = parse_id(display_id) else {
        return Termination::NotFound;
    };
    let entry = shard(id).get(&id).cloned();
    match entry {
        Some(e) if e.display_id == display_id && e.visible_to(caller) => {
            e.terminate(Interrupt::Terminated);
            Termination::Terminated {
                user: e.info.user.clone(),
            }
        }
        _ => Termination::NotFound,
    }
}

/// The id `SHOW TRANSACTIONS` shows for registry id `id`, if it's registered.
pub fn display_id_of(id: u64) -> Option<String> {
    shard(id).get(&id).map(|e| e.display_id.clone())
}

fn parse_id(display_id: &str) -> Option<u64> {
    display_id.rsplit_once("-transaction-")?.1.parse().ok()
}

/// One transaction as `SHOW TRANSACTIONS` reports it.
#[derive(Debug, Clone)]
pub struct TransactionStatus {
    /// Database.
    pub database: String,
    /// `<database>-transaction-<n>`.
    pub transaction_id: String,
    /// `query-<n>` of the running statement, empty when idle.
    pub current_query_id: String,
    /// Host connection id.
    pub connection_id: String,
    /// Client address.
    pub client_address: String,
    /// User.
    pub username: String,
    /// Text of the running statement, empty when idle.
    pub current_query: String,
    /// When it began.
    pub start_time: SystemTime,
    /// `Running`, or `Terminated with reason: ...`.
    pub status: String,
    /// Time since it began.
    pub elapsed: Duration,
    /// Client metadata.
    pub metadata: BTreeMap<String, CypherValue>,
    /// Parameters of the running statement.
    pub parameters: BTreeMap<String, CypherValue>,
    /// When the running statement began.
    pub current_query_start_time: Option<SystemTime>,
    /// `bolt`, `http`, `grpc`, `embedded`, ...
    pub protocol: String,
    /// Server endpoint.
    pub request_uri: String,
    /// `running`, or empty when idle.
    pub current_query_status: String,
    /// Time since the last statement ended; zero while one runs.
    pub idle: Duration,
    /// Time the running statement has run.
    pub current_query_elapsed: Option<Duration>,
    /// Bytes the running statement holds.
    pub current_query_allocated_bytes: Option<u64>,
    /// The caller's own transaction?
    pub is_caller: bool,
}

/// Every transaction the caller may see (everyone's for admins and trusted callers, else their
/// own), oldest first. The caller's own statement is listed even when it isn't registered, so
/// there's always at least that row.
pub fn list(caller: &QueryContext) -> Vec<TransactionStatus> {
    let principal = caller.principal();
    let mut entries: Vec<Arc<Entry>> = Vec::new();
    for shard in REGISTRY.iter() {
        let map = shard.lock().unwrap_or_else(PoisonError::into_inner);
        entries.extend(
            map.values()
                .filter(|e| e.visible_to(principal))
                .map(Arc::clone),
        );
    }
    entries.sort_by_key(|e| e.id);
    let now = Instant::now();
    let mut out: Vec<TransactionStatus> = entries
        .iter()
        .map(|e| status_of(e, now, caller.transaction() == Some(e.id)))
        .collect();
    if caller.transaction().is_none() {
        out.push(unregistered(caller));
    }
    out
}

fn status_of(e: &Entry, now: Instant, is_caller: bool) -> TransactionStatus {
    let state = e.state();
    let running = state.current.as_ref();
    let status = match e.terminated() {
        Some(reason) => format!("Terminated with reason: {}", termination_reason(reason)),
        None => "Running".to_string(),
    };
    TransactionStatus {
        database: e.info.database.clone(),
        transaction_id: e.display_id.clone(),
        current_query_id: running.map_or_else(String::new, |r| format!("query-{}", r.query.id())),
        connection_id: e.info.connection_id.clone(),
        client_address: e.info.client_address.clone(),
        username: e.info.user.clone(),
        current_query: running.map_or_else(String::new, |r| r.text.to_string()),
        start_time: e.started_at,
        status,
        elapsed: now.saturating_duration_since(e.started),
        metadata: e.info.metadata.clone(),
        parameters: running
            .and_then(|r| r.parameters.as_ref())
            .map(|p| p.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        current_query_start_time: running.map(|r| r.started_at),
        protocol: e.info.protocol.to_string(),
        request_uri: e.info.request_uri.clone(),
        current_query_status: if running.is_some() {
            "running".into()
        } else {
            String::new()
        },
        idle: if running.is_some() {
            Duration::ZERO
        } else {
            now.saturating_duration_since(state.idle_since)
        },
        current_query_elapsed: running.map(|r| r.query.elapsed()),
        current_query_allocated_bytes: running.map(|r| r.query.memory().used() as u64),
        is_caller,
    }
}

/// The calling statement when its host didn't register it (plain embedded use).
fn unregistered(caller: &QueryContext) -> TransactionStatus {
    let elapsed = caller.elapsed();
    let start_time = SystemTime::now()
        .checked_sub(elapsed)
        .unwrap_or_else(SystemTime::now);
    TransactionStatus {
        database: String::new(),
        transaction_id: format!("transaction-{}", caller.id()),
        current_query_id: format!("query-{}", caller.id()),
        connection_id: String::new(),
        client_address: String::new(),
        username: caller
            .principal()
            .map_or_else(String::new, |p| p.user.clone()),
        current_query: String::new(),
        start_time,
        status: "Running".into(),
        elapsed,
        metadata: BTreeMap::new(),
        parameters: BTreeMap::new(),
        current_query_start_time: Some(start_time),
        protocol: "embedded".into(),
        request_uri: String::new(),
        current_query_status: "running".into(),
        idle: Duration::ZERO,
        current_query_elapsed: Some(elapsed),
        current_query_allocated_bytes: Some(caller.memory().used() as u64),
        is_caller: true,
    }
}

fn termination_reason(reason: Interrupt) -> &'static str {
    match reason {
        Interrupt::TimedOut => "the transaction timed out",
        Interrupt::Cancelled => "the client cancelled it",
        Interrupt::Terminated => "explicitly terminated by the user",
    }
}

/// `SHOW TRANSACTIONS` columns without `YIELD`, in order (Neo4j's defaults).
pub const DEFAULT_COLUMNS: [&str; 10] = [
    "database",
    "transactionId",
    "currentQueryId",
    "connectionId",
    "clientAddress",
    "username",
    "currentQuery",
    "startTime",
    "status",
    "elapsedTime",
];

/// Every `SHOW TRANSACTIONS` column (`YIELD *`), in order. Columns Traverse doesn't track are null
/// (CPU time, locks, direct memory); page-cache counters are 0, the graph being in memory.
pub const ALL_COLUMNS: [&str; 39] = [
    "database",
    "transactionId",
    "currentQueryId",
    "connectionId",
    "clientAddress",
    "username",
    "currentQuery",
    "startTime",
    "status",
    "elapsedTime",
    "outerTransactionId",
    "metaData",
    "parameters",
    "planner",
    "runtime",
    "indexes",
    "currentQueryStartTime",
    "protocol",
    "requestUri",
    "currentQueryStatus",
    "statusDetails",
    "resourceInformation",
    "activeLockCount",
    "currentQueryActiveLockCount",
    "cpuTime",
    "waitTime",
    "idleTime",
    "currentQueryElapsedTime",
    "currentQueryCpuTime",
    "currentQueryWaitTime",
    "currentQueryIdleTime",
    "currentQueryAllocatedBytes",
    "allocatedDirectBytes",
    "estimatedUsedHeapMemory",
    "pageHits",
    "pageFaults",
    "currentQueryPageHits",
    "currentQueryPageFaults",
    "initializationStackTrace",
];

/// `TERMINATE TRANSACTIONS` columns, in order.
pub const TERMINATE_COLUMNS: [&str; 3] = ["transactionId", "username", "message"];

/// A status as values in [`ALL_COLUMNS`] order.
pub fn row(s: &TransactionStatus) -> Vec<CypherValue> {
    let string = |v: &str| CypherValue::String(v.to_string());
    let opt_time = |t: Option<SystemTime>| t.map_or(CypherValue::String(String::new()), iso_time);
    let opt_duration = |d: Option<Duration>| d.map_or(CypherValue::Null, duration);
    vec![
        string(&s.database),
        string(&s.transaction_id),
        string(&s.current_query_id),
        string(&s.connection_id),
        string(&s.client_address),
        string(&s.username),
        string(&s.current_query),
        iso_time(s.start_time),
        string(&s.status),
        duration(s.elapsed),
        string(""),
        CypherValue::Map(s.metadata.clone()),
        CypherValue::Map(s.parameters.clone()),
        string(if s.current_query.is_empty() {
            ""
        } else {
            "cost"
        }),
        string(if s.current_query.is_empty() {
            ""
        } else {
            "slotted"
        }),
        CypherValue::List(Vec::new()),
        opt_time(s.current_query_start_time),
        string(&s.protocol),
        string(&s.request_uri),
        string(&s.current_query_status),
        string(""),
        CypherValue::Map(BTreeMap::new()),
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        duration(s.idle),
        opt_duration(s.current_query_elapsed),
        CypherValue::Null,
        CypherValue::Null,
        CypherValue::Null,
        s.current_query_allocated_bytes
            .map_or(CypherValue::Null, |b| CypherValue::Integer(b as i64)),
        CypherValue::Null,
        s.current_query_allocated_bytes
            .map_or(CypherValue::Null, |b| CypherValue::Integer(b as i64)),
        CypherValue::Integer(0),
        CypherValue::Integer(0),
        CypherValue::Integer(0),
        CypherValue::Integer(0),
        string(""),
    ]
}

/// `2026-10-28T09:15:02.123Z`, the form Neo4j shows.
fn iso_time(t: SystemTime) -> CypherValue {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let text = chrono::DateTime::from_timestamp(since.as_secs() as i64, since.subsec_nanos())
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default();
    CypherValue::String(text)
}

fn duration(d: Duration) -> CypherValue {
    CypherValue::Duration(crate::temporal::CypherDuration::from_nanos(
        0,
        0,
        d.as_nanos() as i128,
    ))
}

/// A row of [`TERMINATE_COLUMNS`] for terminating `id`.
pub fn terminate_row(id: &str, caller: Option<&Principal>) -> Vec<CypherValue> {
    match terminate(id, caller) {
        Termination::Terminated { user } => vec![
            CypherValue::String(id.to_string()),
            CypherValue::String(user),
            CypherValue::String("Transaction terminated.".into()),
        ],
        Termination::NotFound => vec![
            CypherValue::String(id.to_string()),
            CypherValue::Null,
            CypherValue::String("Transaction not found.".into()),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(user: &str) -> TransactionInfo {
        TransactionInfo {
            database: "neo4j".into(),
            user: user.into(),
            protocol: "bolt",
            ..TransactionInfo::default()
        }
    }

    #[test]
    fn listed_while_registered() {
        let tx = register(info("alice"));
        let me = QueryContext::new(QueryOptions::default());
        assert!(
            list(&me)
                .iter()
                .any(|s| s.transaction_id == tx.display_id())
        );
        let id = tx.display_id().to_string();
        drop(tx);
        assert!(!list(&me).iter().any(|s| s.transaction_id == id));
    }

    #[test]
    fn terminate_cancels_the_statement_and_flags_the_transaction() {
        let tx = register(info("bob"));
        let q = QueryContext::new(tx.query_options(QueryOptions::default()));
        let running = tx.run(&q, "MATCH (n) RETURN n", None);
        assert_eq!(
            terminate(tx.display_id(), None),
            Termination::Terminated { user: "bob".into() }
        );
        assert_eq!(q.interrupt(), Some(Interrupt::Terminated));
        assert_eq!(tx.terminated(), Some(Interrupt::Terminated));
        drop(running);
        // the next statement starts stopped
        let next = QueryContext::new(QueryOptions::default());
        let _r = tx.run(&next, "RETURN 1", None);
        assert_eq!(next.interrupt(), Some(Interrupt::Terminated));
    }

    #[test]
    fn users_see_and_stop_only_their_own() {
        let theirs = register(info("carol"));
        let dave = Principal {
            user: "dave".into(),
            admin: false,
        };
        let me = QueryContext::new(QueryOptions {
            principal: Some(dave.clone()),
            ..QueryOptions::default()
        });
        assert!(
            !list(&me)
                .iter()
                .any(|s| s.transaction_id == theirs.display_id())
        );
        assert_eq!(
            terminate(theirs.display_id(), Some(&dave)),
            Termination::NotFound
        );
        assert_eq!(theirs.terminated(), None);
        let admin = Principal {
            user: "root".into(),
            admin: true,
        };
        assert!(matches!(
            terminate(theirs.display_id(), Some(&admin)),
            Termination::Terminated { .. }
        ));
    }

    #[test]
    fn unknown_ids_are_not_found() {
        assert_eq!(terminate("nope", None), Termination::NotFound);
        assert_eq!(
            terminate("neo4j-transaction-999999999", None),
            Termination::NotFound
        );
        // right number, wrong database
        let tx = register(info("erin"));
        let wrong = format!("other-transaction-{}", tx.id());
        assert_eq!(terminate(&wrong, None), Termination::NotFound);
    }

    #[test]
    fn an_unregistered_caller_sees_itself() {
        let me = QueryContext::new(QueryOptions::default());
        let rows = list(&me);
        let mine: Vec<_> = rows.iter().filter(|s| s.is_caller).collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].status, "Running");
        assert_eq!(row(mine[0]).len(), ALL_COLUMNS.len());
    }
}
