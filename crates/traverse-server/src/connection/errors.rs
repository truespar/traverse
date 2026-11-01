//! Server errors as Neo4j / GQL status codes for FAILURE messages.

use traverse_cypher::status::GqlStatus;

use crate::error::ServerError;

/// What a FAILURE reports for an error: the Neo4j code (its classification drives the drivers'
/// retries), the GQLSTATUS when the error names one, and the message.
pub(super) struct Failure {
    pub code: &'static str,
    pub status: Option<GqlStatus>,
    pub message: String,
}

impl Failure {
    fn of(status: GqlStatus, message: &str) -> Self {
        Self {
            code: status.neo4j_code(),
            status: Some(status),
            message: message.to_string(),
        }
    }
}

/// Server error -> FAILURE content. Statement errors carry their GQLSTATUS; the server's own errors
/// map by their kind.
pub(super) fn failure_of(e: &ServerError) -> Failure {
    let (code, message) = match e {
        ServerError::Statement { status, message } => return Failure::of(*status, message),
        // the server's limit; a client's tx_timeout is mapped by the connection
        ServerError::Interrupted { reason, message } => {
            return Failure::of(GqlStatus::of_interrupt(*reason), message);
        }
        // drivers retry TransientErrors inside managed transactions
        ServerError::Conflict(msg) => return Failure::of(GqlStatus::TransactionConflict, msg),
        ServerError::Graph(msg) => {
            if msg.contains("memory limit exceeded") {
                (
                    "Neo.TransientError.General.MemoryPoolOutOfMemoryError",
                    msg.clone(),
                )
            } else if msg.contains("deadlock") {
                (
                    "Neo.TransientError.Transaction.DeadlockDetected",
                    msg.clone(),
                )
            } else if msg.contains("constraint") {
                (
                    "Neo.ClientError.Schema.ConstraintValidationFailed",
                    msg.clone(),
                )
            } else {
                ("Neo.DatabaseError.General.UnknownError", msg.clone())
            }
        }
        ServerError::Protocol(msg) => ("Neo.ClientError.Request.Invalid", msg.clone()),
        other => ("Neo.DatabaseError.General.UnknownError", other.to_string()),
    };
    Failure {
        code,
        status: None,
        message,
    }
}

/// Neo4j error code -> GQLSTATUS code + standard description for the Bolt 5.7+ FAILURE format,
/// following Neo4j's coarse mapping: syntax errors in the SQLSTATE-style 42xxx class, missing
/// parameters 42N51, everything unclassified 50N42 ("general processing exception - unexpected
/// error"), as Neo4j does for non-GQL-aware errors.
pub(super) fn gql_status_from_neo4j_code(code: &str) -> (&'static str, &'static str) {
    if code.ends_with("Statement.SyntaxError") {
        (
            "42001",
            "error: syntax error or access rule violation - invalid syntax",
        )
    } else if code.ends_with("Statement.ParameterMissing") {
        (
            "42N51",
            "error: syntax error or access rule violation - invalid parameter",
        )
    } else if code.contains(".Security.") {
        (
            "42NFF",
            "error: syntax error or access rule violation - permission/access denied",
        )
    } else if code.contains("TransientError") {
        (
            "51T01",
            "error: system configuration or operation exception - transient error",
        )
    } else {
        (
            "50N42",
            "error: general processing exception - unexpected error",
        )
    }
}

/// Severity from a Neo4j error code (e.g. "Neo.ClientError.Statement.SyntaxError" -> "ERROR").
pub(super) fn severity_from_neo4j_code(code: &str) -> &'static str {
    if code.contains("TransientError") {
        "WARNING"
    } else {
        "ERROR"
    }
}

/// Classification from a Neo4j error code.
pub(super) fn classification_from_neo4j_code(code: &str) -> &'static str {
    if code.contains("ClientError") {
        "CLIENT_ERROR"
    } else if code.contains("TransientError") {
        "TRANSIENT_ERROR"
    } else {
        "DATABASE_ERROR"
    }
}
