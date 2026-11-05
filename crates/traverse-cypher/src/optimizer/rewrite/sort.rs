//! `LIMIT` over `ORDER BY` over a projection: the sort needs only its keys, so the projection
//! moves above the limit and the sort reads the keys straight from the projected expressions.
//! The top-k then stages the surviving rows only, and every other projected column is computed
//! for those rows alone instead of for the whole input.

use crate::ast::{Expr, ProjectionItem, SortItem};
use crate::planner::LogicalPlan;

use super::walk::recurse_transform;

/// `Limit(Skip?(Sort(Project(X))))` -> `Project(Limit(Skip?(Sort'(X))))` when the sort keys
/// can be written over `X` (aliases replaced by their expressions).
pub fn project_after_top_k(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &rewrite)
}

fn rewrite(plan: LogicalPlan) -> LogicalPlan {
    let LogicalPlan::Limit { input, count } = &plan else {
        return plan;
    };
    let (skip, sort) = match &**input {
        LogicalPlan::Skip { input, count } => (Some(count.clone()), &**input),
        other => (None, other),
    };
    let LogicalPlan::Sort {
        input: proj,
        items: sort_items,
    } = sort
    else {
        return plan;
    };
    let LogicalPlan::Project {
        input: base,
        items,
        distinct: false,
        where_clause: None,
    } = &**proj
    else {
        return plan;
    };
    if items.is_empty() {
        return plan;
    }
    let names: Vec<String> = items.iter().map(crate::executor::projection_name).collect();
    let mut keys = Vec::with_capacity(sort_items.len());
    for it in sort_items {
        let Some(expr) = substitute(&it.expr, &names, items) else {
            return plan;
        };
        keys.push(SortItem {
            expr,
            ascending: it.ascending,
        });
    }
    let mut inner = LogicalPlan::Sort {
        input: base.clone(),
        items: keys,
    };
    if let Some(s) = skip {
        inner = LogicalPlan::Skip {
            input: Box::new(inner),
            count: s,
        };
    }
    LogicalPlan::Project {
        input: Box::new(LogicalPlan::Limit {
            input: Box::new(inner),
            count: count.clone(),
        }),
        items: items.clone(),
        distinct: false,
        where_clause: None,
    }
}

/// `e` with every projected name replaced by its expression; `None` for a shape the walk
/// doesn't cover (then the rewrite is skipped).
fn substitute(e: &Expr, names: &[String], items: &[ProjectionItem]) -> Option<Expr> {
    let sub = |x: &Expr| substitute(x, names, items);
    Some(match e {
        Expr::Variable(v) => match names.iter().position(|n| n == v) {
            Some(i) => items[i].expr.clone(),
            None => e.clone(),
        },
        Expr::Literal(_) | Expr::Parameter(_) => e.clone(),
        Expr::Property(inner, key) => Expr::Property(Box::new(sub(inner)?), key.clone()),
        Expr::UnaryOp(op, inner) => Expr::UnaryOp(*op, Box::new(sub(inner)?)),
        Expr::IsNull(inner) => Expr::IsNull(Box::new(sub(inner)?)),
        Expr::IsNotNull(inner) => Expr::IsNotNull(Box::new(sub(inner)?)),
        Expr::BinaryOp(l, op, r) => Expr::BinaryOp(Box::new(sub(l)?), *op, Box::new(sub(r)?)),
        Expr::StringOp(l, op, r) => Expr::StringOp(Box::new(sub(l)?), *op, Box::new(sub(r)?)),
        Expr::In(l, r) => Expr::In(Box::new(sub(l)?), Box::new(sub(r)?)),
        Expr::ListIndex(l, r) => Expr::ListIndex(Box::new(sub(l)?), Box::new(sub(r)?)),
        Expr::Comparison(first, rest) => Expr::Comparison(
            Box::new(sub(first)?),
            rest.iter()
                .map(|(op, x)| sub(x).map(|x| (*op, x)))
                .collect::<Option<Vec<_>>>()?,
        ),
        Expr::FunctionCall {
            name,
            distinct,
            args,
        } => {
            if crate::planner::is_aggregate_function(name) {
                return None;
            }
            Expr::FunctionCall {
                name: name.clone(),
                distinct: *distinct,
                args: args.iter().map(sub).collect::<Option<Vec<_>>>()?,
            }
        }
        Expr::ListLiteral(xs) => Expr::ListLiteral(xs.iter().map(sub).collect::<Option<Vec<_>>>()?),
        _ => return None,
    })
}
