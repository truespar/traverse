//! GQLSTATUS: the condition behind an error, as a client branches on it. Codes follow ISO/IEC
//! 39075 section 23 and Neo4j's published extensions of it (`42N62`, `25N17`, ...), each with its standard
//! description and the Neo4j status code drivers already know, so a status reads the same over
//! Bolt, HTTP, gRPC and the embedded bindings.

use crate::query::Interrupt;
use traverse_core::error::{Error as CoreError, ViolationKind};

/// One error condition. [`CypherError::status`](crate::error::CypherError::status) gives it for
/// any error: the condition its site named, or its class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GqlStatus {
    /// `42001`: the text doesn't parse.
    InvalidSyntax,
    /// `42000`: another syntax error or access-rule violation.
    SyntaxOrAccessRule,
    /// `42N62`: a variable used but never defined.
    VariableNotDefined,
    /// `42N59`: a variable declared twice.
    VariableAlreadyDeclared,
    /// `42N78`: a bound variable redefined by a later clause.
    VariableAlreadyBound,
    /// `42N48`: no such function.
    UnknownFunction,
    /// `42N08`: no such procedure.
    UnknownProcedure,
    /// `42N51`: a parameter missing or of the wrong type.
    InvalidParameter,
    /// `42I24`: an aggregate function where none may be.
    InvalidAggregation,
    /// `42N02`: a write in a read-only transaction.
    WriteInReadAccessMode,
    /// `25G03`: a write in a GQL `READ ONLY` transaction block.
    ReadOnlyTransaction,
    /// `25G04`: one request using two graphs.
    MultipleGraphs,
    /// `42NA7`: `USE` names a graph that doesn't exist.
    GraphNotFound,
    /// `22000`: another data exception.
    DataException,
    /// `22012`: division by zero.
    DivisionByZero,
    /// `22003`: a number out of range (integer overflow).
    NumericOutOfRange,
    /// `22G03`: a value of the wrong type.
    InvalidValueType,
    /// `22N11`: an invalid argument.
    InvalidArgument,
    /// `22N02`: a negative number where it can't be (`LIMIT`, `SKIP`).
    NegativeValue,
    /// `22007`: an invalid date, time or datetime format.
    InvalidTemporalFormat,
    /// `22N79`: a uniqueness (or key) constraint broken.
    UniquenessViolated,
    /// `22N77`: a required property missing (existence or key constraint).
    PropertyPresenceViolated,
    /// `22N78`: a property of the wrong type (type constraint).
    PropertyTypeViolated,
    /// `G1001`: deleting a node that still has relationships.
    EdgesStillExist,
    /// `25N06`: the writer lock stayed taken for the whole wait.
    TransactionStartFailed,
    /// `25N05`: the transaction (or its database) was closed.
    TransactionClosed,
    /// `25N02`: a statement failed after writing, so the transaction can only roll back.
    TransactionFailed,
    /// `25N11`: a concurrent transaction committed a conflicting change first.
    TransactionConflict,
    /// `25N17`: needs an auto-commit statement (`CALL { ... } IN TRANSACTIONS`).
    ImplicitTransactionRequired,
    /// `25N16`: stopped by a cancel or `TERMINATE TRANSACTIONS`.
    TransactionTerminated,
    /// `25N16`: stopped by the server's time limit.
    TransactionTimedOut,
    /// `25N14`: stopped by the client's own time limit (a Bolt `tx_timeout`).
    TransactionTimedOutClient,
    /// `51N73`: the query's memory budget ran out.
    MemoryLimit,
    /// `51N36`: the process ran out of memory.
    OutOfMemory,
    /// `51N66`: another resource ran out.
    ResourceExhausted,
    /// `51N26`: not available in this implementation.
    NotSupported,
    /// `52N37`: a procedure failed.
    ProcedureFailed,
    /// `50N42`: anything else.
    Unexpected,
}

impl GqlStatus {
    /// The five-character GQLSTATUS code.
    pub fn code(self) -> &'static str {
        use GqlStatus as S;
        match self {
            S::InvalidSyntax => "42001",
            S::SyntaxOrAccessRule => "42000",
            S::VariableNotDefined => "42N62",
            S::VariableAlreadyDeclared => "42N59",
            S::VariableAlreadyBound => "42N78",
            S::UnknownFunction => "42N48",
            S::UnknownProcedure => "42N08",
            S::InvalidParameter => "42N51",
            S::InvalidAggregation => "42I24",
            S::WriteInReadAccessMode => "42N02",
            S::ReadOnlyTransaction => "25G03",
            S::MultipleGraphs => "25G04",
            S::GraphNotFound => "42NA7",
            S::DataException => "22000",
            S::DivisionByZero => "22012",
            S::NumericOutOfRange => "22003",
            S::InvalidValueType => "22G03",
            S::InvalidArgument => "22N11",
            S::NegativeValue => "22N02",
            S::InvalidTemporalFormat => "22007",
            S::UniquenessViolated => "22N79",
            S::PropertyPresenceViolated => "22N77",
            S::PropertyTypeViolated => "22N78",
            S::EdgesStillExist => "G1001",
            S::TransactionStartFailed => "25N06",
            S::TransactionClosed => "25N05",
            S::TransactionFailed => "25N02",
            S::TransactionConflict => "25N11",
            S::ImplicitTransactionRequired => "25N17",
            S::TransactionTerminated | S::TransactionTimedOut => "25N16",
            S::TransactionTimedOutClient => "25N14",
            S::MemoryLimit => "51N73",
            S::OutOfMemory => "51N36",
            S::ResourceExhausted => "51N66",
            S::NotSupported => "51N26",
            S::ProcedureFailed => "52N37",
            S::Unexpected => "50N42",
        }
    }

    /// The standard description: `error: <class> - <condition>`.
    pub fn description(self) -> &'static str {
        use GqlStatus as S;
        match self {
            S::InvalidSyntax => "error: syntax error or access rule violation - invalid syntax",
            S::SyntaxOrAccessRule => "error: syntax error or access rule violation",
            S::VariableNotDefined => {
                "error: syntax error or access rule violation - variable not defined"
            }
            S::VariableAlreadyDeclared => {
                "error: syntax error or access rule violation - variable already defined"
            }
            S::VariableAlreadyBound => {
                "error: syntax error or access rule violation - variable already bound"
            }
            S::UnknownFunction => "error: syntax error or access rule violation - no such function",
            S::UnknownProcedure => {
                "error: syntax error or access rule violation - no such procedure"
            }
            S::InvalidParameter => {
                "error: syntax error or access rule violation - invalid parameter"
            }
            S::InvalidAggregation => {
                "error: syntax error or access rule violation - invalid use of aggregate function"
            }
            S::WriteInReadAccessMode => {
                "error: syntax error or access rule violation - writing in read access mode"
            }
            S::ReadOnlyTransaction => {
                "error: invalid transaction state - read-only GQL-transaction"
            }
            S::MultipleGraphs => {
                "error: invalid transaction state - accessing multiple graphs not supported"
            }
            S::GraphNotFound => {
                "error: syntax error or access rule violation - referenced database not found"
            }
            S::DataException => "error: data exception",
            S::DivisionByZero => "error: data exception - division by zero",
            S::NumericOutOfRange => "error: data exception - numeric value out of range",
            S::InvalidValueType => "error: data exception - invalid value type",
            S::InvalidArgument => "error: data exception - invalid argument",
            S::NegativeValue => "error: data exception - specified negative numeric value",
            S::InvalidTemporalFormat => {
                "error: data exception - invalid date, time, or datetime format"
            }
            S::UniquenessViolated => {
                "error: data exception - property uniqueness constraint violated"
            }
            S::PropertyPresenceViolated => {
                "error: data exception - property presence verification failed"
            }
            S::PropertyTypeViolated => "error: data exception - property type verification failed",
            S::EdgesStillExist => "error: dependent object error - edges still exist",
            S::TransactionStartFailed => {
                "error: invalid transaction state - transaction start failed"
            }
            S::TransactionClosed => "error: invalid transaction state - transaction closed",
            S::TransactionFailed => {
                "error: invalid transaction state - unable to complete transaction"
            }
            S::TransactionConflict => {
                "error: invalid transaction state - conflicting transaction state"
            }
            S::ImplicitTransactionRequired => {
                "error: invalid transaction state - implicit transaction required"
            }
            S::TransactionTerminated | S::TransactionTimedOut => {
                "error: invalid transaction state - transaction termination transient error"
            }
            S::TransactionTimedOutClient => {
                "error: invalid transaction state - transaction termination client error"
            }
            S::MemoryLimit => {
                "error: system configuration or operation exception - transaction memory limit reached"
            }
            S::OutOfMemory => "error: system configuration or operation exception - out of memory",
            S::ResourceExhausted => {
                "error: system configuration or operation exception - resource exhaustion"
            }
            S::NotSupported => {
                "error: system configuration or operation exception - not supported in this version"
            }
            S::ProcedureFailed => "error: procedure exception - procedure execution error",
            S::Unexpected => "error: general processing exception - unexpected error",
        }
    }

    /// The Neo4j status code (`Neo.<Classification>.<Category>.<Title>`) drivers branch and retry
    /// on.
    pub fn neo4j_code(self) -> &'static str {
        use GqlStatus as S;
        match self {
            S::InvalidSyntax
            | S::VariableNotDefined
            | S::VariableAlreadyDeclared
            | S::VariableAlreadyBound
            | S::UnknownFunction
            | S::InvalidAggregation => "Neo.ClientError.Statement.SyntaxError",
            S::SyntaxOrAccessRule => "Neo.ClientError.Statement.SemanticError",
            S::UnknownProcedure => "Neo.ClientError.Procedure.ProcedureNotFound",
            S::InvalidParameter => "Neo.ClientError.Statement.ParameterMissing",
            S::WriteInReadAccessMode | S::ReadOnlyTransaction => {
                "Neo.ClientError.Statement.AccessMode"
            }
            S::MultipleGraphs => "Neo.ClientError.Statement.SemanticError",
            S::GraphNotFound => "Neo.ClientError.Database.DatabaseNotFound",
            S::DataException | S::InvalidValueType => "Neo.ClientError.Statement.TypeError",
            S::DivisionByZero | S::NumericOutOfRange => "Neo.ClientError.Statement.ArithmeticError",
            S::InvalidArgument | S::NegativeValue | S::InvalidTemporalFormat => {
                "Neo.ClientError.Statement.ArgumentError"
            }
            S::UniquenessViolated
            | S::PropertyPresenceViolated
            | S::PropertyTypeViolated
            | S::EdgesStillExist => "Neo.ClientError.Schema.ConstraintValidationFailed",
            S::TransactionStartFailed => "Neo.TransientError.Transaction.LockAcquisitionTimeout",
            S::TransactionClosed => "Neo.ClientError.Transaction.TransactionNotFound",
            S::TransactionFailed => "Neo.ClientError.Transaction.TransactionMarkedAsFailed",
            S::TransactionConflict => "Neo.TransientError.Transaction.Outdated",
            S::ImplicitTransactionRequired => {
                "Neo.ClientError.Transaction.ForbiddenDueToTransactionType"
            }
            S::TransactionTerminated => "Neo.TransientError.Transaction.Terminated",
            S::TransactionTimedOut => "Neo.TransientError.Transaction.TransactionTimedOut",
            S::TransactionTimedOutClient => {
                "Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration"
            }
            S::MemoryLimit => "Neo.TransientError.General.MemoryPoolOutOfMemoryError",
            S::OutOfMemory => "Neo.TransientError.General.OutOfMemoryError",
            S::ResourceExhausted => "Neo.TransientError.Request.ResourceExhaustion",
            S::NotSupported | S::ProcedureFailed | S::Unexpected => {
                "Neo.DatabaseError.Statement.ExecutionFailed"
            }
        }
    }

    /// Whether retrying the same work may succeed (a Neo4j `TransientError`): drivers retry these
    /// inside managed transactions.
    pub fn is_transient(self) -> bool {
        self.neo4j_code().starts_with("Neo.TransientError.")
    }

    /// A statement stopped for `reason`.
    pub fn of_interrupt(reason: Interrupt) -> Self {
        match reason {
            Interrupt::TimedOut => GqlStatus::TransactionTimedOut,
            Interrupt::Cancelled | Interrupt::Terminated => GqlStatus::TransactionTerminated,
        }
    }

    /// The condition behind an engine error.
    pub fn of_core(e: &CoreError) -> Self {
        use GqlStatus as S;
        match e {
            CoreError::ConstraintViolation { kind, .. } => match kind {
                ViolationKind::Uniqueness => S::UniquenessViolated,
                ViolationKind::Existence => S::PropertyPresenceViolated,
                ViolationKind::Type => S::PropertyTypeViolated,
            },
            CoreError::NodeHasEdges(_) => S::EdgesStillExist,
            CoreError::ReadOnlyTransaction => S::WriteInReadAccessMode,
            CoreError::TransactionFinished | CoreError::DatabaseClosed => S::TransactionClosed,
            CoreError::TransactionFailed => S::TransactionFailed,
            CoreError::TransactionConflict(_) => S::TransactionConflict,
            CoreError::Busy(_) => S::TransactionStartFailed,
            CoreError::MemoryLimitExceeded { .. } => S::MemoryLimit,
            CoreError::AllocationFailed(_) => S::OutOfMemory,
            CoreError::TooManyReaders(_) => S::ResourceExhausted,
            CoreError::Unsupported(_) => S::NotSupported,
            CoreError::NodeNotFound(_)
            | CoreError::EdgeNotFound(_)
            | CoreError::SourceNodeNotFound(_)
            | CoreError::TargetNodeNotFound(_)
            | CoreError::LabelNotFound(..)
            | CoreError::DuplicateLabel(..) => S::DataException,
            _ => S::Unexpected,
        }
    }
}

impl std::fmt::Display for GqlStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::GqlStatus as S;

    #[test]
    fn codes_are_five_characters_and_descriptions_match_their_class() {
        let all = [
            S::InvalidSyntax,
            S::SyntaxOrAccessRule,
            S::VariableNotDefined,
            S::VariableAlreadyDeclared,
            S::VariableAlreadyBound,
            S::UnknownFunction,
            S::UnknownProcedure,
            S::InvalidParameter,
            S::InvalidAggregation,
            S::WriteInReadAccessMode,
            S::ReadOnlyTransaction,
            S::MultipleGraphs,
            S::GraphNotFound,
            S::DataException,
            S::DivisionByZero,
            S::NumericOutOfRange,
            S::InvalidValueType,
            S::InvalidArgument,
            S::NegativeValue,
            S::InvalidTemporalFormat,
            S::UniquenessViolated,
            S::PropertyPresenceViolated,
            S::PropertyTypeViolated,
            S::EdgesStillExist,
            S::TransactionStartFailed,
            S::TransactionClosed,
            S::TransactionFailed,
            S::TransactionConflict,
            S::ImplicitTransactionRequired,
            S::TransactionTerminated,
            S::TransactionTimedOut,
            S::TransactionTimedOutClient,
            S::MemoryLimit,
            S::OutOfMemory,
            S::ResourceExhausted,
            S::NotSupported,
            S::ProcedureFailed,
            S::Unexpected,
        ];
        for s in all {
            assert_eq!(s.code().len(), 5, "{s:?}");
            let class = match &s.code()[..2] {
                "42" => "syntax error or access rule violation",
                "22" => "data exception",
                "25" => "invalid transaction state",
                "G1" => "dependent object error",
                "51" => "system configuration or operation exception",
                "52" => "procedure exception",
                "50" => "general processing exception",
                other => panic!("unexpected class {other}"),
            };
            assert!(
                s.description().starts_with(&format!("error: {class}")),
                "{s:?}"
            );
            assert!(s.neo4j_code().starts_with("Neo."), "{s:?}");
        }
        assert!(S::TransactionConflict.is_transient());
        assert!(!S::DivisionByZero.is_transient());
    }
}
