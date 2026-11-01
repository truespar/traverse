//! Operator fusion: distinct expand chains, and scan + filter + project.

use std::collections::HashSet;

use traverse_core::graph::types::Direction;
use traverse_core::tx::view::GraphView;

use crate::ast::{BinOp, Expr, Literal};
use crate::planner::{LogicalPlan, collect_expr_variables};

use super::walk::recurse_transform;

// -- Pass: fuse distinct expand chains -------------------------------

/// `Project { distinct: true } -> [Filter(label)] -> Expand* chain` (or a single var-length
/// `Expand*`) under a distinct projection -> `DistinctExpand`.
#[recursive::recursive]
pub fn fuse_distinct_expand(plan: LogicalPlan) -> LogicalPlan {
    // top-down: match at the Project only
    match plan {
        LogicalPlan::Project {
            input,
            items,
            distinct: true,
            where_clause,
        } => {
            // projection items must use ONLY the final target variable (`n.id`, `n`, `n.name`)
            let proj_vars = {
                let mut vars = HashSet::new();
                for item in &items {
                    collect_top_level_vars(&item.expr, &mut vars);
                }
                vars
            };
            // exactly one variable
            if proj_vars.len() != 1 {
                return LogicalPlan::Project {
                    input: Box::new(fuse_distinct_expand(*input)),
                    items,
                    distinct: true,
                    where_clause,
                };
            }
            let target_var = proj_vars.into_iter().next().unwrap();

            // expand chain from the input; look through a non-label Filter child (e.g. WHERE
            // n.age >= 18) and keep it above the DistinctExpand
            let inner = *input;
            let (effective_inner, outer_filter) = match &inner {
                LogicalPlan::Filter {
                    input: filter_input,
                    predicate,
                } => {
                    if extract_has_label_filter(predicate, &target_var).is_some() {
                        // label filter: try_extract_distinct_expand_chain handles it
                        (&inner, None)
                    } else {
                        // non-label filter: look through, wrap later
                        (filter_input.as_ref(), Some(predicate.clone()))
                    }
                }
                _ => (&inner, None),
            };

            if let Some((
                base_input,
                from_var,
                to_var,
                min_hops,
                max_hops,
                direction,
                edge_types,
                target_label,
            )) = try_extract_distinct_expand_chain(effective_inner, &target_var)
                && to_var == target_var
            {
                let fused = LogicalPlan::DistinctExpand {
                    input: Box::new(fuse_distinct_expand(base_input)),
                    from: from_var,
                    to: to_var,
                    min_hops,
                    max_hops,
                    direction,
                    edge_types,
                    target_label,
                    global_dedup: true,
                    target_filter: outer_filter,
                };
                return LogicalPlan::Project {
                    input: Box::new(fused),
                    items,
                    distinct: true,
                    where_clause,
                };
            }
            // can't fuse: recurse into children
            LogicalPlan::Project {
                input: Box::new(fuse_distinct_expand(inner)),
                items,
                distinct: true,
                where_clause,
            }
        }
        // recurse into other nodes
        other => recurse_transform(other, &|p| {
            if let LogicalPlan::Project {
                input,
                items,
                distinct: true,
                where_clause,
            } = p
            {
                fuse_distinct_expand(LogicalPlan::Project {
                    input,
                    items,
                    distinct: true,
                    where_clause,
                })
            } else {
                p
            }
        }),
    }
}

// -- Pass: fuse scan + filter + project ------------------------------

/// `Project(Filter(Scan))` / `Project(Scan)` -> one `ScanFilterProject` (no intermediate
/// allocations); also absorbs a `Limit` on top.
pub fn fuse_scan_filter_project(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    recurse_transform(plan, &|p| fuse_scan_filter_project_inner(p, graph))
}

/// Min scan cardinality for SFP fusion; below it batch overhead beats the gain over the pull path.
const SFP_MIN_CARDINALITY: usize = 50;

fn fuse_scan_filter_project_inner(plan: LogicalPlan, graph: &dyn GraphView) -> LogicalPlan {
    // Limit -> ScanFilterProject: absorb the limit
    if let LogicalPlan::Limit {
        ref input,
        ref count,
    } = plan
        && let LogicalPlan::ScanFilterProject {
            ref variable,
            ref label,
            ref predicate,
            ref items,
            distinct,
            limit: None,
        } = **input
        && !distinct
    {
        // DISTINCT must see every row first
        if let Expr::Literal(Literal::Integer(n)) = count
            && *n > 0
        {
            return LogicalPlan::ScanFilterProject {
                variable: variable.clone(),
                label: label.clone(),
                predicate: predicate.clone(),
                items: items.clone(),
                distinct,
                limit: Some(*n as usize),
            };
        }
    }

    if let LogicalPlan::Project {
        ref input,
        ref items,
        distinct,
        where_clause: None,
    } = plan
    {
        if items.is_empty() {
            return plan; // RETURN * needs dynamic column discovery
        }

        // pattern 1: Project -> Filter -> Scan
        if let LogicalPlan::Filter {
            input: ref filter_input,
            ref predicate,
        } = **input
            && let LogicalPlan::Scan {
                ref variable,
                ref label,
            } = **filter_input
            && estimate_scan_card(label.as_deref(), graph) >= SFP_MIN_CARDINALITY
        {
            return LogicalPlan::ScanFilterProject {
                variable: variable.clone(),
                label: label.clone(),
                predicate: Some(predicate.clone()),
                items: items.clone(),
                distinct,
                limit: None,
            };
        }
        // pattern 2: Project -> Scan (no filter)
        if let LogicalPlan::Scan {
            ref variable,
            ref label,
        } = **input
            && estimate_scan_card(label.as_deref(), graph) >= SFP_MIN_CARDINALITY
        {
            return LogicalPlan::ScanFilterProject {
                variable: variable.clone(),
                label: label.clone(),
                predicate: None,
                items: items.clone(),
                distinct,
                limit: None,
            };
        }
    }
    plan
}

/// Scan cardinality estimate for the gate.
fn estimate_scan_card(label: Option<&str>, graph: &dyn GraphView) -> usize {
    match label {
        None => graph.node_count(),
        Some(l) => {
            if let Some(stats) = graph.statistics()
                && let Some(ls) = stats.label_stats.get(l)
            {
                return ls.count as usize;
            }
            // no stats: assume big enough for SFP fusion (nodes_by_label().len() is O(N), too
            // costly while planning)
            graph.node_count()
        }
    }
}

/// Top-level variables of an expression (base variable of property accesses and bare variables).
#[recursive::recursive]
fn collect_top_level_vars(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Variable(name) => {
            out.insert(name.clone());
        }
        Expr::Property(base, _) => collect_top_level_vars(base, out),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_top_level_vars(a, out);
            }
        }
        _ => {
            // complex expressions: all variables, conservatively
            collect_expr_variables(expr, out);
        }
    }
}

/// AST RelDirection -> core Direction.
fn rel_direction_to_direction(expand: &LogicalPlan) -> Option<Direction> {
    match expand {
        LogicalPlan::ExpandOut { .. } => Some(Direction::Outgoing),
        LogicalPlan::ExpandIn { .. } => Some(Direction::Incoming),
        LogicalPlan::ExpandBoth { .. } => Some(Direction::Both),
        _ => None,
    }
}

/// Expand fields of any ExpandOut/In/Both.
fn extract_expand_fields(
    plan: &LogicalPlan,
) -> Option<(
    &LogicalPlan,
    &str,
    &str,
    &str,
    &[String],
    &Option<(Option<u32>, Option<u32>)>,
)> {
    match plan {
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            ..
        }
        | LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            ..
        }
        | LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            ..
        } => Some((
            input.as_ref(),
            from,
            rel_var,
            to,
            edge_types.as_slice(),
            range,
        )),
        _ => None,
    }
}

/// Chain of same-direction fixed-length expands (or one var-length expand) fusable into a
/// `DistinctExpand`: `(base_input, from_var, to_var, min_hops, max_hops, direction, edge_types,
/// target_label)`.
fn try_extract_distinct_expand_chain(
    plan: &LogicalPlan,
    target_var: &str,
) -> Option<(
    LogicalPlan,
    String,
    String,
    usize,
    usize,
    Direction,
    Vec<String>,
    Option<String>,
)> {
    // peel an optional label filter on the target
    let (inner, target_label) = match plan {
        LogicalPlan::Filter { input, predicate } => {
            if let Some(label) = extract_has_label_filter(predicate, target_var) {
                (input.as_ref(), Some(label))
            } else {
                (plan, None)
            }
        }
        _ => (plan, None),
    };

    // case 1: one var-length expand
    if let Some((base_input, from, _rel_var, to, edge_types, range)) = extract_expand_fields(inner)
        && let Some((min_opt, max_opt)) = range
    {
        let direction = rel_direction_to_direction(inner)?;
        let min = min_opt.unwrap_or(1) as usize;
        let max = max_opt.unwrap_or(30) as usize;
        if to == target_var {
            return Some((
                base_input.clone(),
                from.to_string(),
                to.to_string(),
                min,
                max,
                direction,
                edge_types.to_vec(),
                target_label,
            ));
        }
    }

    // case 2: chain of fixed-length expands; walk down collecting hops
    let mut current = inner;
    let mut hops = 0usize;
    let mut chain_direction: Option<Direction> = None;
    let mut chain_edge_types: Option<&[String]> = None;
    let mut final_to: Option<&str> = None;
    let mut first_from: Option<&str> = None;

    while let Some((base_input, from, _rel_var, to, edge_types, range)) =
        extract_expand_fields(current)
    {
        // fixed length only
        if range.is_some() {
            break;
        }
        let direction = rel_direction_to_direction(current)?;

        // first expand (topmost = last hop)
        if hops == 0 {
            final_to = Some(to);
            chain_direction = Some(direction);
            chain_edge_types = Some(edge_types);
        } else if Some(direction) != chain_direction || chain_edge_types != Some(edge_types) {
            // same direction and edge types throughout
            break;
        }

        hops += 1;
        first_from = Some(from);
        current = base_input;
    }

    // at least 2 hops to be worth it
    if hops >= 2
        && let (Some(to), Some(from)) = (final_to, first_from)
        && to == target_var
    {
        return Some((
            current.clone(),
            from.to_string(),
            to.to_string(),
            hops,
            hops,
            chain_direction.unwrap(),
            chain_edge_types.unwrap().to_vec(),
            target_label,
        ));
    }

    None
}

/// Splits AND expressions into conjuncts, recursively.
#[recursive::recursive]
pub(super) fn split_and_conjuncts(expr: &Expr, out: &mut Vec<Expr>) {
    if let Expr::BinaryOp(left, BinOp::And, right) = expr {
        split_and_conjuncts(left, out);
        split_and_conjuncts(right, out);
    } else {
        out.push(expr.clone());
    }
}

/// Label of a `__has_label(target_var, "Label")` filter predicate.
pub(super) fn extract_has_label_filter(predicate: &Expr, target_var: &str) -> Option<String> {
    if let Expr::FunctionCall { name, args, .. } = predicate
        && name == "__has_label"
        && args.len() == 2
        && let (Expr::Variable(var), Expr::Literal(Literal::String(label))) = (&args[0], &args[1])
        && var == target_var
    {
        return Some(label.clone());
    }
    None
}
