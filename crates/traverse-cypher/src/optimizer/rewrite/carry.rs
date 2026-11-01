//! Scalar-carry aggregates: a grouped aggregate over a one-row cross product becomes an
//! ungrouped one on the right side.

use std::collections::HashSet;

use crate::ast::{Expr, ProjectionItem};
use crate::planner::LogicalPlan;

use super::walk::{expr_variables, recurse_transform};

/// Lifts a grouped aggregate over a one-row cross product back into an ungrouped
/// aggregate on the right side.
///
/// `WITH count(...) AS a MATCH ... RETURN a, count(...)` plans as
/// `Aggregate{keys: [a]} over CrossProduct{left: 1-row, right}`: the keys are
/// constants from the one-row left side, yet every right row gets hashed for a
/// single group. `CrossProduct{left, Aggregate{keys: []}(right)}` brings back the
/// ungrouped count fast paths (label-index / edge-type-index counts).
pub fn lift_scalar_carry_aggregate(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &lift_scalar_carry_aggregate_inner)
}

/// Output columns of a plan that provably yields exactly one row; `None` if not provable.
#[recursive::recursive]
fn one_row_outputs(plan: &LogicalPlan) -> Option<HashSet<String>> {
    match plan {
        // an ungrouped aggregate always emits one row
        LogicalPlan::Aggregate { keys, aggs, .. } if keys.is_empty() => {
            Some(aggs.iter().map(crate::executor::projection_name).collect())
        }
        LogicalPlan::CrossProduct { left, right } => {
            let mut l = one_row_outputs(left)?;
            l.extend(one_row_outputs(right)?);
            Some(l)
        }
        _ => None,
    }
}

fn lift_scalar_carry_aggregate_inner(plan: LogicalPlan) -> LogicalPlan {
    let LogicalPlan::Aggregate { input, keys, aggs } = plan else {
        return plan;
    };
    if keys.is_empty() {
        return LogicalPlan::Aggregate { input, keys, aggs };
    }
    let LogicalPlan::CrossProduct { left, right } = *input else {
        return LogicalPlan::Aggregate { input, keys, aggs };
    };
    let lift = (|| {
        let left_cols = one_row_outputs(&left)?;
        // each key must be a bare passthrough of a left column (same name) and each aggregate may
        // only use variables the left side does NOT provide, so grouping by the keys is grouping by
        // constants
        for k in &keys {
            let Expr::Variable(v) = &k.expr else {
                return None;
            };
            if !left_cols.contains(v) {
                return None;
            }
            if let Some(alias) = &k.alias
                && alias != v
            {
                return None;
            }
        }
        for a in &aggs {
            if expr_variables(&a.expr)
                .iter()
                .any(|v| left_cols.contains(v))
            {
                return None;
            }
        }
        Some(())
    })();
    match lift {
        Some(()) => {
            // keep the Aggregate's output contract (column names, keys-then-aggs order):
            // downstream, incl. the executor's root columns, reads the shape from the plan and a
            // bare CrossProduct declares none. Reference the computed columns by name;
            // re-evaluating the aggregates here would be wrong
            let items: Vec<ProjectionItem> = keys
                .iter()
                .chain(aggs.iter())
                .map(|item| {
                    let name = crate::executor::projection_name(item);
                    ProjectionItem {
                        expr: Expr::Variable(name.clone()),
                        alias: Some(name),
                        source_text: item.source_text.clone(),
                    }
                })
                .collect();
            LogicalPlan::Project {
                input: Box::new(LogicalPlan::CrossProduct {
                    left,
                    right: Box::new(LogicalPlan::Aggregate {
                        input: right,
                        keys: vec![],
                        aggs,
                    }),
                }),
                items,
                distinct: false,
                where_clause: None,
            }
        }
        None => LogicalPlan::Aggregate {
            input: Box::new(LogicalPlan::CrossProduct { left, right }),
            keys,
            aggs,
        },
    }
}
