//! Parallel aggregate pushdown, in its own compilation unit so it doesn't push the sequential
//! aggregate code out of the I-cache.

use std::cmp::Ordering;

use rustc_hash::FxHashMap;

use traverse_core::graph::types::{NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::ast::ProjectionItem;
use crate::error::CypherError;
use crate::executor::projection_name;
use crate::frame::{Frame, SlotValue, SymbolTable, new_frame};

use super::aggregate::{AGG_BATCH_SIZE, AggKind, AggSpec, ScanAcc};
use super::batch_types::{
    acc_avg_column_i64, acc_count_valid, acc_max_column_i64, acc_min_column_i64,
    acc_sum_column_i64, compare_property_values,
};

/// Parallel global aggregation over typed i64 buffers.
///
/// Splits `node_ids` into large chunks for rayon workers with thread-local
/// accumulators, merged after the parallel section. Only for read transactions
/// that provide a `Sync` reference.
#[inline(never)]
pub(crate) fn agg_pushdown_global_typed_i64_par(
    graph: &(dyn GraphView + Sync),
    node_ids: &[NodeId],
    mut accs: Vec<ScanAcc>,
    agg_specs: &[AggSpec],
    resolved_kids: &[PropertyKeyId],
    nk: usize,
    col_offsets: &[usize],
    aggs: &[ProjectionItem],
    symtab: &SymbolTable,
) -> Result<Option<Vec<Frame>>, CypherError> {
    use rayon::prelude::*;

    const PAR_CHUNK: usize = 65536; // 64K nodes per rayon task

    let partial_results: Result<Vec<Vec<ScanAcc>>, CypherError> = node_ids
        .par_chunks(PAR_CHUNK)
        .map(|big_chunk| {
            let mut local_accs: Vec<ScanAcc> =
                agg_specs.iter().map(|s| ScanAcc::new(&s.kind)).collect();
            let mut val_buf = vec![0i64; AGG_BATCH_SIZE * nk];
            let mut valid_buf = vec![false; AGG_BATCH_SIZE * nk];

            for chunk in big_chunk.chunks(AGG_BATCH_SIZE) {
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

                for ((acc, spec), &col) in local_accs
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
                            acc_min_column_i64(
                                best,
                                &val_buf[..n],
                                &valid_buf[..n],
                                nk,
                                col,
                                chunk.len(),
                            );
                        }
                        (ScanAcc::Max(best), AggKind::Max) => {
                            acc_max_column_i64(
                                best,
                                &val_buf[..n],
                                &valid_buf[..n],
                                nk,
                                col,
                                chunk.len(),
                            );
                        }
                        _ => unreachable!(),
                    }
                }
            }
            Ok(local_accs)
        })
        .collect();

    // merge partial accumulators
    for partial in partial_results? {
        for (target, source) in accs.iter_mut().zip(partial.iter()) {
            merge_scan_acc(target, source);
        }
    }

    // finalize
    let mut out = new_frame(symtab);
    for (acc, spec) in accs.iter().zip(agg_specs.iter()) {
        let val = acc.finalize();
        let name = projection_name(&aggs[spec.agg_idx]);
        if let Some(slot) = symtab.slot(&name) {
            out[slot] = val.into();
        }
    }
    Ok(Some(vec![out]))
}

/// Typed i64 single-key grouped aggregation: `batch_read_node_properties_i64` for key and aggregate
/// columns, so no PropertyValue is built when every column is integer.
#[inline(never)]
pub(crate) fn agg_pushdown_single_key_typed_i64(
    graph: &mut dyn GraphView,
    node_ids: &[NodeId],
    agg_specs: &[AggSpec],
    resolved_kids: &[PropertyKeyId],
    nk: usize,
    key_col: usize,
    col_offsets: &[usize],
    keys: &[ProjectionItem],
    aggs: &[ProjectionItem],
    symtab: &SymbolTable,
) -> Result<Option<Vec<Frame>>, CypherError> {
    let mut group_map: FxHashMap<i64, usize> = FxHashMap::default();
    let mut groups: Vec<(i64, Vec<ScanAcc>)> = Vec::new();
    let mut null_group: Option<Vec<ScanAcc>> = None;

    let mut val_buf = vec![0i64; AGG_BATCH_SIZE * nk];
    let mut valid_buf = vec![false; AGG_BATCH_SIZE * nk];

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
        for i in 0..chunk.len() {
            let base = i * nk;
            if !valid_buf[base + key_col] {
                // null key group
                let accs = null_group.get_or_insert_with(|| {
                    agg_specs.iter().map(|s| ScanAcc::new(&s.kind)).collect()
                });
                accumulate_i64_row(accs, agg_specs, col_offsets, &val_buf, &valid_buf, base);
                continue;
            }
            let k = val_buf[base + key_col];
            let gidx = if let Some(&idx) = group_map.get(&k) {
                idx
            } else {
                let idx = groups.len();
                group_map.insert(k, idx);
                groups.push((k, agg_specs.iter().map(|s| ScanAcc::new(&s.kind)).collect()));
                idx
            };
            accumulate_i64_row(
                &mut groups[gidx].1,
                agg_specs,
                col_offsets,
                &val_buf,
                &valid_buf,
                base,
            );
        }
    }

    // output
    let key_name = projection_name(&keys[0]);
    let key_slot = symtab.slot(&key_name);
    let agg_slots: Vec<(usize, Option<usize>)> = agg_specs
        .iter()
        .map(|spec| {
            let name = projection_name(&aggs[spec.agg_idx]);
            (spec.agg_idx, symtab.slot(&name))
        })
        .collect();

    let mut result = Vec::with_capacity(groups.len() + if null_group.is_some() { 1 } else { 0 });
    for (key_val, group_accs) in &groups {
        let mut out = new_frame(symtab);
        if let Some(slot) = key_slot {
            out[slot] = SlotValue::Integer(*key_val);
        }
        for (acc, &(_, slot)) in group_accs.iter().zip(agg_slots.iter()) {
            if let Some(s) = slot {
                out[s] = finalize_acc_to_slot(acc);
            }
        }
        result.push(out);
    }
    if let Some(null_accs) = &null_group {
        let mut out = new_frame(symtab);
        if let Some(slot) = key_slot {
            out[slot] = SlotValue::Null;
        }
        for (acc, &(_, slot)) in null_accs.iter().zip(agg_slots.iter()) {
            if let Some(s) = slot {
                out[s] = finalize_acc_to_slot(acc);
            }
        }
        result.push(out);
    }
    Ok(Some(result))
}

/// Accumulates one row of i64 values into the group accumulators.
#[inline(always)]
fn accumulate_i64_row(
    accs: &mut [ScanAcc],
    agg_specs: &[AggSpec],
    col_offsets: &[usize],
    val_buf: &[i64],
    valid_buf: &[bool],
    base: usize,
) {
    for ((acc, spec), &col) in accs
        .iter_mut()
        .zip(agg_specs.iter())
        .zip(col_offsets.iter())
    {
        match (acc, &spec.kind) {
            (ScanAcc::Count(c), AggKind::CountStar) => {
                *c += 1;
            }
            (ScanAcc::Count(c), AggKind::CountProp) => {
                if valid_buf[base + col] {
                    *c += 1;
                }
            }
            (
                ScanAcc::Sum {
                    int_sum,
                    float_sum,
                    has_any,
                    ..
                },
                AggKind::Sum,
            ) => {
                if valid_buf[base + col] {
                    *has_any = true;
                    *int_sum += val_buf[base + col];
                    *float_sum += val_buf[base + col] as f64;
                }
            }
            (ScanAcc::Avg { sum, count }, AggKind::Avg) => {
                if valid_buf[base + col] {
                    *sum += val_buf[base + col] as f64;
                    *count += 1;
                }
            }
            (ScanAcc::Min(best), AggKind::Min) => {
                if valid_buf[base + col] {
                    let v = val_buf[base + col];
                    if best.is_none()
                        || v < best.as_ref().map_or(i64::MAX, |pv| match pv {
                            traverse_core::graph::property::PropertyValue::Int(i) => *i,
                            _ => i64::MAX,
                        })
                    {
                        *best = Some(traverse_core::graph::property::PropertyValue::Int(v));
                    }
                }
            }
            (ScanAcc::Max(best), AggKind::Max) if valid_buf[base + col] => {
                let v = val_buf[base + col];
                if best.is_none()
                    || v > best.as_ref().map_or(i64::MIN, |pv| match pv {
                        traverse_core::graph::property::PropertyValue::Int(i) => *i,
                        _ => i64::MIN,
                    })
                {
                    *best = Some(traverse_core::graph::property::PropertyValue::Int(v));
                }
            }
            _ => {}
        }
    }
}

/// ScanAcc straight to SlotValue (no CypherValue in between).
#[inline(always)]
fn finalize_acc_to_slot(acc: &ScanAcc) -> SlotValue {
    match acc {
        ScanAcc::Count(c) => SlotValue::Integer(*c),
        ScanAcc::Min(Some(traverse_core::graph::property::PropertyValue::Int(i))) => {
            SlotValue::Integer(*i)
        }
        ScanAcc::Min(None) => SlotValue::Null,
        ScanAcc::Max(Some(traverse_core::graph::property::PropertyValue::Int(i))) => {
            SlotValue::Integer(*i)
        }
        ScanAcc::Max(None) => SlotValue::Null,
        ScanAcc::Sum {
            int_sum,
            float_sum,
            has_float,
            has_any,
        } => {
            if !has_any {
                SlotValue::Integer(0)
            } else if *has_float {
                SlotValue::Float(*float_sum)
            } else {
                SlotValue::Integer(*int_sum)
            }
        }
        ScanAcc::Avg { sum, count } => {
            if *count == 0 {
                SlotValue::Null
            } else {
                SlotValue::Float(*sum / *count as f64)
            }
        }
        // non-integer Min/Max shouldn't get here, but handle it
        _ => acc.finalize().into(),
    }
}

/// Merges `source` into `target` (parallel reduce).
fn merge_scan_acc(target: &mut ScanAcc, source: &ScanAcc) {
    match (target, source) {
        (ScanAcc::Count(t), ScanAcc::Count(s)) => *t += s,
        (ScanAcc::Min(t), ScanAcc::Min(s)) => {
            if let Some(sv) = s
                && (t.is_none()
                    || compare_property_values(sv, t.as_ref().unwrap()) == Ordering::Less)
            {
                *t = Some(sv.clone());
            }
        }
        (ScanAcc::Max(t), ScanAcc::Max(s)) => {
            if let Some(sv) = s
                && (t.is_none()
                    || compare_property_values(sv, t.as_ref().unwrap()) == Ordering::Greater)
            {
                *t = Some(sv.clone());
            }
        }
        (
            ScanAcc::Sum {
                int_sum: ti,
                float_sum: tf,
                has_float: thf,
                has_any: tha,
            },
            ScanAcc::Sum {
                int_sum: si,
                float_sum: sf,
                has_float: shf,
                has_any: sha,
            },
        ) => {
            if *sha {
                *tha = true;
                *ti += si;
                *tf += sf;
                if *shf {
                    *thf = true;
                }
            }
        }
        (ScanAcc::Avg { sum: ts, count: tc }, ScanAcc::Avg { sum: ss, count: sc }) => {
            *ts += ss;
            *tc += sc;
        }
        _ => {}
    }
}
