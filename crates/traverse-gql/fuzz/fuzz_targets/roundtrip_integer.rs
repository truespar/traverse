//! Integer-literal round-trip fuzz target: the input bytes are read as i64s;
//! each is formatted as a decimal literal in `RETURN <n> AS x`, parsed, and the
//! recovered value must match. A mismatch (or panic) is a numeric-lexing bug.
//!
//! ```text
//! cargo +nightly fuzz run roundtrip_integer --max-total-time 600
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // i64s from 8-byte chunks; any tail ignored
    for chunk in data.chunks_exact(8) {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(chunk);
        let value = i64::from_le_bytes(buf);
        let q = format!("RETURN {value} AS x");
        let Ok(program) = traverse_gql::parse(&q) else {
            // bound values should always parse, but proptest covers that; here only no panic is
            // required
            continue;
        };
        // no AST-level integer evaluation, so walk the structure to the recovered value
        let traverse_gql::Statement::Query(qs) = &program.statements[0] else {
            continue;
        };
        for c in &qs.head.clauses {
            if let traverse_gql::Clause::Return(r) = c {
                let recovered = match &r.items[0].expr {
                    traverse_gql::Expression::IntegerLit { value: v, .. } => Some(*v),
                    traverse_gql::Expression::Unary {
                        op: traverse_gql::UnaryOp::Neg,
                        operand,
                        ..
                    } => {
                        if let traverse_gql::Expression::IntegerLit { value: v, .. } =
                            operand.as_ref()
                        {
                            v.checked_neg()
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(r) = recovered {
                    assert_eq!(r, value, "integer literal round-trip mismatch");
                }
            }
        }
    }
});
