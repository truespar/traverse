//! ISO/IEC 39075 GQL parser for Traverse.
//!
//! [`parse`] returns a GQL [`Program`] or a list of [`ParseError`]s;
//! [`lower`] turns a program into the Cypher AST the shared executor runs, and
//! [`execute_gql`] does parse -> lower -> execute in one call. See `DESIGN.md`
//! for the architecture and `CONFORMANCE.md` for spec coverage.

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod ast;
pub mod error;
pub mod exec;
pub mod lexer;
pub mod lower;
pub mod parser;

pub use ast::{
    BinaryOp, CallClause, CallSubqueryClause, CaseArm, Clause, ConstraintKind, DeleteClause,
    EdgeDirection, EdgePattern, EdgeQuantifier, Expression, FilterClause, ForClause, GraphTypeBody,
    InsertClause, LabelExpression, LetClause, MatchClause, NodePattern, OrderItem, PathPattern,
    Program, PropertyMap, QueryBody, QueryStatement, RemoveClause, RemoveItem, ReturnClause,
    ReturnItem, SchemaPath, SetClause, SetItem, SortDirection, Statement, TypeKind, UnaryOp,
    UnionKind, UnionPart, UnwindClause, UseReference, WithClause, YieldItem,
};
pub use error::{ParseError, ParseErrorKind};
pub use exec::{
    execute as execute_gql, execute_on as execute_gql_on, execute_timed as execute_gql_timed,
    query_writes, use_target,
};
pub use lexer::Span;
pub use parser::{parse, parse_with_stats};

/// The ISO/IEC 39075 edition this crate targets; bumped with a new edition, so consumers can gate
/// on it.
pub const DIALECT_VERSION: &str = "ISO/IEC 39075:2024";

#[cfg(test)]
mod smoke {
    use super::*;

    #[test]
    fn dialect_version_is_set() {
        assert!(!DIALECT_VERSION.is_empty());
    }

    #[test]
    fn parse_handles_trivia_only_inputs() {
        for src in ["", " ", "\n", "-- comment\n", "/* block */"] {
            let p = parse(src).expect(src);
            assert!(p.statements.is_empty(), "input {:?}", src);
        }
    }

    #[test]
    fn parse_round_trips_simple_match_return() {
        let p = parse("MATCH (n:Person) WHERE n.age > 30 RETURN n.name").expect("parse");
        assert_eq!(p.statements.len(), 1);
    }
}
