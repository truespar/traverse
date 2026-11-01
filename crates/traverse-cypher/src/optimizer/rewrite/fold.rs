//! Constant folding.

use crate::ast::{BinOp, CmpOp, Expr, Literal, UnaryOp};
use crate::planner::LogicalPlan;

use super::walk::recurse_transform;

/// Folds compile-time constants and prunes Filters whose predicate is provably
/// true/false:
/// - literal comparisons: `1 = 0`, `2 < 3`, `'a' = 'b'`, ...
/// - boolean short-circuits: `false AND x -> false`, `true OR x -> true`,
///   `true AND x -> x`, `false OR x -> x`
/// - `NOT` of literals: `NOT true -> false`
///
/// A Filter folding to `Bool(true)` is removed. One folding to `Bool(false)`
/// stays with the literal predicate, and the operator builder emits a zero-row
/// op without touching the input; keeping the Filter preserves the column
/// schema downstream aggregates / EXISTS / projections need.
pub fn fold_constants(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &fold_constants_inner)
}

fn fold_constants_inner(plan: LogicalPlan) -> LogicalPlan {
    if let LogicalPlan::Filter { input, predicate } = plan {
        let folded = fold_expr(predicate);
        match folded {
            Expr::Literal(Literal::Bool(true)) => *input,
            other => LogicalPlan::Filter {
                input,
                predicate: other,
            },
        }
    } else {
        plan
    }
}

/// Folds compile-time constants in an expression; only fully literal subtrees collapse, everything
/// else passes through.
#[recursive::recursive]
pub(crate) fn fold_expr(expr: Expr) -> Expr {
    match expr {
        Expr::UnaryOp(UnaryOp::Not, inner) => {
            let inner = fold_expr(*inner);
            match inner {
                Expr::Literal(Literal::Bool(b)) => Expr::Literal(Literal::Bool(!b)),
                Expr::Literal(Literal::Null) => Expr::Literal(Literal::Null),
                other => Expr::UnaryOp(UnaryOp::Not, Box::new(other)),
            }
        }
        Expr::BinaryOp(left, op, right) => {
            let left = fold_expr(*left);
            let right = fold_expr(*right);
            match (op, &left, &right) {
                // boolean short-circuits with one literal side
                (BinOp::And, Expr::Literal(Literal::Bool(false)), _)
                | (BinOp::And, _, Expr::Literal(Literal::Bool(false))) => {
                    Expr::Literal(Literal::Bool(false))
                }
                (BinOp::Or, Expr::Literal(Literal::Bool(true)), _)
                | (BinOp::Or, _, Expr::Literal(Literal::Bool(true))) => {
                    Expr::Literal(Literal::Bool(true))
                }
                (BinOp::And, Expr::Literal(Literal::Bool(true)), _) => right,
                (BinOp::And, _, Expr::Literal(Literal::Bool(true))) => left,
                (BinOp::Or, Expr::Literal(Literal::Bool(false)), _) => right,
                (BinOp::Or, _, Expr::Literal(Literal::Bool(false))) => left,
                _ => Expr::BinaryOp(Box::new(left), op, Box::new(right)),
            }
        }
        Expr::Comparison(first, rest) => {
            let first = fold_expr(*first);
            let folded_rest: Vec<(CmpOp, Expr)> =
                rest.into_iter().map(|(op, e)| (op, fold_expr(e))).collect();
            // fold single comparisons with literals on BOTH sides only; chains like `1 < x < 5`
            // keep their variables
            if folded_rest.len() == 1
                && let (Expr::Literal(a), (op, Expr::Literal(b))) = (&first, &folded_rest[0])
                && let Some(b_val) = compare_literals(a, *op, b)
            {
                return Expr::Literal(Literal::Bool(b_val));
            }
            Expr::Comparison(Box::new(first), folded_rest)
        }
        // everything else (variables, properties, parameters, function calls, lists, maps, ...)
        // passes through: folding it would be wrong or run user code at plan time
        other => other,
    }
}

/// Compares two literals. `None` for type mismatches or NULL (runtime three-valued logic decides).
fn compare_literals(a: &Literal, op: CmpOp, b: &Literal) -> Option<bool> {
    use Literal::*;
    match (a, b) {
        (Integer(x), Integer(y)) => Some(cmp_ord(*x, *y, op)),
        (Float(x), Float(y)) => Some(cmp_ord(*x, *y, op)),
        (Integer(x), Float(y)) => Some(cmp_ord(*x as f64, *y, op)),
        (Float(x), Integer(y)) => Some(cmp_ord(*x, *y as f64, op)),
        (String(x), String(y)) => Some(cmp_ord(x.as_str(), y.as_str(), op)),
        (Bool(x), Bool(y)) if op == CmpOp::Eq => Some(x == y),
        (Bool(x), Bool(y)) if op == CmpOp::Neq => Some(x != y),
        // NULL comparisons are unknown in three-valued logic; leave them so runtime returns NULL,
        // not a wrong Bool
        _ => None,
    }
}

fn cmp_ord<T: PartialOrd>(a: T, b: T, op: CmpOp) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Neq => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Lte => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Gte => a >= b,
    }
}
