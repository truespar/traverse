//! Flow-control operators: Limit, Skip, Sort, Unwind, Union.

use traverse_core::graph::types::PropertyKeyId;
use traverse_core::tx::view::GraphView;

use crate::ast::{Expr, SortItem};
use crate::error::CypherError;
use crate::executor::{
    compare_values, dedup_frames, eval_expr, eval_integer_expr, exec_streaming_expand_chain,
    extract_expand_chain,
};
use crate::frame::{Frame, SlotValue, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Operator;

// -- LimitOp ------------------------------------------------------------

pub(crate) struct LimitOp<'p> {
    pub(crate) child: Box<dyn Operator + 'p>,
    pub(crate) count_expr: &'p Expr,
    limit: Option<usize>,
    emitted: usize,
}

impl<'p> LimitOp<'p> {
    pub(crate) fn new(child: Box<dyn Operator + 'p>, count_expr: &'p Expr) -> Self {
        Self {
            child,
            count_expr,
            limit: None,
            emitted: 0,
        }
    }
}

impl<'p> Operator for LimitOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.limit.is_none() {
            let n = eval_integer_expr(self.count_expr, &*graph)?;
            if n < 0 {
                return Err(CypherError::semantic(
                    "LIMIT must be a non-negative integer".to_string(),
                )
                .with_status(crate::status::GqlStatus::NegativeValue));
            }
            let lim = n as usize;
            self.limit = Some(lim);
            self.child.limit_hint(lim);
        }
        let lim = self.limit.unwrap();
        if self.emitted >= lim {
            return Ok(None);
        }
        if let Some(frame) = self.child.next(graph, symtab)? {
            self.emitted += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

/// Streaming expand chain + limit: under a Limit, chains of 2+ consecutive fixed-length expands run
/// depth-first without materializing intermediate frames. Wraps `exec_streaming_expand_chain`.
pub(crate) struct StreamingExpandChainLimitOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> StreamingExpandChainLimitOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for StreamingExpandChainLimitOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            // re-extract the chain at execution time
            let LogicalPlan::Limit { input, count } = self.plan else {
                unreachable!();
            };
            let n = eval_integer_expr(count, &*graph)?;
            if n < 0 {
                return Err(CypherError::semantic(
                    "LIMIT must be a non-negative integer".to_string(),
                )
                .with_status(crate::status::GqlStatus::NegativeValue));
            }
            let lim = n as usize;
            let (base, hops) = extract_expand_chain(input).unwrap();
            let frames = exec_streaming_expand_chain(base, &hops, graph, lim, symtab)?;
            self.buffer = Some(frames);
        }
        let buf = self.buffer.as_mut().unwrap();
        if self.pos < buf.len() {
            let frame = std::mem::take(&mut buf[self.pos]);
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

/// Materializing LIMIT for write queries: side effects (CREATE, DELETE, ...) must run fully before
/// LIMIT truncates, so drain the child via `exec_via_operators`, then yield at most `count`.
pub(crate) struct MaterializingLimitOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> MaterializingLimitOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for MaterializingLimitOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::Limit { input, count } = self.plan else {
                unreachable!();
            };
            let n = eval_integer_expr(count, &*graph)?;
            if n < 0 {
                return Err(CypherError::semantic(
                    "LIMIT must be a non-negative integer".to_string(),
                )
                .with_status(crate::status::GqlStatus::NegativeValue));
            }
            let lim = n as usize;
            // materialize ALL child frames (running side effects), then truncate
            let all_frames = super::exec_via_operators(input, graph, symtab)?;
            self.buffer = Some(all_frames.into_iter().take(lim).collect());
        }
        let buf = self.buffer.as_mut().unwrap();
        if self.pos < buf.len() {
            let frame = std::mem::take(&mut buf[self.pos]);
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- SkipOp -------------------------------------------------------------

pub(crate) struct SkipOp<'p> {
    pub(crate) child: Box<dyn Operator + 'p>,
    pub(crate) count_expr: &'p Expr,
    skip: Option<usize>,
    skipped: usize,
}

impl<'p> SkipOp<'p> {
    pub(crate) fn new(child: Box<dyn Operator + 'p>, count_expr: &'p Expr) -> Self {
        Self {
            child,
            count_expr,
            skip: None,
            skipped: 0,
        }
    }
}

impl<'p> Operator for SkipOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.skip.is_none() {
            let n = eval_integer_expr(self.count_expr, &*graph)?;
            if n < 0 {
                return Err(CypherError::semantic(
                    "SKIP must be a non-negative integer".to_string(),
                )
                .with_status(crate::status::GqlStatus::NegativeValue));
            }
            self.skip = Some(n as usize);
        }
        let skip = self.skip.unwrap();
        // skip the first N frames
        while self.skipped < skip {
            if self.child.next(graph, symtab)?.is_none() {
                return Ok(None);
            }
            self.skipped += 1;
        }
        self.child.next(graph, symtab)
    }
}

// -- SortOp -------------------------------------------------------------

pub(crate) struct SortOp<'p> {
    pub(crate) child: Box<dyn Operator + 'p>,
    pub(crate) items: &'p [SortItem],
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> SortOp<'p> {
    pub(crate) fn new(child: Box<dyn Operator + 'p>, items: &'p [SortItem]) -> Self {
        Self {
            child,
            items,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for SortOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            // the whole input is held for sorting: charge it to the query memory budget as it
            // arrives
            let mut frames = crate::memory::RowBuffer::new();
            while let Some(frame) = self.child.next(graph, symtab)? {
                frames.push(frame)?;
            }
            if frames.is_empty() {
                self.buffer = Some(frames.into_vec());
            } else {
                let items = self.items;
                let n_items = items.len();
                let n_frames = frames.len();

                // sort items of the form Property(Variable(var), key) on a NodeRef slot get
                // batch-read
                let batch_specs: Vec<Option<(usize, PropertyKeyId)>> = items
                    .iter()
                    .map(|item| {
                        if let Expr::Property(base, key) = &item.expr
                            && let Expr::Variable(var) = base.as_ref()
                            && let Some(slot) = symtab.slot(var)
                            && let Some(pid) = graph.strings().get(key).map(PropertyKeyId)
                        {
                            // first frame has a NodeRef in this slot?
                            if matches!(frames[0][slot], SlotValue::NodeRef(_)) {
                                return Some((slot, pid));
                            }
                        }
                        None
                    })
                    .collect();

                // group batch specs by slot to batch-read several properties per node
                let all_batchable = batch_specs.iter().all(|s| s.is_some());

                if all_batchable && n_frames > 64 {
                    // ALL sort keys are batchable NodeRef reads; same slot for all -> i64 fast path
                    let first_slot = batch_specs[0].unwrap().0;
                    let same_slot = batch_specs.iter().all(|s| s.unwrap().0 == first_slot);

                    if same_slot {
                        // single slot: dedup node IDs to avoid repeated reads
                        let node_ids: Vec<_> = frames
                            .iter()
                            .map(|f| match f[first_slot] {
                                SlotValue::NodeRef(id) => id,
                                _ => traverse_core::graph::types::NodeId(0),
                            })
                            .collect();
                        let pids: Vec<PropertyKeyId> =
                            batch_specs.iter().map(|s| s.unwrap().1).collect();

                        // unique node list + frame -> unique index
                        let mut unique_map: rustc_hash::FxHashMap<
                            traverse_core::graph::types::NodeId,
                            u32,
                        > = rustc_hash::FxHashMap::default();
                        let mut unique_nodes: Vec<traverse_core::graph::types::NodeId> = Vec::new();
                        let mut frame_to_unique: Vec<u32> = Vec::with_capacity(n_frames);
                        for &nid in &node_ids {
                            let idx = unique_map.entry(nid).or_insert_with(|| {
                                let i = unique_nodes.len() as u32;
                                unique_nodes.push(nid);
                                i
                            });
                            frame_to_unique.push(*idx);
                        }

                        let n_unique = unique_nodes.len();
                        let mut i64_vals = vec![0i64; n_unique * n_items];
                        let mut i64_valid = vec![false; n_unique * n_items];
                        graph
                            .batch_read_node_properties_i64(
                                &unique_nodes,
                                &pids,
                                &mut i64_vals,
                                &mut i64_valid,
                            )
                            .map_err(CypherError::from)?;

                        // ALL values valid integers?
                        let all_int = i64_valid.iter().all(|&v| v);
                        if all_int {
                            // pure i64 sort, no CypherValue allocation
                            let mut indices: Vec<u32> = (0..n_frames as u32).collect();
                            indices.sort_by(|&a, &b| {
                                let ua = frame_to_unique[a as usize] as usize * n_items;
                                let ub = frame_to_unique[b as usize] as usize * n_items;
                                for (i, item) in items.iter().enumerate() {
                                    let va = i64_vals[ua + i];
                                    let vb = i64_vals[ub + i];
                                    let ord = va.cmp(&vb);
                                    let ord = if item.ascending { ord } else { ord.reverse() };
                                    if ord != std::cmp::Ordering::Equal {
                                        return ord;
                                    }
                                }
                                std::cmp::Ordering::Equal
                            });
                            let mut frames_opt: Vec<Option<Frame>> =
                                frames.into_iter().map(Some).collect();
                            let sorted: Vec<Frame> = indices
                                .iter()
                                .map(|&i| frames_opt[i as usize].take().unwrap())
                                .collect();
                            self.buffer = Some(sorted);
                        } else {
                            // not all ints: re-read through the TYPED batch. `i64_valid == false`
                            // means "not an integer", NOT "missing": floats land here and must sort
                            // by value, not as Null
                            let mut typed: Vec<
                                Option<traverse_core::graph::property::PropertyValue>,
                            > = vec![None; n_unique * n_items];
                            graph
                                .batch_read_node_properties(&unique_nodes, &pids, &mut typed)
                                .map_err(CypherError::from)?;
                            let mut all_keys: Vec<CypherValue> =
                                Vec::with_capacity(n_frames * n_items);
                            for fi in 0..n_frames {
                                let ui = frame_to_unique[fi] as usize;
                                for ki in 0..n_items {
                                    let idx = ui * n_items + ki;
                                    all_keys.push(match &typed[idx] {
                                        Some(pv) => CypherValue::from_property(pv),
                                        None => CypherValue::Null,
                                    });
                                }
                            }
                            let mut indices: Vec<u32> = (0..n_frames as u32).collect();
                            indices.sort_by(|&a, &b| {
                                let base_a = a as usize * n_items;
                                let base_b = b as usize * n_items;
                                for (i, item) in items.iter().enumerate() {
                                    let ord = compare_values(
                                        &all_keys[base_a + i],
                                        &all_keys[base_b + i],
                                    );
                                    let ord = if item.ascending { ord } else { ord.reverse() };
                                    if ord != std::cmp::Ordering::Equal {
                                        return ord;
                                    }
                                }
                                std::cmp::Ordering::Equal
                            });
                            let mut frames_opt: Vec<Option<Frame>> =
                                frames.into_iter().map(Some).collect();
                            let sorted: Vec<Frame> = indices
                                .iter()
                                .map(|&i| frames_opt[i as usize].take().unwrap())
                                .collect();
                            self.buffer = Some(sorted);
                        }
                    } else {
                        // multi-slot: group by slot, generic batch
                        use rustc_hash::FxHashMap;
                        let mut slot_groups: FxHashMap<usize, Vec<(usize, PropertyKeyId)>> =
                            FxHashMap::default();
                        for (item_idx, spec) in batch_specs.iter().enumerate() {
                            let (slot, pid) = spec.unwrap();
                            slot_groups.entry(slot).or_default().push((item_idx, pid));
                        }
                        let mut all_keys: Vec<CypherValue> =
                            vec![CypherValue::Null; n_frames * n_items];
                        for (slot, props) in &slot_groups {
                            let node_ids: Vec<_> = frames
                                .iter()
                                .map(|f| match f[*slot] {
                                    SlotValue::NodeRef(id) => id,
                                    _ => traverse_core::graph::types::NodeId(0),
                                })
                                .collect();
                            let pids: Vec<PropertyKeyId> =
                                props.iter().map(|(_, pid)| *pid).collect();
                            let mut out = vec![None; n_frames * pids.len()];
                            graph
                                .batch_read_node_properties(&node_ids, &pids, &mut out)
                                .map_err(CypherError::from)?;
                            for (pi, &(item_idx, _)) in props.iter().enumerate() {
                                for fi in 0..n_frames {
                                    let pv = out[fi * pids.len() + pi].take();
                                    all_keys[fi * n_items + item_idx] = match pv {
                                        Some(pv) => CypherValue::from_property(&pv),
                                        None => CypherValue::Null,
                                    };
                                }
                            }
                        }
                        let mut indices: Vec<u32> = (0..n_frames as u32).collect();
                        indices.sort_by(|&a, &b| {
                            let base_a = a as usize * n_items;
                            let base_b = b as usize * n_items;
                            for (i, item) in items.iter().enumerate() {
                                let ord =
                                    compare_values(&all_keys[base_a + i], &all_keys[base_b + i]);
                                let ord = if item.ascending { ord } else { ord.reverse() };
                                if ord != std::cmp::Ordering::Equal {
                                    return ord;
                                }
                            }
                            std::cmp::Ordering::Equal
                        });
                        let mut frames_opt: Vec<Option<Frame>> =
                            frames.into_iter().map(Some).collect();
                        let sorted: Vec<Frame> = indices
                            .iter()
                            .map(|&i| frames_opt[i as usize].take().unwrap())
                            .collect();
                        self.buffer = Some(sorted);
                    }
                } else {
                    // fallback: eval_expr per frame (some keys aren't batchable)
                    let mut all_keys: Vec<CypherValue> = Vec::with_capacity(n_frames * n_items);
                    for frame in &frames {
                        for item in items {
                            all_keys.push(
                                eval_expr(&item.expr, frame, &*graph, symtab)
                                    .unwrap_or(CypherValue::Null),
                            );
                        }
                    }
                    let mut indices: Vec<u32> = (0..n_frames as u32).collect();
                    indices.sort_by(|&a, &b| {
                        let base_a = a as usize * n_items;
                        let base_b = b as usize * n_items;
                        for (i, item) in items.iter().enumerate() {
                            let ord = compare_values(&all_keys[base_a + i], &all_keys[base_b + i]);
                            let ord = if item.ascending { ord } else { ord.reverse() };
                            if ord != std::cmp::Ordering::Equal {
                                return ord;
                            }
                        }
                        std::cmp::Ordering::Equal
                    });
                    let mut frames_opt: Vec<Option<Frame>> = frames.into_iter().map(Some).collect();
                    let sorted: Vec<Frame> = indices
                        .iter()
                        .map(|&i| frames_opt[i as usize].take().unwrap())
                        .collect();
                    self.buffer = Some(sorted);
                }
            }
        }
        let buf = self.buffer.as_mut().unwrap();
        if self.pos < buf.len() {
            let frame = std::mem::take(&mut buf[self.pos]);
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

// -- UnwindOp -----------------------------------------------------------
// Streaming UNWIND: pulls a frame, evaluates the list, yields one frame per
// item; a downstream LIMIT doesn't pay for unwinding everything.

pub(crate) struct UnwindOp<'p> {
    child: Box<dyn Operator + 'p>,
    expr: &'p Expr,
    alias: &'p str,
    /// Planner-proven: `expr` is a variable no later clause reads, so MOVE the list out of its slot
    /// instead of cloning, keeping a dead (maybe huge) list out of every downstream clone.
    clear_source: bool,
    alias_slot: Option<usize>,
    // current list + its source frame
    current_items: Vec<CypherValue>,
    current_frame: Option<Frame>,
    item_pos: usize,
}

impl<'p> UnwindOp<'p> {
    pub(crate) fn new(
        child: Box<dyn Operator + 'p>,
        expr: &'p Expr,
        alias: &'p str,
        clear_source: bool,
    ) -> Self {
        Self {
            child,
            expr,
            alias,
            clear_source,
            alias_slot: None,
            current_items: Vec::new(),
            current_frame: None,
            item_pos: 0,
        }
    }
}

impl<'p> Operator for UnwindOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        let alias_slot = *self
            .alias_slot
            .get_or_insert_with(|| symtab.slot(self.alias).unwrap());

        loop {
            // yield from the current list while items remain
            if self.item_pos < self.current_items.len() {
                let item =
                    std::mem::replace(&mut self.current_items[self.item_pos], CypherValue::Null);
                self.item_pos += 1;
                let mut new_frame = self.current_frame.as_ref().unwrap().clone();
                new_frame[alias_slot] = item.into();
                return Ok(Some(new_frame));
            }

            // next frame from the child
            let frame = match self.child.next(graph, symtab)? {
                Some(f) => f,
                None => return Ok(None),
            };

            let mut frame = frame;
            let val = if self.clear_source {
                // take the list out of the dead source slot: no clone, emitted frames carry a Null
                // slot
                if let Expr::Variable(v) = self.expr {
                    let slot = symtab.slot(v).unwrap();
                    std::mem::replace(&mut frame[slot], SlotValue::Null).into_cypher_value()
                } else {
                    eval_expr(self.expr, &frame, graph, symtab)?
                }
            } else {
                eval_expr(self.expr, &frame, graph, symtab)?
            };
            match val {
                CypherValue::List(items) => {
                    self.current_items = items;
                    self.current_frame = Some(frame);
                    self.item_pos = 0;
                    // loop back to yield the first item
                }
                CypherValue::Null => {
                    // UNWIND null yields nothing: next frame
                }
                // Neo4j treats a non-list as a singleton list (`UNWIND 1 AS x` yields one row); the
                // openCypher TCK is silent here
                other => {
                    self.current_items = vec![other];
                    self.current_frame = Some(frame);
                    self.item_pos = 0;
                }
            }
        }
    }
}

// -- UnionOp ------------------------------------------------------------

pub(crate) struct UnionOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<crate::memory::RowBuffer>,
    pos: usize,
}

impl<'p> UnionOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for UnionOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::Union { left, right, all } = self.plan else {
                unreachable!();
            };
            let mut left_frames = super::exec_via_operators(left, graph, symtab)?;
            let right_frames = super::exec_via_operators(right, graph, symtab)?;
            left_frames.extend(right_frames)?;
            if !all {
                left_frames = crate::memory::RowBuffer::from_vec(dedup_frames(
                    left_frames.into_vec(),
                    symtab,
                ))?;
            }
            self.buffer = Some(left_frames);
        }
        let buf = self.buffer.as_mut().unwrap();
        if self.pos < buf.len() {
            let frame = std::mem::take(&mut buf[self.pos]);
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}
