//! Index scans: property equality and ranges, text (trigram) and point (spatial) predicates.

use traverse_core::tx::view::GraphView;

use crate::ast::{CmpOp, Expr};
use crate::planner::{
    IndexHints, LogicalPlan, SpatialQuery, TextPredicateKind, is_indexable_value,
};

use super::walk::{expr_uses_var, recurse_transform};

// -- Pass: Filter -> IndexScan ------------------------------------------

/// `Filter(Scan(:L), prop = val)` -> `IndexScan` when an index exists.
pub fn convert_filter_to_index_scan(plan: LogicalPlan, hints: &IndexHints) -> LogicalPlan {
    recurse_transform(plan, &|p| convert_filter_to_index_scan_inner(p, hints))
}

fn convert_filter_to_index_scan_inner(plan: LogicalPlan, hints: &IndexHints) -> LogicalPlan {
    if let LogicalPlan::Filter {
        ref input,
        ref predicate,
    } = plan
        && let LogicalPlan::Scan {
            ref variable,
            label: Some(ref label),
        } = **input
    {
        // `var.prop = val` or `val = var.prop`?
        if let Some((prop, value)) = extract_eq_property_check(predicate, variable)
            && hints.contains(&(label.clone(), prop.clone()))
        {
            return LogicalPlan::IndexScan {
                variable: variable.clone(),
                label: label.clone(),
                property: prop,
                value,
            };
        }
        // composite index: an equality conjunction covering EVERY component of a composite hint
        // (label, "a\u{1F}b") seeds an IndexScan with `__composite_key(v_a, v_b)`; the ORIGINAL
        // Filter stays on top (the seek only prunes), so results are identical, like the TEXT
        // rewrite
        {
            let eqs = collect_eq_conjuncts(predicate, variable);
            if eqs.len() >= 2 {
                for (hl, hp) in hints.iter() {
                    if hl != label || !hp.contains('\u{1F}') {
                        continue;
                    }
                    let comps: Vec<&str> = hp.split('\u{1F}').collect();
                    let values: Option<Vec<Expr>> =
                        comps.iter().map(|c| eqs.get(*c).cloned()).collect();
                    if let Some(values) = values {
                        return LogicalPlan::Filter {
                            input: Box::new(LogicalPlan::IndexScan {
                                variable: variable.clone(),
                                label: label.clone(),
                                property: hp.clone(),
                                value: Expr::FunctionCall {
                                    name: "__composite_key".to_string(),
                                    distinct: false,
                                    args: values,
                                },
                            }),
                            predicate: predicate.clone(),
                        };
                    }
                }
            }
        }
        // fallback: range predicate (>, >=, <, <=)
        if let Some((prop, op, value)) = extract_range_property_check(predicate, variable)
            && hints.contains(&(label.clone(), prop.clone()))
        {
            return LogicalPlan::IndexRangeScan {
                variable: variable.clone(),
                label: label.clone(),
                property: prop,
                op,
                value,
            };
        }
    }
    plan
}

/// Top-level `var.prop = <indexable>` conjuncts of an AND tree -> prop -> value (first wins).
fn collect_eq_conjuncts(
    predicate: &Expr,
    variable: &str,
) -> std::collections::HashMap<String, Expr> {
    #[recursive::recursive]
    fn walk(e: &Expr, var: &str, out: &mut std::collections::HashMap<String, Expr>) {
        if let Expr::BinaryOp(l, crate::ast::BinOp::And, r) = e {
            walk(l, var, out);
            walk(r, var, out);
            return;
        }
        if let Some((prop, value)) = extract_eq_property_check(e, var) {
            out.entry(prop).or_insert(value);
        }
    }
    let mut out = std::collections::HashMap::new();
    walk(predicate, variable, &mut out);
    out
}

// -- Text (trigram) index acceleration --------------------------------
//
// String predicates on a TEXT-indexed property go to a trigram `TextIndexScan`,
// the original `Filter` staying on top so results match the scan path (the
// index only prunes):
//
//   WHERE n.p CONTAINS $s      WHERE n.p ENDS WITH $s
//   WHERE n.p STARTS WITH $s   WHERE n.p = $s   (string equality)

/// Rewrites string-predicate `Filter`s to use a text index covering the `(label, property)`.
pub fn convert_text_predicates(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    recurse_transform(plan, &|p| convert_text_predicates_inner(p, graph))
}

fn convert_text_predicates_inner(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    let LogicalPlan::Filter { input, predicate } = &plan else {
        return plan;
    };
    let LogicalPlan::Scan {
        variable,
        label: Some(label),
    } = &**input
    else {
        return plan;
    };

    let matched: Option<(String, TextPredicateKind, Expr)> = match predicate {
        // n.p CONTAINS / STARTS WITH / ENDS WITH s (regex isn't indexable)
        Expr::StringOp(lhs, op, rhs) => {
            let kind = match op {
                crate::ast::StringOp::Contains => TextPredicateKind::Contains,
                crate::ast::StringOp::StartsWith => TextPredicateKind::StartsWith,
                crate::ast::StringOp::EndsWith => TextPredicateKind::EndsWith,
                crate::ast::StringOp::Regex => return plan,
            };
            match property_of_var(lhs, variable) {
                Some(prop) if !expr_uses_var(rhs, variable) => Some((prop, kind, (**rhs).clone())),
                _ => None,
            }
        }
        // n.p = s / s = n.p: only when the b-tree pass didn't take it (no b-tree index on (label,
        // prop))
        Expr::Comparison(lhs, pairs) if pairs.len() == 1 && pairs[0].0 == CmpOp::Eq => {
            let rhs = &pairs[0].1;
            if let Some(prop) = property_of_var(lhs, variable) {
                (!expr_uses_var(rhs, variable))
                    .then(|| (prop, TextPredicateKind::Equals, rhs.clone()))
            } else if let Some(prop) = property_of_var(rhs, variable) {
                (!expr_uses_var(lhs, variable))
                    .then(|| (prop, TextPredicateKind::Equals, (**lhs).clone()))
            } else {
                None
            }
        }
        _ => None,
    };

    let Some((property, kind, pattern)) = matched else {
        return plan;
    };
    if !graph.has_text_index_for(label, &property) {
        return plan;
    }
    LogicalPlan::Filter {
        input: Box::new(LogicalPlan::TextIndexScan {
            variable: variable.clone(),
            label: label.clone(),
            property,
            kind,
            pattern,
        }),
        predicate: predicate.clone(),
    }
}

/// `prop` if `e` is `variable.prop`.
fn property_of_var(e: &Expr, variable: &str) -> Option<String> {
    if let Expr::Property(base, key) = e
        && let Expr::Variable(v) = base.as_ref()
        && v == variable
    {
        return Some(key.clone());
    }
    None
}

// -- Spatial (point) index acceleration -------------------------------
//
// Three shapes on a POINT-indexed property go to an R-tree `PointIndexScan`, the
// original `Filter`/`Sort`/`Limit` staying on top so results match the scan
// path (the index only prunes):
//
//   WHERE point.distance(n.loc, $p) < r
//   WHERE point.withinBBox(n.loc, $ll, $ur)
//   ORDER BY point.distance(n.loc, $p) LIMIT k

/// Rewrites spatial `Filter`s and kNN `Limit(Sort(Scan))` shapes to use a point index covering the
/// `(label, property)`.
pub fn convert_spatial_predicates(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    recurse_transform(plan, &|p| convert_spatial_predicates_inner(p, graph))
}

fn convert_spatial_predicates_inner(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    // shapes 1 & 2: Filter(Scan, point.distance/withinBBox predicate)
    if let LogicalPlan::Filter { input, predicate } = &plan
        && let LogicalPlan::Scan {
            variable,
            label: Some(label),
        } = &**input
    {
        if let Some((property, center, radius)) = match_distance_pred(predicate, variable)
            && graph.has_point_index_for(label, &property)
        {
            return wrap_point_scan(
                variable,
                label,
                property,
                SpatialQuery::Distance { center, radius },
                predicate.clone(),
            );
        }
        if let Some((property, ll, ur)) = match_bbox_pred(predicate, variable)
            && graph.has_point_index_for(label, &property)
        {
            return wrap_point_scan(
                variable,
                label,
                property,
                SpatialQuery::BBox { ll, ur },
                predicate.clone(),
            );
        }
    }

    // shape 3: Limit(Sort(Scan, [point.distance(...) ASC]), k) -> kNN
    if let LogicalPlan::Limit { input, count } = &plan
        && let LogicalPlan::Sort {
            input: sort_input,
            items,
        } = &**input
        && items.len() == 1
        && items[0].ascending
        && let LogicalPlan::Scan {
            variable,
            label: Some(label),
        } = &**sort_input
        && let Some((property, center)) = match_point_distance(&items[0].expr, variable)
        && graph.has_point_index_for(label, &property)
    {
        let scan = LogicalPlan::PointIndexScan {
            variable: variable.clone(),
            label: label.clone(),
            property,
            query: SpatialQuery::Knn {
                center,
                k: count.clone(),
            },
        };
        return LogicalPlan::Limit {
            input: Box::new(LogicalPlan::Sort {
                input: Box::new(scan),
                items: items.clone(),
            }),
            count: count.clone(),
        };
    }

    plan
}

/// `Filter(PointIndexScan, predicate)`: the filter enforces the exact predicate on the pruned
/// candidates.
fn wrap_point_scan(
    variable: &str,
    label: &str,
    property: String,
    query: SpatialQuery,
    predicate: Expr,
) -> LogicalPlan {
    LogicalPlan::Filter {
        input: Box::new(LogicalPlan::PointIndexScan {
            variable: variable.to_string(),
            label: label.to_string(),
            property,
            query,
        }),
        predicate,
    }
}

/// `point.distance(var.prop, C)` in either argument order -> `(property, center)`; the centre must
/// not use the scan variable.
fn match_point_distance(expr: &Expr, variable: &str) -> Option<(String, Expr)> {
    let Expr::FunctionCall { name, args, .. } = expr else {
        return None;
    };
    if name != "point.distance" || args.len() != 2 {
        return None;
    }
    if let Some(prop) = property_of(&args[0], variable)
        && !expr_uses_var(&args[1], variable)
    {
        return Some((prop, args[1].clone()));
    }
    if let Some(prop) = property_of(&args[1], variable)
        && !expr_uses_var(&args[0], variable)
    {
        return Some((prop, args[0].clone()));
    }
    None
}

/// Within-radius predicate (`point.distance(var.prop, C) < r`, `<=`, or commuted `r >
/// point.distance(...)`) -> `(property, center, radius)`. Only `<`/`<=` on the distance side bounds
/// a region.
fn match_distance_pred(predicate: &Expr, variable: &str) -> Option<(String, Expr, Expr)> {
    let Expr::Comparison(left, pairs) = predicate else {
        return None;
    };
    if pairs.len() != 1 {
        return None;
    }
    let (op, right) = (&pairs[0].0, &pairs[0].1);
    // distance(...) < r / <= r
    if matches!(op, CmpOp::Lt | CmpOp::Lte)
        && let Some((prop, center)) = match_point_distance(left, variable)
        && !expr_uses_var(right, variable)
    {
        return Some((prop, center, right.clone()));
    }
    // r > distance(...) / r >= distance(...) <=> distance < r / <= r
    if matches!(op, CmpOp::Gt | CmpOp::Gte)
        && let Some((prop, center)) = match_point_distance(right, variable)
        && !expr_uses_var(left, variable)
    {
        return Some((prop, center, (**left).clone()));
    }
    None
}

/// `point.withinBBox(var.prop, ll, ur)` as a boolean predicate -> `(property, ll, ur)`.
fn match_bbox_pred(predicate: &Expr, variable: &str) -> Option<(String, Expr, Expr)> {
    let Expr::FunctionCall { name, args, .. } = predicate else {
        return None;
    };
    if name != "point.withinbbox" || args.len() != 3 {
        return None;
    }
    let prop = property_of(&args[0], variable)?;
    if expr_uses_var(&args[1], variable) || expr_uses_var(&args[2], variable) {
        return None;
    }
    Some((prop, args[1].clone(), args[2].clone()))
}

/// `prop` if `e` is `variable.prop`.
fn property_of(e: &Expr, variable: &str) -> Option<String> {
    if let Expr::Property(base, key) = e
        && let Expr::Variable(v) = base.as_ref()
        && v == variable
    {
        return Some(key.clone());
    }
    None
}

/// `(property, value)` from `var.prop = val` or `val = var.prop`.
fn extract_eq_property_check(expr: &Expr, variable: &str) -> Option<(String, Expr)> {
    if let Expr::Comparison(left, pairs) = expr
        && pairs.len() == 1
        && pairs[0].0 == CmpOp::Eq
    {
        // left = right
        let right = &pairs[0].1;
        // left is var.prop, right indexable
        if let Some((prop, val)) = try_extract_prop_eq(left, right, variable) {
            return Some((prop, val));
        }
        // right is var.prop, left indexable (commutative)
        if let Some((prop, val)) = try_extract_prop_eq(right, left, variable) {
            return Some((prop, val));
        }
    }
    None
}

fn try_extract_prop_eq(
    prop_side: &Expr,
    val_side: &Expr,
    variable: &str,
) -> Option<(String, Expr)> {
    if let Expr::Property(base, key) = prop_side
        && let Expr::Variable(var_name) = base.as_ref()
        && var_name == variable
        && is_indexable_value(val_side)
    {
        return Some((key.clone(), val_side.clone()));
    }
    None
}

/// `(property, op, value)` from range predicates like `var.prop > val`, `val < var.prop`.
fn extract_range_property_check(expr: &Expr, variable: &str) -> Option<(String, CmpOp, Expr)> {
    if let Expr::Comparison(left, pairs) = expr
        && pairs.len() == 1
    {
        let op = pairs[0].0;
        let right = &pairs[0].1;
        match op {
            CmpOp::Gt | CmpOp::Gte | CmpOp::Lt | CmpOp::Lte => {
                // left op right -> var.prop op val
                if let Some((prop, val)) = try_extract_prop_eq(left, right, variable) {
                    return Some((prop, op, val));
                }
                // commuted: val op var.prop -> var.prop (flipped op) val
                if let Some((prop, val)) = try_extract_prop_eq(right, left, variable) {
                    let flipped = match op {
                        CmpOp::Gt => CmpOp::Lt,
                        CmpOp::Gte => CmpOp::Lte,
                        CmpOp::Lt => CmpOp::Gt,
                        CmpOp::Lte => CmpOp::Gte,
                        _ => unreachable!(),
                    };
                    return Some((prop, flipped, val));
                }
            }
            _ => {}
        }
    }
    None
}
