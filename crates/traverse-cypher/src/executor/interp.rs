//! Record interpreter: runs subquery plans read-only over `Record`s seeded with the outer row.

use std::collections::{BTreeMap, HashMap};

use rustc_hash::FxHashSet;
use traverse_core::graph::types::{Direction, EdgeId, NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::ast::{CmpOp, Expr, Literal, RelDirection, SortItem};
use crate::error::CypherError;
use crate::frame::{self, Frame, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Record;
use super::aggregate::exec_aggregate;
use super::compare::compare_values;
use super::describe::projection_name;
use super::eval::{eval_expr, eval_expr_rec, eval_integer_expr, is_truthy};
use super::expand::{find_variable_length_paths, resolve_edge_type_filter};
use super::values::{JoinKey, cypher_to_join_key, cypher_value_to_index_key};

/// Record key with edge ids EXEMPT from relationship uniqueness: outer edges
/// seeded into a pattern comprehension's record so its WHERE/projection can use
/// them (Neo4j scopes uniqueness to the comprehension, not the enclosing MATCH).
pub(super) const PC_EXEMPT_EDGES: &str = "__pc_exempt_edges";

/// Edge IDs already bound in the record (relationship isomorphism), minus the
/// exempt ones.
fn bound_edge_ids(rec: &Record) -> FxHashSet<EdgeId> {
    let mut ids = FxHashSet::default();
    for val in rec.values() {
        match val {
            CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                ids.insert(*id);
            }
            CypherValue::List(items) => {
                for item in items {
                    match item {
                        CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                            ids.insert(*id);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(CypherValue::List(exempt)) = rec.get(PC_EXEMPT_EDGES) {
        for e in exempt {
            if let CypherValue::Integer(i) = e {
                ids.remove(&EdgeId(*i as u64));
            }
        }
    }
    ids
}

/// Runs BindPathEndpoints: start/end nodes from a pre-bound variable-length relationship list.
pub(crate) fn exec_bind_path_endpoints(
    input: &LogicalPlan,
    rel_var: &str,
    from_var: &str,
    to_var: &str,
    direction: &RelDirection,
    graph: &dyn GraphView,
    outer_rec: Option<&Record>,
) -> Result<Vec<Record>, CypherError> {
    let records = match outer_rec {
        Some(outer) => exec_subquery(input, outer, graph)?,
        None => {
            // input always comes from another plan; read-only, so exec_subquery with an empty
            // record
            let empty = Record::new();
            exec_subquery(input, &empty, graph)?
        }
    };
    let mut result = Vec::new();
    for rec in records {
        let edges = match rec.get(rel_var) {
            Some(CypherValue::List(list)) => list,
            _ => continue,
        };
        if edges.is_empty() {
            continue;
        }
        // edge IDs, checking connectivity
        let mut current: Option<NodeId> = None;
        let mut start_node: Option<NodeId> = None;
        let mut valid = true;
        for (i, cv) in edges.iter().enumerate() {
            let (src, tgt) = match cv {
                CypherValue::Edge { source, target, .. }
                | CypherValue::EdgeRef { source, target, .. } => (*source, *target),
                _ => {
                    valid = false;
                    break;
                }
            };
            let (step_from, step_to) = match direction {
                RelDirection::Outgoing => (src, tgt),
                RelDirection::Incoming => (tgt, src),
                RelDirection::Both => {
                    if let Some(cur) = current {
                        if src == cur {
                            (src, tgt)
                        } else if tgt == cur {
                            (tgt, src)
                        } else {
                            valid = false;
                            break;
                        }
                    } else {
                        (src, tgt)
                    }
                }
            };
            if i == 0 {
                start_node = Some(step_from);
                current = Some(step_to);
            } else if current == Some(step_from) {
                current = Some(step_to);
            } else {
                valid = false;
                break;
            }
        }
        if valid && let (Some(start), Some(end)) = (start_node, current) {
            // check against already-bound variables
            match rec.get(from_var) {
                Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id))
                    if *id != start =>
                {
                    continue;
                }
                _ => {}
            }
            match rec.get(to_var) {
                Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id))
                    if *id != end =>
                {
                    continue;
                }
                _ => {}
            }
            let mut new_rec = rec;
            new_rec.insert(from_var.to_string(), CypherValue::NodeRef(start));
            new_rec.insert(to_var.to_string(), CypherValue::NodeRef(end));
            result.push(new_rec);
        }
    }
    Ok(result)
}

/// Record-based BindEdgeEndpoints, same logic as `operators/bind.rs`: reads the
/// bound edge's endpoints, applies the type filter, binds the endpoint variables
/// (both orientations for an undirected pattern).
fn exec_bind_edge_endpoints(
    input: &LogicalPlan,
    edge_var: &str,
    from_var: &str,
    to_var: &str,
    direction: &RelDirection,
    edge_types: &[String],
    graph: &dyn GraphView,
    outer_rec: Option<&Record>,
) -> Result<Vec<Record>, CypherError> {
    let records = match outer_rec {
        Some(outer) => exec_subquery(input, outer, graph)?,
        None => {
            let empty = Record::new();
            exec_subquery(input, &empty, graph)?
        }
    };
    // type filter as interned ids
    let type_ids: Option<Vec<u32>> = if edge_types.is_empty() {
        None
    } else {
        let strings = graph.strings();
        let mut ids = Vec::with_capacity(edge_types.len());
        for t in edge_types {
            if let Some(id) = strings.get(t) {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Some(ids)
    };
    let mut result = Vec::with_capacity(records.len());
    for rec in records {
        let (etid, src, tgt) = match rec.get(edge_var) {
            Some(CypherValue::EdgeRef {
                edge_type_id,
                source,
                target,
                ..
            }) => (edge_type_id.0, *source, *target),
            Some(CypherValue::Edge {
                edge_type,
                source,
                target,
                ..
            }) => match graph.strings().get(edge_type) {
                Some(tid) => (tid, *source, *target),
                None => continue,
            },
            _ => continue,
        };
        if let Some(ref ids) = type_ids
            && !ids.contains(&etid)
        {
            continue;
        }
        let push_oriented = |result: &mut Vec<Record>, from_id: NodeId, to_id: NodeId| {
            if let Some(v) = rec.get(from_var) {
                match v {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) if *id != from_id => {
                        return;
                    }
                    CypherValue::Null => {}
                    CypherValue::Node { .. } | CypherValue::NodeRef(_) => {}
                    _ => return,
                }
            }
            if let Some(v) = rec.get(to_var) {
                match v {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) if *id != to_id => {
                        return;
                    }
                    CypherValue::Null => {}
                    CypherValue::Node { .. } | CypherValue::NodeRef(_) => {}
                    _ => return,
                }
            }
            let mut new_rec = rec.clone();
            new_rec.insert(from_var.to_string(), CypherValue::NodeRef(from_id));
            new_rec.insert(to_var.to_string(), CypherValue::NodeRef(to_id));
            result.push(new_rec);
        };
        match direction {
            RelDirection::Outgoing => push_oriented(&mut result, src, tgt),
            RelDirection::Incoming => push_oriented(&mut result, tgt, src),
            RelDirection::Both => {
                push_oriented(&mut result, src, tgt);
                if src != tgt {
                    push_oriented(&mut result, tgt, src);
                }
            }
        }
    }
    Ok(result)
}

/// Runs a subquery plan read-only, seeded with the outer record's variables.
pub(crate) fn exec_subquery(
    plan: &LogicalPlan,
    outer_rec: &Record,
    graph: &dyn GraphView,
) -> Result<Vec<Record>, CypherError> {
    match plan {
        LogicalPlan::Scan { variable, label } => {
            // already bound in the outer record: use it
            if let Some(existing) = outer_rec.get(variable) {
                let matches = match (label, existing) {
                    (Some(lbl), CypherValue::Node { labels, .. }) => {
                        labels.iter().any(|l| l == lbl)
                    }
                    (Some(lbl), CypherValue::NodeRef(id)) => {
                        if let Ok(node) = graph.get_node(*id) {
                            node.labels
                                .iter()
                                .any(|lid| graph.strings().resolve(lid.0) == lbl.as_str())
                        } else {
                            false
                        }
                    }
                    (None, CypherValue::Node { .. }) | (None, CypherValue::NodeRef(_)) => true,
                    _ => false,
                };
                if matches {
                    return Ok(vec![outer_rec.clone()]);
                } else {
                    return Ok(Vec::new());
                }
            }
            // not bound: scan all nodes
            let node_ids: Vec<NodeId> = match label {
                Some(lbl) => graph.nodes_by_label(lbl),
                None => graph.nodes_vec(),
            };
            let mut records = Vec::with_capacity(node_ids.len());
            for nid in node_ids {
                let mut rec = outer_rec.clone();
                rec.insert(variable.clone(), CypherValue::NodeRef(nid));
                records.push(rec);
            }
            Ok(records)
        }
        LogicalPlan::EdgeTypeScan {
            from_var,
            rel_var,
            to_var,
            edge_types,
            direction,
        } => {
            // anchorless edge scan: every edge of the types binds (from, rel, to), like
            // EdgeTypeScanOp; undirected patterns emit both orientations; outer-bound variables
            // constrain instead of rebinding
            let bound_from = get_node_id_opt(outer_rec, from_var);
            let bound_to = get_node_id_opt(outer_rec, to_var);
            let bound_rel = match outer_rec.get(rel_var) {
                Some(CypherValue::Edge { id, .. }) | Some(CypherValue::EdgeRef { id, .. }) => {
                    Some(*id)
                }
                _ => None,
            };
            let mut records = Vec::new();
            for ty in edge_types {
                for eid in graph.edges_by_type(ty) {
                    if bound_rel.is_some_and(|b| b != eid) {
                        continue;
                    }
                    let Ok(edge) = graph.get_edge(eid) else {
                        continue;
                    };
                    let mut orientations = vec![(edge.source, edge.target)];
                    match direction {
                        Direction::Outgoing => {}
                        Direction::Incoming => orientations[0] = (edge.target, edge.source),
                        Direction::Both => orientations.push((edge.target, edge.source)),
                    }
                    for (f, t) in orientations {
                        if bound_from.is_some_and(|b| b != f) || bound_to.is_some_and(|b| b != t) {
                            continue;
                        }
                        let mut rec = outer_rec.clone();
                        if !from_var.is_empty() {
                            rec.insert(from_var.clone(), CypherValue::NodeRef(f));
                        }
                        if !rel_var.is_empty() {
                            rec.insert(
                                rel_var.clone(),
                                CypherValue::EdgeRef {
                                    id: eid,
                                    edge_type_id: edge.edge_type,
                                    source: edge.source,
                                    target: edge.target,
                                },
                            );
                        }
                        if !to_var.is_empty() {
                            rec.insert(to_var.clone(), CypherValue::NodeRef(t));
                        }
                        records.push(rec);
                    }
                }
            }
            Ok(records)
        }
        LogicalPlan::Filter { input, predicate } => {
            // constant-folded `false`: never touch the input (else `EXISTS { MATCH (n) WHERE 1 = 0
            // RETURN n }` scans every node)
            if matches!(predicate, Expr::Literal(Literal::Bool(false))) {
                return Ok(Vec::new());
            }
            // constant `true`: skip per-record evaluation
            if matches!(predicate, Expr::Literal(Literal::Bool(true))) {
                return exec_subquery(input, outer_rec, graph);
            }
            let records = exec_subquery(input, outer_rec, graph)?;
            if records.is_empty() {
                return Ok(records);
            }
            let mut symtab = frame::records_to_symtab(&records);
            frame::collect_expr_variables(predicate, &mut symtab);
            let mut result = Vec::new();
            for rec in records {
                let frame = frame::record_to_frame(&rec, &symtab);
                let val = eval_expr(predicate, &frame, graph, &symtab)?;
                if is_truthy(&val) {
                    result.push(rec);
                }
            }
            Ok(result)
        }
        LogicalPlan::ExpandOut {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            ..
        } => exec_subquery_expand(
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            Direction::Outgoing,
            outer_rec,
            graph,
        ),
        LogicalPlan::ExpandIn {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            ..
        } => exec_subquery_expand(
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            Direction::Incoming,
            outer_rec,
            graph,
        ),
        LogicalPlan::ExpandBoth {
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            ..
        } => exec_subquery_expand(
            input,
            from,
            rel_var,
            to,
            edge_types,
            range,
            Direction::Both,
            outer_rec,
            graph,
        ),
        LogicalPlan::BindVarLists { input, groups } => {
            // multi-step group variables: each member slot's values into a LIST (Record mirror of
            // bind_var_list_frames)
            let records = exec_subquery(input, outer_rec, graph)?;
            let mut result = Vec::new();
            for mut rec in records {
                for (target, members) in groups {
                    let items: Vec<CypherValue> = members
                        .iter()
                        .map(|m| rec.get(m).cloned().unwrap_or(CypherValue::Null))
                        .collect();
                    rec.insert(target.clone(), CypherValue::List(items));
                }
                result.push(rec);
            }
            Ok(result)
        }
        LogicalPlan::BindQppGroups {
            input,
            anchor_var,
            rel_var,
            src_var,
            dst_var,
        } => {
            // QPP group variables: per-iteration node lists from the anchor + var-length edges
            // (Record mirror of operators::bind::bind_qpp_group_frames)
            let records = exec_subquery(input, outer_rec, graph)?;
            let mut result = Vec::new();
            for mut rec in records {
                let anchor = match rec.get(anchor_var) {
                    Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => {
                        Some(*id)
                    }
                    _ => None,
                };
                let edges: Option<Vec<(NodeId, NodeId)>> = match rec.get(rel_var) {
                    Some(CypherValue::List(items)) => items
                        .iter()
                        .map(|e| match e {
                            CypherValue::Edge { source, target, .. }
                            | CypherValue::EdgeRef { source, target, .. } => {
                                Some((*source, *target))
                            }
                            _ => None,
                        })
                        .collect(),
                    _ => None,
                };
                match (anchor, edges) {
                    (Some(anchor), Some(edge_pairs)) => {
                        let mut cur = anchor;
                        let mut srcs = Vec::with_capacity(edge_pairs.len());
                        let mut dsts = Vec::with_capacity(edge_pairs.len());
                        for (s, t) in edge_pairs {
                            let next = if s == cur { t } else { s };
                            srcs.push(CypherValue::NodeRef(cur));
                            dsts.push(CypherValue::NodeRef(next));
                            cur = next;
                        }
                        if let Some(v) = src_var {
                            rec.insert(v.clone(), CypherValue::List(srcs));
                        }
                        if let Some(v) = dst_var {
                            rec.insert(v.clone(), CypherValue::List(dsts));
                        }
                    }
                    _ => {
                        if let Some(v) = src_var {
                            rec.insert(v.clone(), CypherValue::Null);
                        }
                        if let Some(v) = dst_var {
                            rec.insert(v.clone(), CypherValue::Null);
                        }
                    }
                }
                result.push(rec);
            }
            Ok(result)
        }
        LogicalPlan::BindPath {
            input,
            variable,
            node_vars,
            edge_vars,
        } => {
            let records = exec_subquery(input, outer_rec, graph)?;
            let mut result = Vec::new();
            for mut rec in records {
                let mut nodes: Vec<CypherValue> = Vec::new();
                let mut edges: Vec<CypherValue> = Vec::new();
                if let Some(first) = node_vars.first() {
                    nodes.push(rec.get(first).cloned().unwrap_or(CypherValue::Null));
                }
                for (i, ev) in edge_vars.iter().enumerate() {
                    let val = rec.get(ev).cloned().unwrap_or(CypherValue::Null);
                    if let CypherValue::List(items) = val {
                        if !items.is_empty() {
                            let item_count = items.len();
                            let mut current_id = match nodes.last() {
                                Some(CypherValue::Node { id, .. })
                                | Some(CypherValue::NodeRef(id)) => *id,
                                _ => NodeId(0),
                            };
                            for (j, edge_val) in items.iter().enumerate() {
                                edges.push(edge_val.clone());
                                match edge_val {
                                    CypherValue::Edge { source, target, .. }
                                    | CypherValue::EdgeRef { source, target, .. } => {
                                        let next_id = if *source == current_id {
                                            *target
                                        } else {
                                            *source
                                        };
                                        current_id = next_id;
                                        // intermediate nodes stay lightweight NodeRefs
                                        if j < item_count - 1 {
                                            nodes.push(CypherValue::NodeRef(next_id));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if i + 1 < node_vars.len() {
                                nodes.push(
                                    rec.get(&node_vars[i + 1])
                                        .cloned()
                                        .unwrap_or(CypherValue::Null),
                                );
                            }
                        }
                    } else {
                        edges.push(val);
                        if i + 1 < node_vars.len() {
                            nodes.push(
                                rec.get(&node_vars[i + 1])
                                    .cloned()
                                    .unwrap_or(CypherValue::Null),
                            );
                        }
                    }
                }
                rec.insert(variable.clone(), CypherValue::Path { nodes, edges });
                result.push(rec);
            }
            Ok(result)
        }
        LogicalPlan::Project {
            input,
            items,
            distinct,
            where_clause,
        } => {
            let records = exec_subquery(input, outer_rec, graph)?;
            if items.is_empty() {
                if let Some(wc) = where_clause {
                    if records.is_empty() {
                        return Ok(records);
                    }
                    let mut symtab = frame::records_to_symtab(&records);
                    frame::collect_expr_variables(wc, &mut symtab);
                    let mut result = Vec::new();
                    for rec in &records {
                        let frame = frame::record_to_frame(rec, &symtab);
                        let val = eval_expr(wc, &frame, graph, &symtab)?;
                        if is_truthy(&val) {
                            result.push(rec.clone());
                        }
                    }
                    if *distinct {
                        return Ok(dedup_records(result));
                    }
                    return Ok(result);
                }
                if *distinct {
                    return Ok(dedup_records(records));
                }
                return Ok(records);
            }
            // all items plain variables? then move values out of consumed records (zero-copy)
            let all_simple_vars = where_clause.is_none()
                && items
                    .iter()
                    .all(|item| matches!(&item.expr, Expr::Variable(_)));

            if all_simple_vars {
                let mut result = Vec::with_capacity(records.len());
                for mut rec in records {
                    let mut new_rec = Record::new();
                    for item in items {
                        let name = projection_name(item);
                        if let Expr::Variable(var) = &item.expr {
                            let val = rec.remove(var).unwrap_or(CypherValue::Null);
                            new_rec.insert(name, val);
                        }
                    }
                    result.push(new_rec);
                }
                if *distinct {
                    result = dedup_records(result);
                }
                return Ok(result);
            }

            // general path: one SymbolTable for all records
            let symtab = if !records.is_empty() {
                let mut st = frame::record_to_symtab(&records[0]);
                for item in items {
                    frame::collect_expr_variables(&item.expr, &mut st);
                }
                if let Some(wc) = where_clause {
                    frame::collect_expr_variables(wc, &mut st);
                }
                Some(st)
            } else {
                None
            };
            let mut result = Vec::new();
            for rec in &records {
                let mut new_rec = Record::new();
                let symtab = symtab.as_ref().unwrap();
                let frame = frame::record_to_frame(rec, symtab);
                for item in items {
                    let val = eval_expr(&item.expr, &frame, graph, symtab)?;
                    let name = projection_name(item);
                    new_rec.insert(name, val);
                }
                if let Some(wc) = where_clause {
                    let mut merged = rec.clone();
                    for (k, v) in &new_rec {
                        merged.insert(k.clone(), v.clone());
                    }
                    let val = eval_expr_rec(wc, &merged, graph)?;
                    if !is_truthy(&val) {
                        continue;
                    }
                }
                result.push(new_rec);
            }
            if *distinct {
                result = dedup_records(result);
            }
            Ok(result)
        }
        LogicalPlan::Aggregate { input, keys, aggs } => {
            let records = exec_subquery(input, outer_rec, graph)?;
            // SymbolTable from the first record (all records share keys)
            let mut sub_symtab = if records.is_empty() {
                SymbolTable::new()
            } else {
                frame::record_to_symtab(&records[0])
            };
            // add key and agg output names so exec_aggregate can store results
            for item in keys.iter().chain(aggs.iter()) {
                let name = projection_name(item);
                if sub_symtab.slot(&name).is_none() {
                    sub_symtab.add(name);
                }
                frame::collect_expr_variables(&item.expr, &mut sub_symtab);
            }
            let frames: Vec<Frame> = records
                .into_iter()
                .map(|r| frame::record_to_frame_move(r, &sub_symtab))
                .collect();
            let result_frames = exec_aggregate(&frames, keys, aggs, graph, &sub_symtab)?;
            Ok(result_frames
                .into_iter()
                .map(|f| frame::frame_to_record(&f, &sub_symtab))
                .collect())
        }
        LogicalPlan::Sort { input, items } => {
            let records = exec_subquery(input, outer_rec, graph)?;
            if records.is_empty() {
                return Ok(records);
            }
            let items = items.clone();
            // sort keys computed once (Schwartzian transform), not per comparison. Plain
            // property keys (p2.highlight) read straight from embedded CypherValue::Node
            // properties; NodeRef keys (from expansions) are batch-read under one lock.
            let needs_full_eval = items.iter().any(|item| {
                !matches!(&item.expr,
                    Expr::Property(base, _) if matches!(base.as_ref(), Expr::Variable(_))
                ) && !matches!(&item.expr, Expr::Variable(_))
            });
            let symtab = if needs_full_eval {
                let mut st = frame::records_to_symtab(&records);
                for item in &items {
                    frame::collect_expr_variables(&item.expr, &mut st);
                }
                Some(st)
            } else {
                None
            };

            // batch-read NodeRef sort keys (one lock, not one per record)
            let batch_keys = batch_sort_keys_from_noderef(&records, &items, graph);

            // all keys in one flat array, then sort an index array: no per-record Vec allocations
            let n_items = items.len();
            let n_records = records.len();
            let mut all_keys: Vec<CypherValue> = Vec::with_capacity(n_records * n_items);
            for (idx, rec) in records.iter().enumerate() {
                for (ki, item) in items.iter().enumerate() {
                    if let Some(batch) = batch_keys.get(&ki) {
                        all_keys.push(batch[idx].clone());
                    } else {
                        all_keys.push(eval_sort_key_fast(&item.expr, rec, graph, symtab.as_ref()));
                    }
                }
            }

            let mut indices: Vec<usize> = (0..n_records).collect();
            indices.sort_by(|&a, &b| {
                for (i, item) in items.iter().enumerate() {
                    let ord =
                        compare_values(&all_keys[a * n_items + i], &all_keys[b * n_items + i]);
                    let ord = if item.ascending { ord } else { ord.reverse() };
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });

            // reorder records by sorted index
            let mut records_opt: Vec<Option<Record>> = records.into_iter().map(Some).collect();
            let sorted: Vec<Record> = indices
                .iter()
                .map(|&i| records_opt[i].take().unwrap())
                .collect();
            Ok(sorted)
        }
        LogicalPlan::Eager { input } => exec_subquery(input, outer_rec, graph),
        LogicalPlan::Skip { input, count } => {
            let records = exec_subquery(input, outer_rec, graph)?;
            let n = eval_integer_expr(count, graph)?;
            Ok(records.into_iter().skip(n as usize).collect())
        }
        LogicalPlan::Limit { input, count } => {
            let records = exec_subquery(input, outer_rec, graph)?;
            let n = eval_integer_expr(count, graph)?;
            Ok(records.into_iter().take(n as usize).collect())
        }
        LogicalPlan::Unwind {
            input, expr, alias, ..
        } => {
            let records = exec_subquery(input, outer_rec, graph)?;
            let mut result = Vec::new();
            for rec in records {
                let val = eval_expr_rec(expr, &rec, graph)?;
                if let CypherValue::List(items) = val {
                    for item in items {
                        let mut new_rec = rec.clone();
                        new_rec.insert(alias.clone(), item);
                        result.push(new_rec);
                    }
                } else if !matches!(val, CypherValue::Null) {
                    let mut new_rec = rec.clone();
                    new_rec.insert(alias.clone(), val);
                    result.push(new_rec);
                }
            }
            Ok(result)
        }
        LogicalPlan::LoadCsv {
            input,
            url,
            with_headers,
            row_alias,
            field_terminator,
        } => {
            let records = exec_subquery(input, outer_rec, graph)?;
            let mut result = Vec::new();
            for rec in records {
                let path_val = eval_expr_rec(url, &rec, graph)?;
                let path_str = match &path_val {
                    CypherValue::String(s) => s.clone(),
                    _ => {
                        return Err(CypherError::type_error(format!(
                            "LOAD CSV FROM expects a string path, got {}",
                            path_val.type_name()
                        )));
                    }
                };
                let file_path = path_str.strip_prefix("file://").unwrap_or(&path_str);
                let delimiter = field_terminator.unwrap_or(',');
                // same source handling as LoadCsvOp: http(s) URLs fetch into memory (Neo4j
                // semantics), ragged rows tolerated
                let source: Box<dyn std::io::Read> =
                    if file_path.starts_with("http://") || file_path.starts_with("https://") {
                        // wasm32 has no outbound HTTP; only local (OPFS-backed) paths
                        #[cfg(target_arch = "wasm32")]
                        {
                            return Err(CypherError::runtime(format!(
                                "LOAD CSV: cannot fetch '{file_path}': remote URLs \
                             are not available in WASM builds"
                            )));
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let resp = ureq::config::Config::builder()
                                .timeout_global(Some(std::time::Duration::from_secs(60)))
                                .user_agent("traverse-load-csv")
                                .build()
                                .new_agent()
                                .get(file_path)
                                .call()
                                .map_err(|e| {
                                    CypherError::runtime(format!(
                                        "LOAD CSV: cannot fetch '{file_path}': {e}"
                                    ))
                                })?;
                            // stream the body into the CSV reader, no full buffering
                            Box::new(resp.into_body().into_reader())
                        }
                    } else {
                        Box::new(std::fs::File::open(file_path).map_err(|e| {
                            CypherError::runtime(format!(
                                "LOAD CSV: cannot open '{}': {}",
                                file_path, e
                            ))
                        })?)
                    };
                let mut rdr = csv::ReaderBuilder::new()
                    .has_headers(*with_headers)
                    .delimiter(delimiter as u8)
                    .flexible(true)
                    .from_reader(source);
                if *with_headers {
                    let headers: Vec<String> = rdr
                        .headers()
                        .map_err(|e| {
                            CypherError::runtime(format!("LOAD CSV: error reading headers: {}", e))
                        })?
                        .iter()
                        .map(|h| h.to_string())
                        .collect();
                    for record in rdr.records() {
                        let record = record.map_err(|e| {
                            CypherError::runtime(format!("LOAD CSV: error reading row: {}", e))
                        })?;
                        let mut map = BTreeMap::new();
                        for (i, field) in record.iter().enumerate() {
                            let key = headers.get(i).cloned().unwrap_or_else(|| i.to_string());
                            map.insert(key, CypherValue::String(field.to_string()));
                        }
                        let mut new_rec = rec.clone();
                        new_rec.insert(row_alias.clone(), CypherValue::Map(map));
                        result.push(new_rec);
                    }
                } else {
                    for record in rdr.records() {
                        let record = record.map_err(|e| {
                            CypherError::runtime(format!("LOAD CSV: error reading row: {}", e))
                        })?;
                        let items: Vec<CypherValue> = record
                            .iter()
                            .map(|f| CypherValue::String(f.to_string()))
                            .collect();
                        let mut new_rec = rec.clone();
                        new_rec.insert(row_alias.clone(), CypherValue::List(items));
                        result.push(new_rec);
                    }
                }
            }
            Ok(result)
        }
        LogicalPlan::EmptyRow => Ok(vec![outer_rec.clone()]),

        LogicalPlan::BindPathEndpoints {
            input,
            rel_var,
            from_var,
            to_var,
            direction,
        } => exec_bind_path_endpoints(
            input,
            rel_var,
            from_var,
            to_var,
            direction,
            graph,
            Some(outer_rec),
        ),
        LogicalPlan::BindEdgeEndpoints {
            input,
            edge_var,
            from_var,
            to_var,
            direction,
            edge_types,
        } => exec_bind_edge_endpoints(
            input,
            edge_var,
            from_var,
            to_var,
            direction,
            edge_types,
            graph,
            Some(outer_rec),
        ),
        LogicalPlan::DistinctExpand {
            input,
            from,
            to,
            min_hops,
            max_hops,
            direction,
            edge_types,
            target_label,
            global_dedup,
            target_filter: _, // not evaluated in the subquery path
        } => {
            let records = exec_subquery(input, outer_rec, graph)?;
            let type_filter = resolve_edge_type_filter(graph, edge_types);
            let label_id = target_label.as_ref().and_then(|l| {
                graph
                    .strings()
                    .get(l)
                    .map(traverse_core::graph::types::LabelId)
            });
            let mut result = Vec::new();
            let mut global_seen: FxHashSet<NodeId> = FxHashSet::default();

            for rec in &records {
                let start_id = get_node_id(rec, from)?;
                let estimated = graph.node_count().min(10_000);
                let mut frontier: FxHashSet<NodeId> =
                    FxHashSet::with_capacity_and_hasher(estimated, Default::default());
                frontier.insert(start_id);
                let mut collected: FxHashSet<NodeId> =
                    FxHashSet::with_capacity_and_hasher(estimated, Default::default());

                for depth in 1..=*max_hops {
                    let mut next: FxHashSet<NodeId> = FxHashSet::default();
                    for &nid in &frontier {
                        graph.for_each_neighbor(
                            nid,
                            *direction,
                            &type_filter,
                            &mut |neighbor| {
                                next.insert(neighbor);
                            },
                        )?;
                    }
                    frontier = next;
                    if depth >= *min_hops {
                        collected.extend(&frontier);
                    }
                }

                for &nid in &collected {
                    if *global_dedup && !global_seen.insert(nid) {
                        continue;
                    }
                    if let Some(lid) = label_id {
                        if let Ok(node) = graph.get_node(nid) {
                            if !node.labels.contains(&lid) {
                                continue;
                            }
                        } else {
                            continue;
                        }
                    }
                    let mut new_rec = rec.clone();
                    new_rec.insert(to.clone(), CypherValue::NodeRef(nid));
                    result.push(new_rec);
                }
            }
            Ok(result)
        }
        LogicalPlan::CrossProduct { left, right } => {
            let left_records = exec_subquery(left, outer_rec, graph)?;
            let mut result = Vec::new();
            for lrec in &left_records {
                let right_records = exec_subquery(right, lrec, graph)?;
                for rrec in right_records {
                    let mut merged = lrec.clone();
                    for (k, v) in rrec {
                        merged.insert(k, v);
                    }
                    result.push(merged);
                }
            }
            Ok(result)
        }
        LogicalPlan::NodeById {
            variable,
            label,
            id_expr,
        } => {
            // NodeByIdOp for the subquery interpreter: subquery plans go through the optimizer, so
            // `WHERE id(n) = lit` inside EXISTS/COUNT arrives here
            let symtab = frame::records_to_symtab(std::slice::from_ref(outer_rec));
            let frame = frame::record_to_frame(outer_rec, &symtab);
            let id_val = eval_expr(id_expr, &frame, graph, &symtab)?;
            let id = match id_val {
                CypherValue::Integer(n) if n >= 0 => NodeId(n as u64),
                _ => return Ok(Vec::new()),
            };
            let node = match graph.get_node(id) {
                Ok(n) => n,
                Err(_) => return Ok(Vec::new()),
            };
            if let Some(lbl) = label {
                let want = match graph.strings().get(lbl) {
                    Some(raw) => raw,
                    None => return Ok(Vec::new()),
                };
                if !node.labels.iter().any(|lid| lid.0 == want) {
                    return Ok(Vec::new());
                }
            }
            let mut rec = outer_rec.clone();
            rec.insert(variable.clone(), CypherValue::NodeRef(id));
            Ok(vec![rec])
        }
        LogicalPlan::ScanFilterProject {
            variable,
            label,
            predicate,
            items,
            distinct,
            limit,
        } => {
            // the main plan's SFP fusion recurses into CallSubquery bodies on large graphs, but
            // this interpreter models the unfused shape: desugar to Scan -> Filter -> Project (->
            // Limit) and re-enter
            let mut unfused = LogicalPlan::Scan {
                variable: variable.clone(),
                label: label.clone(),
            };
            if let Some(pred) = predicate {
                unfused = LogicalPlan::Filter {
                    input: Box::new(unfused),
                    predicate: pred.clone(),
                };
            }
            unfused = LogicalPlan::Project {
                input: Box::new(unfused),
                items: items.clone(),
                distinct: *distinct,
                where_clause: None,
            };
            if let Some(n) = limit {
                unfused = LogicalPlan::Limit {
                    input: Box::new(unfused),
                    count: Expr::Literal(Literal::Integer(*n as i64)),
                };
            }
            exec_subquery(&unfused, outer_rec, graph)
        }
        // index rewrites recurse into CALL subquery bodies, so a property-filtered body can arrive
        // as IndexScan / IndexRangeScan: desugar to Scan + Filter (index vs scan is only
        // performance) and re-enter
        LogicalPlan::IndexScan {
            variable,
            label,
            property,
            value,
        } => {
            // real index seek; the key may reference OUTER variables (comprehension access-path
            // rewrites), so evaluate it against the outer record. Composite keys keep the desugared
            // Scan+Filter path
            if property.contains('\u{1F}') {
                let pred = Expr::Comparison(
                    Box::new(Expr::Property(
                        Box::new(Expr::Variable(variable.clone())),
                        property.clone(),
                    )),
                    vec![(CmpOp::Eq, value.clone())],
                );
                let desugared = LogicalPlan::Filter {
                    input: Box::new(LogicalPlan::Scan {
                        variable: variable.clone(),
                        label: Some(label.clone()),
                    }),
                    predicate: pred,
                };
                return exec_subquery(&desugared, outer_rec, graph);
            }
            let val = eval_expr_rec(value, outer_rec, graph)?;
            let ids: Vec<NodeId> = match cypher_value_to_index_key(&val) {
                Some(k) => graph
                    .index_lookup(label, property, &k)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|&nid| graph.get_node(nid).is_ok())
                    .collect(),
                None => vec![],
            };
            let mut records = Vec::with_capacity(ids.len());
            for nid in ids {
                let mut rec = outer_rec.clone();
                rec.insert(variable.clone(), CypherValue::NodeRef(nid));
                records.push(rec);
            }
            Ok(records)
        }
        LogicalPlan::IndexLookupJoin {
            input,
            variable,
            label,
            property,
            key,
        } => {
            let inputs = exec_subquery(input, outer_rec, graph)?;
            let mut records = Vec::new();
            for rec in inputs {
                let val = eval_expr_rec(key, &rec, graph)?;
                if let Some(k) = cypher_value_to_index_key(&val) {
                    for nid in graph.index_lookup(label, property, &k).unwrap_or_default() {
                        if graph.get_node(nid).is_ok() {
                            let mut out = rec.clone();
                            out.insert(variable.clone(), CypherValue::NodeRef(nid));
                            records.push(out);
                        }
                    }
                }
            }
            Ok(records)
        }
        LogicalPlan::Union { left, right, all } => {
            let mut records = exec_subquery(left, outer_rec, graph)?;
            records.extend(exec_subquery(right, outer_rec, graph)?);
            if !all {
                let mut seen = std::collections::HashSet::new();
                records.retain(|r| seen.insert(format!("{r:?}")));
            }
            Ok(records)
        }
        LogicalPlan::IndexRangeScan {
            variable,
            label,
            property,
            op,
            value,
        } => {
            let pred = Expr::Comparison(
                Box::new(Expr::Property(
                    Box::new(Expr::Variable(variable.clone())),
                    property.clone(),
                )),
                vec![(*op, value.clone())],
            );
            let desugared = LogicalPlan::Filter {
                input: Box::new(LogicalPlan::Scan {
                    variable: variable.clone(),
                    label: Some(label.clone()),
                }),
                predicate: pred,
            };
            exec_subquery(&desugared, outer_rec, graph)
        }
        // a spatial index scan always wraps an exact Filter/Sort/Limit enforcing the predicate, so
        // here it desugars to a plain label scan (only acceleration is lost)
        LogicalPlan::PointIndexScan {
            variable, label, ..
        } => {
            let desugared = LogicalPlan::Scan {
                variable: variable.clone(),
                label: Some(label.clone()),
            };
            exec_subquery(&desugared, outer_rec, graph)
        }
        // same for the trigram text index scan: the wrapping Filter is exact
        LogicalPlan::TextIndexScan {
            variable, label, ..
        } => {
            let desugared = LogicalPlan::Scan {
                variable: variable.clone(),
                label: Some(label.clone()),
            };
            exec_subquery(&desugared, outer_rec, graph)
        }
        // convert_filter_to_node_by_id_range emits this for `id(n) < N` bodies
        LogicalPlan::NodeByIdRange {
            variable,
            label,
            lower,
            upper,
        } => {
            let symtab = frame::records_to_symtab(std::slice::from_ref(outer_rec));
            let frame = frame::record_to_frame(outer_rec, &symtab);
            let eval_bound = |e: &Option<Expr>| -> Result<Option<u64>, CypherError> {
                match e {
                    None => Ok(None),
                    Some(expr) => match eval_expr(expr, &frame, graph, &symtab)? {
                        CypherValue::Integer(n) if n >= 0 => Ok(Some(n as u64)),
                        _ => Ok(Some(0)),
                    },
                }
            };
            let lo = eval_bound(lower)?.unwrap_or(0);
            let hi = eval_bound(upper)?.unwrap_or(graph.node_count() as u64);
            let want_label = match label {
                Some(lbl) => match graph.strings().get(lbl) {
                    Some(raw) => Some(raw),
                    None => return Ok(Vec::new()),
                },
                None => None,
            };
            let mut out = Vec::new();
            for id in lo..hi {
                let nid = NodeId(id);
                let node = match graph.get_node(nid) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                if let Some(want) = want_label
                    && !node.labels.iter().any(|lid| lid.0 == want)
                {
                    continue;
                }
                let mut rec = outer_rec.clone();
                rec.insert(variable.clone(), CypherValue::NodeRef(nid));
                out.push(rec);
            }
            Ok(out)
        }
        // OPTIONAL MATCH in a subquery body: run the input, then the match per row, left-join (no
        // match -> one row with the new variables Null)
        LogicalPlan::OptionalMatch {
            input,
            match_plan,
            new_variables,
        } => {
            let input_recs = exec_subquery(input, outer_rec, graph)?;
            let mut out = Vec::new();
            for rec in input_recs {
                let matched = exec_subquery(match_plan, &rec, graph)?;
                if matched.is_empty() {
                    let mut r = rec.clone();
                    for v in new_variables {
                        r.insert(v.clone(), CypherValue::Null);
                    }
                    out.push(r);
                } else {
                    out.extend(matched);
                }
            }
            Ok(out)
        }
        // read-only algorithm procedures (gds.* / traverse.* stream, stats, mutate; overlays use
        // interior mutability) run through the operator path over a read-only shim; write modes
        // stay unsupported (the shim's mutators are unreachable)
        LogicalPlan::CallProcedure {
            input,
            procedure_name,
            args,
            yield_columns,
        } => {
            let read_only_algo = (procedure_name.starts_with("gds.")
                || procedure_name.starts_with("traverse."))
                && !procedure_name.ends_with(".write")
                && !procedure_name.contains("nodeProperties.write");
            if !read_only_algo {
                return Err(CypherError::runtime(format!(
                    "subquery body contains an operation not yet supported inside \
                     CALL/EXISTS/COUNT subqueries: procedure call ({procedure_name})"
                )));
            }
            let Some(sync_graph) = graph.as_sync_ref() else {
                return Err(CypherError::runtime(
                    "procedure call in subquery requires a sync graph view".to_string(),
                ));
            };
            let records = exec_subquery(input, outer_rec, graph)?;
            if records.is_empty() {
                return Ok(Vec::new());
            }
            let mut symtab = frame::records_to_symtab(&records);
            if let Some(args) = args {
                for a in args {
                    frame::collect_expr_variables(a, &mut symtab);
                }
            }
            for (_, bound) in yield_columns {
                if symtab.slot(bound).is_none() {
                    symtab.add(bound.clone());
                }
            }
            let frames: Vec<Frame> = records
                .into_iter()
                .map(|r| frame::record_to_frame_move(r, &symtab))
                .collect();
            let one_off = LogicalPlan::CallProcedure {
                input: Box::new(LogicalPlan::SeededRows(frames)),
                procedure_name: procedure_name.clone(),
                args: args.clone(),
                yield_columns: yield_columns.clone(),
            };
            let mut shim = crate::operators::pipeline_par::ReadOnlyGraphView(sync_graph);
            let mut op = crate::operators::build_operator_tree(&one_off);
            let mut out = Vec::new();
            while let Some(f) = op.next(&mut shim, &symtab)? {
                out.push(frame::frame_to_record(&f, &symtab));
            }
            Ok(out)
        }
        // unhandled variants (EdgeTypeScan, HashJoin, nested CallSubquery/CallProcedure,
        // ShortestPath, ...) ERROR instead of silently returning no rows
        other => Err(CypherError::runtime(format!(
            "subquery body contains an operation not yet supported inside \
             CALL/EXISTS/COUNT subqueries: {}",
            plan_variant_name(other),
        ))),
    }
}

/// Short name of a LogicalPlan variant, for the unsupported-subquery error.
fn plan_variant_name(plan: &LogicalPlan) -> &'static str {
    match plan {
        LogicalPlan::EdgeTypeScan { .. } => "anchorless edge scan",
        LogicalPlan::HashJoin { .. } => "hash join",
        LogicalPlan::CallSubquery { .. } => "nested CALL subquery",
        LogicalPlan::CallInTransactions { .. } => "CALL in transactions",
        LogicalPlan::CallProcedure { .. } => "procedure call",
        LogicalPlan::ShortestPath { .. } => "shortest path",
        LogicalPlan::WeightedShortestPath { .. } => "weighted shortest path",
        LogicalPlan::Union { .. } => "UNION",
        LogicalPlan::VectorSearchScan { .. } => "SEARCH clause",
        _ => "operation",
    }
}

fn exec_subquery_expand(
    input: &LogicalPlan,
    from: &str,
    rel_var: &str,
    to: &str,
    edge_types: &[String],
    range: &Option<(Option<u32>, Option<u32>)>,
    direction: Direction,
    outer_rec: &Record,
    graph: &dyn GraphView,
) -> Result<Vec<Record>, CypherError> {
    let records = exec_subquery(input, outer_rec, graph)?;
    let mut result = Vec::new();
    for rec in &records {
        let from_id = get_node_id(rec, from)?;
        // edges used for uniqueness, except the one being expanded (pre-bound from outer scope it
        // constrains, not excludes)
        let mut used_edges = bound_edge_ids(rec);
        match rec.get(rel_var) {
            Some(CypherValue::Edge { id, .. }) | Some(CypherValue::EdgeRef { id, .. }) => {
                used_edges.remove(id);
            }
            _ => {}
        }
        // rel_var already bound to an edge: constrain to it
        let bound_rel_id = match rec.get(rel_var) {
            Some(CypherValue::Edge { id, .. }) | Some(CypherValue::EdgeRef { id, .. }) => Some(*id),
            _ => None,
        };
        // target already bound: constrain to it
        let bound_to_id = get_node_id_opt(rec, to);
        if let Some((min_opt, max_opt)) = range {
            let min = min_opt.unwrap_or(1) as usize;
            let max = max_opt.unwrap_or(10) as usize;
            let paths =
                find_variable_length_paths(graph, from_id, direction, edge_types, min, max)?;
            for (end_node_id, path_edges) in paths {
                if path_edges.iter().any(|ep| used_edges.contains(&ep.edge_id)) {
                    continue;
                }
                if let Some(bid) = bound_to_id
                    && end_node_id != bid
                {
                    continue;
                }
                let mut new_rec = rec.clone();
                {
                    let edge_list: Vec<CypherValue> = path_edges
                        .iter()
                        .map(|ep| CypherValue::EdgeRef {
                            id: ep.edge_id,
                            edge_type_id: ep.edge_type,
                            source: ep.source,
                            target: ep.target,
                        })
                        .collect();
                    new_rec.insert(rel_var.to_string(), CypherValue::List(edge_list));
                }
                new_rec.insert(to.to_string(), CypherValue::NodeRef(end_node_id));
                result.push(new_rec);
            }
        } else {
            let rel_key = rel_var.to_string();
            let to_key = to.to_string();
            let endpoints = graph
                .edges_with_neighbor(from_id, direction, None)
                .map_err(CypherError::from)?;
            let mut seen_edge_ids = FxHashSet::default();
            for ep in endpoints {
                if direction == Direction::Both && !seen_edge_ids.insert(ep.edge_id) {
                    continue;
                }
                if used_edges.contains(&ep.edge_id) {
                    continue;
                }
                // pre-bound relationship variable: constrain to that edge
                if let Some(bound_id) = bound_rel_id
                    && ep.edge_id != bound_id
                {
                    continue;
                }
                if !edge_types.is_empty() {
                    let etype = graph.strings().resolve(ep.edge_type.0);
                    if !edge_types.iter().any(|t| t == etype) {
                        continue;
                    }
                }
                if let Some(bid) = bound_to_id
                    && ep.neighbor != bid
                {
                    continue;
                }
                let mut new_rec = rec.clone();
                new_rec.insert(
                    rel_key.clone(),
                    CypherValue::EdgeRef {
                        id: ep.edge_id,
                        edge_type_id: ep.edge_type,
                        source: ep.source,
                        target: ep.target,
                    },
                );
                new_rec.insert(to_key.clone(), CypherValue::NodeRef(ep.neighbor));
                result.push(new_rec);
            }
        }
    }
    Ok(result)
}

pub(crate) fn get_node_id(rec: &Record, var: &str) -> Result<NodeId, CypherError> {
    match rec.get(var) {
        Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => Ok(*id),
        Some(other) => Err(CypherError::type_error(format!(
            "expected node for variable '{var}', got {}",
            other.type_name()
        ))),
        None => Err(CypherError::semantic(format!("variable '{var}' not found"))),
    }
}

/// `get_node_id`, but None when the variable is unbound instead of an error.
fn get_node_id_opt(rec: &Record, var: &str) -> Option<NodeId> {
    match rec.get(var) {
        Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => Some(*id),
        _ => None,
    }
}

fn dedup_records(records: Vec<Record>) -> Vec<Record> {
    let mut seen: FxHashSet<Vec<(String, JoinKey)>> = FxHashSet::default();
    let mut result = Vec::new();
    for rec in records {
        let mut key: Vec<(String, JoinKey)> = rec
            .iter()
            .map(|(k, v)| (k.clone(), cypher_to_join_key(v).unwrap_or(JoinKey::Null)))
            .collect();
        key.sort_by(|a, b| a.0.cmp(&b.0));
        if seen.insert(key) {
            result.push(rec);
        }
    }
    result
}

/// Batch-reads sort keys for NodeRef variables: plain property sorts (`c2.score`)
/// on NodeRef values (from expansions) read in bulk via
/// `batch_read_node_properties` under one lock, not N `get_node_property` calls.
/// Returns sort-item index -> values for all records.
fn batch_sort_keys_from_noderef(
    records: &[Record],
    items: &[SortItem],
    graph: &dyn GraphView,
) -> HashMap<usize, Vec<CypherValue>> {
    let mut result = HashMap::new();
    for (ki, item) in items.iter().enumerate() {
        if let Expr::Property(base, key) = &item.expr
            && let Expr::Variable(var) = base.as_ref()
        {
            // NodeRef variable? (first record decides)
            let is_noderef = records
                .first()
                .and_then(|r| r.get(var))
                .is_some_and(|v| matches!(v, CypherValue::NodeRef(_)));
            if !is_noderef {
                continue;
            }
            let pid = match graph.strings().get(key).map(PropertyKeyId) {
                Some(p) => p,
                None => {
                    // key never interned -> all Null
                    result.insert(ki, vec![CypherValue::Null; records.len()]);
                    continue;
                }
            };
            let node_ids: Vec<NodeId> = records
                .iter()
                .map(|r| match r.get(var) {
                    Some(CypherValue::NodeRef(id)) => *id,
                    _ => NodeId(u64::MAX),
                })
                .collect();
            let keys_arr = [pid];
            let mut out = vec![None; node_ids.len()];
            if graph
                .batch_read_node_properties(&node_ids, &keys_arr, &mut out)
                .is_ok()
            {
                let values: Vec<CypherValue> = out
                    .into_iter()
                    .map(|pv| {
                        pv.map(|p| CypherValue::from_property(&p))
                            .unwrap_or(CypherValue::Null)
                    })
                    .collect();
                result.insert(ki, values);
            }
        }
    }
    result
}

/// Sort key from a Record. Plain `p.key` reads embedded `CypherValue::Node`/`Edge`/`Map` properties
/// directly (no SymbolTable/Frame per record); complex expressions use eval_expr with the pre-built
/// `symtab`.
fn eval_sort_key_fast(
    expr: &Expr,
    rec: &Record,
    graph: &dyn GraphView,
    symtab: Option<&SymbolTable>,
) -> CypherValue {
    match expr {
        Expr::Property(base, key) => {
            if let Expr::Variable(var) = base.as_ref() {
                if let Some(val) = rec.get(var) {
                    return match val {
                        CypherValue::Node { properties, .. } => properties
                            .get(key.as_str())
                            .map(CypherValue::from_property)
                            .unwrap_or(CypherValue::Null),
                        CypherValue::Edge { properties, .. } => properties
                            .get(key.as_str())
                            .map(CypherValue::from_property)
                            .unwrap_or(CypherValue::Null),
                        CypherValue::Map(m) => m.get(key).cloned().unwrap_or(CypherValue::Null),
                        CypherValue::NodeRef(id) => graph
                            .get_node_property(*id, key)
                            .ok()
                            .flatten()
                            .map(|pv| CypherValue::from_property(&pv))
                            .unwrap_or(CypherValue::Null),
                        CypherValue::EdgeRef { id, .. } => graph
                            .get_edge_property(*id, key)
                            .ok()
                            .flatten()
                            .map(|pv| CypherValue::from_property(&pv))
                            .unwrap_or(CypherValue::Null),
                        _ => CypherValue::Null,
                    };
                }
                return CypherValue::Null;
            }
        }
        Expr::Variable(var) => {
            return rec.get(var).cloned().unwrap_or(CypherValue::Null);
        }
        _ => {}
    }
    // fallback: full evaluation with the pre-built symtab
    if let Some(st) = symtab {
        let frame = frame::record_to_frame(rec, st);
        eval_expr(expr, &frame, graph, st).unwrap_or(CypherValue::Null)
    } else {
        eval_expr_rec(expr, rec, graph).unwrap_or(CypherValue::Null)
    }
}
