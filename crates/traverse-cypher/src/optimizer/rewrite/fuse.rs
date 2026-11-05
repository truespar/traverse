//! Operator fusion: distinct expand chains, and scan + filter + project.

use std::collections::HashSet;

use traverse_core::graph::types::Direction;
use traverse_core::tx::view::GraphView;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::ast::{BinOp, CmpOp, Expr, Literal, ProjectionItem};
use crate::planner::{LogicalPlan, collect_expr_variables};

use super::walk::{plan_output_variables, recurse_transform};

// -- Pass: fuse distinct expand chains -------------------------------

/// A chain (a fixed one of two or more hops, or one var-length hop) becomes a `DistinctExpand`
/// under a distinct projection, or under an aggregate blind to repeated rows, that reads its
/// target and nothing of the chain's inner nodes and relationships: one dedup across every row
/// when only the target is read, each row's own distinct targets otherwise.
pub fn fuse_distinct_expand(plan: LogicalPlan) -> LogicalPlan {
    // bottom-up: the walker rebuilds the subtrees first, then looks at the node once; the
    // fusion itself never walks again
    recurse_transform(plan, &fuse_at)
}

/// The fusion at one node whose subtrees are done.
fn fuse_at(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Project {
            input,
            items,
            distinct: true,
            where_clause,
        } => {
            let mut read = HashSet::new();
            for item in &items {
                collect_top_level_vars(&item.expr, &mut read);
            }
            let mut needed = read.clone();
            if let Some(w) = &where_clause {
                collect_expr_variables(w, &mut needed);
            }
            let input = match fuse_chain(&input, &read, &needed) {
                Some(fused) => Box::new(fused),
                None => input,
            };
            LogicalPlan::Project {
                input,
                items,
                distinct: true,
                where_clause,
            }
        }
        LogicalPlan::Aggregate { input, keys, aggs }
            if !aggs.is_empty() && aggs.iter().all(|a| repeat_blind(&a.expr)) =>
        {
            let mut read = HashSet::new();
            for item in keys.iter().chain(&aggs) {
                collect_expr_variables(&item.expr, &mut read);
            }
            let Some(fused) = fuse_chain(&input, &read, &read) else {
                return LogicalPlan::Aggregate { input, keys, aggs };
            };
            // `count(DISTINCT end)` alone: every target once, counted
            if keys.is_empty()
                && aggs.len() == 1
                && let LogicalPlan::DistinctExpand {
                    to,
                    global_dedup: true,
                    ..
                } = &fused
                && distinct_count_var(&aggs[0].expr) == Some(to.as_str())
            {
                let count_all = ProjectionItem {
                    expr: Expr::FunctionCall {
                        name: "count".to_string(),
                        distinct: false,
                        args: Vec::new(),
                    },
                    alias: aggs[0].alias.clone(),
                    source_text: aggs[0].source_text.clone(),
                };
                return LogicalPlan::Aggregate {
                    input: Box::new(fused),
                    keys,
                    aggs: vec![count_all],
                };
            }
            LogicalPlan::Aggregate {
                input: Box::new(fused),
                keys,
                aggs,
            }
        }
        other => other,
    }
}

/// An aggregate the same over rows however often each repeats: a DISTINCT one (not the
/// order-dependent `apoc.agg.*`), or `min` / `max`. Anything around the call (`count(..) + 1`)
/// is not taken apart.
fn repeat_blind(expr: &Expr) -> bool {
    let Expr::FunctionCall {
        name,
        distinct,
        args,
    } = expr
    else {
        return false;
    };
    !args.is_empty()
        && match name.to_ascii_lowercase().as_str() {
            "min" | "max" => true,
            "count" | "collect" | "sum" | "avg" | "percentiledisc" | "percentilecont" | "stdev"
            | "stdevp" => *distinct,
            _ => false,
        }
}

/// Suffix of the fresh end a chain into a bound node expands to.
static FRESH_END: AtomicU64 = AtomicU64::new(0);

fn id_of(var: &str) -> Expr {
    Expr::FunctionCall {
        name: "id".to_string(),
        distinct: false,
        args: vec![Expr::Variable(var.to_string())],
    }
}

/// `plan` (through one non-label filter, which becomes the target filter) as a
/// `DistinctExpand` to one of the variables `read` above it, when it ends in a fusable chain to
/// that variable with `needed` (and the filter) bound outside the chain. One dedup across every
/// row when the target is all that is read; else each row's own distinct targets, the row
/// staying in place for the rest.
fn fuse_chain(
    plan: &LogicalPlan,
    read: &HashSet<String>,
    needed: &HashSet<String>,
) -> Option<LogicalPlan> {
    let mut candidates: Vec<&String> = read.iter().collect();
    candidates.sort();
    for target in candidates {
        let (inner, filter) = match plan {
            LogicalPlan::Filter { input, predicate }
                if extract_has_label_filter(predicate, target).is_none() =>
            {
                (input.as_ref(), Some(predicate.clone()))
            }
            other => (other, None),
        };
        let mut needed = needed.clone();
        if let Some(e) = &filter {
            collect_expr_variables(e, &mut needed);
        }
        if let Some((
            base_input,
            from,
            to,
            min_hops,
            max_hops,
            direction,
            edge_types,
            target_label,
        )) = try_extract_distinct_expand_chain(inner, target, &needed)
        {
            // an end bound below (both ends known, or a cycle back to the source): the expansion
            // binds a fresh end, the target filter keeps it to the bound one, and the row stays in
            // place as the filter reads it
            let (to, target_filter) = if plan_output_variables(&base_input).contains(&to) {
                let fresh = format!(
                    "__dx_end_{to}_{}",
                    FRESH_END.fetch_add(1, Ordering::Relaxed)
                );
                let same = Expr::Comparison(Box::new(id_of(&fresh)), vec![(CmpOp::Eq, id_of(&to))]);
                let filter = match filter {
                    Some(f) => Expr::BinaryOp(Box::new(f), BinOp::And, Box::new(same)),
                    None => same,
                };
                (fresh, Some(filter))
            } else {
                (to, filter)
            };
            return Some(LogicalPlan::DistinctExpand {
                input: Box::new(base_input),
                from,
                to,
                min_hops,
                max_hops,
                direction,
                edge_types,
                target_label,
                global_dedup: read.iter().all(|v| v == target),
                target_filter,
            });
        }
    }
    None
}

/// The variable of a bare `count(DISTINCT v)`.
fn distinct_count_var(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::FunctionCall {
            name,
            distinct: true,
            args,
        } if name.eq_ignore_ascii_case("count") => match args.as_slice() {
            [Expr::Variable(v)] => Some(v.as_str()),
            _ => None,
        },
        _ => None,
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

/// The relationships bound earlier in the pattern that an ExpandOut/In/Both must not reuse.
fn prior_edges_of(plan: &LogicalPlan) -> &[String] {
    match plan {
        LogicalPlan::ExpandOut {
            prior_edge_vars, ..
        }
        | LogicalPlan::ExpandIn {
            prior_edge_vars, ..
        }
        | LogicalPlan::ExpandBoth {
            prior_edge_vars, ..
        } => prior_edge_vars,
        _ => &[],
    }
}

/// Chain of same-direction fixed-length expands (or one var-length expand) fusable into a
/// `DistinctExpand`: `(base_input, from_var, to_var, min_hops, max_hops, direction, edge_types,
/// target_label)`.
fn try_extract_distinct_expand_chain(
    plan: &LogicalPlan,
    target_var: &str,
    needed: &HashSet<String>,
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

    // case 1: one var-length expand, with no relationship bound earlier in its pattern to keep
    // off (the distinct expansion walks without one), under TRAIL (the expansion's own mode; the
    // plain one follows the statement's)
    if let Some((base_input, from, rel_var, to, edge_types, range)) = extract_expand_fields(inner)
        && let Some((min_opt, max_opt)) = range
        && prior_edges_of(inner).is_empty()
        && crate::session::current_path_mode() == crate::ast::PathMode::Trail
        && !needed.contains(rel_var)
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
    // the chain's relationships, top hop first, and what each hop keeps off: the expansion keeps
    // a trail over the whole chain, so each must keep off every hop below it (not so when the
    // chain spans MATCH clauses or a WALK part) and nothing bound outside it
    let mut chain_rels: HashSet<&str> = HashSet::new();
    let mut hop_rels: Vec<&str> = Vec::new();
    let mut hop_priors: Vec<&[String]> = Vec::new();
    // the inner nodes, which the expansion walks through unbound: none may be bound below the
    // chain (an end bound there is kept by a target filter, see `fuse_chain`)
    let mut inner_nodes: Vec<&str> = Vec::new();

    while let Some((base_input, from, rel_var, to, edge_types, range)) =
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

        // an inner node: every hop's target but the last's
        if hops > 0 && needed.contains(to) {
            break;
        }
        hops += 1;
        first_from = Some(from);
        if hops > 1 {
            inner_nodes.push(to);
        }
        chain_rels.insert(rel_var);
        hop_rels.push(rel_var);
        hop_priors.push(prior_edges_of(current));
        current = base_input;
    }

    // at least 2 hops to be worth it
    if hops >= 2
        && let (Some(to), Some(from)) = (final_to, first_from)
        && to == target_var
        && hop_priors.iter().enumerate().all(|(i, priors)| {
            priors.iter().all(|p| chain_rels.contains(p.as_str()))
                && hop_rels[i + 1..]
                    .iter()
                    .all(|below| priors.iter().any(|p| p == below))
        })
        && !chain_rels.iter().any(|r| needed.contains(*r))
        && {
            let below = plan_output_variables(current);
            !inner_nodes.iter().any(|v| below.contains(*v))
        }
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
