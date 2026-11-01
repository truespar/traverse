//! Projection: RETURN / WITH items, and the fused scan + filter + project.

use rustc_hash::{FxHashMap, FxHashSet};
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::ast::{Expr, ProjectionItem};
use crate::error::CypherError;
use crate::frame::{Frame, SlotValue, SymbolTable, new_frame};
use crate::types::CypherValue;

use super::bound::{bound_capture_armed, flush_bound};
use super::describe::projection_name;
use super::eval::{eval_expr, is_truthy};
use super::values::dedup_frames;

#[inline(never)]
pub(crate) fn exec_project_items(
    frames: &[Frame],
    items: &[ProjectionItem],
    distinct: bool,
    where_clause: Option<&Expr>,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    // fast path: single-property DISTINCT projection (RETURN DISTINCT n.id) fusing property read +
    // dedup in one batch pass, no per-row get_node clones, eval_expr or Vec-key dedup
    if distinct
        && items.len() == 1
        && where_clause.is_none()
        && !frames.is_empty()
        && let Expr::Property(base, key) = &items[0].expr
        && let Expr::Variable(var) = base.as_ref()
        && let (Some(src_slot), Some(pid)) = (
            symtab.slot(var),
            graph.strings().get(key).map(PropertyKeyId),
        )
        && matches!(frames[0][src_slot], SlotValue::NodeRef(_))
    {
        let name = projection_name(&items[0]);
        if let Some(dst_slot) = symtab.slot(&name) {
            let node_ids: Vec<NodeId> = frames
                .iter()
                .filter_map(|f| match f[src_slot] {
                    SlotValue::NodeRef(id) => Some(id),
                    _ => None,
                })
                .collect();

            let keys = &[pid];
            let mut vals = vec![0i64; node_ids.len()];
            let mut valid = vec![false; node_ids.len()];
            graph
                .batch_read_node_properties_i64(&node_ids, keys, &mut vals, &mut valid)
                .map_err(CypherError::from)?;

            // i64 fast path only if some value was valid (integer property); else
            // the generic path handles strings/other types
            if valid.iter().any(|&v| v) {
                let mut seen = FxHashSet::default();
                let mut seen_null = false;
                let mut result = Vec::new();
                for i in 0..node_ids.len() {
                    if valid[i] {
                        if seen.insert(vals[i]) {
                            let mut f = new_frame(symtab);
                            f[dst_slot] = SlotValue::Integer(vals[i]);
                            result.push(f);
                        }
                    } else if !seen_null {
                        seen_null = true;
                        let f = new_frame(symtab);
                        result.push(f);
                    }
                }
                return Ok(result);
            }
        }
    }

    // group property accesses by source slot so each node/edge is fetched once for all its
    // properties; keys pre-resolved to PropertyKeyId
    let mut prop_batches: FxHashMap<usize, Vec<(usize, PropertyKeyId)>> = FxHashMap::default();
    for (idx, item) in items.iter().enumerate() {
        if let Expr::Property(base, key) = &item.expr
            && let Expr::Variable(var) = base.as_ref()
            && let Some(slot) = symtab.slot(var)
            && let Some(pid) = graph.strings().get(key).map(PropertyKeyId)
        {
            prop_batches.entry(slot).or_default().push((idx, pid));
        }
    }
    prop_batches.retain(|_, v| v.len() >= 2);

    // -- destination slots computed once, not per frame --
    let dst_slots: Vec<Option<usize>> = items
        .iter()
        .map(|item| symtab.slot(&projection_name(item)))
        .collect();

    // -- last-value cache per item for consecutive same-entity lookups --
    // expansions emit runs of frames with the same source node (one per edge), so
    // caching skips most repeated `n.prop` reads
    struct PropCacheEntry {
        src_id: NodeId, // also used for edges (EdgeId reinterpreted)
        value: SlotValue,
    }
    // per item: (source_slot, property_key_id) for plain `var.key`
    let prop_cache_info: Vec<Option<(usize, PropertyKeyId)>> = items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            // already handled by prop_batches
            if prop_batches
                .values()
                .any(|batch| batch.iter().any(|(i, _)| *i == idx))
            {
                return None;
            }
            if let Expr::Property(base, key) = &item.expr
                && let Expr::Variable(var) = base.as_ref()
                && let Some(slot) = symtab.slot(var)
                && let Some(pid) = graph.strings().get(key).map(PropertyKeyId)
            {
                return Some((slot, pid));
            }
            None
        })
        .collect();
    let mut prop_cache: Vec<Option<PropCacheEntry>> = (0..items.len()).map(|_| None).collect();

    // -- batch i64 path: when EVERY item is NodeRef.property, batch-read --
    // a couple of batch calls per morsel instead of one lookup (and lock) per row
    if !items.is_empty() && where_clause.is_none() && !frames.is_empty() {
        // every item must be a plain `var.key` on a NodeRef slot
        let specs: Vec<Option<(usize, PropertyKeyId)>> = items
            .iter()
            .map(|item| {
                if let Expr::Property(base, key) = &item.expr
                    && let Expr::Variable(var) = base.as_ref()
                    && let Some(slot) = symtab.slot(var)
                    && let Some(pid) = graph.strings().get(key).map(PropertyKeyId)
                    && matches!(frames[0][slot], SlotValue::NodeRef(_))
                {
                    return Some((slot, pid));
                }
                None
            })
            .collect();

        if specs.iter().all(|x| x.is_some()) {
            let specs: Vec<(usize, PropertyKeyId)> =
                specs.into_iter().map(|x| x.unwrap()).collect();
            let n = frames.len();
            let nk = items.len();

            // dedup (slot, pid): several items may share slot+key
            let mut unique_keys: Vec<(usize, PropertyKeyId)> = Vec::new();
            // item_idx -> index into unique_keys
            let mut item_to_unique: Vec<usize> = Vec::with_capacity(nk);
            for &(slot, pid) in &specs {
                let pos = unique_keys.iter().position(|&(s, p)| s == slot && p == pid);
                if let Some(p) = pos {
                    item_to_unique.push(p);
                } else {
                    item_to_unique.push(unique_keys.len());
                    unique_keys.push((slot, pid));
                }
            }

            // batch-read each unique (slot, pid)
            let mut all_vals: Vec<Vec<i64>> = Vec::with_capacity(unique_keys.len());
            let mut all_valid: Vec<Vec<bool>> = Vec::with_capacity(unique_keys.len());
            let mut all_i64 = true;
            for &(slot, pid) in &unique_keys {
                let node_ids: Vec<NodeId> = frames
                    .iter()
                    .map(|f| match f[slot] {
                        SlotValue::NodeRef(id) => id,
                        _ => NodeId(u64::MAX),
                    })
                    .collect();
                let keys = &[pid];
                let mut vals = vec![0i64; n];
                let mut valid = vec![false; n];
                graph
                    .batch_read_node_properties_i64(&node_ids, keys, &mut vals, &mut valid)
                    .map_err(CypherError::from)?;
                // nothing valid: non-i64 or missing property, bail (the fallback handles the rare
                // all-null case)
                if !valid.iter().any(|&v| v) {
                    all_i64 = false;
                    break;
                }
                all_vals.push(vals);
                all_valid.push(valid);
            }

            if all_i64 {
                let mut result = Vec::new();
                for i in 0..n {
                    let mut nf = new_frame(symtab);
                    for item_idx in 0..nk {
                        if let Some(dst) = dst_slots[item_idx] {
                            let uk = item_to_unique[item_idx];
                            if all_valid[uk][i] {
                                nf[dst] = SlotValue::Integer(all_vals[uk][i]);
                            }
                            // else SlotValue::Null (default)
                        }
                    }
                    result.push(nf);
                }
                if distinct {
                    result = dedup_frames(result, symtab);
                }
                return Ok(result);
            }
            // non-i64 property found: fall through to the per-frame loop
        }
    }

    let mut result = Vec::new();
    for frame in frames {
        let mut nf = new_frame(symtab);
        // pre-resolve batched accesses (2+ props from the same entity)
        let mut precomputed: Vec<Option<CypherValue>> = vec![None; items.len()];
        for (&slot, batch) in &prop_batches {
            match &frame[slot] {
                SlotValue::NodeRef(id) => {
                    if let Ok(node) = graph.get_node(*id) {
                        for (item_idx, pid) in batch {
                            precomputed[*item_idx] = Some(
                                node.properties
                                    .get(*pid)
                                    .map(|pv| CypherValue::from_property(&pv))
                                    .unwrap_or(CypherValue::Null),
                            );
                        }
                    }
                }
                SlotValue::EdgeRef { id, .. } => {
                    if let Ok(edge) = graph.get_edge(*id) {
                        for (item_idx, pid) in batch {
                            precomputed[*item_idx] = Some(
                                edge.properties
                                    .get(*pid)
                                    .map(|pv| CypherValue::from_property(&pv))
                                    .unwrap_or(CypherValue::Null),
                            );
                        }
                    }
                }
                SlotValue::Heap(cv) => match cv.as_ref() {
                    CypherValue::NodeRef(id) | CypherValue::Node { id, .. } => {
                        if let Ok(node) = graph.get_node(*id) {
                            for (item_idx, pid) in batch {
                                precomputed[*item_idx] = Some(
                                    node.properties
                                        .get(*pid)
                                        .map(|pv| CypherValue::from_property(&pv))
                                        .unwrap_or(CypherValue::Null),
                                );
                            }
                        }
                    }
                    CypherValue::EdgeRef { id, .. } | CypherValue::Edge { id, .. } => {
                        if let Ok(edge) = graph.get_edge(*id) {
                            for (item_idx, pid) in batch {
                                precomputed[*item_idx] = Some(
                                    edge.properties
                                        .get(*pid)
                                        .map(|pv| CypherValue::from_property(&pv))
                                        .unwrap_or(CypherValue::Null),
                                );
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        for i in 0..items.len() {
            let dst = match dst_slots[i] {
                Some(s) => s,
                None => continue,
            };
            if let Some(cached) = precomputed[i].take() {
                nf[dst] = cached.into();
                continue;
            }
            // property cache: same source entity as the last frame?
            if let Some((src_slot, pid)) = &prop_cache_info[i] {
                match &frame[*src_slot] {
                    SlotValue::NodeRef(nid) => {
                        if let Some(entry) = &prop_cache[i]
                            && entry.src_id == *nid
                        {
                            // hit: reuse
                            nf[dst] = entry.value.clone();
                            continue;
                        }
                        // miss: look up. A deleted entity must error, not read as Null, so
                        // DELETE-then-access fails as the TCK requires
                        let sv = match graph.get_node_property_kid(*nid, *pid) {
                            Ok(Some(pv)) => SlotValue::from_property(pv),
                            Ok(None) => SlotValue::Null,
                            Err(_) => {
                                return Err(CypherError::semantic(
                                    "deleted entity access".to_string(),
                                ));
                            }
                        };
                        nf[dst] = sv.clone();
                        prop_cache[i] = Some(PropCacheEntry {
                            src_id: *nid,
                            value: sv,
                        });
                        continue;
                    }
                    SlotValue::EdgeRef { id, .. } => {
                        let eid_as_nid = NodeId(id.0);
                        if let Some(entry) = &prop_cache[i]
                            && entry.src_id == eid_as_nid
                        {
                            nf[dst] = entry.value.clone();
                            continue;
                        }
                        let sv = match graph.get_edge(*id) {
                            Ok(edge) => edge
                                .properties
                                .get(*pid)
                                .map(SlotValue::from_property)
                                .unwrap_or(SlotValue::Null),
                            Err(_) => {
                                return Err(CypherError::semantic(
                                    "deleted entity access".to_string(),
                                ));
                            }
                        };
                        nf[dst] = sv.clone();
                        prop_cache[i] = Some(PropCacheEntry {
                            src_id: eid_as_nid,
                            value: sv,
                        });
                        continue;
                    }
                    _ => {} // fall through to eval_expr
                }
            }
            let val = eval_expr(&items[i].expr, frame, graph, symtab)?;
            nf[dst] = val.into();
        }
        // WHERE sees old (frame) and new (new_frame) variables
        if let Some(wc) = where_clause {
            let mut merged = frame.clone();
            for (i, val) in nf.iter().enumerate() {
                if !val.is_null() {
                    merged[i] = val.clone();
                }
            }
            let val = eval_expr(wc, &merged, graph, symtab)?;
            if !is_truthy(&val) {
                continue;
            }
        }
        result.push(nf);
    }
    if distinct {
        result = dedup_frames(result, symtab);
    }
    Ok(result)
}

// -- Fused scan+filter+project -----------------------------------------

const AGG_BATCH_SIZE: usize = 1024;

/// Fused scan + filter + project in one pass.
#[inline(never)]
pub(crate) fn exec_scan_filter_project(
    variable: &str,
    label: &Option<String>,
    predicate: &Option<Expr>,
    items: &[ProjectionItem],
    distinct: bool,
    graph: &mut dyn GraphView,
    limit: Option<usize>,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    let node_ids: Vec<NodeId> = match label {
        Some(lbl) => graph.nodes_by_label(lbl),
        None => graph.nodes_vec(),
    };
    let scan_slot = symtab.slot(variable).unwrap();

    // property keys resolved once (string -> PropertyKeyId)
    let prop_keys: Vec<Option<PropertyKeyId>> = items
        .iter()
        .map(|item| {
            if let Expr::Property(base, key) = &item.expr
                && let Expr::Variable(var) = base.as_ref()
                && var == variable
            {
                return graph.strings().get(key).map(PropertyKeyId);
            }
            None
        })
        .collect();
    let out_slots: Vec<Option<usize>> = items
        .iter()
        .map(|item| symtab.slot(&projection_name(item)))
        .collect();

    let all_prop_keys = prop_keys.iter().all(|k| k.is_some());
    let has_prop_keys = prop_keys.iter().any(|k| k.is_some());
    let mut result = Vec::new();
    let mut frame = new_frame(symtab);
    // this op never emits its scan variable, so capture provenance here or the binding is lost (see
    // CypherResult)
    let mut bound_batch: Vec<CypherValue> = Vec::new();

    if all_prop_keys && !items.is_empty() {
        // fast path, all projections plain n.prop: phase 1 filters and collects passing IDs
        let mut passing_ids: Vec<NodeId> = Vec::new();
        for &nid in &node_ids {
            frame[scan_slot] = SlotValue::NodeRef(nid);
            if let Some(pred) = predicate
                && !is_truthy(&eval_expr(pred, &frame, graph, symtab)?)
            {
                continue;
            }
            passing_ids.push(nid);
            if limit.is_some_and(|l| passing_ids.len() >= l) {
                break;
            }
        }
        if bound_capture_armed() {
            bound_batch.extend(passing_ids.iter().map(|&n| CypherValue::NodeRef(n)));
        }

        // phase 2: batch-read projected properties (one lock per 1024 nodes)
        let resolved_keys: Vec<PropertyKeyId> = prop_keys.iter().map(|k| k.unwrap()).collect();
        let nk = resolved_keys.len();
        let mut pv_buf: Vec<Option<PropertyValue>> = vec![None; AGG_BATCH_SIZE * nk];

        // probe the first chunk for all-integer projections
        let first_chunk = &passing_ids[..passing_ids.len().min(AGG_BATCH_SIZE)];
        if !first_chunk.is_empty() {
            let probe_buf = &mut pv_buf[..first_chunk.len() * nk];
            graph.batch_read_node_properties(first_chunk, &resolved_keys, probe_buf)?;
            let all_int = (0..nk).all(|j| {
                crate::operators::batch_types::column_is_integer(
                    probe_buf,
                    nk,
                    j,
                    first_chunk.len(),
                )
            });
            if all_int {
                // typed i64 path, no PropertyValue
                let mut val_buf = vec![0i64; AGG_BATCH_SIZE * nk];
                let mut valid_buf = vec![false; AGG_BATCH_SIZE * nk];
                for chunk in passing_ids.chunks(AGG_BATCH_SIZE) {
                    let n = chunk.len() * nk;
                    for v in valid_buf[..n].iter_mut() {
                        *v = false;
                    }
                    graph.batch_read_node_properties_i64(
                        chunk,
                        &resolved_keys,
                        &mut val_buf[..n],
                        &mut valid_buf[..n],
                    )?;
                    for i in 0..chunk.len() {
                        let mut out = new_frame(symtab);
                        for j in 0..nk {
                            if let Some(slot) = out_slots[j] {
                                let idx = i * nk + j;
                                out[slot] = if valid_buf[idx] {
                                    SlotValue::Integer(val_buf[idx])
                                } else {
                                    SlotValue::Null
                                };
                            }
                        }
                        result.push(out);
                    }
                }
                if distinct {
                    result = dedup_frames(result, symtab);
                }
                flush_bound(bound_batch);
                return Ok(result);
            }
        }
        // PropertyValue path
        for chunk in passing_ids.chunks(AGG_BATCH_SIZE) {
            let buf = &mut pv_buf[..chunk.len() * nk];
            graph.batch_read_node_properties(chunk, &resolved_keys, buf)?;
            for i in 0..chunk.len() {
                let mut out = new_frame(symtab);
                for j in 0..nk {
                    if let Some(slot) = out_slots[j] {
                        out[slot] = match buf[i * nk + j].take() {
                            Some(pv) => SlotValue::from_property(pv),
                            None => SlotValue::Null,
                        };
                    }
                }
                result.push(out);
            }
        }
    } else {
        // fallback: mixed projections or no items, per node
        for nid in node_ids {
            frame[scan_slot] = SlotValue::NodeRef(nid);

            if let Some(pred) = predicate
                && !is_truthy(&eval_expr(pred, &frame, graph, symtab)?)
            {
                continue;
            }

            let mut out = new_frame(symtab);
            let node = if has_prop_keys {
                graph.get_node(nid).ok()
            } else {
                None
            };
            for (i, item) in items.iter().enumerate() {
                if let Some(slot) = out_slots[i] {
                    out[slot] = if let Some(kid) = prop_keys[i] {
                        node.as_ref()
                            .and_then(|n| n.properties.get(kid))
                            .map(SlotValue::from_property)
                            .unwrap_or(SlotValue::Null)
                    } else {
                        eval_expr(&item.expr, &frame, graph, symtab)?.into()
                    };
                }
            }
            if bound_capture_armed() {
                bound_batch.push(CypherValue::NodeRef(nid));
            }
            result.push(out);
            if limit.is_some_and(|l| result.len() >= l) {
                break;
            }
        }
    }
    if distinct {
        result = dedup_frames(result, symtab);
    }
    flush_bound(bound_batch);
    Ok(result)
}
