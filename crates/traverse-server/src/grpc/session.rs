use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::Mutex;
use tonic::Status;
use uuid::Uuid;

use traverse_core::tx::{MvccStore, OwnedOptimisticTx};
use traverse_cypher::transactions::Transaction;

/// One explicit transaction session: an optimistic transaction on its database (runs on a
/// snapshot, applies at commit, blocks no one while open), like Bolt's.
struct TransactionSession {
    tx: OwnedOptimisticTx,
    /// Its entry in the transaction registry (`SHOW` / `TERMINATE TRANSACTIONS`).
    registration: Transaction,
    database_name: String,
    created_at: Instant,
    /// Last request in this transaction; the idle limit counts from here.
    last_used: Instant,
    timeout_ms: Option<i64>,
    /// A statement failed after changing data: only a rollback ends the transaction now.
    failed: bool,
}

impl TransactionSession {
    /// Err once the transaction is past its timeout (total time since BEGIN) or was terminated.
    fn check_live(&self) -> Result<(), Status> {
        if self.failed {
            return Err(Status::failed_precondition(
                "the transaction failed: a statement in it failed after changing data; roll it back",
            ));
        }
        if self.registration.terminated().is_some() {
            return Err(Status::aborted(
                "the transaction was terminated; roll it back and retry",
            ));
        }
        if self.remaining().is_some_and(|left| left.is_zero()) {
            return Err(Status::deadline_exceeded("transaction timeout exceeded"));
        }
        Ok(())
    }

    /// Time left before its timeout.
    fn remaining(&self) -> Option<Duration> {
        self.timeout_ms
            .filter(|ms| *ms > 0)
            .map(|ms| Duration::from_millis(ms as u64).saturating_sub(self.created_at.elapsed()))
    }
}

type SessionEntry = Arc<Mutex<TransactionSession>>;

/// gRPC status of a core error: conflicts are ABORTED (retry the transaction), the rest keep
/// their usual mapping.
pub(crate) fn tx_err_to_status(e: traverse_core::error::Error) -> Status {
    use traverse_core::error::Error;
    let gql = traverse_cypher::status::GqlStatus::of_core(&e);
    let code = match &e {
        Error::NodeNotFound(_)
        | Error::EdgeNotFound(_)
        | Error::SourceNodeNotFound(_)
        | Error::TargetNodeNotFound(_)
        | Error::LabelNotFound(..) => tonic::Code::NotFound,
        _ => super::service::grpc_code(gql),
    };
    let message = match e {
        Error::TransactionConflict(m) => m,
        Error::ConstraintViolation { message, .. } => message,
        other => other.to_string(),
    };
    super::service::with_gql_status(Status::new(code, message), gql)
}

/// UUID-keyed transaction sessions with idle expiry. `DashMap` with a per-session `Arc<Mutex>`,
/// so concurrent transactions don't serialize on one global lock: the map lock covers only
/// lookup/insert/remove, queries run under the per-session mutex.
pub struct SessionManager {
    sessions: DashMap<String, SessionEntry>,
    /// Idle limit: a session unused this long is rolled back (it pins the snapshot it reads).
    /// `None`: no limit.
    ttl: Option<Duration>,
}

impl SessionManager {
    pub fn new(ttl: Option<Duration>) -> Self {
        Self {
            sessions: DashMap::new(),
            ttl,
        }
    }

    /// Starts a background task rolling back sessions idle past the limit.
    pub fn spawn_cleanup(self: &Arc<Self>) {
        let Some(ttl) = self.ttl else {
            return;
        };
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(ttl.min(Duration::from_secs(5)));
            loop {
                interval.tick().await;
                mgr.sessions.retain(|_, entry| {
                    // a session in use isn't idle; keep it
                    match entry.try_lock() {
                        Ok(session) => session.last_used.elapsed() < ttl,
                        Err(_) => true,
                    }
                });
            }
        });
    }

    /// Begins a transaction, returning its UUID.
    pub async fn begin(
        &self,
        store: &Arc<MvccStore>,
        database_name: String,
        read_only: bool,
        timeout_ms: Option<i64>,
        registration: Transaction,
    ) -> Result<String, Status> {
        let tx = OwnedOptimisticTx::begin(store, read_only).map_err(tx_err_to_status)?;
        let id = Uuid::new_v4().to_string();
        let now = Instant::now();
        let session = TransactionSession {
            tx,
            registration,
            database_name,
            created_at: now,
            last_used: now,
            timeout_ms,
            failed: false,
        };
        self.sessions
            .insert(id.clone(), Arc::new(Mutex::new(session)));
        Ok(id)
    }

    /// Database of a transaction.
    pub async fn database_name(&self, tx_id: &str) -> Result<String, Status> {
        let entry = self.entry(tx_id)?;
        let session = entry.lock().await;
        Ok(session.database_name.clone())
    }

    fn entry(&self, tx_id: &str) -> Result<SessionEntry, Status> {
        Ok(self
            .sessions
            .get(tx_id)
            .ok_or_else(|| Status::not_found(format!("transaction {tx_id} not found")))?
            .value()
            .clone())
    }

    fn take(&self, tx_id: &str) -> Result<TransactionSession, Status> {
        let (_, entry) = self
            .sessions
            .remove(tx_id)
            .ok_or_else(|| Status::not_found(format!("transaction {tx_id} not found")))?;
        // sole owner after removal: unwrap the Arc
        Arc::try_unwrap(entry)
            .map(Mutex::into_inner)
            .map_err(|_| Status::aborted("transaction is still in use"))
    }

    /// Commits a transaction; returns (bookmark, database name) so the caller can mark the database
    /// dirty for persist-all. A conflict with a concurrent commit is ABORTED: retry the
    /// transaction.
    pub async fn commit(&self, tx_id: &str) -> Result<(String, String), Status> {
        let session = self.take(tx_id)?;
        session.check_live()?;
        let database_name = session.database_name;
        let ts = session.tx.commit().map_err(tx_err_to_status)?;
        Ok((format!("FB:{}:{}", ts.physical, ts.logical), database_name))
    }

    /// Rolls back a transaction: nothing was applied, so dropping it is the whole job.
    pub async fn rollback(&self, tx_id: &str) -> Result<(), Status> {
        drop(self.take(tx_id)?);
        Ok(())
    }

    /// Runs a closure on the session's transaction. The DashMap shard lock is released before
    /// execution; only the per-session mutex is held during the closure.
    pub async fn with_session<F, T>(&self, tx_id: &str, f: F) -> Result<T, Status>
    where
        F: FnOnce(&mut OwnedOptimisticTx) -> Result<T, Status>,
    {
        let entry = self.entry(tx_id)?;
        // DashMap shard lock released here, other sessions can proceed
        let mut session = entry.lock().await;
        session.check_live()?;
        session.last_used = Instant::now();
        f(&mut session.tx)
    }

    /// Runs a query statement on the session's transaction: `f` gets the transaction, its
    /// registration and the time it has left. `write` rejects read-only sessions.
    pub async fn with_session_query<F, T>(
        &self,
        tx_id: &str,
        write: bool,
        f: F,
    ) -> Result<T, Status>
    where
        F: FnOnce(&mut OwnedOptimisticTx, &Transaction, Option<Duration>) -> Result<T, Status>,
    {
        let entry = self.entry(tx_id)?;
        let mut session = entry.lock().await;
        session.check_live()?;
        if write && session.tx.is_read_only() {
            return Err(Status::permission_denied("read-only transaction"));
        }
        session.last_used = Instant::now();
        let remaining = session.remaining();
        let TransactionSession {
            tx,
            registration,
            failed,
            ..
        } = &mut *session;
        let before = traverse_core::tx::GraphView::write_count(tx);
        let result = f(tx, registration, remaining);
        // partial changes of a failed statement can't be committed
        if result.is_err() && traverse_core::tx::GraphView::write_count(tx) != before {
            *failed = true;
        }
        result
    }

    /// Runs a write closure, rejecting read-only sessions.
    pub async fn with_session_write<F, T>(&self, tx_id: &str, f: F) -> Result<T, Status>
    where
        F: FnOnce(&mut OwnedOptimisticTx) -> Result<T, Status>,
    {
        let entry = self.entry(tx_id)?;
        let mut session = entry.lock().await;
        session.check_live()?;
        if session.tx.is_read_only() {
            return Err(Status::permission_denied("read-only transaction"));
        }
        session.last_used = Instant::now();
        f(&mut session.tx)
    }
}
