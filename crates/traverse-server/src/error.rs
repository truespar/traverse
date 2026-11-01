use std::io;

use thiserror::Error;

/// Server error types.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("PackStream error: {0}")]
    PackStream(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("handshake error: {0}")]
    Handshake(String),

    /// A statement failed: the engine named its GQLSTATUS.
    #[error("{message}")]
    Statement {
        /// The condition.
        status: traverse_cypher::status::GqlStatus,
        /// What happened.
        message: String,
    },

    #[error("graph error: {0}")]
    Graph(String),

    /// A concurrent transaction committed a conflicting change first (retryable).
    #[error("{0}")]
    Conflict(String),

    /// The statement was stopped: its time limit, a client cancel, or `TERMINATE TRANSACTIONS`.
    #[error("{message}")]
    Interrupted {
        /// Why it stopped.
        reason: traverse_cypher::query::Interrupt,
        /// What happened.
        message: String,
    },

    #[error("connection closed")]
    ConnectionClosed,
}

impl From<traverse_core::error::Error> for ServerError {
    fn from(e: traverse_core::error::Error) -> Self {
        match e {
            traverse_core::error::Error::TransactionConflict(m) => ServerError::Conflict(m),
            other => ServerError::Statement {
                status: traverse_cypher::status::GqlStatus::of_core(&other),
                message: other.to_string(),
            },
        }
    }
}

impl From<traverse_cypher::error::CypherError> for ServerError {
    fn from(e: traverse_cypher::error::CypherError) -> Self {
        use traverse_cypher::error::CypherError;
        match e {
            CypherError::GraphError(ge) => ServerError::from(ge),
            CypherError::Interrupted { reason, message } => {
                ServerError::Interrupted { reason, message }
            }
            other => ServerError::Statement {
                status: other.status(),
                message: other.message(),
            },
        }
    }
}

pub type Result<T> = std::result::Result<T, ServerError>;
