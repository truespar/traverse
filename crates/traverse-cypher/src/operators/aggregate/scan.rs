//! Aggregate pushdown into the scan: global, single-key and multi-key aggregation without
//! frames, with a typed i64 fast path.

use std::cmp::Ordering;

use rustc_hash::{FxHashMap, FxHashSet};
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::ast::{Expr, ProjectionItem};
use crate::error::CypherError;
use crate::executor::{eval_expr, is_truthy, projection_name};
use crate::frame::{Frame, SlotValue, SymbolTable, new_frame};
use crate::planner::{LogicalPlan, is_aggregate_function};
use crate::types::CypherValue;

use super::super::batch_types::{
    PropJoinKey, acc_avg_column, acc_avg_column_i64, acc_count_column, acc_count_valid,
    acc_max_column, acc_max_column_i64, acc_min_column, acc_min_column_i64, acc_sum_column,
    acc_sum_column_i64, column_is_integer, compare_property_values, prop_to_cypher,
    prop_to_join_key,
};

// -- Aggregate pushdown types -----------------------------------------

/// Accumulator for one aggregate in scan pushdown.
pub(crate) enum ScanAcc {
    /// count(*), count(n), count(n.prop): non-null count.
    Count(i64),
    /// min(n.prop), as PropertyValue until finalization.
    Min(Option<PropertyValue>),
    /// max(n.prop), as PropertyValue until finalization.
    Max(Option<PropertyValue>),
    /// sum(n.prop), int and float sums kept apart.
    Sum {
        int_sum: i64,
        float_sum: f64,
        has_float: bool,
        has_any: bool,
    },
    /// avg(n.prop) as f64.
    Avg { sum: f64, count: i64 },
}

/// What one aggregate needs from the scan.
pub(crate) struct AggSpec {
    /// Index into aggs.
    pub(crate) agg_idx: usize,
    /// Aggregate function.
    pub(crate) kind: AggKind,
    /// Property to read (None for count(*) / count(var)).
    pub(crate) prop: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) enum AggKind {
    CountStar,
    CountProp,
    Min,
    Max,
    Sum,
    Avg,
}

pub(crate) const AGG_BATCH_SIZE: usize = 1024;

// -- Aggregate pushdown entry point -----------------------------------

/// Pushes aggregation into the scan, no frames; global and grouped. `Some(result)` on success,
/// `None` to fall back.
pub(super) fn try_aggregate_pushdown(
    input: &LogicalPlan,
    keys: &[ProjectionItem],
    aggs: &[ProjectionItem],
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Option<Vec<Frame>>, CypherError> {
    // anchorless edge count (`Aggregate{keys: []}` over EdgeTypeScan, every agg count(*) /
    // count(<scan var>)): the edge-type index size, no iteration. UNDIRECTED scans emit each edge
    // twice, so the count doubles
    if keys.is_empty()
        && let LogicalPlan::EdgeTypeScan {
            from_var,
            rel_var,
            to_var,
            edge_types,
            direction,
        } = input
    {
        let all_counts = !aggs.is_empty()
            && aggs.iter().all(|item| match &item.expr {
                Expr::FunctionCall {
                    name,
                    args,
                    distinct,
                } if name == "count" && !*distinct => {
                    args.is_empty()
                        || matches!(
                            &args[0],
                            Expr::Variable(v)
                                if v == from_var
                                    || v == to_var
                                    || (!rel_var.is_empty() && v == rel_var)
                        )
                }
                _ => false,
            });
        if all_counts {
            let mut n: i64 = edge_types
                .iter()
                .map(|t| graph.edges_by_type(t).len() as i64)
                .sum();
            if matches!(direction, traverse_core::graph::types::Direction::Both) {
                n *= 2;
            }
            let mut out = new_frame(symtab);
            for item in aggs {
                let name = projection_name(item);
                if let Some(slot) = symtab.slot(&name) {
                    out[slot] = CypherValue::Integer(n).into();
                }
            }
            return Ok(Some(vec![out]));
        }
    }

    // bare Scan or Filter(Scan), label optional
    let (scan_var, scan_label, filter_predicate) = match input {
        LogicalPlan::Scan { variable, label } => (variable.as_str(), label.as_deref(), None),
        LogicalPlan::Filter {
            input: inner,
            predicate,
        } => {
            if let LogicalPlan::Scan { variable, label } = inner.as_ref() {
                (variable.as_str(), label.as_deref(), Some(predicate))
            } else {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    };

    // keys must be Expr::Property(Expr::Variable(scan_var), prop)
    let mut key_props: Vec<String> = Vec::new();
    for key_item in keys {
        match &key_item.expr {
            Expr::Property(base, prop) => {
                if let Expr::Variable(v) = base.as_ref() {
                    if v == scan_var {
                        key_props.push(prop.clone());
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

    // aggregates -> AggSpecs
    let mut agg_specs: Vec<AggSpec> = Vec::new();
    for (idx, item) in aggs.iter().enumerate() {
        match &item.expr {
            Expr::FunctionCall {
                name,
                args,
                distinct,
            } if is_aggregate_function(name) && !*distinct => {
                match name.as_str() {
                    "count" => {
                        if args.is_empty() {
                            agg_specs.push(AggSpec {
                                agg_idx: idx,
                                kind: AggKind::CountStar,
                                prop: None,
                            });
                        } else if args.len() == 1 {
                            match &args[0] {
                                Expr::Variable(v) if v == scan_var => {
                                    agg_specs.push(AggSpec {
                                        agg_idx: idx,
                                        kind: AggKind::CountStar,
                                        prop: None,
                                    });
                                }
                                Expr::Property(base, prop) => {
                                    if let Expr::Variable(v) = base.as_ref() {
                                        if v == scan_var {
                                            agg_specs.push(AggSpec {
                                                agg_idx: idx,
                                                kind: AggKind::CountProp,
                                                prop: Some(prop.clone()),
                                            });
                                        } else {
                                            return Ok(None);
                                        }
                                    } else {
                                        return Ok(None);
                                    }
                                }
                                _ => return Ok(None),
                            }
                        } else {
                            return Ok(None);
                        }
                    }
                    "min" | "max" | "avg" | "sum" => {
                        if args.len() != 1 {
                            return Ok(None);
                        }
                        let prop = match &args[0] {
                            Expr::Property(base, prop) => {
                                if let Expr::Variable(v) = base.as_ref() {
                                    if v == scan_var {
                                        prop.clone()
                                    } else {
                                        return Ok(None);
                                    }
                                } else {
                                    return Ok(None);
                                }
                            }
                            _ => return Ok(None),
                        };
                        let kind = match name.as_str() {
                            "min" => AggKind::Min,
                            "max" => AggKind::Max,
                            "avg" => AggKind::Avg,
                            "sum" => AggKind::Sum,
                            _ => unreachable!(),
                        };
                        agg_specs.push(AggSpec {
                            agg_idx: idx,
                            kind,
                            prop: Some(prop),
                        });
                    }
                    _ => return Ok(None), // collect, percentile, ...
                }
            }
            _ => return Ok(None),
        }
    }

    // unique property names across keys and aggs
    let mut all_props: Vec<String> = Vec::new();
    let mut prop_set: FxHashSet<String> = FxHashSet::default();
    for p in &key_props {
        if prop_set.insert(p.clone()) {
            all_props.push(p.clone());
        }
    }
    for spec in &agg_specs {
        if let Some(p) = &spec.prop
            && prop_set.insert(p.clone())
        {
            all_props.push(p.clone());
        }
    }

    // property name -> index in all_props
    let prop_index: FxHashMap<&str, usize> = all_props
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i))
        .collect();

    // resolve string -> PropertyKeyId once, not per node
    let strings = graph.strings();
    let prop_kids: Vec<Option<PropertyKeyId>> = all_props
        .iter()
        .map(|p| strings.get(p).map(PropertyKeyId))
        .collect();

    // node IDs
    let mut node_ids = match scan_label {
        Some(lbl) => graph.nodes_by_label(lbl),
        None => graph.nodes_vec(),
    };

    // Filter(Scan): pre-filter node_ids so the sub-functions stay unchanged; node_ids.len() is then
    // the CountStar answer
    if let Some(pred) = filter_predicate
        && let Some(scan_slot) = symtab.slot(scan_var)
    {
        let mut scratch = new_frame(symtab);
        node_ids.retain(|&nid| {
            scratch[scan_slot] = SlotValue::NodeRef(nid);
            eval_expr(pred, &scratch, &*graph, symtab)
                .map(|v| is_truthy(&v))
                .unwrap_or(false)
        });
    }

    if keys.is_empty() {
        agg_pushdown_global(
            graph,
            &node_ids,
            &agg_specs,
            &prop_kids,
            &all_props,
            &prop_index,
            aggs,
            symtab,
        )
    } else if key_props.len() == 1 {
        agg_pushdown_single_key(
            graph,
            &node_ids,
            &agg_specs,
            &prop_kids,
            &all_props,
            &prop_index,
            &key_props,
            keys,
            aggs,
            symtab,
        )
    } else {
        agg_pushdown_multi_key(
            graph,
            &node_ids,
            &agg_specs,
            &prop_kids,
            &all_props,
            &prop_index,
            &key_props,
            keys,
            aggs,
            symtab,
        )
    }
}

// -- Helper functions -------------------------------------------------

impl ScanAcc {
    pub(crate) fn new(kind: &AggKind) -> Self {
        match kind {
            AggKind::CountStar | AggKind::CountProp => ScanAcc::Count(0),
            AggKind::Min => ScanAcc::Min(None),
            AggKind::Max => ScanAcc::Max(None),
            AggKind::Sum => ScanAcc::Sum {
                int_sum: 0,
                float_sum: 0.0,
                has_float: false,
                has_any: false,
            },
            AggKind::Avg => ScanAcc::Avg { sum: 0.0, count: 0 },
        }
    }

    pub(crate) fn finalize(&self) -> CypherValue {
        finalize_acc(self)
    }
}

fn new_scan_acc(kind: &AggKind) -> ScanAcc {
    ScanAcc::new(kind)
}

/// Option<PropertyKeyId> -> PropertyKeyId with u32::MAX for None; CompactPropertyStore::find_entry
/// never matches it, giving None.
#[inline(always)]
fn resolve_prop_kids(prop_kids: &[Option<PropertyKeyId>]) -> Vec<PropertyKeyId> {
    prop_kids
        .iter()
        .map(|k| k.unwrap_or(PropertyKeyId(u32::MAX)))
        .collect()
}

/// Final accumulator -> CypherValue.
fn finalize_acc(acc: &ScanAcc) -> CypherValue {
    match acc {
        ScanAcc::Count(c) => CypherValue::Integer(*c),
        ScanAcc::Min(Some(pv)) => prop_to_cypher(pv),
        ScanAcc::Min(None) => CypherValue::Null,
        ScanAcc::Max(Some(pv)) => prop_to_cypher(pv),
        ScanAcc::Max(None) => CypherValue::Null,
        ScanAcc::Sum {
            int_sum,
            float_sum,
            has_float,
            has_any,
        } => {
            if !has_any {
                CypherValue::Integer(0)
            } else if *has_float {
                CypherValue::Float(*float_sum)
            } else {
                CypherValue::Integer(*int_sum)
            }
        }
        ScanAcc::Avg { sum, count } => {
            if *count == 0 {
                CypherValue::Null
            } else {
                CypherValue::Float(*sum / *count as f64)
            }
        }
    }
}

/// Updates an accumulator from the current node's values (`&[Option<PropertyValue>]`, no
/// CypherValue conversion).
fn update_acc(
    acc: &mut ScanAcc,
    spec: &AggSpec,
    prop_vals: &[Option<PropertyValue>],
    prop_index: &FxHashMap<&str, usize>,
) -> Result<(), CypherError> {
    match (acc, &spec.kind) {
        (ScanAcc::Count(c), AggKind::CountStar) => {
            *c += 1;
        }
        (ScanAcc::Count(c), AggKind::CountProp) => {
            let val = &prop_vals[prop_index[spec.prop.as_ref().unwrap().as_str()]];
            if val.is_some() && !matches!(val, Some(PropertyValue::Null)) {
                *c += 1;
            }
        }
        (ScanAcc::Min(best), AggKind::Min) => {
            let val = &prop_vals[prop_index[spec.prop.as_ref().unwrap().as_str()]];
            if let Some(pv) = val
                && !matches!(pv, PropertyValue::Null)
                && (best.is_none()
                    || compare_property_values(pv, best.as_ref().unwrap()) == Ordering::Less)
            {
                *best = Some(pv.clone());
            }
        }
        (ScanAcc::Max(best), AggKind::Max) => {
            let val = &prop_vals[prop_index[spec.prop.as_ref().unwrap().as_str()]];
            if let Some(pv) = val
                && !matches!(pv, PropertyValue::Null)
                && (best.is_none()
                    || compare_property_values(pv, best.as_ref().unwrap()) == Ordering::Greater)
            {
                *best = Some(pv.clone());
            }
        }
        (
            ScanAcc::Sum {
                int_sum,
                float_sum,
                has_float,
                has_any,
            },
            AggKind::Sum,
        ) => {
            let val = &prop_vals[prop_index[spec.prop.as_ref().unwrap().as_str()]];
            if let Some(pv) = val {
                match pv {
                    PropertyValue::Int(i) => {
                        *has_any = true;
                        *int_sum += i;
                        *float_sum += *i as f64;
                    }
                    PropertyValue::Float(f) => {
                        *has_any = true;
                        *has_float = true;
                        *float_sum += f;
                    }
                    PropertyValue::Null => {}
                    _ => {
                        return Err(CypherError::type_error(format!(
                            "sum() requires numeric values, got {:?}",
                            pv
                        )));
                    }
                }
            }
        }
        (ScanAcc::Avg { sum, count }, AggKind::Avg) => {
            let val = &prop_vals[prop_index[spec.prop.as_ref().unwrap().as_str()]];
            if let Some(pv) = val {
                match pv {
                    PropertyValue::Int(i) => {
                        *sum += *i as f64;
                        *count += 1;
                    }
                    PropertyValue::Float(f) => {
                        *sum += f;
                        *count += 1;
                    }
                    PropertyValue::Null => {}
                    _ => {
                        return Err(CypherError::type_error(format!(
                            "avg() requires numeric values, got {:?}",
                            pv
                        )));
                    }
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

// -- Pushdown sub-functions -------------------------------------------

/// Global aggregation (no keys).
#[inline(never)]
fn agg_pushdown_global(
    graph: &mut dyn GraphView,
    node_ids: &[NodeId],
    agg_specs: &[AggSpec],
    prop_kids: &[Option<PropertyKeyId>],
    all_props: &[String],
    prop_index: &FxHashMap<&str, usize>,
    aggs: &[ProjectionItem],
    symtab: &SymbolTable,
) -> Result<Option<Vec<Frame>>, CypherError> {
    let mut accs: Vec<ScanAcc> = agg_specs.iter().map(|s| new_scan_acc(&s.kind)).collect();

    // CountStar accumulators are just node_ids.len()
    for (acc, spec) in accs.iter_mut().zip(agg_specs.iter()) {
        if matches!(spec.kind, AggKind::CountStar) {
            *acc = ScanAcc::Count(node_ids.len() as i64);
        }
    }

    // all CountStar: no iteration
    let needs_iteration = agg_specs
        .iter()
        .any(|s| !matches!(s.kind, AggKind::CountStar));
    if needs_iteration {
        let nk = all_props.len();
        let resolved_kids = resolve_prop_kids(prop_kids);
        let mut pv_buf: Vec<Option<PropertyValue>> = vec![None; AGG_BATCH_SIZE * nk];

        // column offsets of the non-CountStar accumulators
        let col_offsets: Vec<usize> = agg_specs
            .iter()
            .map(|s| match &s.prop {
                Some(p) => prop_index[p.as_str()],
                None => 0,
            })
            .collect();

        // probe the first chunk: are all non-CountStar columns integers?
        let probe_size = node_ids.len().min(AGG_BATCH_SIZE);
        let probe_buf = &mut pv_buf[..probe_size * nk];
        graph.batch_read_node_properties(&node_ids[..probe_size], &resolved_kids, probe_buf)?;
        let all_integer = agg_specs
            .iter()
            .zip(col_offsets.iter())
            .all(|(spec, &col)| {
                matches!(spec.kind, AggKind::CountStar | AggKind::CountProp)
                    || column_is_integer(probe_buf, nk, col, probe_size)
            });

        if all_integer {
            return agg_pushdown_global_typed_i64(
                graph,
                node_ids,
                accs,
                agg_specs,
                &resolved_kids,
                nk,
                &col_offsets,
                aggs,
                symtab,
            );
        }

        for chunk in node_ids.chunks(AGG_BATCH_SIZE) {
            let buf = &mut pv_buf[..chunk.len() * nk];
            graph.batch_read_node_properties(chunk, &resolved_kids, buf)?;
            // column-oriented: one ScanAcc dispatch per chunk, not per row
            for ((acc, spec), &col) in accs
                .iter_mut()
                .zip(agg_specs.iter())
                .zip(col_offsets.iter())
            {
                match (acc, &spec.kind) {
                    (_, AggKind::CountStar) => {}
                    (ScanAcc::Count(c), AggKind::CountProp) => {
                        acc_count_column(c, buf, nk, col, chunk.len())
                    }
                    (ScanAcc::Min(best), AggKind::Min) => {
                        acc_min_column(best, buf, nk, col, chunk.len())
                    }
                    (ScanAcc::Max(best), AggKind::Max) => {
                        acc_max_column(best, buf, nk, col, chunk.len())
                    }
                    (
                        ScanAcc::Sum {
                            int_sum,
                            float_sum,
                            has_float,
                            has_any,
                        },
                        AggKind::Sum,
                    ) => acc_sum_column(
                        int_sum,
                        float_sum,
                        has_float,
                        has_any,
                        buf,
                        nk,
                        col,
                        chunk.len(),
                    )?,
                    (ScanAcc::Avg { sum, count }, AggKind::Avg) => {
                        acc_avg_column(sum, count, buf, nk, col, chunk.len())?
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    let mut out = new_frame(symtab);
    for (acc, spec) in accs.iter().zip(agg_specs.iter()) {
        let val = finalize_acc(acc);
        let name = projection_name(&aggs[spec.agg_idx]);
        if let Some(slot) = symtab.slot(&name) {
            out[slot] = val.into();
        }
    }

    Ok(Some(vec![out]))
}

/// Single-key grouped aggregation, no per-node Vec.
#[inline(never)]
fn agg_pushdown_single_key(
    graph: &mut dyn GraphView,
    node_ids: &[NodeId],
    agg_specs: &[AggSpec],
    prop_kids: &[Option<PropertyKeyId>],
    all_props: &[String],
    prop_index: &FxHashMap<&str, usize>,
    key_props: &[String],
    keys: &[ProjectionItem],
    aggs: &[ProjectionItem],
    symtab: &SymbolTable,
) -> Result<Option<Vec<Frame>>, CypherError> {
    let key_prop_idx = prop_index[key_props[0].as_str()];

    let nk = all_props.len();
    let resolved_kids = resolve_prop_kids(prop_kids);
    let mut pv_buf: Vec<Option<PropertyValue>> = vec![None; AGG_BATCH_SIZE * nk];

    // integer key? (first chunk decides) for fast hashing
    let first_chunk = &node_ids[..node_ids.len().min(AGG_BATCH_SIZE)];
    let first_buf = &mut pv_buf[..first_chunk.len() * nk];
    graph.batch_read_node_properties(first_chunk, &resolved_kids, first_buf)?;
    let key_is_int = column_is_integer(first_buf, nk, key_prop_idx, first_chunk.len());

    // typed i64 path when the key AND all agg columns are integers
    if key_is_int {
        let col_offsets: Vec<usize> = agg_specs
            .iter()
            .map(|s| s.prop.as_ref().map_or(0, |p| prop_index[p.as_str()]))
            .collect();
        let all_agg_int = agg_specs
            .iter()
            .zip(col_offsets.iter())
            .all(|(spec, &col)| {
                matches!(spec.kind, AggKind::CountStar | AggKind::CountProp)
                    || column_is_integer(first_buf, nk, col, first_chunk.len())
            });
        if all_agg_int {
            return crate::operators::aggregate_par::agg_pushdown_single_key_typed_i64(
                graph,
                node_ids,
                agg_specs,
                &resolved_kids,
                nk,
                key_prop_idx,
                &col_offsets,
                keys,
                aggs,
                symtab,
            );
        }
    }

    if key_is_int {
        // integer keys: FxHashMap<i64, usize> (~2 cycles to hash vs ~5 for PropJoinKey)
        let mut group_map: FxHashMap<i64, usize> = FxHashMap::default();
        let mut groups: Vec<(i64, Vec<ScanAcc>)> = Vec::new();
        let mut null_group: Option<Vec<ScanAcc>> = None;

        for chunk in node_ids.chunks(AGG_BATCH_SIZE) {
            let buf = &mut pv_buf[..chunk.len() * nk];
            // re-reading the first chunk is cheap and simpler than skipping it
            graph.batch_read_node_properties(chunk, &resolved_kids, buf)?;
            for i in 0..chunk.len() {
                let row = &buf[i * nk..(i + 1) * nk];
                match &row[key_prop_idx] {
                    Some(PropertyValue::Int(k)) => {
                        let gidx = if let Some(&idx) = group_map.get(k) {
                            idx
                        } else {
                            let idx = groups.len();
                            group_map.insert(*k, idx);
                            groups.push((
                                *k,
                                agg_specs.iter().map(|s| new_scan_acc(&s.kind)).collect(),
                            ));
                            idx
                        };
                        let group_accs = &mut groups[gidx].1;
                        for (acc, spec) in group_accs.iter_mut().zip(agg_specs.iter()) {
                            update_acc(acc, spec, row, prop_index)?;
                        }
                    }
                    _ => {
                        // null or missing key
                        let accs = null_group.get_or_insert_with(|| {
                            agg_specs.iter().map(|s| new_scan_acc(&s.kind)).collect()
                        });
                        for (acc, spec) in accs.iter_mut().zip(agg_specs.iter()) {
                            update_acc(acc, spec, row, prop_index)?;
                        }
                    }
                }
            }
        }

        let mut result =
            Vec::with_capacity(groups.len() + if null_group.is_some() { 1 } else { 0 });
        for (key_val, group_accs) in &groups {
            let mut out = new_frame(symtab);
            let name = projection_name(&keys[0]);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = SlotValue::Integer(*key_val);
            }
            for (acc, spec) in group_accs.iter().zip(agg_specs.iter()) {
                let val = finalize_acc(acc);
                let name = projection_name(&aggs[spec.agg_idx]);
                if let Some(slot) = symtab.slot(&name) {
                    out[slot] = val.into();
                }
            }
            result.push(out);
        }
        if let Some(null_accs) = &null_group {
            let mut out = new_frame(symtab);
            let name = projection_name(&keys[0]);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = SlotValue::Null;
            }
            for (acc, spec) in null_accs.iter().zip(agg_specs.iter()) {
                let val = finalize_acc(acc);
                let name = projection_name(&aggs[spec.agg_idx]);
                if let Some(slot) = symtab.slot(&name) {
                    out[slot] = val.into();
                }
            }
            result.push(out);
        }
        Ok(Some(result))
    } else {
        // generic PropJoinKey path (strings, booleans, mixed types)
        let mut group_map: FxHashMap<PropJoinKey, usize> = FxHashMap::default();
        let mut groups: Vec<(Option<PropertyValue>, Vec<ScanAcc>)> = Vec::new();

        for chunk in node_ids.chunks(AGG_BATCH_SIZE) {
            let buf = &mut pv_buf[..chunk.len() * nk];
            graph.batch_read_node_properties(chunk, &resolved_kids, buf)?;
            for i in 0..chunk.len() {
                let row = &buf[i * nk..(i + 1) * nk];
                let key_pv = &row[key_prop_idx];
                let hash_key = prop_to_join_key(key_pv);

                let group_idx = if let Some(&idx) = group_map.get(&hash_key) {
                    idx
                } else {
                    let idx = groups.len();
                    group_map.insert(hash_key, idx);
                    groups.push((
                        key_pv.clone(),
                        agg_specs.iter().map(|s| new_scan_acc(&s.kind)).collect(),
                    ));
                    idx
                };

                let group_accs = &mut groups[group_idx].1;
                for (acc, spec) in group_accs.iter_mut().zip(agg_specs.iter()) {
                    update_acc(acc, spec, row, prop_index)?;
                }
            }
        }

        let mut result = Vec::with_capacity(groups.len());
        for (key_pv, group_accs) in &groups {
            let mut out = new_frame(symtab);

            let name = projection_name(&keys[0]);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = match key_pv {
                    Some(pv) => SlotValue::from(prop_to_cypher(pv)),
                    None => SlotValue::Null,
                };
            }

            for (acc, spec) in group_accs.iter().zip(agg_specs.iter()) {
                let val = finalize_acc(acc);
                let name = projection_name(&aggs[spec.agg_idx]);
                if let Some(slot) = symtab.slot(&name) {
                    out[slot] = val.into();
                }
            }

            result.push(out);
        }

        Ok(Some(result))
    }
}

/// Multi-key grouped aggregation.
#[inline(never)]
fn agg_pushdown_multi_key(
    graph: &mut dyn GraphView,
    node_ids: &[NodeId],
    agg_specs: &[AggSpec],
    prop_kids: &[Option<PropertyKeyId>],
    all_props: &[String],
    prop_index: &FxHashMap<&str, usize>,
    key_props: &[String],
    keys: &[ProjectionItem],
    aggs: &[ProjectionItem],
    symtab: &SymbolTable,
) -> Result<Option<Vec<Frame>>, CypherError> {
    let mut group_map: FxHashMap<Vec<PropJoinKey>, usize> = FxHashMap::default();
    let mut groups: Vec<(Vec<Option<PropertyValue>>, Vec<ScanAcc>)> = Vec::new();

    let nk = all_props.len();
    let resolved_kids = resolve_prop_kids(prop_kids);
    let mut pv_buf: Vec<Option<PropertyValue>> = vec![None; AGG_BATCH_SIZE * nk];

    for chunk in node_ids.chunks(AGG_BATCH_SIZE) {
        let buf = &mut pv_buf[..chunk.len() * nk];
        graph.batch_read_node_properties(chunk, &resolved_kids, buf)?;
        for i in 0..chunk.len() {
            let row = &buf[i * nk..(i + 1) * nk];
            let key_pvs: Vec<Option<PropertyValue>> = key_props
                .iter()
                .map(|p| row[prop_index[p.as_str()]].clone())
                .collect();
            let hash_key: Vec<PropJoinKey> = key_pvs.iter().map(prop_to_join_key).collect();

            let group_idx = if let Some(&idx) = group_map.get(&hash_key) {
                idx
            } else {
                let idx = groups.len();
                group_map.insert(hash_key, idx);
                groups.push((
                    key_pvs,
                    agg_specs.iter().map(|s| new_scan_acc(&s.kind)).collect(),
                ));
                idx
            };

            let group_accs = &mut groups[group_idx].1;
            for (acc, spec) in group_accs.iter_mut().zip(agg_specs.iter()) {
                update_acc(acc, spec, row, prop_index)?;
            }
        }
    }

    // empty input, no keys: one row of defaults
    if groups.is_empty() && keys.is_empty() {
        groups.push((
            Vec::new(),
            agg_specs.iter().map(|s| new_scan_acc(&s.kind)).collect(),
        ));
    }

    let mut result = Vec::with_capacity(groups.len());
    for (key_pvs, group_accs) in &groups {
        let mut out = new_frame(symtab);

        for (i, key_item) in keys.iter().enumerate() {
            let name = projection_name(key_item);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = match &key_pvs[i] {
                    Some(pv) => SlotValue::from(prop_to_cypher(pv)),
                    None => SlotValue::Null,
                };
            }
        }

        for (acc, spec) in group_accs.iter().zip(agg_specs.iter()) {
            let val = finalize_acc(acc);
            let name = projection_name(&aggs[spec.agg_idx]);
            if let Some(slot) = symtab.slot(&name) {
                out[slot] = val.into();
            }
        }

        result.push(out);
    }

    Ok(Some(result))
}

// -- Typed i64 global aggregation ---------------------------------------

/// Global aggregation on typed i64 buffers, no PropertyValue.
#[inline(never)]
fn agg_pushdown_global_typed_i64(
    graph: &mut dyn GraphView,
    node_ids: &[NodeId],
    mut accs: Vec<ScanAcc>,
    agg_specs: &[AggSpec],
    resolved_kids: &[PropertyKeyId],
    nk: usize,
    col_offsets: &[usize],
    aggs: &[ProjectionItem],
    symtab: &SymbolTable,
) -> Result<Option<Vec<Frame>>, CypherError> {
    // parallel for read transactions
    if let Some(sync_graph) = graph.as_sync_ref() {
        return crate::operators::aggregate_par::agg_pushdown_global_typed_i64_par(
            sync_graph,
            node_ids,
            accs,
            agg_specs,
            resolved_kids,
            nk,
            col_offsets,
            aggs,
            symtab,
        );
    }

    let buf_size = AGG_BATCH_SIZE * nk;
    let mut val_buf = vec![0i64; buf_size];
    let mut valid_buf = vec![false; buf_size];

    for chunk in node_ids.chunks(AGG_BATCH_SIZE) {
        let n = chunk.len() * nk;
        for v in valid_buf[..n].iter_mut() {
            *v = false;
        }
        graph.batch_read_node_properties_i64(
            chunk,
            resolved_kids,
            &mut val_buf[..n],
            &mut valid_buf[..n],
        )?;

        for ((acc, spec), &col) in accs
            .iter_mut()
            .zip(agg_specs.iter())
            .zip(col_offsets.iter())
        {
            match (acc, &spec.kind) {
                (_, AggKind::CountStar) => {}
                (ScanAcc::Count(c), AggKind::CountProp) => {
                    acc_count_valid(c, &valid_buf[..n], nk, col, chunk.len());
                }
                (
                    ScanAcc::Sum {
                        int_sum,
                        float_sum,
                        has_float: _,
                        has_any,
                    },
                    AggKind::Sum,
                ) => {
                    acc_sum_column_i64(
                        int_sum,
                        float_sum,
                        has_any,
                        &val_buf[..n],
                        &valid_buf[..n],
                        nk,
                        col,
                        chunk.len(),
                    );
                }
                (ScanAcc::Avg { sum, count }, AggKind::Avg) => {
                    acc_avg_column_i64(
                        sum,
                        count,
                        &val_buf[..n],
                        &valid_buf[..n],
                        nk,
                        col,
                        chunk.len(),
                    );
                }
                (ScanAcc::Min(best), AggKind::Min) => {
                    acc_min_column_i64(best, &val_buf[..n], &valid_buf[..n], nk, col, chunk.len());
                }
                (ScanAcc::Max(best), AggKind::Max) => {
                    acc_max_column_i64(best, &val_buf[..n], &valid_buf[..n], nk, col, chunk.len());
                }
                _ => unreachable!(),
            }
        }
    }

    // finalize via finalize_acc + projection_name
    let mut out = new_frame(symtab);
    for (acc, spec) in accs.iter().zip(agg_specs.iter()) {
        let val = finalize_acc(acc);
        let name = projection_name(&aggs[spec.agg_idx]);
        if let Some(slot) = symtab.slot(&name) {
            out[slot] = val.into();
        }
    }
    Ok(Some(vec![out]))
}
