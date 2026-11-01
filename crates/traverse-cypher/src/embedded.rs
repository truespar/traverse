//! Statements of the language bindings. Each runs under its own [`QueryContext`] with the
//! database's statement timeout, is listed by `SHOW TRANSACTIONS`, and stops from any thread:
//! [`EmbeddedTransaction`] is an explicit transaction's place in the registry (its
//! [`handle`](EmbeddedTransaction::handle) cancels the running statement), [`auto_commit`] runs one
//! statement as a transaction of its own, a [`StatementCanceller`] stops one statement before or
//! while it runs, and `TERMINATE TRANSACTIONS` ends either kind.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use traverse_core::embedded::{SharedDatabase, Transaction};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::CypherResult;
use crate::plan_cache::PlanCache;
use crate::query::{Interrupt, Params, QueryContext, QueryOptions};
use crate::transactions::{self, Parameters, TransactionHandle, TransactionInfo};

/// A binding's transaction in the registry. Lives as long as the binding's transaction handle.
#[derive(Debug)]
pub struct EmbeddedTransaction {
    registration: transactions::Transaction,
    timeout: Option<Duration>,
    database: Arc<str>,
}

impl EmbeddedTransaction {
    /// Registers a transaction on `database`; its statements stop after `timeout`.
    pub fn begin(database: &str, timeout: Option<Duration>) -> Self {
        Self {
            registration: transactions::register(TransactionInfo {
                database: database.to_string(),
                protocol: "embedded",
                ..TransactionInfo::default()
            }),
            timeout,
            database: database.into(),
        }
    }

    /// The database's name: the graph a GQL `USE` may name.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// Registers a transaction on `db`: its name, its statement timeout.
    pub fn on(db: &SharedDatabase) -> Self {
        Self::begin(&db.name(), db.statement_timeout())
    }

    /// Stops this transaction's statements from another thread: the handle outlives the
    /// transaction (a cancel after it ended does nothing).
    pub fn handle(&self) -> TransactionHandle {
        self.registration.handle()
    }

    /// Runs one statement: refused once the transaction was terminated; otherwise `run` executes
    /// under a context of its own (the statement timeout, this transaction), listed as the
    /// transaction's running statement. `run` gets that context: cancelling it stops this
    /// statement and no later one.
    pub fn run<T>(
        &self,
        text: &str,
        parameters: Option<Parameters>,
        run: impl FnOnce(&Arc<QueryContext>) -> Result<T, CypherError>,
    ) -> Result<T, CypherError> {
        self.check_live()?;
        let ctx = QueryContext::new(self.registration.query_options(QueryOptions {
            timeout: self.timeout,
            ..QueryOptions::default()
        }));
        let _running = self.registration.run(&ctx, text, parameters);
        let _scope = ctx.enter();
        run(&ctx)
    }

    /// Error if the transaction was terminated; a commit checks this first.
    pub fn check_live(&self) -> Result<(), CypherError> {
        match self.registration.terminated() {
            Some(reason) => {
                Err(QueryContext::new(QueryOptions::default()).interrupted_error(reason))
            }
            None => Ok(()),
        }
    }
}

/// Cancels the statement `handle`'s transaction is running, if any.
pub fn cancel(handle: &TransactionHandle) {
    handle.cancel_statement(Interrupt::Cancelled);
}

/// Stops one statement from any thread, whether it already runs or starts later (a cancel before
/// the start stops it as it starts). Stays cancelled. Clones share the state.
#[derive(Clone, Default)]
pub struct StatementCanceller(Arc<Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    statement: Option<Arc<QueryContext>>,
}

impl std::fmt::Debug for StatementCanceller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatementCanceller")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl StatementCanceller {
    /// Not cancelled, no statement yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stops the statement: now if it runs, as it starts otherwise.
    pub fn cancel(&self) {
        let mut state = self.lock();
        state.cancelled = true;
        if let Some(ctx) = &state.statement {
            ctx.cancel(Interrupt::Cancelled);
        }
    }

    /// Whether [`cancel`](Self::cancel) ran.
    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Makes `ctx` the statement this stops, stopping it at once when already cancelled.
    pub fn attach(&self, ctx: &Arc<QueryContext>) {
        let mut state = self.lock();
        if state.cancelled {
            ctx.cancel(Interrupt::Cancelled);
        }
        state.statement = Some(Arc::clone(ctx));
    }

    fn lock(&self) -> MutexGuard<'_, CancelState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Runs one statement as a transaction of its own on `db`: a read on a snapshot, so it never
/// waits; a write in a write transaction committed when the statement succeeds and rolled back
/// when it fails, in which `CALL { ... } IN TRANSACTIONS` commits its batches. `writes` decides
/// which on a snapshot (from the statement's plan); `run` executes it on the chosen view. Listed by
/// `SHOW TRANSACTIONS` while it runs, limited by the database's statement timeout (the wait for
/// the writer lock included), stopped by `canceller`.
pub fn auto_commit<T>(
    db: &SharedDatabase,
    text: &str,
    parameters: Option<Parameters>,
    canceller: Option<&StatementCanceller>,
    writes: impl FnOnce(&mut dyn GraphView) -> Result<bool, CypherError>,
    run: impl FnOnce(&mut dyn GraphView) -> Result<T, CypherError>,
) -> Result<T, CypherError> {
    EmbeddedTransaction::on(db).run(text, parameters, |ctx| {
        if let Some(canceller) = canceller {
            canceller.attach(ctx);
        }
        ctx.check_interrupt()?;
        let read = db.begin_read()?;
        let lease = match read.with(|tx| tx.statement(writes))? {
            Ok(false) => read,
            Ok(true) => {
                let _ = read.finish(Transaction::abort);
                db.begin_auto_commit(|| ctx.check_interrupt())?
            }
            Err(e) => {
                let _ = read.finish(Transaction::abort);
                return Err(e);
            }
        };
        match lease.with(|tx| tx.statement(run))? {
            Ok(value) => {
                lease.finish(Transaction::commit).and_then(|r| r)?;
                Ok(value)
            }
            Err(e) => {
                let _ = lease.finish(Transaction::abort);
                Err(e)
            }
        }
    })
}

/// [`auto_commit`] for a Cypher statement planned through `plans`: its plan decides read or
/// write.
pub fn execute_auto_commit(
    db: &SharedDatabase,
    plans: &PlanCache,
    text: &str,
    params: &Params,
    canceller: Option<&StatementCanceller>,
) -> Result<CypherResult, CypherError> {
    let shown = (!params.is_empty()).then(|| Arc::clone(params));
    auto_commit(
        db,
        text,
        shown,
        canceller,
        |view| Ok(plans.prepare(view, text, params)?.writes()),
        |view| crate::plan_cache::execute(plans, view, text, params),
    )
}
