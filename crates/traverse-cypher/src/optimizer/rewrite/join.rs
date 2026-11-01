//! Joins: cross products with an equality filter to hash joins or index lookup joins.

use std::collections::HashSet;

use crate::ast::{BinOp, CmpOp, Expr};
use crate::planner::{IndexHints, LogicalPlan, collect_expr_variables};

use super::walk::{expr_variables, plan_output_variables, recurse_transform};

// -- Pass: CrossProduct + equality filter -> HashJoin --------------------

/// `Filter(CrossProduct(left, right), equi-predicate)` -> `HashJoin` when one side of an equality
/// uses only left variables and the other only right ones. Other predicates stay as a Filter on
/// top.
pub fn convert_cross_to_hash_join(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &convert_cross_to_hash_join_inner)
}

fn convert_cross_to_hash_join_inner(plan: LogicalPlan) -> LogicalPlan {
    if let LogicalPlan::Filter { input, predicate } = plan {
        if let LogicalPlan::CrossProduct { left, right } = *input {
            let left_vars = plan_output_variables(&left);
            let right_vars = plan_output_variables(&right);

            let mut conjuncts = Vec::new();
            collect_conjuncts_owned(predicate, &mut conjuncts);

            let mut left_keys = Vec::new();
            let mut right_keys = Vec::new();
            let mut remaining = Vec::new();

            for conj in conjuncts {
                if let Some((lk, rk)) = try_extract_equi_join_key(&conj, &left_vars, &right_vars) {
                    left_keys.push(lk);
                    right_keys.push(rk);
                } else {
                    remaining.push(conj);
                }
            }

            if !left_keys.is_empty() {
                let mut result: LogicalPlan = LogicalPlan::HashJoin {
                    left,
                    right,
                    left_keys,
                    right_keys,
                };
                for pred in remaining {
                    result = LogicalPlan::Filter {
                        input: Box::new(result),
                        predicate: pred,
                    };
                }
                return result;
            }

            // no join keys: rebuild unchanged
            let mut result: LogicalPlan = LogicalPlan::CrossProduct { left, right };
            for pred in remaining {
                result = LogicalPlan::Filter {
                    input: Box::new(result),
                    predicate: pred,
                };
            }
            return result;
        }

        return LogicalPlan::Filter { input, predicate };
    }
    plan
}

/// Nested ANDs -> list of conjuncts (owned).
#[recursive::recursive]
fn collect_conjuncts_owned(expr: Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::BinaryOp(left, BinOp::And, right) => {
            collect_conjuncts_owned(*left, out);
            collect_conjuncts_owned(*right, out);
        }
        other => out.push(other),
    }
}

/// Equi-join key pair from `Comparison(lhs, [(Eq, rhs)])`: `(left_key, right_key)` if one side uses
/// only left-plan variables and the other only right-plan ones.
fn try_extract_equi_join_key(
    expr: &Expr,
    left_vars: &HashSet<String>,
    right_vars: &HashSet<String>,
) -> Option<(Expr, Expr)> {
    if let Expr::Comparison(lhs, pairs) = expr
        && pairs.len() == 1
        && pairs[0].0 == CmpOp::Eq
    {
        let rhs = &pairs[0].1;
        let lhs_vars = expr_variables(lhs);
        let rhs_vars = expr_variables(rhs);

        // lhs from left, rhs from right
        if !lhs_vars.is_empty()
            && !rhs_vars.is_empty()
            && lhs_vars.is_subset(left_vars)
            && rhs_vars.is_subset(right_vars)
        {
            return Some((*lhs.clone(), rhs.clone()));
        }
        // commutative: lhs from right, rhs from left
        if !lhs_vars.is_empty()
            && !rhs_vars.is_empty()
            && lhs_vars.is_subset(right_vars)
            && rhs_vars.is_subset(left_vars)
        {
            return Some((rhs.clone(), *lhs.clone()));
        }
    }
    None
}

/// `Filter(... AND var.prop = key AND ...)` over `CrossProduct(input, Scan(var:Label))`
/// -> [`LogicalPlan::IndexLookupJoin`] when `(Label, prop)` is indexed and `key`
/// comes from the DRIVING side (doesn't use `var`): the lookup join behind
/// `UNWIND $rows AS r MATCH (a:L {id: r.a})` and correlated
/// `MATCH ... MATCH (a:L) WHERE a.id = m.id + 1`, instead of an O(input x label) scan.
pub fn convert_cross_product_to_index_lookup_join(
    plan: LogicalPlan,
    hints: &IndexHints,
) -> LogicalPlan {
    recurse_transform(plan, &|p| lookup_join_inner(p, hints))
}

fn lookup_join_inner(plan: LogicalPlan, hints: &IndexHints) -> LogicalPlan {
    let LogicalPlan::Filter { input, predicate } = plan else {
        return plan;
    };
    let LogicalPlan::CrossProduct { left, right } = *input else {
        return LogicalPlan::Filter { input, predicate };
    };
    // the sought side must be a bare labeled Scan; the other drives
    let (driving, scan_var, scan_label, scan_on_right) = match (&*left, &*right) {
        (
            _,
            LogicalPlan::Scan {
                variable,
                label: Some(l),
            },
        ) => (left.clone(), variable.clone(), l.clone(), true),
        (
            LogicalPlan::Scan {
                variable,
                label: Some(l),
            },
            _,
        ) => (right.clone(), variable.clone(), l.clone(), false),
        _ => {
            return LogicalPlan::Filter {
                input: Box::new(LogicalPlan::CrossProduct { left, right }),
                predicate,
            };
        }
    };
    let driving_vars = plan_output_variables(&driving);

    // split the AND chain; find a usable `var.prop = key`
    let mut conjuncts = Vec::new();
    split_and_chain(&predicate, &mut conjuncts);
    let mut chosen: Option<(usize, String, Expr)> = None;
    for (i, c) in conjuncts.iter().enumerate() {
        if let Some((prop, key)) = extract_eq_property_check_any(c, &scan_var) {
            if !hints.contains(&(scan_label.clone(), prop.clone())) {
                continue;
            }
            let mut key_vars = HashSet::new();
            collect_expr_variables(&key, &mut key_vars);
            // key must come from the driving side alone
            if key_vars.contains(&scan_var) || !key_vars.is_subset(&driving_vars) {
                continue;
            }
            // static keys are covered by convert_filter_to_index_scan + join ordering; only
            // correlated keys here
            if key_vars.is_empty() {
                continue;
            }
            chosen = Some((i, prop, key));
            break;
        }
    }
    let Some((idx, prop, key)) = chosen else {
        return LogicalPlan::Filter {
            input: Box::new(LogicalPlan::CrossProduct { left, right }),
            predicate,
        };
    };
    let _ = scan_on_right;
    let mut out = LogicalPlan::IndexLookupJoin {
        input: driving,
        variable: scan_var,
        label: scan_label,
        property: prop,
        key,
    };
    // re-apply the other conjuncts; the chosen one is dropped since the seek enforces exactly
    // `var.prop = key`
    let rest: Vec<Expr> = conjuncts
        .into_iter()
        .enumerate()
        .filter_map(|(i, c)| (i != idx).then_some(c))
        .collect();
    if let Some(pred) = rest
        .into_iter()
        .reduce(|a, b| Expr::BinaryOp(Box::new(a), BinOp::And, Box::new(b)))
    {
        out = LogicalPlan::Filter {
            input: Box::new(out),
            predicate: pred,
        };
    }
    out
}

#[recursive::recursive]
pub(super) fn split_and_chain(e: &Expr, out: &mut Vec<Expr>) {
    if let Expr::BinaryOp(l, BinOp::And, r) = e {
        split_and_chain(l, out);
        split_and_chain(r, out);
    } else {
        out.push(e.clone());
    }
}

/// `extract_eq_property_check` WITHOUT the literal/parameter gate: the key may be any expression
/// (lookup joins evaluate it per row).
pub(super) fn extract_eq_property_check_any(expr: &Expr, variable: &str) -> Option<(String, Expr)> {
    let Expr::Comparison(left, pairs) = expr else {
        return None;
    };
    if pairs.len() != 1 || pairs[0].0 != CmpOp::Eq {
        return None;
    }
    let right = &pairs[0].1;
    for (prop_side, val_side) in [(&**left, right), (right, &**left)] {
        if let Expr::Property(base, key) = prop_side
            && let Expr::Variable(v) = base.as_ref()
            && v == variable
        {
            return Some((key.clone(), val_side.clone()));
        }
    }
    None
}
