//! Cypher engine errors.

use crate::status::GqlStatus;

/// Errors from Cypher parsing and execution. [`status`](Self::status) gives each its GQLSTATUS.
#[derive(Debug, thiserror::Error)]
pub enum CypherError {
    /// Syntax error in the query string.
    #[error("syntax error at position {position}: {message}")]
    SyntaxError {
        /// What went wrong.
        message: String,
        /// Byte offset in the query where it was detected.
        position: usize,
        /// The condition, when the site knows a more specific one than invalid syntax (an
        /// undefined variable).
        status: Option<GqlStatus>,
    },

    /// Type error during expression evaluation.
    #[error("type error: {message}")]
    TypeError {
        /// What the mismatch was.
        message: String,
        /// The condition, when the site knows it (see [`with_status`](CypherError::with_status)).
        status: Option<GqlStatus>,
    },

    /// Semantic error (undefined variable, invalid aggregation, ...).
    #[error("semantic error: {message}")]
    SemanticError {
        /// What the issue was.
        message: String,
        /// The condition, when the site knows it.
        status: Option<GqlStatus>,
    },

    /// Runtime error (file I/O, resource access, ...).
    #[error("runtime error: {message}")]
    RuntimeError {
        /// What failed.
        message: String,
        /// The condition, when the site knows it.
        status: Option<GqlStatus>,
    },

    /// Error from the underlying graph engine.
    #[error("graph error: {0}")]
    GraphError(#[from] traverse_core::error::Error),

    /// The query was stopped before it finished: its deadline passed, its client cancelled it, or
    /// its transaction was terminated.
    #[error("{message}")]
    Interrupted {
        /// Why it stopped.
        reason: crate::query::Interrupt,
        /// What happened, for the caller.
        message: String,
    },
}

/// Result alias for Cypher operations.
pub type Result<T> = std::result::Result<T, CypherError>;

impl CypherError {
    /// Syntax error at `position`.
    pub fn syntax(message: impl Into<String>, position: usize) -> Self {
        Self::SyntaxError {
            message: message.into(),
            position,
            status: None,
        }
    }

    /// Type error.
    pub fn type_error(message: impl Into<String>) -> Self {
        Self::TypeError {
            message: message.into(),
            status: None,
        }
    }

    /// Semantic error.
    pub fn semantic(message: impl Into<String>) -> Self {
        Self::SemanticError {
            message: message.into(),
            status: None,
        }
    }

    /// Runtime error.
    pub fn runtime(message: impl Into<String>) -> Self {
        Self::RuntimeError {
            message: message.into(),
            status: None,
        }
    }

    /// Names the condition of a syntax, type, semantic or runtime error; the other kinds know
    /// theirs.
    pub fn with_status(mut self, condition: GqlStatus) -> Self {
        if let Self::SyntaxError { status, .. }
        | Self::TypeError { status, .. }
        | Self::SemanticError { status, .. }
        | Self::RuntimeError { status, .. } = &mut self
        {
            *status = Some(condition);
        }
        self
    }

    /// The GQLSTATUS: the condition the error's site named, or its kind's class (a type error
    /// `22G03`, a semantic error `42000`, a runtime error `50N42`).
    pub fn status(&self) -> GqlStatus {
        match self {
            Self::SyntaxError { status, .. } => status.unwrap_or(GqlStatus::InvalidSyntax),
            Self::TypeError { status, .. } => status.unwrap_or(GqlStatus::InvalidValueType),
            Self::SemanticError { status, .. } => status.unwrap_or(GqlStatus::SyntaxOrAccessRule),
            Self::RuntimeError { status, .. } => status.unwrap_or(GqlStatus::Unexpected),
            Self::GraphError(e) => GqlStatus::of_core(e),
            Self::Interrupted { reason, .. } => GqlStatus::of_interrupt(*reason),
        }
    }

    /// The message without the kind's prefix (`type error: ...`).
    pub fn message(&self) -> String {
        match self {
            Self::SyntaxError {
                message, position, ..
            } => format!("at position {position}: {message}"),
            Self::TypeError { message, .. }
            | Self::SemanticError { message, .. }
            | Self::RuntimeError { message, .. }
            | Self::Interrupted { message, .. } => message.clone(),
            Self::GraphError(e) => e.to_string(),
        }
    }
}
