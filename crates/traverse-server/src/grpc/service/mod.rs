//! gRPC service: `FgServiceImpl`, with authentication, role checks, error mapping and database
//! resolution; the RPCs, procedures and result streaming live in the submodules.

mod procedures;
mod rpc;
mod stream;

use std::sync::Arc;

use base64::Engine;
use tonic::Status;
use traverse_core::tx::MvccStore;

use crate::auth::Role;
use crate::registry::{DatabaseEntry, DatabaseRegistry};
use crate::sync::Unpoisoned as _;

use super::session::SessionManager;

pub struct FgServiceImpl {
    pub registry: Arc<DatabaseRegistry>,
    pub sessions: Arc<SessionManager>,
    pub auth_catalog: Option<Arc<std::sync::RwLock<crate::auth::AuthCatalog>>>,
    /// Credentials checked recently: requests carry Basic auth, and bcrypt per call would cost
    /// more than most queries.
    pub verified: crate::auth::VerifiedCache,
}

// -- Authentication & RBAC -------------------------------------------

#[derive(Clone)]
struct AuthInfo {
    role: Option<Role>, // None = auth disabled (open access)
    username: Option<String>,
}

impl FgServiceImpl {
    fn extract_auth(&self, metadata: &tonic::metadata::MetadataMap) -> Result<AuthInfo, Status> {
        let catalog = match &self.auth_catalog {
            Some(c) => c,
            None => {
                return Ok(AuthInfo {
                    role: None,
                    username: None,
                });
            }
        };
        let (open, generation) = {
            let catalog = catalog.read_unpoisoned();
            (catalog.is_empty(), catalog.generation())
        };
        if open {
            return Ok(AuthInfo {
                role: None,
                username: None,
            });
        }

        let auth_val = metadata
            .get("authorization")
            .ok_or_else(|| Status::unauthenticated("authentication required"))?
            .to_str()
            .map_err(|_| Status::unauthenticated("invalid authorization header"))?;

        let encoded = auth_val
            .strip_prefix("Basic ")
            .ok_or_else(|| Status::unauthenticated("unsupported auth scheme; use Basic"))?;

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| Status::unauthenticated("invalid credentials encoding"))?;
        let decoded_str = String::from_utf8(decoded)
            .map_err(|_| Status::unauthenticated("invalid credentials encoding"))?;

        let (user, pass) = decoded_str
            .split_once(':')
            .ok_or_else(|| Status::unauthenticated("invalid credentials format"))?;

        let role = match self.verified.get(user, pass, generation) {
            Some(role) => role,
            None => {
                let role = crate::auth::authenticate(catalog, user, pass)
                    .ok_or_else(|| Status::unauthenticated("invalid principal or credentials"))?;
                self.verified.insert(user, pass, generation, role);
                role
            }
        };
        Ok(AuthInfo {
            role: Some(role),
            username: Some(user.to_string()),
        })
    }
}

fn require_admin(auth: &AuthInfo) -> Result<(), Status> {
    match auth.role {
        None => Ok(()), // auth disabled
        Some(Role::Admin) => Ok(()),
        _ => Err(Status::permission_denied("admin privilege required")),
    }
}

fn require_write(auth: &AuthInfo) -> Result<(), Status> {
    match auth.role {
        None => Ok(()),
        Some(Role::Reader) => Err(Status::permission_denied("write privilege required")),
        _ => Ok(()),
    }
}

// -- Helper: map CypherError/ServerError to tonic::Status -------------

fn cypher_err_to_status(e: traverse_cypher::error::CypherError) -> Status {
    use traverse_cypher::error::CypherError;
    use traverse_cypher::query::Interrupt;
    let status = e.status();
    let code = match &e {
        CypherError::Interrupted { reason, .. } => match reason {
            Interrupt::TimedOut => tonic::Code::DeadlineExceeded,
            Interrupt::Cancelled => tonic::Code::Cancelled,
            // retryable, like Bolt's Transaction.Terminated
            Interrupt::Terminated => tonic::Code::Aborted,
        },
        _ => grpc_code(status),
    };
    with_gql_status(Status::new(code, e.to_string()), status)
}

/// The gRPC code for a GQLSTATUS.
pub(crate) fn grpc_code(status: traverse_cypher::status::GqlStatus) -> tonic::Code {
    use tonic::Code;
    use traverse_cypher::status::GqlStatus as S;
    match status {
        S::UnknownFunction | S::UnknownProcedure => Code::NotFound,
        S::WriteInReadAccessMode => Code::PermissionDenied,
        S::UniquenessViolated
        | S::PropertyPresenceViolated
        | S::PropertyTypeViolated
        | S::EdgesStillExist
        | S::TransactionClosed
        | S::TransactionFailed
        | S::ImplicitTransactionRequired => Code::FailedPrecondition,
        S::TransactionConflict | S::TransactionTerminated => Code::Aborted,
        S::TransactionTimedOut | S::TransactionTimedOutClient => Code::DeadlineExceeded,
        S::TransactionStartFailed => Code::Unavailable,
        S::MemoryLimit | S::OutOfMemory | S::ResourceExhausted => Code::ResourceExhausted,
        S::NotSupported => Code::Unimplemented,
        S::ProcedureFailed | S::Unexpected => Code::Internal,
        _ => Code::InvalidArgument,
    }
}

/// Adds the `gql-status` metadata entry (the five-character code) to `status`.
pub(crate) fn with_gql_status(
    mut status: Status,
    gql: traverse_cypher::status::GqlStatus,
) -> Status {
    if let Ok(value) = gql.code().parse() {
        status.metadata_mut().insert("gql-status", value);
    }
    status
}

/// Cancels `ctx` when the client stops listening (drops the response stream), so a statement
/// stops at once rather than when it next sends rows. Abort the returned task when the statement
/// ends: it holds a sender, which keeps the stream open.
fn watch_disconnect<T: Send + 'static>(
    rt: &tokio::runtime::Handle,
    sender: &tokio::sync::mpsc::Sender<T>,
    ctx: &std::sync::Arc<traverse_cypher::query::QueryContext>,
) -> tokio::task::JoinHandle<()> {
    let sender = sender.clone();
    let ctx = std::sync::Arc::clone(ctx);
    rt.spawn(async move {
        sender.closed().await;
        ctx.cancel(traverse_cypher::query::Interrupt::Cancelled);
    })
}

/// The client's time limit for a request (`timeout_ms`, 0 = none).
fn client_limit(timeout_ms: i64) -> Option<std::time::Duration> {
    (timeout_ms > 0).then(|| std::time::Duration::from_millis(timeout_ms as u64))
}

fn graph_err_to_status(e: traverse_core::error::Error) -> Status {
    super::session::tx_err_to_status(e)
}

// -- Helper: resolve database from registry ---------------------------

fn resolve_database(
    database: &str,
    registry: &DatabaseRegistry,
) -> Result<(Arc<DatabaseEntry>, String), Status> {
    if database.is_empty() {
        let name = registry.default_name().to_string();
        Ok((registry.default_entry(), name))
    } else {
        let resolved = registry.resolve_alias(database);
        match registry.get(&resolved) {
            Some(entry) => Ok((entry, resolved)),
            None => Err(Status::not_found(format!(
                "database '{}' not found",
                database
            ))),
        }
    }
}

// -- Helper: validate bookmarks ---------------------------------------

fn validate_bookmarks(bookmarks: &[String], store: &MvccStore) -> Result<(), Status> {
    if bookmarks.is_empty() {
        return Ok(());
    }
    let current = store.visible_ts();
    for bm in bookmarks {
        // Parse "FB:physical:logical" format
        if let Some(rest) = bm.strip_prefix("FB:")
            && let Some((phys_str, log_str)) = rest.split_once(':')
            && let (Ok(phys), Ok(log)) = (phys_str.parse::<u64>(), log_str.parse::<u32>())
            && (phys > current.physical || (phys == current.physical && log > current.logical))
        {
            return Err(Status::failed_precondition(format!(
                "bookmark {bm} not yet visible (current: FB:{}:{})",
                current.physical, current.logical,
            )));
        }
        // unknown bookmark formats are accepted silently (forward compat)
    }
    Ok(())
}
