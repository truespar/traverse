//! Aggregate pushdown past a terminal expand: neighbors iterated inline instead of a frame
//! per edge, and the two-pass fan-in for 2-hop counts.

use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};
use traverse_core::graph::property::IndexKey;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{Direction, EdgeId, LabelId, NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::ast::{BinOp, CmpOp, Expr, Literal, ProjectionItem};
use crate::bytecode;
use crate::error::CypherError;
use crate::executor::{
    JoinKey, collect_prior_edge_ids, cypher_to_join_key, eval_expr, get_node_id_frame,
    get_query_param, is_truthy, projection_name,
};
use crate::frame::{Frame, SlotValue, SymbolTable, new_frame};
use crate::planner::{LogicalPlan, is_aggregate_function};
use crate::types::CypherValue;

use super::super::batch_types::{PropJoinKey, prop_to_cypher, prop_to_join_key};
use super::super::expand::{bitvec_test, build_in_bitvec, build_not_in_bitvec};

/// Aggregate kind for the expand pushdown.
enum ExpandAggKind {
    /// count(DISTINCT to_var), via HashSet<NodeId>.
    CountDistinctTo,
    /// count(*), count(from_var), count(to_var), count(rel_var): plain counter.
    TotalCount,
    /// count(DISTINCT rel.property), via HashSet<CypherValue>.
    CountDistinctRelProperty { prop_key: String },
    /// sum(var.prop) as f64.
    Sum { var: String, prop_key: String },
    /// min(var.prop) as f64.
    Min { var: String, prop_key: String },
    /// max(var.prop) as f64.
    Max { var: String, prop_key: String },
    /// collect(var.prop) / collect(var) into a list.
    Collect {
        var: String,
        prop_key: Option<String>,
    },
}

/// Per-group accumulators for grouped expand pushdown.
struct GroupAccumulators {
    distinct_set: FxHashSet<NodeId>,
    total_count: i64,
    distinct_rel_props: FxHashMap<usize, FxHashSet<PropJoinKey>>,
    sum_accum: FxHashMap<usize, f64>,
    min_accum: FxHashMap<usize, Option<f64>>,
    max_accum: FxHashMap<usize, Option<f64>>,
    collect_accum: FxHashMap<usize, Vec<CypherValue>>,
}

impl GroupAccumulators {
    fn new(agg_kinds: &[(usize, ExpandAggKind)]) -> Self {
        let mut distinct_rel_props = FxHashMap::default();
        let mut collect_accum = FxHashMap::default();
        for (idx, kind) in agg_kinds {
            if matches!(kind, ExpandAggKind::CountDistinctRelProperty { .. }) {
                distinct_rel_props.insert(*idx, FxHashSet::default());
            }
            if matches!(kind, ExpandAggKind::Collect { .. }) {
                collect_accum.insert(*idx, Vec::new());
            }
        }
        Self {
            distinct_set: FxHashSet::default(),
            total_count: 0,
            distinct_rel_props,
            sum_accum: FxHashMap::default(),
            min_accum: FxHashMap::default(),
            max_accum: FxHashMap::default(),
            collect_accum,
        }
    }
}

/// Accumulator -> final CypherValue.
fn resolve_agg_value(
    idx: &usize,
    kind: &ExpandAggKind,
    distinct_set: &FxHashSet<NodeId>,
    total_count: i64,
    distinct_rel_props: &FxHashMap<usize, FxHashSet<PropJoinKey>>,
    sum_accum: &FxHashMap<usize, f64>,
    min_accum: &FxHashMap<usize, Option<f64>>,
    max_accum: &FxHashMap<usize, Option<f64>>,
    collect_accum: &FxHashMap<usize, Vec<CypherValue>>,
    avg_indices: &FxHashSet<usize>,
) -> CypherValue {
    match kind {
        ExpandAggKind::CountDistinctTo => CypherValue::Integer(distinct_set.len() as i64),
        ExpandAggKind::TotalCount => CypherValue::Integer(total_count),
        ExpandAggKind::CountDistinctRelProperty { .. } => {
            CypherValue::Integer(distinct_rel_props.get(idx).map_or(0, |s| s.len()) as i64)
        }
        ExpandAggKind::Sum { .. } => {
            if avg_indices.contains(idx) {
                if total_count > 0 {
                    CypherValue::Float(
                        sum_accum.get(idx).copied().unwrap_or(0.0) / total_count as f64,
                    )
                } else {
                    CypherValue::Null
                }
            } else {
                let s = sum_accum.get(idx).copied().unwrap_or(0.0);
                if s.fract() == 0.0 && s.abs() < i64::MAX as f64 {
                    CypherValue::Integer(s as i64)
                } else {
                    CypherValue::Float(s)
                }
            }
        }
        ExpandAggKind::Min { .. } => match min_accum.get(idx).and_then(|v| *v) {
            Some(v) if v.fract() == 0.0 && v.abs() < i64::MAX as f64 => {
                CypherValue::Integer(v as i64)
            }
            Some(v) => CypherValue::Float(v),
            None => CypherValue::Null,
        },
        ExpandAggKind::Max { .. } => match max_accum.get(idx).and_then(|v| *v) {
            Some(v) if v.fract() == 0.0 && v.abs() < i64::MAX as f64 => {
                CypherValue::Integer(v as i64)
            }
            Some(v) => CypherValue::Float(v),
            None => CypherValue::Null,
        },
        ExpandAggKind::Collect { .. } => {
            CypherValue::List(collect_accum.get(idx).cloned().unwrap_or_default())
        }
    }
}

/// Compound-expression sub-index -> CypherValue.
fn resolve_sub_agg_value(
    sub_idx: &usize,
    agg_kinds: &[(usize, ExpandAggKind)],
    distinct_set: &FxHashSet<NodeId>,
    total_count: i64,
    distinct_rel_props: &FxHashMap<usize, FxHashSet<PropJoinKey>>,
    sum_accum: &FxHashMap<usize, f64>,
    min_accum: &FxHashMap<usize, Option<f64>>,
    max_accum: &FxHashMap<usize, Option<f64>>,
    collect_accum: &FxHashMap<usize, Vec<CypherValue>>,
) -> CypherValue {
    for (idx, kind) in agg_kinds {
        if idx == sub_idx {
            // sub-values never use avg (avg_indices empty)
            let empty = FxHashSet::default();
            return resolve_agg_value(
                idx,
                kind,
                distinct_set,
                total_count,
                distinct_rel_props,
                sum_accum,
                min_accum,
                max_accum,
                collect_accum,
                &empty,
            );
        }
    }
    CypherValue::Null
}

/// Evaluates a compound binary-op expression from its resolved sub-aggregates.
fn eval_compound_agg(op: &BinOp, left_val: CypherValue, right_val: CypherValue) -> CypherValue {
    match (left_val, right_val) {
        (CypherValue::Integer(l), CypherValue::Integer(r)) => match op {
            BinOp::Add => CypherValue::Integer(l + r),
            BinOp::Sub => CypherValue::Integer(l - r),
            BinOp::Mul => CypherValue::Integer(l * r),
            BinOp::Div if r != 0 => CypherValue::Float(l as f64 / r as f64),
            _ => CypherValue::Null,
        },
        (CypherValue::Float(l), CypherValue::Float(r)) => match op {
            BinOp::Add => CypherValue::Float(l + r),
            BinOp::Sub => CypherValue::Float(l - r),
            BinOp::Mul => CypherValue::Float(l * r),
            BinOp::Div if r != 0.0 => CypherValue::Float(l / r),
            _ => CypherValue::Null,
        },
        (CypherValue::Integer(l), CypherValue::Float(r)) => match op {
            BinOp::Add => CypherValue::Float(l as f64 + r),
            BinOp::Sub => CypherValue::Float(l as f64 - r),
            BinOp::Mul => CypherValue::Float(l as f64 * r),
            BinOp::Div if r != 0.0 => CypherValue::Float(l as f64 / r),
            _ => CypherValue::Null,
        },
        (CypherValue::Float(l), CypherValue::Integer(r)) => match op {
            BinOp::Add => CypherValue::Float(l + r as f64),
            BinOp::Sub => CypherValue::Float(l - r as f64),
            BinOp::Mul => CypherValue::Float(l * r as f64),
            BinOp::Div if r != 0 => CypherValue::Float(l / r as f64),
            _ => CypherValue::Null,
        },
        _ => CypherValue::Null,
    }
}

/// Updates accumulators for one passing edge (grouped and ungrouped paths).
#[inline(always)]
fn update_accumulators(
    idx: usize,
    kind: &ExpandAggKind,
    edge_id: EdgeId,
    neighbor: NodeId,
    from_id: NodeId,
    to: &str,
    not_in_rel: &Option<(String, FxHashSet<String>)>,
    not_in_prop_val: &mut Option<Option<PropertyValue>>,
    distinct_rel_props: &mut FxHashMap<usize, FxHashSet<PropJoinKey>>,
    sum_accum: &mut FxHashMap<usize, f64>,
    min_accum: &mut FxHashMap<usize, Option<f64>>,
    max_accum: &mut FxHashMap<usize, Option<f64>>,
    collect_accum: &mut FxHashMap<usize, Vec<CypherValue>>,
    resolved_prop_keys: &FxHashMap<usize, PropertyKeyId>,
    graph: &dyn GraphView,
) -> Result<(), CypherError> {
    match kind {
        ExpandAggKind::CountDistinctRelProperty { prop_key } => {
            let pv = if not_in_rel.as_ref().is_some_and(|(k, _)| k == prop_key) {
                not_in_prop_val.take().unwrap_or_else(|| {
                    graph
                        .get_edge_property(edge_id, prop_key)
                        .ok()
                        .flatten()
                        .map(Some)
                        .unwrap_or(None)
                })
            } else {
                graph
                    .get_edge_property(edge_id, prop_key)
                    .map_err(CypherError::from)?
            };
            let key = prop_to_join_key(&pv);
            if !matches!(key, PropJoinKey::Null) {
                distinct_rel_props.get_mut(&idx).unwrap().insert(key);
            }
        }
        ExpandAggKind::Sum { var, .. }
        | ExpandAggKind::Min { var, .. }
        | ExpandAggKind::Max { var, .. } => {
            let node_id = if var == to { neighbor } else { from_id };
            let pv = if let Some(&kid) = resolved_prop_keys.get(&idx) {
                graph
                    .get_node_property_kid(node_id, kid)
                    .map_err(CypherError::from)?
            } else {
                None
            };
            let f = match pv {
                Some(PropertyValue::Int(v)) => Some(v as f64),
                Some(PropertyValue::Float(v)) => Some(v),
                _ => None,
            };
            if let Some(f) = f {
                match kind {
                    ExpandAggKind::Sum { .. } => {
                        *sum_accum.entry(idx).or_insert(0.0) += f;
                    }
                    ExpandAggKind::Min { .. } => {
                        let entry = min_accum.entry(idx).or_insert(None);
                        *entry = Some(match *entry {
                            Some(cur) => cur.min(f),
                            None => f,
                        });
                    }
                    ExpandAggKind::Max { .. } => {
                        let entry = max_accum.entry(idx).or_insert(None);
                        *entry = Some(match *entry {
                            Some(cur) => cur.max(f),
                            None => f,
                        });
                    }
                    _ => {}
                }
            }
        }
        ExpandAggKind::Collect { var, prop_key } => {
            let node_id = if var == to { neighbor } else { from_id };
            match prop_key {
                Some(_) => {
                    let pv = if let Some(&kid) = resolved_prop_keys.get(&idx) {
                        graph
                            .get_node_property_kid(node_id, kid)
                            .map_err(CypherError::from)?
                    } else {
                        None
                    };
                    // collect() skips nulls
                    if let Some(pv) = pv
                        && !matches!(pv, PropertyValue::Null)
                    {
                        collect_accum
                            .entry(idx)
                            .or_default()
                            .push(prop_to_cypher(&pv));
                    }
                }
                None => {
                    collect_accum
                        .entry(idx)
                        .or_default()
                        .push(CypherValue::NodeRef(node_id));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// AND-conjuncts of an expression (BinaryOp::And split recursively).
#[recursive::recursive]
fn flatten_and_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryOp(left, BinOp::And, right) => {
            let mut out = flatten_and_conjuncts(left);
            out.extend(flatten_and_conjuncts(right));
            out
        }
        _ => vec![expr],
    }
}

/// Extracts `__has_label(to_var, "Label")` conjuncts (the expand target only): returns the labels
/// and removes those conjuncts from `filter_preds`.
fn extract_has_label_for_to(filter_preds: &mut Vec<&Expr>, to_var: &str) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    let mut i = 0;
    while i < filter_preds.len() {
        let conjuncts = flatten_and_conjuncts(filter_preds[i]);
        let mut found = Vec::new();
        for (ci, conjunct) in conjuncts.iter().enumerate() {
            if let Expr::FunctionCall { name, args, .. } = conjunct
                && name == "__has_label"
                && args.len() == 2
                && let (Expr::Variable(var), Expr::Literal(Literal::String(label))) =
                    (&args[0], &args[1])
                && var == to_var
            {
                labels.push(label.clone());
                found.push(ci);
            }
        }
        if !found.is_empty() {
            // drop the found conjuncts, rebuild from the rest
            let remaining: Vec<&Expr> = conjuncts
                .iter()
                .enumerate()
                .filter(|(j, _)| !found.contains(j))
                .map(|(_, c)| *c)
                .collect();
            if remaining.is_empty() {
                filter_preds.remove(i);
            } else {
                // rebuild the predicate without has_label conjuncts; filter_preds holds references,
                // so leak a Box for a 'static owned Expr (callers never outlive the query)
                let mut rebuilt = remaining[0].clone();
                for c in &remaining[1..] {
                    rebuilt = Expr::BinaryOp(Box::new(rebuilt), BinOp::And, Box::new((*c).clone()));
                }
                filter_preds[i] = Box::leak(Box::new(rebuilt));
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    labels
}

/// Extracts `to_var.prop <> literal` and resolves it via the property index to excluded NodeIds;
/// removes the conjunct(s).
fn extract_node_exclusions(
    filter_preds: &mut Vec<&Expr>,
    to_var: &str,
    to_labels: &[String],
    graph: &dyn GraphView,
) -> FxHashSet<NodeId> {
    let mut excluded = FxHashSet::default();
    let mut i = 0;
    while i < filter_preds.len() {
        let conjuncts = flatten_and_conjuncts(filter_preds[i]);
        let mut found = Vec::new();
        for (ci, conjunct) in conjuncts.iter().enumerate() {
            // to_var.prop <> literal
            if let Expr::Comparison(lhs, cmps) = conjunct
                && cmps.len() == 1
                && cmps[0].0 == CmpOp::Neq
                && let Expr::Property(base, prop_key) = lhs.as_ref()
                && let Expr::Variable(var) = base.as_ref()
                && var == to_var
            {
                // literal -> IndexKey
                let idx_key = match &cmps[0].1 {
                    Expr::Literal(Literal::Integer(v)) => Some(IndexKey::Int(*v)),
                    Expr::Literal(Literal::String(v)) => Some(IndexKey::String(v.clone())),
                    Expr::Literal(Literal::Float(v)) => Some(IndexKey::Float(*v)),
                    Expr::Literal(Literal::Bool(v)) => Some(IndexKey::Bool(*v)),
                    Expr::Parameter(name) => match get_query_param(name) {
                        Some(CypherValue::Integer(v)) => Some(IndexKey::Int(v)),
                        Some(CypherValue::String(v)) => Some(IndexKey::String(v)),
                        Some(CypherValue::Float(v)) => Some(IndexKey::Float(v)),
                        Some(CypherValue::Bool(v)) => Some(IndexKey::Bool(v)),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(key) = idx_key {
                    // index lookup per label
                    let mut resolved = false;
                    for label in to_labels {
                        if let Some(ids) = graph.index_lookup(label, prop_key, &key) {
                            excluded.extend(ids);
                            resolved = true;
                        }
                    }
                    if resolved {
                        found.push(ci);
                    }
                }
            }
        }
        if !found.is_empty() {
            let remaining: Vec<&Expr> = conjuncts
                .iter()
                .enumerate()
                .filter(|(j, _)| !found.contains(j))
                .map(|(_, c)| *c)
                .collect();
            if remaining.is_empty() {
                filter_preds.remove(i);
            } else {
                let mut rebuilt = remaining[0].clone();
                for c in &remaining[1..] {
                    rebuilt = Expr::BinaryOp(Box::new(rebuilt), BinOp::And, Box::new((*c).clone()));
                }
                filter_preds[i] = Box::leak(Box::new(rebuilt));
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    excluded
}

/// Extracts `NOT rel_var.prop IN [values]` -> `(prop_key, excluded_values)`, removing the conjunct;
/// values resolved from parameters if needed.
fn extract_not_in_rel_prop(
    filter_preds: &mut Vec<&Expr>,
    rel_var: &str,
) -> Option<(String, FxHashSet<String>)> {
    for i in 0..filter_preds.len() {
        let conjuncts = flatten_and_conjuncts(filter_preds[i]);
        for (ci, conjunct) in conjuncts.iter().enumerate() {
            if let Some((var, prop_key, values)) = bytecode::extract_not_in_str_list(conjunct)
                && var == rel_var
            {
                let excluded: FxHashSet<String> = values.into_iter().collect();
                // drop this conjunct from filter_preds
                let remaining: Vec<&Expr> = conjuncts
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != ci)
                    .map(|(_, c)| *c)
                    .collect();
                if remaining.is_empty() {
                    filter_preds.remove(i);
                } else {
                    let mut rebuilt = remaining[0].clone();
                    for c in &remaining[1..] {
                        rebuilt =
                            Expr::BinaryOp(Box::new(rebuilt), BinOp::And, Box::new((*c).clone()));
                    }
                    filter_preds[i] = Box::leak(Box::new(rebuilt));
                }
                return Some((prop_key.to_string(), excluded));
            }
        }
    }
    None
}

/// Extracts positive `rel_var.prop IN [values]` -> `(prop_key, included_values)`, removing the
/// conjunct.
fn extract_in_rel_prop(
    filter_preds: &mut Vec<&Expr>,
    rel_var: &str,
) -> Option<(String, FxHashSet<String>)> {
    for i in 0..filter_preds.len() {
        let conjuncts = flatten_and_conjuncts(filter_preds[i]);
        for (ci, conjunct) in conjuncts.iter().enumerate() {
            if let Some((var, prop_key, values)) = bytecode::extract_in_str_list(conjunct)
                .or_else(|| bytecode::extract_or_eq_as_in_str_list(conjunct))
                && var == rel_var
            {
                let included: FxHashSet<String> = values.into_iter().collect();
                let remaining: Vec<&Expr> = conjuncts
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != ci)
                    .map(|(_, c)| *c)
                    .collect();
                if remaining.is_empty() {
                    filter_preds.remove(i);
                } else {
                    let mut rebuilt = remaining[0].clone();
                    for c in &remaining[1..] {
                        rebuilt =
                            Expr::BinaryOp(Box::new(rebuilt), BinOp::And, Box::new((*c).clone()));
                    }
                    filter_preds[i] = Box::leak(Box::new(rebuilt));
                }
                return Some((prop_key.to_string(), included));
            }
        }
    }
    None
}

/// Pushes the aggregate past a terminal expand, without frames. For
/// `Aggregate(Filter*(Expand(upstream)), keys=[], [count(DISTINCT to_var), ...])`
/// it iterates neighbors inline with one probe frame for filter evaluation
/// instead of materializing a frame per edge.
#[inline(never)]
pub(super) fn try_expand_aggregate_pushdown(
    input: &LogicalPlan,
    keys: &[ProjectionItem],
    aggs: &[ProjectionItem],
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Option<Vec<Frame>>, CypherError> {
    let has_group_by = !keys.is_empty();

    // 1. walk the Filter chain to a terminal Expand
    let mut filter_preds: Vec<&Expr> = Vec::new();
    let mut current = input;
    while let LogicalPlan::Filter {
        input: inner,
        predicate,
    } = current
    {
        filter_preds.push(predicate);
        current = inner.as_ref();
    }

    // must be a fixed-length Expand
    let (expand_input, from, rel_var, to, edge_types, range, prior_edge_vars, direction) =
        match current {
            LogicalPlan::ExpandOut {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } => (
                input.as_ref(),
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
                input.as_ref(),
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
                input.as_ref(),
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
                Direction::Both,
            ),
            _ => return Ok(None),
        };

    // no variable length
    if range.is_some() {
        return Ok(None);
    }

    // 2a. 2-hop: expand_input is itself a fixed-length Expand; inline both hops so the intermediate
    // frames are never built
    struct InnerExpand<'a> {
        actual_upstream: &'a LogicalPlan,
        inner_from: &'a str,
        inner_edge_types: &'a [String],
        direction: Direction,
    }

    let inner_expand_info: Option<InnerExpand<'_>> = {
        // peel filters between the outer and inner expand
        let mut inner_current = expand_input;
        let mut has_mid_filters = false;
        while let LogicalPlan::Filter { input, .. } = inner_current {
            has_mid_filters = true;
            inner_current = input.as_ref();
        }

        // only the clean case (no filters in between)
        if has_mid_filters {
            None
        } else {
            match inner_current {
                LogicalPlan::ExpandOut {
                    input,
                    from,
                    edge_types,
                    range,
                    prior_edge_vars,
                    ..
                } if range.is_none() && prior_edge_vars.is_empty() => Some(InnerExpand {
                    actual_upstream: input.as_ref(),
                    inner_from: from,
                    inner_edge_types: edge_types,
                    direction: Direction::Outgoing,
                }),
                LogicalPlan::ExpandIn {
                    input,
                    from,
                    edge_types,
                    range,
                    prior_edge_vars,
                    ..
                } if range.is_none() && prior_edge_vars.is_empty() => Some(InnerExpand {
                    actual_upstream: input.as_ref(),
                    inner_from: from,
                    inner_edge_types: edge_types,
                    direction: Direction::Incoming,
                }),
                LogicalPlan::ExpandBoth {
                    input,
                    from,
                    edge_types,
                    range,
                    prior_edge_vars,
                    ..
                } if range.is_none() && prior_edge_vars.is_empty() => Some(InnerExpand {
                    actual_upstream: input.as_ref(),
                    inner_from: from,
                    inner_edge_types: edge_types,
                    direction: Direction::Both,
                }),
                _ => None,
            }
        }
    };

    // 3. validate aggregates
    let mut agg_kinds: Vec<(usize, ExpandAggKind)> = Vec::new();
    // compound aggs: (output_idx, op, left_sub_idx, right_sub_idx)
    let mut compound_agg_exprs: Vec<(usize, BinOp, usize, usize)> = Vec::new();
    for (idx, item) in aggs.iter().enumerate() {
        match &item.expr {
            Expr::FunctionCall {
                name,
                args,
                distinct,
            } if is_aggregate_function(name) => {
                match name.as_str() {
                    "count" => {
                        if *distinct {
                            // count(DISTINCT x): x must be Var(to_var)
                            if args.len() == 1 {
                                if let Expr::Variable(v) = &args[0] {
                                    if v == to {
                                        agg_kinds.push((idx, ExpandAggKind::CountDistinctTo));
                                    } else {
                                        return Ok(None);
                                    }
                                } else if let Expr::Property(base, prop_key) = &args[0] {
                                    // count(DISTINCT rel.property)
                                    if let Expr::Variable(v) = base.as_ref() {
                                        if v == rel_var {
                                            agg_kinds.push((
                                                idx,
                                                ExpandAggKind::CountDistinctRelProperty {
                                                    prop_key: prop_key.clone(),
                                                },
                                            ));
                                        } else {
                                            return Ok(None);
                                        }
                                    } else {
                                        return Ok(None);
                                    }
                                } else {
                                    return Ok(None);
                                }
                            } else {
                                return Ok(None);
                            }
                        } else {
                            // count(*), count(var)
                            if args.is_empty() {
                                agg_kinds.push((idx, ExpandAggKind::TotalCount));
                            } else if args.len() == 1 {
                                if let Expr::Variable(v) = &args[0] {
                                    if v == from || v == to || v == rel_var {
                                        agg_kinds.push((idx, ExpandAggKind::TotalCount));
                                    } else {
                                        return Ok(None);
                                    }
                                } else {
                                    return Ok(None);
                                }
                            } else {
                                return Ok(None);
                            }
                        }
                    }
                    "sum" => {
                        if args.len() == 1 {
                            if let Expr::Property(base, prop_key) = &args[0] {
                                if let Expr::Variable(v) = base.as_ref() {
                                    if v == to || v == from {
                                        agg_kinds.push((
                                            idx,
                                            ExpandAggKind::Sum {
                                                var: v.clone(),
                                                prop_key: prop_key.clone(),
                                            },
                                        ));
                                    } else {
                                        return Ok(None);
                                    }
                                } else {
                                    return Ok(None);
                                }
                            } else {
                                return Ok(None);
                            }
                        } else {
                            return Ok(None);
                        }
                    }
                    "min" => {
                        if args.len() == 1 {
                            if let Expr::Property(base, prop_key) = &args[0] {
                                if let Expr::Variable(v) = base.as_ref() {
                                    if v == to || v == from {
                                        agg_kinds.push((
                                            idx,
                                            ExpandAggKind::Min {
                                                var: v.clone(),
                                                prop_key: prop_key.clone(),
                                            },
                                        ));
                                    } else {
                                        return Ok(None);
                                    }
                                } else {
                                    return Ok(None);
                                }
                            } else {
                                return Ok(None);
                            }
                        } else {
                            return Ok(None);
                        }
                    }
                    "max" => {
                        if args.len() == 1 {
                            if let Expr::Property(base, prop_key) = &args[0] {
                                if let Expr::Variable(v) = base.as_ref() {
                                    if v == to || v == from {
                                        agg_kinds.push((
                                            idx,
                                            ExpandAggKind::Max {
                                                var: v.clone(),
                                                prop_key: prop_key.clone(),
                                            },
                                        ));
                                    } else {
                                        return Ok(None);
                                    }
                                } else {
                                    return Ok(None);
                                }
                            } else {
                                return Ok(None);
                            }
                        } else {
                            return Ok(None);
                        }
                    }
                    "avg" => {
                        // avg(var.prop) -> Sum + TotalCount, avg = sum / count at the end
                        if args.len() == 1 {
                            if let Expr::Property(base, prop_key) = &args[0] {
                                if let Expr::Variable(v) = base.as_ref() {
                                    if v == to || v == from {
                                        agg_kinds.push((
                                            idx,
                                            ExpandAggKind::Sum {
                                                var: v.clone(),
                                                prop_key: prop_key.clone(),
                                            },
                                        ));
                                        // mark for avg (sum/count); TotalCount is tracked anyway
                                    } else {
                                        return Ok(None);
                                    }
                                } else {
                                    return Ok(None);
                                }
                            } else {
                                return Ok(None);
                            }
                        } else {
                            return Ok(None);
                        }
                    }
                    "collect" => {
                        if *distinct {
                            return Ok(None); // collect(DISTINCT ...) unsupported
                        }
                        if args.len() == 1 {
                            if let Expr::Variable(v) = &args[0] {
                                if v == to || v == from {
                                    agg_kinds.push((
                                        idx,
                                        ExpandAggKind::Collect {
                                            var: v.clone(),
                                            prop_key: None,
                                        },
                                    ));
                                } else {
                                    return Ok(None);
                                }
                            } else if let Expr::Property(base, prop_key) = &args[0] {
                                if let Expr::Variable(v) = base.as_ref() {
                                    if v == to || v == from {
                                        agg_kinds.push((
                                            idx,
                                            ExpandAggKind::Collect {
                                                var: v.clone(),
                                                prop_key: Some(prop_key.clone()),
                                            },
                                        ));
                                    } else {
                                        return Ok(None);
                                    }
                                } else {
                                    return Ok(None);
                                }
                            } else {
                                return Ok(None);
                            }
                        } else {
                            return Ok(None);
                        }
                    }
                    _ => return Ok(None),
                }
            }
            // compound expressions like max(...) - min(...): extract the aggregates, do the
            // arithmetic at result assembly
            Expr::BinaryOp(left, op, right) => {
                let extract_agg_fn = |expr: &Expr| -> Option<ExpandAggKind> {
                    if let Expr::FunctionCall { name, args, .. } = expr
                        && is_aggregate_function(name)
                        && args.len() == 1
                        && let Expr::Property(base, prop_key) = &args[0]
                        && let Expr::Variable(v) = base.as_ref()
                        && (v == to || v == from)
                    {
                        return match name.as_str() {
                            "sum" => Some(ExpandAggKind::Sum {
                                var: v.clone(),
                                prop_key: prop_key.clone(),
                            }),
                            "min" => Some(ExpandAggKind::Min {
                                var: v.clone(),
                                prop_key: prop_key.clone(),
                            }),
                            "max" => Some(ExpandAggKind::Max {
                                var: v.clone(),
                                prop_key: prop_key.clone(),
                            }),
                            "count" => Some(ExpandAggKind::TotalCount),
                            _ => None,
                        };
                    }
                    None
                };
                match (extract_agg_fn(left), extract_agg_fn(right)) {
                    (Some(left_kind), Some(right_kind)) => {
                        // synthetic sub-indices: left idx*2+1000, right idx*2+1001
                        let left_sub = idx * 2 + 1000;
                        let right_sub = idx * 2 + 1001;
                        agg_kinds.push((left_sub, left_kind));
                        agg_kinds.push((right_sub, right_kind));
                        compound_agg_exprs.push((idx, *op, left_sub, right_sub));
                    }
                    _ => return Ok(None),
                }
            }
            _ => return Ok(None),
        }
    }

    // avg indices (stored as Sum, need sum/count at the end)
    let avg_indices: FxHashSet<usize> = aggs
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            if let Expr::FunctionCall { name, .. } = &item.expr {
                if name == "avg" { Some(idx) } else { None }
            } else {
                None
            }
        })
        .collect();

    // 4. run the upstream plan; for 2-hop only the inner expand's upstream (the Scan), so
    //    intermediate frames never exist
    let actual_upstream = if let Some(ref info) = inner_expand_info {
        info.actual_upstream
    } else {
        expand_input
    };
    let upstream_frames = crate::operators::exec_via_operators(actual_upstream, graph, symtab)?;

    // 5. slots
    let from_slot = match symtab.slot(from) {
        Some(s) => s,
        None => return Ok(None),
    };
    let rel_slot = symtab.slot(rel_var).unwrap_or(usize::MAX);
    let to_slot = match symtab.slot(to) {
        Some(s) => s,
        None => return Ok(None),
    };
    // cyclic `MATCH (n)-[r]->(n)`: same variable on both ends; endpoint iteration doesn't know
    // that, so filter ep.neighbor == expand_from_id
    let is_cyclic_endpoint = from_slot == to_slot;
    let prior_edge_slots: Vec<usize> = prior_edge_vars
        .iter()
        .filter_map(|v| symtab.slot(v))
        .collect();

    // 5b. has_label(to_var) from the filter, checked inline instead of bytecode
    let to_labels = extract_has_label_for_to(&mut filter_preds, to);
    let to_label_ids: Vec<LabelId> = to_labels
        .iter()
        .filter_map(|l| graph.strings().get(l).map(LabelId))
        .collect();
    // a label that can't be resolved doesn't exist in the graph: every row fails, count = 0
    if to_label_ids.len() != to_labels.len() {
        let mut out = new_frame(symtab);
        for (idx, kind) in &agg_kinds {
            if *idx >= 1000 {
                continue;
            } // skip synthetic sub-indices
            let val = match kind {
                ExpandAggKind::CountDistinctTo => CypherValue::Integer(0),
                ExpandAggKind::TotalCount => CypherValue::Integer(0),
                ExpandAggKind::CountDistinctRelProperty { .. } => CypherValue::Integer(0),
                ExpandAggKind::Sum { .. } => {
                    if avg_indices.contains(idx) {
                        CypherValue::Null
                    } else {
                        CypherValue::Integer(0)
                    }
                }
                ExpandAggKind::Min { .. } | ExpandAggKind::Max { .. } => CypherValue::Null,
                ExpandAggKind::Collect { .. } => CypherValue::List(Vec::new()),
            };
            let name = projection_name(&aggs[*idx]);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = val.into();
            }
        }
        // compound aggs: null sub-aggs -> null result
        for (out_idx, _, _, _) in &compound_agg_exprs {
            let name = projection_name(&aggs[*out_idx]);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = CypherValue::Null.into();
            }
        }
        return Ok(Some(vec![out]));
    }

    // 5c. to_var.prop <> literal -> excluded NodeIds via index
    let excluded_nodes = extract_node_exclusions(&mut filter_preds, to, &to_labels, &*graph);

    // 5d. NOT rel.prop IN [values], evaluated inline reading the edge property directly (no
    // bytecode)
    let not_in_rel = extract_not_in_rel_prop(&mut filter_preds, rel_var);

    // 5e. positive rel.prop IN [values], inline
    let in_rel = extract_in_rel_prop(&mut filter_preds, rel_var);

    // bitvec from edge indexes for the NOT IN filter, unless a CountDistinctRelProperty aggregate
    // reads that property per edge anyway (then reuse the read)
    let needs_rel_prop_read = not_in_rel.as_ref().is_some_and(|(pk, _)| {
        agg_kinds.iter().any(|(_, k)| matches!(k, ExpandAggKind::CountDistinctRelProperty { prop_key } if prop_key == pk))
    });
    let not_in_bitvec: Option<Rc<Vec<u64>>> = if needs_rel_prop_read {
        None
    } else {
        not_in_rel
            .as_ref()
            .and_then(|(pk, vals)| build_not_in_bitvec(edge_types, pk, vals, &*graph))
    };

    // bitvec from edge indexes for the positive IN filter
    let in_bitvec: Option<Rc<Vec<u64>>> = in_rel
        .as_ref()
        .and_then(|(pk, vals)| build_in_bitvec(edge_types, pk, vals, &*graph));

    // 6. compile the remaining filters ANDed together
    let combined_pred: Option<Expr> = if filter_preds.is_empty() {
        None
    } else {
        // AND them (order doesn't matter)
        let mut combined = filter_preds[0].clone();
        for pred in &filter_preds[1..] {
            combined = Expr::BinaryOp(Box::new(combined), BinOp::And, Box::new((*pred).clone()));
        }
        Some(combined)
    };

    let compiled = combined_pred
        .as_ref()
        .and_then(|pred| bytecode::try_compile(pred, symtab));

    // 7. per-type edge filters
    let type_filters: Vec<Option<&str>> = if edge_types.is_empty() {
        vec![None]
    } else {
        edge_types.iter().map(|t| Some(t.as_str())).collect()
    };

    // 7b. inner expand setup for 2-hop
    let inner_from_slot = inner_expand_info
        .as_ref()
        .and_then(|info| symtab.slot(info.inner_from));
    let inner_type_filters: Vec<Option<&str>> =
        inner_expand_info.as_ref().map_or_else(Vec::new, |info| {
            if info.inner_edge_types.is_empty() {
                vec![None]
            } else {
                info.inner_edge_types
                    .iter()
                    .map(|t| Some(t.as_str()))
                    .collect()
            }
        });
    let inner_direction = inner_expand_info.as_ref().map(|info| info.direction);

    // 8. inline expand + filter + aggregate; grouped state (only with GROUP BY)
    let mut groups: FxHashMap<Vec<JoinKey>, (Vec<CypherValue>, GroupAccumulators)> =
        FxHashMap::default();

    // ungrouped accumulators (only without GROUP BY)
    let mut distinct_set: FxHashSet<NodeId> = FxHashSet::default();
    let mut total_count: i64 = 0;
    // per-property distinct sets for CountDistinctRelProperty
    let mut distinct_rel_props: FxHashMap<usize, FxHashSet<PropJoinKey>> = FxHashMap::default();
    // Sum/Min/Max accumulators by agg index
    let mut sum_accum: FxHashMap<usize, f64> = FxHashMap::default();
    let mut min_accum: FxHashMap<usize, Option<f64>> = FxHashMap::default();
    let mut max_accum: FxHashMap<usize, Option<f64>> = FxHashMap::default();
    let mut collect_accum: FxHashMap<usize, Vec<CypherValue>> = FxHashMap::default();
    for (idx, kind) in &agg_kinds {
        if matches!(kind, ExpandAggKind::CountDistinctRelProperty { .. }) {
            distinct_rel_props.insert(*idx, FxHashSet::default());
        }
        if matches!(kind, ExpandAggKind::Collect { .. }) {
            collect_accum.insert(*idx, Vec::new());
        }
    }
    // property keys resolved once, not per edge
    let mut resolved_prop_keys: FxHashMap<usize, PropertyKeyId> = FxHashMap::default();
    for (idx, kind) in &agg_kinds {
        let pk_str = match kind {
            ExpandAggKind::Sum { prop_key, .. }
            | ExpandAggKind::Min { prop_key, .. }
            | ExpandAggKind::Max { prop_key, .. } => Some(prop_key.as_str()),
            ExpandAggKind::Collect {
                prop_key: Some(pk), ..
            } => Some(pk.as_str()),
            _ => None,
        };
        if let Some(pk) = pk_str
            && let Some(raw) = graph.strings().get(pk)
        {
            resolved_prop_keys.insert(*idx, PropertyKeyId(raw));
        }
    }
    // GROUP BY keys only on upstream variables (not `to` or `rel_var`)? Then the key is constant
    // per upstream frame: compute once, not per edge
    let group_key_upstream_only = if has_group_by && inner_expand_info.is_none() {
        let mut key_vars = std::collections::HashSet::new();
        for k in keys {
            crate::planner::collect_expr_variables(&k.expr, &mut key_vars);
        }
        !key_vars.contains(to) && !key_vars.contains(rel_var)
    } else {
        false
    };

    let mut probe = new_frame(symtab);

    let is_2hop = inner_expand_info.is_some();

    // -- two-pass fan-in fast path for 2-hop count aggregates --------
    if is_2hop
        && combined_pred.is_none()
        && excluded_nodes.is_empty()
        && not_in_bitvec.is_none()
        && in_bitvec.is_none()
        && not_in_rel.is_none()
        && in_rel.is_none()
    {
        let all_total_count = agg_kinds
            .iter()
            .all(|(_, k)| matches!(k, ExpandAggKind::TotalCount));
        let group_prop_key: Option<&str> = if has_group_by {
            if keys.len() == 1 {
                if let Expr::Property(base, prop) = &keys[0].expr {
                    if matches!(base.as_ref(), Expr::Variable(v) if v == to) {
                        Some(prop.as_str())
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            Some("") // sentinel: no group-by
        };
        if all_total_count && let Some(group_prop_key) = group_prop_key {
            let info = inner_expand_info.as_ref().unwrap();
            return two_pass_count_2hop(
                &upstream_frames,
                info.inner_from,
                info.inner_edge_types,
                info.direction,
                from,
                edge_types,
                direction,
                to,
                &to_label_ids,
                group_prop_key,
                has_group_by,
                keys,
                aggs,
                &agg_kinds,
                graph,
                symtab,
            );
        }
    }

    // closure-probe cache for cycle-closing expands: with `to` already bound (triangles
    // `(a)-[]->(b)-[]->(c)-[]->(a)`), filtering expand_from's adjacency for `neighbor == target` is
    // O(deg) per (b, c). The target is usually loop-invariant, so build its REVERSE adjacency
    // (`neighbor -> matching endpoints`) once and each pair is an O(1) probe. Keyed by (target,
    // type-filter index); capped, falling back to the plain scan for many targets
    let mut closure_cache: FxHashMap<
        (NodeId, usize),
        FxHashMap<NodeId, Vec<traverse_core::graph::types::EdgeEndpoint>>,
    > = FxHashMap::default();
    const CLOSURE_CACHE_CAP: usize = 128;

    for frame in &upstream_frames {
        // copy the upstream frame into the probe
        probe.clone_from(frame);

        // cycle closure: `to_slot` already bound (`to` introduced earlier, e.g. `a` in
        // `(a)-[]->(b)-[]->(c)-[]->(a)`), so the outer expand must FILTER on `ep.neighbor ==
        // bound_to`, not rebind. Otherwise every 3-path a->b->c->d counts, not just closed triangles
        // (ExpandOp does the same check)
        let bound_to: Option<NodeId> = match &frame[to_slot] {
            SlotValue::NodeRef(id) => Some(*id),
            SlotValue::Heap(cv) => match cv.as_ref() {
                CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => Some(*id),
                _ => None,
            },
            _ => None,
        };

        // expansion sources (outer_from_id, inner_edge_to_skip): 1-hop is (from_id, None), 2-hop
        // one per inner edge (mid_id, Some(inner_edge_id))
        let expansion_sources: Vec<(NodeId, Option<EdgeId>)> = if is_2hop {
            let scan_id = get_node_id_frame(frame, inner_from_slot.unwrap())?;
            let mut sources = Vec::new();
            // dedup inner edges: a self-loop under Direction::Both appears in both adjacency lists
            // and would seed the outer hop twice
            let mut seen_inner_edges: FxHashSet<EdgeId> = FxHashSet::default();
            for inner_tf in &inner_type_filters {
                let inner_eps = graph
                    .edges_with_neighbor(scan_id, inner_direction.unwrap(), *inner_tf)
                    .map_err(CypherError::from)?;
                for inner_ep in inner_eps {
                    if !seen_inner_edges.insert(inner_ep.edge_id) {
                        continue;
                    }
                    sources.push((inner_ep.neighbor, Some(inner_ep.edge_id)));
                }
            }
            sources
        } else {
            let from_id = get_node_id_frame(frame, from_slot)?;
            vec![(from_id, None)]
        };

        let used_edges_1hop = if !is_2hop {
            collect_prior_edge_ids(frame, &prior_edge_slots)
        } else {
            FxHashSet::default()
        };

        // GROUP BY key on upstream variables only: compute once, no eval_expr + hash + probe per
        // edge
        let precomputed_key: Option<(Vec<JoinKey>, Vec<CypherValue>)> = if group_key_upstream_only {
            let key_vals: Vec<CypherValue> = keys
                .iter()
                .map(|k| eval_expr(&k.expr, &probe, &*graph, symtab))
                .collect::<Result<_, _>>()?;
            let hash_key: Vec<JoinKey> = key_vals
                .iter()
                .map(cypher_to_join_key)
                .collect::<Result<_, _>>()?;
            Some((hash_key, key_vals))
        } else {
            None
        };
        // raw pointer to the group's accumulators, set on the first matching edge. SAFETY: stays
        // valid since no other entry is inserted/removed during the edge loop
        let mut precomputed_accs: *mut GroupAccumulators = std::ptr::null_mut();

        for &(expand_from_id, skip_edge) in &expansion_sources {
            if is_2hop {
                probe[from_slot] = SlotValue::NodeRef(expand_from_id);
            }

            let mut seen_edge_ids: FxHashSet<EdgeId> = FxHashSet::default();

            for (tf_idx, type_filter) in type_filters.iter().enumerate() {
                let endpoints: Vec<traverse_core::graph::types::EdgeEndpoint> = match bound_to {
                    Some(target)
                        if closure_cache.len() < CLOSURE_CACHE_CAP
                            || closure_cache.contains_key(&(target, tf_idx)) =>
                    {
                        let rev = match direction {
                            Direction::Outgoing => Direction::Incoming,
                            Direction::Incoming => Direction::Outgoing,
                            Direction::Both => Direction::Both,
                        };
                        let map = match closure_cache.entry((target, tf_idx)) {
                            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                            std::collections::hash_map::Entry::Vacant(e) => {
                                let mut by_neighbor: FxHashMap<
                                    NodeId,
                                    Vec<traverse_core::graph::types::EdgeEndpoint>,
                                > = FxHashMap::default();
                                for rep in graph
                                    .edges_with_neighbor(target, rev, *type_filter)
                                    .map_err(CypherError::from)?
                                {
                                    // re-orient: the edge's forward view lands on `target`
                                    let fwd = traverse_core::graph::types::EdgeEndpoint {
                                        edge_id: rep.edge_id,
                                        edge_type: rep.edge_type,
                                        neighbor: target,
                                        source: rep.source,
                                        target: rep.target,
                                    };
                                    by_neighbor.entry(rep.neighbor).or_default().push(fwd);
                                }
                                e.insert(by_neighbor)
                            }
                        };
                        map.get(&expand_from_id).cloned().unwrap_or_default()
                    }
                    _ => graph
                        .edges_with_neighbor(expand_from_id, direction, *type_filter)
                        .map_err(CypherError::from)?,
                };

                for ep in endpoints {
                    // edge dedup (Both with several type filters)
                    if !seen_edge_ids.insert(ep.edge_id) {
                        continue;
                    }
                    // cyclic `(n)-[r]->(n)`: neighbor must be the source
                    if is_cyclic_endpoint && ep.neighbor != expand_from_id {
                        continue;
                    }
                    // cycle closure: `to` bound earlier, so the edge must land on that node (see
                    // `bound_to` above)
                    if let Some(target) = bound_to
                        && ep.neighbor != target
                    {
                        continue;
                    }
                    // edge isomorphism: 2-hop skips the inner edge, 1-hop uses prior_edge_vars
                    if is_2hop {
                        if let Some(inner_eid) = skip_edge
                            && ep.edge_id == inner_eid
                        {
                            continue;
                        }
                    } else if used_edges_1hop.contains(&ep.edge_id) {
                        continue;
                    }
                    // inline has_label check (extracted from the filter)
                    if !to_label_ids.is_empty() {
                        let mut has_all = true;
                        for lid in &to_label_ids {
                            if !graph.has_label(ep.neighbor, *lid)? {
                                has_all = false;
                                break;
                            }
                        }
                        if !has_all {
                            continue;
                        }
                    }
                    // inline excluded-node check (to_var.prop <> literal)
                    if !excluded_nodes.is_empty() && excluded_nodes.contains(&ep.neighbor) {
                        continue;
                    }
                    // inline NOT IN on the rel property: bitvec (index-backed) or one property
                    // read, reused by CountDistinctRelProperty
                    let mut not_in_prop_val: Option<Option<PropertyValue>> = None;
                    if let Some(ref bv) = not_in_bitvec {
                        if bitvec_test(bv, ep.edge_id) {
                            continue;
                        }
                    } else if let Some((ref prop_key, ref excluded_vals)) = not_in_rel {
                        let pv = graph
                            .get_edge_property(ep.edge_id, prop_key)
                            .map_err(CypherError::from)?;
                        let skip = if let Some(PropertyValue::String(ref s)) = pv {
                            excluded_vals.contains(s.as_str())
                        } else {
                            false
                        };
                        not_in_prop_val = Some(pv);
                        if skip {
                            continue;
                        }
                    }
                    // inline positive IN on the rel property: bitvec or property read
                    if let Some(ref bv) = in_bitvec {
                        if !bitvec_test(bv, ep.edge_id) {
                            continue;
                        }
                    } else if let Some((ref prop_key, ref included_vals)) = in_rel {
                        let pv = graph
                            .get_edge_property(ep.edge_id, prop_key)
                            .map_err(CypherError::from)?;
                        let included = if let Some(PropertyValue::String(ref s)) = pv {
                            included_vals.contains(s.as_str())
                        } else {
                            false
                        };
                        if !included {
                            continue;
                        }
                    }

                    // set rel and to slots in the probe
                    if rel_slot != usize::MAX {
                        probe[rel_slot] = SlotValue::EdgeRef {
                            id: ep.edge_id,
                            edge_type_id: ep.edge_type,
                            source: ep.source,
                            target: ep.target,
                        };
                    }
                    probe[to_slot] = SlotValue::NodeRef(ep.neighbor);

                    // remaining filter predicate, if any survived extraction
                    let pass = if let Some(ref pred) = combined_pred {
                        if let Some(ref bc) = compiled {
                            bytecode::eval_bytecode(bc, &probe, &*graph)?
                        } else {
                            is_truthy(&eval_expr(pred, &probe, &*graph, symtab)?)
                        }
                    } else {
                        true
                    };

                    if pass {
                        if has_group_by {
                            let accs = if let Some((pk, kv)) = &precomputed_key {
                                // upstream-only GROUP BY key: insert on the first edge, reuse the
                                // pointer after
                                if precomputed_accs.is_null() {
                                    let entry = groups.entry(pk.clone()).or_insert_with(|| {
                                        (kv.clone(), GroupAccumulators::new(&agg_kinds))
                                    });
                                    precomputed_accs = &mut entry.1 as *mut GroupAccumulators;
                                }
                                // SAFETY: the entry exists and the map isn't changed structurally
                                // during the loop.
                                unsafe { &mut *precomputed_accs }
                            } else {
                                // GROUP BY keys per edge
                                let key_vals: Vec<CypherValue> = keys
                                    .iter()
                                    .map(|k| eval_expr(&k.expr, &probe, &*graph, symtab))
                                    .collect::<Result<_, _>>()?;
                                let hash_key: Vec<JoinKey> = key_vals
                                    .iter()
                                    .map(cypher_to_join_key)
                                    .collect::<Result<_, _>>()?;
                                let group = groups.entry(hash_key).or_insert_with(|| {
                                    (key_vals, GroupAccumulators::new(&agg_kinds))
                                });
                                &mut group.1
                            };
                            accs.distinct_set.insert(ep.neighbor);
                            accs.total_count += 1;
                            for (idx, kind) in &agg_kinds {
                                update_accumulators(
                                    *idx,
                                    kind,
                                    ep.edge_id,
                                    ep.neighbor,
                                    expand_from_id,
                                    to,
                                    &not_in_rel,
                                    &mut not_in_prop_val,
                                    &mut accs.distinct_rel_props,
                                    &mut accs.sum_accum,
                                    &mut accs.min_accum,
                                    &mut accs.max_accum,
                                    &mut accs.collect_accum,
                                    &resolved_prop_keys,
                                    &*graph,
                                )?;
                            }
                        } else {
                            distinct_set.insert(ep.neighbor);
                            total_count += 1;
                            for (idx, kind) in &agg_kinds {
                                update_accumulators(
                                    *idx,
                                    kind,
                                    ep.edge_id,
                                    ep.neighbor,
                                    expand_from_id,
                                    to,
                                    &not_in_rel,
                                    &mut not_in_prop_val,
                                    &mut distinct_rel_props,
                                    &mut sum_accum,
                                    &mut min_accum,
                                    &mut max_accum,
                                    &mut collect_accum,
                                    &resolved_prop_keys,
                                    &*graph,
                                )?;
                            }
                        }
                    }
                }
            }
        }
    }

    // 9. result frame(s)
    if has_group_by {
        // grouped: one frame per group
        let mut results: Vec<Frame> = Vec::with_capacity(groups.len());
        for (_hash_key, (key_vals, accs)) in groups {
            let mut out = new_frame(symtab);
            // GROUP BY keys
            for (i, key_item) in keys.iter().enumerate() {
                let name = projection_name(key_item);
                if let Some(slot) = symtab.slot(&name) {
                    out[slot] = key_vals[i].clone().into();
                }
            }
            // aggregate values
            for (idx, kind) in &agg_kinds {
                if *idx >= 1000 {
                    continue;
                }
                let val = resolve_agg_value(
                    idx,
                    kind,
                    &accs.distinct_set,
                    accs.total_count,
                    &accs.distinct_rel_props,
                    &accs.sum_accum,
                    &accs.min_accum,
                    &accs.max_accum,
                    &accs.collect_accum,
                    &avg_indices,
                );
                let name = projection_name(&aggs[*idx]);
                if let Some(slot) = symtab.slot(&name) {
                    out[slot] = val.into();
                }
            }
            // compound aggs
            for (out_idx, op, left_sub, right_sub) in &compound_agg_exprs {
                let left_val = resolve_sub_agg_value(
                    left_sub,
                    &agg_kinds,
                    &accs.distinct_set,
                    accs.total_count,
                    &accs.distinct_rel_props,
                    &accs.sum_accum,
                    &accs.min_accum,
                    &accs.max_accum,
                    &accs.collect_accum,
                );
                let right_val = resolve_sub_agg_value(
                    right_sub,
                    &agg_kinds,
                    &accs.distinct_set,
                    accs.total_count,
                    &accs.distinct_rel_props,
                    &accs.sum_accum,
                    &accs.min_accum,
                    &accs.max_accum,
                    &accs.collect_accum,
                );
                let result = eval_compound_agg(op, left_val, right_val);
                let name = projection_name(&aggs[*out_idx]);
                if let Some(slot) = symtab.slot(&name) {
                    out[slot] = result.into();
                }
            }
            results.push(out);
        }
        return Ok(Some(results));
    }

    // ungrouped: one frame
    let mut out = new_frame(symtab);
    for (idx, kind) in &agg_kinds {
        if *idx >= 1000 {
            continue;
        }
        let val = resolve_agg_value(
            idx,
            kind,
            &distinct_set,
            total_count,
            &distinct_rel_props,
            &sum_accum,
            &min_accum,
            &max_accum,
            &collect_accum,
            &avg_indices,
        );
        let name = projection_name(&aggs[*idx]);
        if let Some(slot) = symtab.slot(&name) {
            out[slot] = val.into();
        }
    }

    // compound aggs, e.g. max(...) - min(...)
    for (out_idx, op, left_sub, right_sub) in &compound_agg_exprs {
        let left_val = resolve_sub_agg_value(
            left_sub,
            &agg_kinds,
            &distinct_set,
            total_count,
            &distinct_rel_props,
            &sum_accum,
            &min_accum,
            &max_accum,
            &collect_accum,
        );
        let right_val = resolve_sub_agg_value(
            right_sub,
            &agg_kinds,
            &distinct_set,
            total_count,
            &distinct_rel_props,
            &sum_accum,
            &min_accum,
            &max_accum,
            &collect_accum,
        );
        let result = eval_compound_agg(op, left_val, right_val);
        let name = projection_name(&aggs[*out_idx]);
        if let Some(slot) = symtab.slot(&name) {
            out[slot] = result.into();
        }
    }

    Ok(Some(vec![out]))
}

// -- Two-pass fan-in for 2-hop count aggregates ----------------------

/// Fast path for `MATCH (n)-[]->(mid)-[]->(m) RETURN [m.prop,] count(*)`: two O(E)
/// passes instead of iterating every edge pair.
///   pass 1: fan_in[mid] = inner edges arriving at mid
///   pass 2: per mid with fan_in > 0, expand outer edges mid->m and add
///           fan_in[mid] to m's group (with self-loop correction)
fn two_pass_count_2hop(
    upstream_frames: &[Frame],
    inner_from: &str,
    inner_edge_types: &[String],
    inner_direction: Direction,
    _outer_from: &str,
    outer_edge_types: &[String],
    outer_direction: Direction,
    _to: &str,
    to_label_ids: &[LabelId],
    group_prop_key: &str, // "" = no group-by
    has_group_by: bool,
    keys: &[ProjectionItem],
    aggs: &[ProjectionItem],
    agg_kinds: &[(usize, ExpandAggKind)],
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Option<Vec<Frame>>, CypherError> {
    let inner_from_slot = match symtab.slot(inner_from) {
        Some(s) => s,
        None => return Ok(None),
    };

    let inner_type_filters: Vec<Option<&str>> = if inner_edge_types.is_empty() {
        vec![None]
    } else {
        inner_edge_types.iter().map(|t| Some(t.as_str())).collect()
    };
    let outer_type_filters: Vec<Option<&str>> = if outer_edge_types.is_empty() {
        vec![None]
    } else {
        outer_edge_types.iter().map(|t| Some(t.as_str())).collect()
    };

    // pass 1: fan_in[mid] and, when needed, fan_in_edges[mid] across all upstream
    // frames. fan_in_edges records WHICH inner edges reached mid so pass 2 can
    // subtract them for edge isomorphism (a 2-hop pattern needs e1 != e2).
    //
    // What can collide depends on the direction pair:
    //   - (Out, Out) / (In, In), divergent: inner edges land in mid.incoming (or
    //     .outgoing), outer ones leave via mid.outgoing (or .incoming); they only
    //     overlap on self-loops at mid, so record just those.
    //   - (Out, In) / (In, Out), convergent: every inner edge is in the outer
    //     adjacency view; track all.
    //   - any Direction::Both: full tracking + per-list dedup (self-loops appear
    //     in both outgoing and incoming).
    //
    // seen_inner / seen_outer dedup adjacency lists, only needed for Direction::Both.
    let is_divergent = matches!(
        (inner_direction, outer_direction),
        (Direction::Outgoing, Direction::Outgoing) | (Direction::Incoming, Direction::Incoming)
    );
    let inner_needs_dedup = matches!(inner_direction, Direction::Both);
    let outer_needs_dedup = matches!(outer_direction, Direction::Both);

    let mut fan_in: FxHashMap<NodeId, i64> = FxHashMap::default();
    let mut fan_in_edges: FxHashMap<NodeId, FxHashSet<EdgeId>> = FxHashMap::default();
    for frame in upstream_frames {
        let scan_id = get_node_id_frame(frame, inner_from_slot)?;
        let mut seen_inner: Option<FxHashSet<EdgeId>> = if inner_needs_dedup {
            Some(FxHashSet::default())
        } else {
            None
        };
        for itf in &inner_type_filters {
            let eps = graph
                .edges_with_neighbor(scan_id, inner_direction, *itf)
                .map_err(CypherError::from)?;
            for ep in eps {
                if let Some(seen) = seen_inner.as_mut()
                    && !seen.insert(ep.edge_id)
                {
                    continue;
                }
                *fan_in.entry(ep.neighbor).or_insert(0) += 1;
                if is_divergent {
                    // only self-loops can collide with outer edges
                    if ep.neighbor == scan_id {
                        fan_in_edges
                            .entry(ep.neighbor)
                            .or_default()
                            .insert(ep.edge_id);
                    }
                } else {
                    fan_in_edges
                        .entry(ep.neighbor)
                        .or_default()
                        .insert(ep.edge_id);
                }
            }
        }
    }

    // divergent, no self-loops collected: no isomorphism corrections and no outer dedup, so the
    // inner loop is `total += fi` per outer edge
    let no_collisions_possible = is_divergent && fan_in_edges.is_empty();

    // pass 2: per mid, expand outer edges -> m, accumulate
    if has_group_by {
        // grouped: key m.{group_prop_key} -> count
        let mut groups: FxHashMap<JoinKey, (CypherValue, i64)> = FxHashMap::default();
        let empty_set: FxHashSet<EdgeId> = FxHashSet::default();
        for (&mid, &fi) in &fan_in {
            let mid_fan_edges = if no_collisions_possible {
                &empty_set
            } else {
                fan_in_edges.get(&mid).unwrap_or(&empty_set)
            };
            let mut seen_outer: Option<FxHashSet<EdgeId>> = if outer_needs_dedup {
                Some(FxHashSet::default())
            } else {
                None
            };
            for otf in &outer_type_filters {
                let eps = graph
                    .edges_with_neighbor(mid, outer_direction, *otf)
                    .map_err(CypherError::from)?;
                for ep in eps {
                    if let Some(seen) = seen_outer.as_mut()
                        && !seen.insert(ep.edge_id)
                    {
                        continue;
                    }
                    let m = ep.neighbor;
                    // target label check
                    if !to_label_ids.is_empty() {
                        let mut has_all = true;
                        for lid in to_label_ids {
                            if !graph.has_label(m, *lid)? {
                                has_all = false;
                                break;
                            }
                        }
                        if !has_all {
                            continue;
                        }
                    }
                    // edge isomorphism: subtract 1 iff this outer edge also contributed to
                    // fan_in[mid] (e1 == e2)
                    let contrib =
                        if !mid_fan_edges.is_empty() && mid_fan_edges.contains(&ep.edge_id) {
                            fi - 1
                        } else {
                            fi
                        };
                    if contrib <= 0 {
                        continue;
                    }
                    // group key property
                    let pv = graph
                        .get_node_property(m, group_prop_key)
                        .map_err(CypherError::from)?;
                    let cv = match pv {
                        Some(ref v) => CypherValue::from_property(v),
                        None => CypherValue::Null,
                    };
                    let jk = cypher_to_join_key(&cv)?;
                    let entry = groups.entry(jk).or_insert_with(|| (cv, 0));
                    entry.1 += contrib;
                }
            }
        }

        // output frames
        let mut results: Vec<Frame> = Vec::with_capacity(groups.len());
        for (_jk, (key_val, count)) in groups {
            let mut out = new_frame(symtab);
            // GROUP BY key.
            let key_name = projection_name(&keys[0]);
            if let Some(slot) = symtab.slot(&key_name) {
                out[slot] = key_val.into();
            }
            // every aggregate is TotalCount: same count
            for (idx, _kind) in agg_kinds {
                if *idx >= 1000 {
                    continue;
                }
                let name = projection_name(&aggs[*idx]);
                if let Some(slot) = symtab.slot(&name) {
                    out[slot] = CypherValue::Integer(count).into();
                }
            }
            results.push(out);
        }
        Ok(Some(results))
    } else {
        // ungrouped: one total
        let mut total: i64 = 0;
        let empty_set: FxHashSet<EdgeId> = FxHashSet::default();
        for (&mid, &fi) in &fan_in {
            let mid_fan_edges = if no_collisions_possible {
                &empty_set
            } else {
                fan_in_edges.get(&mid).unwrap_or(&empty_set)
            };
            let mut seen_outer: Option<FxHashSet<EdgeId>> = if outer_needs_dedup {
                Some(FxHashSet::default())
            } else {
                None
            };
            for otf in &outer_type_filters {
                let eps = graph
                    .edges_with_neighbor(mid, outer_direction, *otf)
                    .map_err(CypherError::from)?;
                for ep in eps {
                    if let Some(seen) = seen_outer.as_mut()
                        && !seen.insert(ep.edge_id)
                    {
                        continue;
                    }
                    let m = ep.neighbor;
                    if !to_label_ids.is_empty() {
                        let mut has_all = true;
                        for lid in to_label_ids {
                            if !graph.has_label(m, *lid)? {
                                has_all = false;
                                break;
                            }
                        }
                        if !has_all {
                            continue;
                        }
                    }
                    // edge isomorphism: subtract 1 iff this outer edge also contributed to
                    // fan_in[mid]
                    let contrib =
                        if !mid_fan_edges.is_empty() && mid_fan_edges.contains(&ep.edge_id) {
                            fi - 1
                        } else {
                            fi
                        };
                    if contrib <= 0 {
                        continue;
                    }
                    total += contrib;
                }
            }
        }

        let mut out = new_frame(symtab);
        for (idx, _kind) in agg_kinds {
            if *idx >= 1000 {
                continue;
            }
            let name = projection_name(&aggs[*idx]);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = CypherValue::Integer(total).into();
            }
        }
        Ok(Some(vec![out]))
    }
}
