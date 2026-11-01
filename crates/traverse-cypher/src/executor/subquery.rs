//! EXISTS, COUNT and COLLECT subqueries inside expressions.

use traverse_core::graph::types::NodeId;
use traverse_core::tx::view::GraphView;

use crate::ast::{Expr, Literal, MatchClause, SingleQuery};
use crate::error::CypherError;
use crate::frame::{Frame, SlotValue, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Record;
use super::context::{get_query_param, has_query_params};
use super::describe::plan_columns;
use super::interp::exec_subquery;
use super::plan_cache::{cached_plan, subquery_fingerprint};

pub(super) fn eval_full_subquery_exists(
    subquery: &SingleQuery,
    outer_rec: &Record,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    // plan the subquery as a mini-query seeded with outer vars and run the optimizer, so folding /
    // id-seek / push-down apply as at top level (otherwise `EXISTS { MATCH (n) WHERE 1 = 0 RETURN n
    // }` scans every node first)
    let plan = full_subquery_plan(subquery, graph)?;
    let records = exec_subquery(&plan, outer_rec, graph)?;
    Ok(CypherValue::Bool(!records.is_empty()))
}

/// `COLLECT { ... }`: LIST of the body's single-column RETURN values, in order.
/// The body must project exactly one column (Neo4j); zero rows -> empty list, not
/// NULL; more columns -> runtime type error.
pub(super) fn eval_full_subquery_collect(
    subquery: &SingleQuery,
    outer_rec: &Record,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    let plan = full_subquery_plan(subquery, graph)?;
    let records = exec_subquery(&plan, outer_rec, graph)?;
    let columns = plan_columns(&plan);

    // the inner RETURN's column name doesn't matter: take the single visible field per record
    // (`frame_to_record` already drops hidden/anonymous slots)
    if columns.len() > 1 {
        return Err(CypherError::type_error(format!(
            "COLLECT subquery body must project exactly one column, got {}",
            columns.len()
        )));
    }
    let col = columns.first().cloned();

    let mut out: Vec<CypherValue> = Vec::with_capacity(records.len());
    for rec in records {
        let v = match &col {
            Some(name) => rec.get(name).cloned().unwrap_or(CypherValue::Null),
            None => {
                // no column name resolved (alias is the source text): take the first non-outer
                // value
                let outer_keys: std::collections::HashSet<&String> = outer_rec.keys().collect();
                rec.iter()
                    .find(|(k, _)| !outer_keys.contains(k))
                    .map(|(_, v)| v.clone())
                    .unwrap_or(CypherValue::Null)
            }
        };
        out.push(v);
    }
    Ok(CypherValue::List(out))
}

pub(super) fn eval_subquery_exists(
    match_clause: &MatchClause,
    outer_rec: &Record,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    let plan = subquery_plan(match_clause, graph)?;
    let records = exec_subquery(&plan, outer_rec, graph)?;
    Ok(CypherValue::Bool(!records.is_empty()))
}

pub(super) fn eval_subquery_count(
    match_clause: &MatchClause,
    outer_rec: &Record,
    graph: &dyn GraphView,
) -> Result<CypherValue, CypherError> {
    let plan = subquery_plan(match_clause, graph)?;
    let records = exec_subquery(&plan, outer_rec, graph)?;
    Ok(CypherValue::Integer(records.len() as i64))
}

/// Plan for a pattern-form EXISTS / COUNT subquery, planned and optimized once per execution.
fn subquery_plan(
    match_clause: &MatchClause,
    graph: &dyn GraphView,
) -> Result<LogicalPlan, CypherError> {
    cached_plan(
        match_clause as *const MatchClause as usize,
        || subquery_fingerprint(&format!("{match_clause:?}"), 1),
        || {
            let mut plan = crate::planner::plan_match_pattern_pub(&match_clause.pattern)?;
            if let Some(ref where_expr) = match_clause.where_clause {
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: where_expr.clone(),
                };
            }
            Ok(optimize_subquery_plan(plan, graph))
        },
    )
}

/// [`subquery_plan`] for full-query EXISTS / COLLECT bodies.
fn full_subquery_plan(
    subquery: &SingleQuery,
    graph: &dyn GraphView,
) -> Result<LogicalPlan, CypherError> {
    cached_plan(
        subquery as *const SingleQuery as usize,
        || subquery_fingerprint(&format!("{subquery:?}"), 2),
        || {
            Ok(optimize_subquery_plan(
                crate::planner::plan_single_query(subquery)?,
                graph,
            ))
        },
    )
}

/// Optimizations for subquery plans. `exec_subquery` models only a subset of
/// plan variants, so only rewrites staying inside it run:
///
/// - `fold_constants`: prunes Filters with literal predicates (a folded
///   `WHERE 1 = 0` is why subqueries get an optimizer at all).
/// - `convert_filter_to_node_by_id`: emits `NodeById`, which the interpreter handles.
///
/// Heavier rewrites (HashJoin, ScanFilterProject, IndexScan, ...) would produce
/// variants `exec_subquery` doesn't handle, so they're skipped.
fn optimize_subquery_plan(plan: LogicalPlan, _graph: &dyn GraphView) -> LogicalPlan {
    // hot path: most subqueries have no literal-stripped constants that could fold to NoRows; skip
    // the walk (EXISTS / COUNT run per outer row)
    let plan = if has_query_params() {
        // the server's literal stripping turns `WHERE 1 = 0` into `WHERE $__sp0 = $__sp1` before
        // planning; inline known params so folding fires
        resolve_known_params_in_plan(plan)
    } else {
        plan
    };
    let plan = crate::optimizer::rewrite::fold_constants(plan);
    crate::optimizer::rewrite::convert_filter_to_node_by_id(plan)
}

/// Replaces `Expr::Parameter(name)` with `Expr::Literal(...)` when the parameter is bound to a
/// simple literal, throughout the plan.
#[recursive::recursive]
fn resolve_known_params_in_plan(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input: Box::new(resolve_known_params_in_plan(*input)),
            predicate: resolve_known_params_in_expr(predicate),
        },
        LogicalPlan::Project {
            input,
            items,
            distinct,
            where_clause,
        } => {
            let items = items
                .into_iter()
                .map(|item| crate::ast::ProjectionItem {
                    expr: resolve_known_params_in_expr(item.expr),
                    alias: item.alias,
                    source_text: item.source_text,
                })
                .collect();
            LogicalPlan::Project {
                input: Box::new(resolve_known_params_in_plan(*input)),
                items,
                distinct,
                where_clause: where_clause.map(resolve_known_params_in_expr),
            }
        }
        LogicalPlan::Aggregate { input, keys, aggs } => {
            let keys = keys
                .into_iter()
                .map(|item| crate::ast::ProjectionItem {
                    expr: resolve_known_params_in_expr(item.expr),
                    alias: item.alias,
                    source_text: item.source_text,
                })
                .collect();
            let aggs = aggs
                .into_iter()
                .map(|item| crate::ast::ProjectionItem {
                    expr: resolve_known_params_in_expr(item.expr),
                    alias: item.alias,
                    source_text: item.source_text,
                })
                .collect();
            LogicalPlan::Aggregate {
                input: Box::new(resolve_known_params_in_plan(*input)),
                keys,
                aggs,
            }
        }
        LogicalPlan::Sort { input, items } => LogicalPlan::Sort {
            input: Box::new(resolve_known_params_in_plan(*input)),
            items,
        },
        LogicalPlan::Limit { input, count } => LogicalPlan::Limit {
            input: Box::new(resolve_known_params_in_plan(*input)),
            count,
        },
        LogicalPlan::Eager { input } => LogicalPlan::Eager {
            input: Box::new(resolve_known_params_in_plan(*input)),
        },
        LogicalPlan::Skip { input, count } => LogicalPlan::Skip {
            input: Box::new(resolve_known_params_in_plan(*input)),
            count,
        },
        LogicalPlan::Unwind {
            input,
            expr,
            alias,
            clear_source,
        } => LogicalPlan::Unwind {
            input: Box::new(resolve_known_params_in_plan(*input)),
            expr: resolve_known_params_in_expr(expr),
            alias,
            clear_source,
        },
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandOut {
            input: Box::new(resolve_known_params_in_plan(*input)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandIn {
            input: Box::new(resolve_known_params_in_plan(*input)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        } => LogicalPlan::ExpandBoth {
            input: Box::new(resolve_known_params_in_plan(*input)),
            from,
            rel_var,
            to,
            edge_types,
            range,
            prior_edge_vars,
        },
        // other variants (Scan, NodeById, writes, ...) have no parameter exprs where it matters;
        // leaves pass through
        other => other,
    }
}

#[recursive::recursive]
fn resolve_known_params_in_expr(expr: Expr) -> Expr {
    match expr {
        Expr::Parameter(ref name) => match get_query_param(name) {
            Some(CypherValue::Integer(n)) => Expr::Literal(Literal::Integer(n)),
            Some(CypherValue::Float(f)) => Expr::Literal(Literal::Float(f)),
            Some(CypherValue::Bool(b)) => Expr::Literal(Literal::Bool(b)),
            Some(CypherValue::String(s)) => Expr::Literal(Literal::String(s)),
            Some(CypherValue::Null) => Expr::Literal(Literal::Null),
            _ => expr,
        },
        Expr::UnaryOp(op, inner) => {
            Expr::UnaryOp(op, Box::new(resolve_known_params_in_expr(*inner)))
        }
        Expr::BinaryOp(left, op, right) => Expr::BinaryOp(
            Box::new(resolve_known_params_in_expr(*left)),
            op,
            Box::new(resolve_known_params_in_expr(*right)),
        ),
        Expr::Comparison(first, rest) => Expr::Comparison(
            Box::new(resolve_known_params_in_expr(*first)),
            rest.into_iter()
                .map(|(op, e)| (op, resolve_known_params_in_expr(e)))
                .collect(),
        ),
        Expr::FunctionCall {
            name,
            args,
            distinct,
        } => Expr::FunctionCall {
            name,
            args: args.into_iter().map(resolve_known_params_in_expr).collect(),
            distinct,
        },
        other => other,
    }
}

/// Frame-native early-exit existence check for SIMPLE single-part fixed-length
/// chains (no WHERE, var-length, property maps or bound relationship vars).
/// `None` if ineligible; caller uses the generic subquery path.
///
/// Anchors at a chain endpoint bound in the frame (reversing if only the last
/// is), DFSes hop by hop with relationship uniqueness, and returns on the FIRST
/// full match instead of enumerating all.
pub(super) fn try_chain_pattern_exists(
    match_clause: &MatchClause,
    frame: &Frame,
    symtab: &SymbolTable,
    graph: &dyn GraphView,
) -> Result<Option<bool>, CypherError> {
    use crate::ast::{NodePattern, RelDirection};

    if match_clause.where_clause.is_some() || match_clause.pattern.parts.len() != 1 {
        return Ok(None);
    }
    let part = &match_clause.pattern.parts[0];
    if part.variable.is_some()
        || !matches!(part.shortest_path, crate::ast::ShortestPathMode::None)
        || part.element.chain.is_empty()
    {
        return Ok(None);
    }
    // rels: single hop, typeless or static type, anonymous or unbound, no properties; at most one
    // static type (edges_with_neighbor takes one filter)
    for c in &part.element.chain {
        let r = &c.rel;
        if r.range.is_some()
            || !r.dynamic_types.is_empty()
            || r.properties.is_some()
            || r.where_pred.is_some()
            || r.types.len() > 1
        {
            return Ok(None);
        }
        if let Some(v) = &r.variable
            && symtab.slot(v).is_some_and(|s| !frame[s].is_null())
        {
            return Ok(None);
        }
    }
    // nodes: no properties
    let bound_node = |n: &NodePattern| -> Result<Option<Option<NodeId>>, CypherError> {
        // Ok(None) = ineligible; Ok(Some(None)) = fresh var; Ok(Some(Some(id))) = bound to this
        // node
        if n.properties.is_some() || n.where_pred.is_some() || !n.dynamic_labels.is_empty() {
            return Ok(None);
        }
        if let Some(v) = &n.variable
            && let Some(slot) = symtab.slot(v)
        {
            match &frame[slot] {
                SlotValue::Null => return Ok(Some(None)),
                SlotValue::NodeRef(id) => return Ok(Some(Some(*id))),
                SlotValue::Heap(cv) => match cv.as_ref() {
                    CypherValue::NodeRef(id) => return Ok(Some(Some(*id))),
                    CypherValue::Node { id, .. } => return Ok(Some(Some(NodeId(id.0)))),
                    _ => return Ok(None), // bound to a non-node
                },
                _ => return Ok(None),
            }
        }
        Ok(Some(None))
    };

    // flatten to (node, rel, node, rel, ..., node)
    let mut nodes: Vec<&NodePattern> = vec![&part.element.node];
    let mut rels: Vec<(&crate::ast::RelPattern, bool)> = Vec::new(); // (rel, forward)
    for c in &part.element.chain {
        rels.push((&c.rel, true));
        nodes.push(&c.node);
    }
    let mut bindings: Vec<Option<Option<NodeId>>> = Vec::with_capacity(nodes.len());
    for n in &nodes {
        match bound_node(n)? {
            None => return Ok(None),
            some => bindings.push(some),
        }
    }
    // anchor at a bound endpoint, reversing if needed
    let forward_ok = matches!(bindings[0], Some(Some(_)));
    let backward_ok = matches!(bindings[nodes.len() - 1], Some(Some(_)));
    if !forward_ok && !backward_ok {
        return Ok(None);
    }
    if !forward_ok {
        nodes.reverse();
        bindings.reverse();
        rels.reverse();
        for r in &mut rels {
            r.1 = false;
        }
    }

    // label + repeated-var checks per node, resolved up front
    let node_ok = |idx: usize,
                   candidate: NodeId,
                   locals: &mut Vec<(usize, NodeId)>|
     -> Result<bool, CypherError> {
        let n = nodes[idx];
        for lbl in &n.labels {
            match graph.strings().get(lbl) {
                Some(raw) => {
                    if !graph
                        .has_label(candidate, traverse_core::graph::types::LabelId(raw))
                        .unwrap_or(false)
                    {
                        return Ok(false);
                    }
                }
                None => return Ok(false), // label never interned: no match
            }
        }
        if let Some(Some(bound)) = &bindings[idx] {
            return Ok(*bound == candidate);
        }
        // a repeated fresh variable must bind consistently
        if nodes[idx].variable.is_some() {
            let name_idx = idx;
            for (i, id) in locals.iter() {
                if nodes[*i].variable == nodes[name_idx].variable {
                    return Ok(*id == candidate);
                }
            }
            locals.push((idx, candidate));
        }
        Ok(true)
    };

    let start = match bindings[0] {
        Some(Some(id)) => id,
        _ => unreachable!(),
    };
    let mut locals: Vec<(usize, NodeId)> = Vec::new();
    if !node_ok(0, start, &mut locals)? {
        return Ok(Some(false));
    }

    // iterative DFS, early exit
    #[recursive::recursive]
    fn dfs(
        idx: usize,
        current: NodeId,
        nodes_len: usize,
        rels: &[(&crate::ast::RelPattern, bool)],
        used_edges: &mut Vec<traverse_core::graph::types::EdgeId>,
        locals: &mut Vec<(usize, NodeId)>,
        node_ok: &dyn Fn(usize, NodeId, &mut Vec<(usize, NodeId)>) -> Result<bool, CypherError>,
        graph: &dyn GraphView,
    ) -> Result<bool, CypherError> {
        if idx + 1 >= nodes_len {
            return Ok(true);
        }
        let (rel, forward) = rels[idx];
        use traverse_core::graph::types::Direction;
        let dir = match (&rel.direction, forward) {
            (&RelDirection::Both, _) => Direction::Both,
            (&RelDirection::Outgoing, true) | (&RelDirection::Incoming, false) => {
                Direction::Outgoing
            }
            _ => Direction::Incoming,
        };
        let ty = rel.types.first().map(|s| s.as_str());
        let hops = graph
            .edges_with_neighbor(current, dir, ty)
            .unwrap_or_default();
        for hop in hops {
            if used_edges.contains(&hop.edge_id) {
                continue;
            }
            let locals_mark = locals.len();
            if node_ok(idx + 1, hop.neighbor, locals)? {
                used_edges.push(hop.edge_id);
                if dfs(
                    idx + 1,
                    hop.neighbor,
                    nodes_len,
                    rels,
                    used_edges,
                    locals,
                    node_ok,
                    graph,
                )? {
                    return Ok(true);
                }
                used_edges.pop();
            }
            locals.truncate(locals_mark);
        }
        Ok(false)
    }
    let mut used_edges = Vec::new();
    let found = dfs(
        0,
        start,
        nodes.len(),
        &rels,
        &mut used_edges,
        &mut locals,
        &node_ok,
        graph,
    )?;
    Ok(Some(found))
}
