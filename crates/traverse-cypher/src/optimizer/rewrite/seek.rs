//! Anchors: anchorless expands to edge-type scans, id predicates to id seeks, label predicates
//! into the scan.

use crate::ast::{BinOp, CmpOp, Expr, Literal};
use crate::planner::LogicalPlan;

use super::walk::recurse_transform;

// -- Pass: Convert anchorless Expand+Scan to EdgeTypeScan -------------

/// Replaces `Expand{In/Out/Both} { input: Scan(_, None) }` with an `EdgeTypeScan`
/// over the edge-type index, for anchorless patterns like
/// `MATCH ()-[r:T]->() RETURN sum(r.weight)` that would otherwise scan and expand
/// every node.
///
/// Only when:
/// - the child scan has no label (a label restriction would be lost);
/// - the expand has an edge type (for "any edge" a node scan is already best);
/// - the expand is fixed-length without `prior_edge_vars` (var-length and
///   uniqueness-tracked expansions need the node-scan context);
/// - the scan variable is the expand's `from` (else the scan binds something
///   the expand doesn't consume).
pub fn convert_anchorless_expand_to_edge_type_scan(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &convert_anchorless_expand_to_edge_type_scan_inner)
}

fn convert_anchorless_expand_to_edge_type_scan_inner(plan: LogicalPlan) -> LogicalPlan {
    use traverse_core::graph::types::Direction;

    let (input, from, rel_var, to, edge_types, range, prior, dir) = match plan {
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => (
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            Direction::Outgoing,
        ),
        LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => (
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            Direction::Incoming,
        ),
        LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => (
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
            Direction::Both,
        ),
        other => return other,
    };

    // var-length, uniqueness-tracked or no edge type: bail
    if range.is_some() || !prior.is_empty() || edge_types.is_empty() {
        return reconstruct_expand(input, from, rel_var, to, edge_types, range, prior, dir);
    }

    if let LogicalPlan::Scan {
        variable: scan_var,
        label: None,
    } = input.as_ref()
        && scan_var == &from
    {
        return LogicalPlan::EdgeTypeScan {
            from_var: from,
            rel_var,
            to_var: to,
            edge_types,
            direction: dir,
        };
    }

    reconstruct_expand(input, from, rel_var, to, edge_types, range, prior, dir)
}

fn reconstruct_expand(
    input: Box<LogicalPlan>,
    from: String,
    rel_var: String,
    to: String,
    edge_types: Vec<String>,
    range: Option<(Option<u32>, Option<u32>)>,
    prior_edge_vars: Vec<String>,
    direction: traverse_core::graph::types::Direction,
) -> LogicalPlan {
    use traverse_core::graph::types::Direction;
    match direction {
        Direction::Outgoing => LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        Direction::Incoming => LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        Direction::Both => LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
    }
}

// -- Pass: Convert Filter to NodeByIdRange ----------------------------

/// `Filter(Scan{var, label}, id(var) <cmp> lit)` -> bounded node-id scan for range
/// comparisons (`<`, `<=`, `>`, `>=`). `=` is left to the NodeById seek.
///
/// `MATCH (a:User) WHERE id(a) < 100 RETURN avg(a.age)`:
///   before: Filter[id(a) < 100] -> Scan(a, :User)          (every User)
///   after:  NodeByIdRange { lower: None, upper: Some(100) } (100 ids)
pub fn convert_filter_to_node_by_id_range(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &convert_filter_to_node_by_id_range_inner)
}

fn convert_filter_to_node_by_id_range_inner(plan: LogicalPlan) -> LogicalPlan {
    if let LogicalPlan::Filter { input, predicate } = &plan
        && let LogicalPlan::Scan { variable, label } = input.as_ref()
        && let Some((lower, upper)) = extract_id_range_expr(predicate, variable)
    {
        return LogicalPlan::NodeByIdRange {
            variable: variable.clone(),
            label: label.clone(),
            lower,
            upper,
        };
    }
    plan
}

/// Recognises `id(var) <cmp> bound` (and commuted) for ranges; returns `[lower,
/// upper)` as expressions resolved at operator start, so the plan caches.
/// Literal integers and parameters only. Inclusive ends (`<=`/`>=`) wrap the
/// bound in a `+1` AST node instead of computing it, keeping the plan
/// independent of parameter values.
fn extract_id_range_expr(expr: &Expr, variable: &str) -> Option<(Option<Expr>, Option<Expr>)> {
    let Expr::Comparison(first, rest) = expr else {
        return None;
    };
    if rest.len() != 1 {
        return None;
    }
    let (op, second) = &rest[0];

    let first_ref: &Expr = first.as_ref();
    let (id_side_is_left, bound): (bool, Expr) =
        if is_id_var(first_ref, variable) && is_id_bound(second) {
            (true, second.clone())
        } else if is_id_var(second, variable) && is_id_bound(first_ref) {
            (false, first_ref.clone())
        } else {
            return None;
        };

    let effective_op = if id_side_is_left {
        *op
    } else {
        flip_cmp_op(*op)
    };
    let bound_plus_one = Expr::BinaryOp(
        Box::new(bound.clone()),
        BinOp::Add,
        Box::new(Expr::Literal(Literal::Integer(1))),
    );

    match effective_op {
        CmpOp::Lt => Some((None, Some(bound))),
        CmpOp::Lte => Some((None, Some(bound_plus_one))),
        CmpOp::Gt => Some((Some(bound_plus_one), None)),
        CmpOp::Gte => Some((Some(bound), None)),
        _ => None,
    }
}

fn is_id_var(expr: &Expr, variable: &str) -> bool {
    if let Expr::FunctionCall {
        name,
        args,
        distinct: false,
    } = expr
        && name.eq_ignore_ascii_case("id")
        && args.len() == 1
        && let Expr::Variable(v) = &args[0]
    {
        return v == variable;
    }
    false
}

fn is_id_bound(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Literal(Literal::Integer(_)) | Expr::Parameter(_)
    )
}

fn flip_cmp_op(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Lte => CmpOp::Gte,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Gte => CmpOp::Lte,
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Neq => CmpOp::Neq,
    }
}

// -- Pass: push label predicates into the Scan ------------------------

/// `MATCH (n) WHERE n:A&!B` and `MATCH (n:(A|B)&!C)` lower their label
/// expressions to `__has_label` predicates over an unlabeled Scan (a full node
/// scan). This narrows the scan from the predicate's top-level conjuncts; the
/// predicate stays, so it's recheck-safe:
/// * a positive `__has_label(var, 'L')` conjunct fills `Scan.label`;
/// * else a conjunct that's a pure OR of positive `__has_label` leaves becomes a
///   deduplicating Union of per-label scans (a node with several of the labels
///   binds once).
pub fn push_label_filters_into_scan(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &push_label_filters_into_scan_inner)
}

fn push_label_filters_into_scan_inner(plan: LogicalPlan) -> LogicalPlan {
    let LogicalPlan::Filter { input, predicate } = plan else {
        return plan;
    };
    let LogicalPlan::Scan {
        variable,
        label: None,
    } = &*input
    else {
        return LogicalPlan::Filter { input, predicate };
    };
    let variable = variable.clone();

    #[recursive::recursive]
    fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
        match e {
            Expr::BinaryOp(a, BinOp::And, b) => {
                conjuncts(a, out);
                conjuncts(b, out);
            }
            other => out.push(other),
        }
    }
    fn positive_label(e: &Expr, var: &str) -> Option<String> {
        if let Expr::FunctionCall {
            name,
            args,
            distinct: false,
        } = e
            && name == "__has_label"
            && args.len() == 2
            && let (Expr::Variable(v), Expr::Literal(Literal::String(l))) = (&args[0], &args[1])
            && v == var
        {
            return Some(l.clone());
        }
        None
    }
    #[recursive::recursive]
    fn or_labels(e: &Expr, var: &str, out: &mut Vec<String>) -> bool {
        match e {
            Expr::BinaryOp(a, BinOp::Or, b) => or_labels(a, var, out) && or_labels(b, var, out),
            other => match positive_label(other, var) {
                Some(l) => {
                    out.push(l);
                    true
                }
                None => false,
            },
        }
    }

    let mut parts = Vec::new();
    conjuncts(&predicate, &mut parts);

    // a required positive label narrows the scan directly
    if let Some(l) = parts.iter().find_map(|c| positive_label(c, &variable)) {
        return LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Scan {
                variable,
                label: Some(l),
            }),
            predicate,
        };
    }
    // else a disjunctive label conjunct becomes a dedup Union of per-label scans
    for c in &parts {
        let mut alts = Vec::new();
        if or_labels(c, &variable, &mut alts) && alts.len() >= 2 {
            alts.sort();
            alts.dedup();
            let mut scans = alts.into_iter().map(|l| LogicalPlan::Scan {
                variable: variable.clone(),
                label: Some(l),
            });
            let mut union = scans.next().expect("len >= 2");
            for s in scans {
                union = LogicalPlan::Union {
                    left: Box::new(union),
                    right: Box::new(s),
                    all: false,
                };
            }
            return LogicalPlan::Filter {
                input: Box::new(union),
                predicate,
            };
        }
    }
    LogicalPlan::Filter { input, predicate }
}

// -- Pass: Convert Filter to NodeById ---------------------------------

/// `Filter(Scan{var, label}, id(var) = lit-or-param)` -> `NodeById{var, label,
/// id_expr}` anywhere in the plan: O(1) + label check instead of a label scan.
/// Both orders (`id(var) = X`, `X = id(var)`); only literal integers or
/// parameters, since other expressions may use variables a leaf seek can't see.
pub fn convert_filter_to_node_by_id(plan: LogicalPlan) -> LogicalPlan {
    recurse_transform(plan, &convert_filter_to_node_by_id_inner)
}

fn convert_filter_to_node_by_id_inner(plan: LogicalPlan) -> LogicalPlan {
    if let LogicalPlan::Filter {
        ref input,
        ref predicate,
    } = plan
        && let LogicalPlan::Scan {
            ref variable,
            ref label,
        } = **input
        && let Some(id_expr) = extract_id_eq_const(predicate, variable)
    {
        return LogicalPlan::NodeById {
            variable: variable.clone(),
            label: label.clone(),
            id_expr,
        };
    }
    plan
}

/// `id(var) = lit` / `= $param` and commuted forms -> the constant side. Anything else (variables,
/// arithmetic, ...) -> None, keep scan + filter.
fn extract_id_eq_const(expr: &Expr, variable: &str) -> Option<Expr> {
    if let Expr::Comparison(left, pairs) = expr
        && pairs.len() == 1
        && pairs[0].0 == CmpOp::Eq
    {
        let right = &pairs[0].1;
        if let Some(c) = try_extract_id_eq(left, right, variable) {
            return Some(c);
        }
        if let Some(c) = try_extract_id_eq(right, left, variable) {
            return Some(c);
        }
    }
    None
}

fn try_extract_id_eq(id_side: &Expr, val_side: &Expr, variable: &str) -> Option<Expr> {
    if let Expr::FunctionCall {
        name,
        args,
        distinct: false,
    } = id_side
        && name.eq_ignore_ascii_case("id")
        && args.len() == 1
        && let Expr::Variable(v) = &args[0]
        && v == variable
    {
        return match val_side {
            Expr::Literal(Literal::Integer(_)) => Some(val_side.clone()),
            Expr::Parameter(_) => Some(val_side.clone()),
            _ => None,
        };
    }
    None
}
