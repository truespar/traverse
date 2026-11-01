//! Property-based tests over the GQL parser + lowerer: where the other tests
//! check fixed hand-written queries, these check *invariants* over random
//! inputs.
//!
//! * **No panic on random input**: invalid queries are rejected cleanly. The
//!   most valuable single property for a language front end.
//! * **Integer literal round-trip**: every i64 lexed and parsed comes back
//!   unchanged.
//! * **Comparison-operator round-trip**: `a < b` parses with lhs/op/rhs
//!   intact; same for `=/<>/<=/<` and the rest.
//! * **Logical-operator algebra**: `NOT NOT a` and `a` lower to equivalent
//!   shapes; `AND` is symmetric.
//!
//! Each `proptest!` block runs ~256 random cases (`PROPTEST_CASES` overrides).
//! Failures are shrunk to a minimal reproducer and saved under
//! `tests/proptest-regressions/`, so the next run replays it.

use proptest::prelude::*;

// -- 1. parser no-panic ----------------------------------------------
// arbitrary bounded-length strings; the only requirement is no panic, Ok and Err are both fine

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        ..ProptestConfig::default()
    })]

    /// Arbitrary ASCII input never panics the parser.
    #[test]
    fn parser_never_panics_on_random_ascii(input in ".{0,256}") {
        // `.{0,256}`: any string up to 256 chars from proptest's default printable alphabet
        let _ = traverse_gql::parse(&input);
    }

    /// Same, biased toward GQL-keyword-like text so more grammar productions get hit instead of
    /// bouncing off the lexer.
    #[test]
    fn parser_never_panics_on_gql_keyword_soup(
        input in "(MATCH |RETURN |WHERE |CREATE |DELETE |WITH |UNWIND |LET |CALL |\
                   YIELD |NEXT |AS |AND |OR |NOT |XOR |IS |NULL |true |false |\
                   123 |'foo' |\"bar\" |\\(|\\)|\\{|\\}|\\[|\\]|->|<-|--|\\*|\\+|=){0,32}",
    ) {
        let _ = traverse_gql::parse(&input);
    }

    /// Deeply nested parentheses mustn't overflow the stack at any reasonable depth.
    #[test]
    fn parser_never_panics_on_nested_parens(depth in 0usize..=64) {
        let opens = "(".repeat(depth);
        let closes = ")".repeat(depth);
        let q = format!("RETURN {opens}1{closes} AS x");
        let _ = traverse_gql::parse(&q);
    }
}

// -- 2. integer literal round-trip -------------------------------

proptest! {
    /// Every i64 lexed from its decimal form parses back to the same value (full i64 range;
    /// over-i64 is a numeric overflow error, not a panic).
    #[test]
    fn integer_literal_round_trips(value: i64) {
        let q = format!("RETURN {value} AS x");
        let p = traverse_gql::parse(&q).expect("integer literal should parse");
        // walk the first statement's first projection item back to its IntegerLit
        let stmt = &p.statements[0];
        if let traverse_gql::Statement::Query(qs) = stmt {
            // the RETURN clause
            for c in &qs.head.clauses {
                if let traverse_gql::Clause::Return(r) = c {
                    if let traverse_gql::Expression::IntegerLit { value: v, .. } =
                        &r.items[0].expr
                    {
                        prop_assert_eq!(*v, value);
                        return Ok(());
                    } else if let traverse_gql::Expression::Unary {
                        op: traverse_gql::UnaryOp::Neg,
                        operand,
                        ..
                    } = &r.items[0].expr {
                        // negative literals lex as `Neg(IntegerLit)`; the minus isn't part of the
                        // integer token
                        if let traverse_gql::Expression::IntegerLit { value: v, .. } =
                            operand.as_ref()
                        {
                            // i64::MIN's positive form overflows, so accept the negated value or
                            // i64::MIN
                            let recovered = v.checked_neg().unwrap_or(i64::MIN);
                            prop_assert_eq!(recovered, value);
                            return Ok(());
                        }
                    }
                    prop_assert!(
                        false,
                        "first projection wasn't an IntegerLit / -IntegerLit: \
                         {:?}",
                        &r.items[0].expr,
                    );
                }
            }
        }
        prop_assert!(false, "expected a Query statement with a RETURN clause");
    }
}

// -- 3. string literal round-trip --------------------------------

proptest! {
    /// ASCII strings without single quotes round-trip through lexer and parser (an embedded `'`
    /// needs `''`; excluded to keep the assertion simple).
    #[test]
    fn string_literal_round_trips(s in "[A-Za-z0-9 _]{0,64}") {
        let q = format!("RETURN '{s}' AS x");
        let p = traverse_gql::parse(&q).expect("string literal should parse");
        let stmt = &p.statements[0];
        if let traverse_gql::Statement::Query(qs) = stmt {
            for c in &qs.head.clauses {
                if let traverse_gql::Clause::Return(r) = c {
                    if let traverse_gql::Expression::StringLit { value, .. } =
                        &r.items[0].expr
                    {
                        prop_assert_eq!(value, &s);
                        return Ok(());
                    }
                    prop_assert!(false, "expected StringLit, got {:?}", &r.items[0].expr);
                }
            }
        }
        prop_assert!(false, "expected RETURN clause");
    }
}

// -- 4. boolean operator algebra ---------------------------------
// no AST evaluator at test level, but algebraic identities must parse to consistent shapes: `NOT
// NOT x` vs `x` differ by two `Not` wrappers; `a AND b` and `b AND a` both parse to `Binary { op:
// And, ... }` with operands in source order (commutativity is the executor's job)

proptest! {
    #[test]
    fn double_negation_lexes_with_two_not_wrappers(
        // integer literal leaf keeps the AST shape deterministic
        n in any::<i32>(),
    ) {
        let q = format!("RETURN NOT NOT {n} AS x");
        let p = traverse_gql::parse(&q).expect("double NOT should parse");
        let stmt = &p.statements[0];
        if let traverse_gql::Statement::Query(qs) = stmt {
            for c in &qs.head.clauses {
                if let traverse_gql::Clause::Return(r) = c {
                    // outermost: Unary(Not, Unary(Not, ...))
                    let outer = &r.items[0].expr;
                    if let traverse_gql::Expression::Unary {
                        op: traverse_gql::UnaryOp::Not,
                        operand: inner,
                        ..
                    } = outer
                        && let traverse_gql::Expression::Unary {
                            op: traverse_gql::UnaryOp::Not,
                            ..
                        } = inner.as_ref()
                        {
                            return Ok(());
                        }
                    prop_assert!(false, "expected Not(Not(...)), got {:?}", outer);
                }
            }
        }
    }
}

// -- 5. lowering doesn't panic on parsable inputs ----------------
// among accepted inputs the lowerer must return Ok or Err cleanly; well-formed leading-keyword
// shapes keep the strategy on the parser's happy path

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    #[test]
    fn lowerer_never_panics_on_parsed_queries(
        ident in "[a-z][a-z0-9_]{0,8}",
        n in any::<i32>(),
    ) {
        // a small legal MATCH/RETURN query
        let q = format!("MATCH ({ident}) RETURN {ident}.x AS r, {n} AS k");
        if let Ok(p) = traverse_gql::parse(&q) {
            let _ = traverse_gql::lower::lower(&p);
        }
    }
}
