//! The running query: its parameters, statement clock, deadline, stop flag and memory budget,
//! shared by every thread working on it.
//!
//! A host (server session, binding, CLI) builds a [`QueryContext`] per statement, enters it on the
//! thread that runs the statement and keeps the `Arc` to stop it from anywhere: another thread, a
//! socket reader, `TERMINATE TRANSACTIONS`. The engine's entry points run each statement in a child
//! of the entered context (its own parameters and clock, the parent's limits and budget), or in a
//! fresh one when none is entered.
//!
//! Everything a statement reads while it runs comes from its context, so whatever thread enters it
//! (a rayon worker, the task a lazy result resumes on) sees the same parameters, the same `now`
//! and the same limits. Operators poll [`check`] every few thousand rows: a thread-local read and
//! a relaxed atomic load, plus a clock read when there's a deadline. Algorithm kernels hold the
//! context in their cancellation token.

use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
// `web_time::Instant` is `std::time::Instant` natively and a performance.now() shim on wasm32,
// where the std clock panics
use web_time::Instant;

use crate::error::CypherError;
use crate::memory::QueryMemoryTracker;
use crate::types::CypherValue;

/// Why a query stopped before it finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interrupt {
    /// It passed its deadline.
    TimedOut,
    /// The client running it cancelled it: a reset, a closed connection, a cancel token.
    Cancelled,
    /// Someone else stopped it: `TERMINATE TRANSACTIONS`, the admin API, a server shutdown.
    Terminated,
}

impl Interrupt {
    const fn code(self) -> u8 {
        match self {
            Interrupt::TimedOut => 1,
            Interrupt::Cancelled => 2,
            Interrupt::Terminated => 3,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Interrupt::TimedOut),
            2 => Some(Interrupt::Cancelled),
            3 => Some(Interrupt::Terminated),
            _ => None,
        }
    }

    /// Short lowercase name: `timed out`, `cancelled`, `terminated`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Interrupt::TimedOut => "timed out",
            Interrupt::Cancelled => "cancelled",
            Interrupt::Terminated => "terminated",
        }
    }
}

/// Who runs a query, for the commands that act on other users' work (`SHOW TRANSACTIONS`,
/// `TERMINATE TRANSACTIONS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// User name; empty when anonymous.
    pub user: String,
    /// May see and terminate every user's transactions.
    pub admin: bool,
}

/// Settings for a new [`QueryContext`].
#[derive(Debug, Clone, Default)]
pub struct QueryOptions {
    /// Stop after this long.
    pub timeout: Option<Duration>,
    /// Memory budget in bytes (0 = unlimited); `None` takes [`crate::memory::query_limit`].
    pub memory_limit: Option<usize>,
    /// Who runs it; `None` = trusted (embedded use), sees everything.
    pub principal: Option<Principal>,
    /// Registered transaction it runs in (see [`crate::transactions`]).
    pub transaction: Option<u64>,
}

/// Query parameters, shared rather than copied.
pub type Params = Arc<HashMap<String, CypherValue>>;

/// Settings for a [`QueryContext::child`].
#[derive(Debug, Clone, Default)]
pub struct ChildOptions {
    /// A time limit of its own (the parent's still applies).
    pub timeout: Option<Duration>,
    /// Its parameters; `None` keeps the parent's.
    pub params: Option<Params>,
    /// Keep the parent's `now` (nested executions) rather than start a statement clock of its own.
    pub shared_clock: bool,
    /// The registered transaction it runs in; `None` keeps the parent's.
    pub transaction: Option<u64>,
}

static NEXT_QUERY_ID: AtomicU64 = AtomicU64::new(1);

/// One running statement: parameters, clock, limits, stop flag, memory account. Shared as `Arc`;
/// see the module docs.
#[derive(Debug)]
pub struct QueryContext {
    id: u64,
    /// Id of the outermost context: work for one query shares per-thread caches.
    root: u64,
    started: Instant,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
    /// 0 = running, else an [`Interrupt`] code; the first reason wins.
    stop: AtomicU8,
    /// Shared with child contexts, so nested work spends its caller's budget.
    memory: Arc<QueryMemoryTracker>,
    principal: Option<Principal>,
    transaction: Option<u64>,
    params: Params,
    /// The statement's `now` (UTC nanoseconds, local offset in seconds), fixed at first use.
    clock: OnceLock<(i64, i32)>,
    /// Use the parent's clock: nested executions are part of their caller's statement.
    shared_clock: bool,
    /// The query a child context runs inside; it stops when its parent does.
    parent: Option<Arc<QueryContext>>,
}

impl QueryContext {
    /// A context starting now, without parameters.
    pub fn new(options: QueryOptions) -> Arc<Self> {
        Self::with_params(options, Params::default())
    }

    fn with_params(options: QueryOptions, params: Params) -> Arc<Self> {
        let started = Instant::now();
        let id = NEXT_QUERY_ID.fetch_add(1, Ordering::Relaxed);
        Arc::new(Self {
            id,
            root: id,
            started,
            timeout: options.timeout,
            deadline: options.timeout.map(|t| started + t),
            stop: AtomicU8::new(0),
            memory: Arc::new(QueryMemoryTracker::new(
                options
                    .memory_limit
                    .unwrap_or_else(crate::memory::query_limit),
            )),
            principal: options.principal,
            transaction: options.transaction,
            params,
            clock: OnceLock::new(),
            shared_clock: false,
            parent: None,
        })
    }

    /// A context for work inside this query: it stops when this query does (and at its own time
    /// limit), spends this query's memory budget and runs as this query's principal; see
    /// [`ChildOptions`] for the rest.
    pub fn child(self: &Arc<Self>, options: ChildOptions) -> Arc<Self> {
        let started = Instant::now();
        Arc::new(Self {
            id: NEXT_QUERY_ID.fetch_add(1, Ordering::Relaxed),
            root: self.root,
            started,
            timeout: options.timeout,
            deadline: options.timeout.map(|t| started + t),
            stop: AtomicU8::new(0),
            memory: Arc::clone(&self.memory),
            principal: self.principal.clone(),
            transaction: options.transaction.or(self.transaction),
            params: options.params.unwrap_or_else(|| Arc::clone(&self.params)),
            clock: OnceLock::new(),
            shared_clock: options.shared_clock,
            parent: Some(Arc::clone(self)),
        })
    }

    /// The query this child context runs inside.
    pub fn parent(&self) -> Option<&Arc<QueryContext>> {
        self.parent.as_ref()
    }

    /// Process-unique id; `SHOW TRANSACTIONS` shows it as `query-<id>`.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// When it started.
    pub fn started(&self) -> Instant {
        self.started
    }

    /// Time since it started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Its own time limit, if any (a parent's may be shorter).
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Its memory account.
    pub fn memory(&self) -> &QueryMemoryTracker {
        &self.memory
    }

    /// Who runs it.
    pub fn principal(&self) -> Option<&Principal> {
        self.principal.as_ref()
    }

    /// The registered transaction it runs in.
    pub fn transaction(&self) -> Option<u64> {
        self.transaction
    }

    /// Its parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// The statement's `now`, as UTC nanoseconds and the local offset in seconds: sampled at first
    /// use, then the same for every call in the statement, on every thread.
    pub fn statement_now(&self) -> (i64, i32) {
        if self.shared_clock
            && let Some(parent) = &self.parent
        {
            return parent.statement_now();
        }
        *self.clock.get_or_init(|| {
            let now = chrono::Local::now();
            (
                now.timestamp_nanos_opt().unwrap_or(0),
                now.offset().local_minus_utc(),
            )
        })
    }

    /// Stops the query at its next check, from any thread. `true` if this call stopped it, `false`
    /// if it had already stopped (the first reason is kept).
    pub fn cancel(&self, reason: Interrupt) -> bool {
        self.stop
            .compare_exchange(0, reason.code(), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// Why it stopped and which context stopped it (itself or a parent); passing a deadline
    /// counts.
    #[inline]
    fn stopped(&self) -> Option<(Interrupt, &QueryContext)> {
        let code = self.stop.load(Ordering::Relaxed);
        if code != 0 {
            return Interrupt::from_code(code).map(|r| (r, self));
        }
        if let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            self.cancel(Interrupt::TimedOut);
            return Interrupt::from_code(self.stop.load(Ordering::Relaxed)).map(|r| (r, self));
        }
        match &self.parent {
            Some(parent) => parent.stopped(),
            None => None,
        }
    }

    /// Why it stopped, if it has; passing the deadline counts, and so does the parent stopping.
    #[inline]
    pub fn interrupt(&self) -> Option<Interrupt> {
        self.stopped().map(|(reason, _)| reason)
    }

    /// Error if it was stopped.
    #[inline]
    pub fn check_interrupt(&self) -> Result<(), CypherError> {
        match self.stopped() {
            Some((reason, source)) => Err(source.interrupted_error(reason)),
            None => Ok(()),
        }
    }

    /// Error if it was stopped or the process is over its memory ceiling.
    #[inline]
    pub fn check(&self) -> Result<(), CypherError> {
        self.check_interrupt()?;
        if is_memory_exhausted() {
            return Err(crate::memory::process_limit_error());
        }
        Ok(())
    }

    /// The error a query stopped for `reason` fails with, worded from the context that stopped it.
    pub fn interrupted_error(&self, reason: Interrupt) -> CypherError {
        if let Some((actual, source)) = self.stopped()
            && actual == reason
            && !std::ptr::eq(source, self)
        {
            return source.interrupted_error(reason);
        }
        let message = match reason {
            Interrupt::TimedOut => match self.timeout {
                Some(t) => format!("the query timed out after {}", format_duration(t)),
                None => "the query timed out".to_string(),
            },
            Interrupt::Cancelled => "the query was cancelled".to_string(),
            Interrupt::Terminated => "the transaction was terminated".to_string(),
        };
        CypherError::Interrupted { reason, message }
    }

    /// Makes this the current query on this thread until the scope drops, which restores whatever
    /// was current before.
    #[must_use = "the query stops being current when this value is dropped"]
    pub fn enter(self: &Arc<Self>) -> QueryScope {
        let previous = CURRENT.with(|c| c.borrow_mut().replace(Arc::clone(self)));
        QueryScope {
            restore: Some(previous),
            _not_send: PhantomData,
        }
    }
}

/// `1.5s`, `250ms`, `2m 5s`; enough for an error message.
fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        let s = d.as_secs_f64();
        if s.fract() == 0.0 {
            format!("{}s", s as u64)
        } else {
            format!("{s:.1}s")
        }
    } else {
        let s = d.as_secs();
        if s.is_multiple_of(60) {
            format!("{}m", s / 60)
        } else {
            format!("{}m {}s", s / 60, s % 60)
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<QueryContext>>> = const { RefCell::new(None) };
}

/// The query entered on this thread, if any.
pub fn current() -> Option<Arc<QueryContext>> {
    CURRENT.with(|c| c.borrow().clone())
}

/// Runs `f` with the current query, without cloning the `Arc`.
#[inline]
pub fn with_current<R>(f: impl FnOnce(Option<&Arc<QueryContext>>) -> R) -> R {
    CURRENT.with(|c| f(c.borrow().as_ref()))
}

/// A scope that changes nothing.
fn joined() -> QueryScope {
    QueryScope {
        restore: None,
        _not_send: PhantomData,
    }
}

/// Joins the query entered on this thread, or, when there's none, enters a fresh one with default
/// limits and no parameters for the scope's life.
#[must_use = "the query scope ends when this value is dropped"]
pub fn enter() -> QueryScope {
    if with_current(|c| c.is_some()) {
        return joined();
    }
    QueryContext::new(QueryOptions::default()).enter()
}

/// Starts a statement with `params`: a child of the entered query (its limits, budget and
/// transaction; a clock of its own), or a fresh query when none is entered. The engine's entry
/// points call this.
#[must_use = "the query scope ends when this value is dropped"]
pub fn enter_statement(params: impl Into<Params>) -> QueryScope {
    let params = params.into();
    match current() {
        Some(outer) => outer
            .child(ChildOptions {
                params: Some(params),
                ..ChildOptions::default()
            })
            .enter(),
        None => QueryContext::with_params(QueryOptions::default(), params).enter(),
    }
}

/// Runs nested work with `params` (a trigger, `apoc.cypher.run`): a child of the entered query
/// sharing its clock, or a fresh query when none is entered.
#[must_use = "the query scope ends when this value is dropped"]
pub fn enter_nested(params: HashMap<String, CypherValue>) -> QueryScope {
    let params = Arc::new(params);
    match current() {
        Some(outer) => outer
            .child(ChildOptions {
                params: Some(params),
                shared_clock: true,
                ..ChildOptions::default()
            })
            .enter(),
        None => QueryContext::with_params(QueryOptions::default(), params).enter(),
    }
}

/// [`enter`], but a fresh context gets `memory_limit` (0 = unlimited) instead of the default.
#[must_use = "the query scope ends when this value is dropped"]
pub fn enter_with_memory_limit(memory_limit: usize) -> QueryScope {
    if with_current(|c| c.is_some()) {
        return joined();
    }
    QueryContext::new(QueryOptions {
        memory_limit: Some(memory_limit),
        ..QueryOptions::default()
    })
    .enter()
}

/// Enters `ctx` (when given) for the scope's life; for rayon workers taking on a query's work.
#[must_use = "the query stops being current when this value is dropped"]
pub fn enter_opt(ctx: Option<&Arc<QueryContext>>) -> QueryScope {
    match ctx {
        Some(ctx) => ensure(ctx),
        None => joined(),
    }
}

/// Makes `ctx` current for the scope's life unless it already is (then this costs a thread-local
/// read). For results pulled a row at a time, possibly from different threads.
#[inline]
#[must_use = "the query stops being current when this value is dropped"]
pub fn ensure(ctx: &Arc<QueryContext>) -> QueryScope {
    if with_current(|c| c.is_some_and(|c| Arc::ptr_eq(c, ctx))) {
        return joined();
    }
    ctx.enter()
}

/// While alive, a query is current on this thread; see [`QueryContext::enter`] and [`enter`].
/// Thread-bound: never held across an `.await`.
pub struct QueryScope {
    /// `Some(previous)` when this scope installed a context; `None` when it joined one.
    restore: Option<Option<Arc<QueryContext>>>,
    _not_send: PhantomData<*const ()>,
}

impl Drop for QueryScope {
    fn drop(&mut self) {
        if let Some(previous) = self.restore.take() {
            CURRENT.with(|c| *c.borrow_mut() = previous);
        }
    }
}

/// One parameter of the current query.
#[inline]
pub fn param(name: &str) -> Option<CypherValue> {
    with_current(|c| c.and_then(|ctx| ctx.params.get(name).cloned()))
}

/// The current query's parameters (empty outside a query).
pub fn params() -> Params {
    with_current(|c| c.map_or_else(Params::default, |ctx| Arc::clone(&ctx.params)))
}

/// Error if the current query was stopped (deadline, cancel, terminate) or the process is over its
/// memory ceiling. Call from hot loops every few thousand iterations.
#[inline]
pub fn check() -> Result<(), CypherError> {
    with_current(|c| match c {
        Some(ctx) => ctx.check(),
        None if is_memory_exhausted() => Err(crate::memory::process_limit_error()),
        None => Ok(()),
    })
}

/// Error if the current query was stopped; leaves the memory ceiling to the charge path. The
/// cheapest check, for per-row loops.
#[inline]
pub fn check_interrupt() -> Result<(), CypherError> {
    with_current(|c| match c {
        Some(ctx) => ctx.check_interrupt(),
        None => Ok(()),
    })
}

/// Why the current query stopped, if it has.
#[inline]
pub fn interrupt() -> Option<Interrupt> {
    with_current(|c| c.and_then(|ctx| ctx.interrupt()))
}

/// The error for a stop with `reason`, worded from the current query (its time limit) when there
/// is one; for code that learns the reason from a token rather than the context.
pub fn interrupted_error(reason: Interrupt) -> CypherError {
    with_current(|c| match c {
        Some(ctx) => ctx.interrupted_error(reason),
        None => QueryContext::new(QueryOptions::default()).interrupted_error(reason),
    })
}

/// Is process memory over the configured ceiling? Cheap (the RSS lookup is cached, refreshed every
/// 256 calls); `false` with no ceiling.
#[inline]
pub fn is_memory_exhausted() -> bool {
    if let Some(ceiling) = traverse_core::memory::memory_ceiling() {
        traverse_core::memory::allocated_bytes() > ceiling
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_reason_wins() {
        let ctx = QueryContext::new(QueryOptions::default());
        assert_eq!(ctx.interrupt(), None);
        assert!(ctx.cancel(Interrupt::Terminated));
        assert!(!ctx.cancel(Interrupt::Cancelled));
        assert_eq!(ctx.interrupt(), Some(Interrupt::Terminated));
        let err = ctx.check().unwrap_err();
        assert!(matches!(
            err,
            CypherError::Interrupted {
                reason: Interrupt::Terminated,
                ..
            }
        ));
    }

    #[test]
    fn deadline_turns_into_timed_out() {
        let ctx = QueryContext::new(QueryOptions {
            timeout: Some(Duration::ZERO),
            ..QueryOptions::default()
        });
        assert_eq!(ctx.interrupt(), Some(Interrupt::TimedOut));
        // a later cancel doesn't change the reason
        assert!(!ctx.cancel(Interrupt::Cancelled));
        assert_eq!(ctx.interrupt(), Some(Interrupt::TimedOut));
        assert!(ctx.check().unwrap_err().to_string().contains("0ms"));
    }

    #[test]
    fn entering_restores_the_previous_query() {
        assert!(current().is_none());
        let outer = QueryContext::new(QueryOptions::default());
        let inner = QueryContext::new(QueryOptions::default());
        {
            let _a = outer.enter();
            {
                // nested work joins
                let _join = enter();
                assert_eq!(current().unwrap().id(), outer.id());
                let _b = inner.enter();
                assert_eq!(current().unwrap().id(), inner.id());
            }
            assert_eq!(current().unwrap().id(), outer.id());
        }
        assert!(current().is_none());
        {
            let _fresh = enter();
            assert!(current().is_some());
        }
        assert!(current().is_none());
    }

    #[test]
    fn checks_see_a_cancel_from_another_thread() {
        let ctx = QueryContext::new(QueryOptions::default());
        let _scope = ctx.enter();
        assert!(check().is_ok());
        let remote = Arc::clone(&ctx);
        std::thread::spawn(move || remote.cancel(Interrupt::Cancelled))
            .join()
            .unwrap();
        assert_eq!(interrupt(), Some(Interrupt::Cancelled));
        assert!(check_interrupt().is_err());
    }

    #[test]
    fn a_child_stops_on_its_own_deadline_or_with_its_parent() {
        let parent = QueryContext::new(QueryOptions::default());
        let timed = parent.child(ChildOptions {
            timeout: Some(Duration::ZERO),
            ..ChildOptions::default()
        });
        assert_eq!(timed.interrupt(), Some(Interrupt::TimedOut));
        assert_eq!(parent.interrupt(), None, "the child's deadline is its own");

        let child = parent.child(ChildOptions::default());
        assert_eq!(child.interrupt(), None);
        parent.cancel(Interrupt::Terminated);
        assert_eq!(child.interrupt(), Some(Interrupt::Terminated));

        // one budget
        child.memory().add(10).unwrap();
        assert_eq!(parent.memory().used(), 10);
    }

    #[test]
    fn a_parent_timeout_is_worded_from_the_parent() {
        let parent = QueryContext::new(QueryOptions {
            timeout: Some(Duration::from_millis(0)),
            ..QueryOptions::default()
        });
        let child = parent.child(ChildOptions::default());
        let err = child.check_interrupt().unwrap_err().to_string();
        assert!(err.contains("after 0ms"), "{err}");
    }

    #[test]
    fn statements_get_parameters_and_a_clock_of_their_own() {
        let host = QueryContext::new(QueryOptions::default());
        let _host = host.enter();
        let first = {
            let _s = enter_statement(HashMap::from([("x".into(), CypherValue::Integer(1))]));
            assert_eq!(param("x"), Some(CypherValue::Integer(1)));
            let now = current().unwrap().statement_now();
            // nested work shares the statement clock and swaps the parameters
            {
                let _n = enter_nested(HashMap::new());
                assert_eq!(param("x"), None);
                assert_eq!(current().unwrap().statement_now(), now);
            }
            assert_eq!(param("x"), Some(CypherValue::Integer(1)));
            now
        };
        assert_eq!(param("x"), None, "parameters end with the statement");
        // a worker thread entering the statement sees its parameters and clock
        let stmt = host.child(ChildOptions {
            params: Some(Arc::new(HashMap::from([(
                "y".into(),
                CypherValue::Integer(2),
            )]))),
            ..ChildOptions::default()
        });
        let now = stmt.statement_now();
        let seen = std::thread::spawn(move || {
            let _w = stmt.enter();
            (param("y"), current().unwrap().statement_now())
        })
        .join()
        .unwrap();
        assert_eq!(seen, (Some(CypherValue::Integer(2)), now));
        let _ = first;
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(format_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(format_duration(Duration::from_secs(2)), "2s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_duration(Duration::from_secs(120)), "2m");
    }
}
