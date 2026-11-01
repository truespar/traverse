//! Expand operators: single-hop, variable-length, distinct, chained. Blocking ones delegate to
//! `exec_expand` / `DistinctExpand` in the executor, holding the plan subtree and materializing on
//! the first `next()`.

use std::cell::RefCell;
use std::rc::Rc;

use rustc_hash::FxHashSet;

use traverse_core::graph::property::IndexKey;
use traverse_core::graph::types::{Direction, EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::ast::{BinOp, CmpOp, Expr, Literal};
use crate::bytecode::{self, CompiledExpr};
use crate::error::CypherError;
use crate::executor::{
    self, collect_prior_edge_ids, exec_expand, get_node_id_frame, get_query_param,
    resolve_edge_type_filter,
};
use crate::frame::{Frame, SlotValue, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Operator;
use super::traversal_par;

// -- ExpandOp (buffered, variable-length) ------------------------------
// Variable-length Expand (range.is_some()), delegating every sub-case to
// exec_expand; materializes on the first next().

pub(crate) struct ExpandOp<'p> {
    plan: &'p LogicalPlan,
    direction: Direction,
    limit: Option<usize>,
    buffer: Option<crate::memory::RowBuffer>,
    pos: usize,
}

impl<'p> ExpandOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan, direction: Direction) -> Self {
        Self {
            plan,
            direction,
            limit: None,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for ExpandOp<'p> {
    fn limit_hint(&mut self, limit: usize) {
        self.limit = Some(limit);
    }

    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let (input, from, rel_var, to, edge_types, range, prior_edge_vars) = match self.plan {
                LogicalPlan::ExpandOut {
                    input,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    prior_edge_vars,
                }
                | LogicalPlan::ExpandIn {
                    input,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    prior_edge_vars,
                }
                | LogicalPlan::ExpandBoth {
                    input,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    prior_edge_vars,
                } => (input, from, rel_var, to, edge_types, range, prior_edge_vars),
                _ => unreachable!(),
            };
            let frames = exec_expand(
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
                self.direction,
                graph,
                self.limit,
                symtab,
            )?;
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

// -- StreamingExpandOp (single-hop, bounded memory) --------------------
// Streaming state machine for single-hop fixed-length expands: pulls one input
// frame at a time, walks its endpoints, emits one frame per `next()`. Avoids
// materializing a full scan-expand result at once (vital under concurrency).

pub(crate) struct StreamingExpandOp<'p> {
    child: Box<dyn Operator + 'p>,
    direction: Direction,
    from_var: &'p str,
    rel_var: &'p str,
    to_var: &'p str,
    edge_types: &'p [String],
    prior_edge_vars: &'p [String],
    // resolved slots, lazily on first next()
    slots: Option<StreamingSlots>,
    // current frame state
    current_frame: Option<Frame>,
    endpoints: Vec<traverse_core::graph::types::EdgeEndpoint>,
    ep_pos: usize,
    seen_edge_ids: FxHashSet<EdgeId>,
    used_edges: FxHashSet<EdgeId>,
    count: usize,
    limit: Option<usize>,
}

struct StreamingSlots {
    from_slot: usize,
    rel_slot: usize,
    to_slot: usize,
    prior_edge_slots: Vec<usize>,
}

impl<'p> StreamingExpandOp<'p> {
    pub(crate) fn new(
        child: Box<dyn Operator + 'p>,
        direction: Direction,
        from_var: &'p str,
        rel_var: &'p str,
        to_var: &'p str,
        edge_types: &'p [String],
        prior_edge_vars: &'p [String],
    ) -> Self {
        Self {
            child,
            direction,
            from_var,
            rel_var,
            to_var,
            edge_types,
            prior_edge_vars,
            slots: None,
            current_frame: None,
            endpoints: Vec::new(),
            ep_pos: 0,
            seen_edge_ids: FxHashSet::default(),
            used_edges: FxHashSet::default(),
            count: 0,
            limit: None,
        }
    }
}

impl<'p> Operator for StreamingExpandOp<'p> {
    fn limit_hint(&mut self, limit: usize) {
        self.limit = Some(limit);
    }

    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        // lazy slot resolution
        if self.slots.is_none() {
            self.slots = Some(StreamingSlots {
                from_slot: symtab.slot(self.from_var).unwrap(),
                rel_slot: symtab.slot(self.rel_var).unwrap(),
                to_slot: symtab.slot(self.to_var).unwrap(),
                prior_edge_slots: self
                    .prior_edge_vars
                    .iter()
                    .filter_map(|v| symtab.slot(v))
                    .collect(),
            });
        }
        let slots = self.slots.as_ref().unwrap();
        let from_slot = slots.from_slot;
        let rel_slot = slots.rel_slot;
        let to_slot = slots.to_slot;

        loop {
            if let Some(lim) = self.limit
                && self.count >= lim
            {
                return Ok(None);
            }

            // yield from the current endpoints
            while self.ep_pos < self.endpoints.len() {
                let ep = self.endpoints[self.ep_pos];
                self.ep_pos += 1;

                // dedup edges (Both direction, self-loops, multi-type)
                if !self.seen_edge_ids.insert(ep.edge_id) {
                    continue;
                }
                // relationship isomorphism
                if self.used_edges.contains(&ep.edge_id) {
                    continue;
                }

                let frame = self.current_frame.as_ref().unwrap();
                let mut new_frame = frame.clone();
                new_frame[rel_slot] = SlotValue::EdgeRef {
                    id: ep.edge_id,
                    edge_type_id: ep.edge_type,
                    source: ep.source,
                    target: ep.target,
                };
                new_frame[to_slot] = SlotValue::NodeRef(ep.neighbor);
                self.count += 1;
                return Ok(Some(new_frame));
            }

            // next input frame from the child
            let frame = match self.child.next(graph, symtab)? {
                Some(f) => f,
                None => return Ok(None),
            };

            // timeout check when the output count hits a multiple of 4096
            if self.count & 0xFFF == 0 {
                crate::query::check_interrupt()?;
            }

            // skip null sources (OPTIONAL MATCH)
            let from_id = match get_node_id_frame(&frame, from_slot) {
                Ok(id) => id,
                Err(_) if matches!(frame[from_slot], SlotValue::Null) => continue,
                Err(e) => return Err(e),
            };

            // target already bound? (bound-target optimization)
            let bound_target = match &frame[to_slot] {
                SlotValue::NodeRef(id) => Some(*id),
                SlotValue::Heap(cv) => match cv.as_ref() {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => Some(*id),
                    _ => None,
                },
                _ => None,
            };

            let prior_edge_slots = &slots.prior_edge_slots;
            self.used_edges = collect_prior_edge_ids(&frame, prior_edge_slots);
            self.seen_edge_ids.clear();

            if let Some(target_id) = bound_target {
                // both endpoints known: edges_between, O(min degree)
                let type_filter = resolve_edge_type_filter(&*graph, self.edge_types);
                let edge_ids = graph
                    .edges_between(from_id, target_id, self.direction, &type_filter)
                    .map_err(CypherError::from)?;
                self.endpoints.clear();
                self.ep_pos = 0;
                for eid in edge_ids {
                    if self.used_edges.contains(&eid) {
                        continue;
                    }
                    let edge = graph.get_edge(eid).map_err(CypherError::from)?;
                    self.endpoints
                        .push(traverse_core::graph::types::EdgeEndpoint {
                            edge_id: eid,
                            edge_type: edge.edge_type,
                            neighbor: target_id,
                            source: edge.source,
                            target: edge.target,
                        });
                }
                self.current_frame = Some(frame);
                continue;
            }

            // normal single hop
            self.endpoints.clear();
            self.ep_pos = 0;
            if self.edge_types.is_empty() {
                self.endpoints = graph
                    .edges_with_neighbor(from_id, self.direction, None)
                    .map_err(CypherError::from)?;
            } else {
                for t in self.edge_types.iter() {
                    let eps = graph
                        .edges_with_neighbor(from_id, self.direction, Some(t.as_str()))
                        .map_err(CypherError::from)?;
                    self.endpoints.extend(eps);
                }
            }
            self.current_frame = Some(frame);
        }
    }
}

// -- DistinctExpandOp ---------------------------------------------------

pub(crate) struct DistinctExpandOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<crate::memory::RowBuffer>,
    pos: usize,
}

impl<'p> DistinctExpandOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for DistinctExpandOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::DistinctExpand {
                input,
                from,
                to,
                min_hops,
                max_hops,
                direction,
                edge_types,
                target_label,
                global_dedup,
                target_filter,
            } = self.plan
            else {
                unreachable!();
            };
            let frames = super::exec_via_operators(input, graph, symtab)?;
            let type_filter = resolve_edge_type_filter(&*graph, edge_types);
            let label_id = target_label.as_ref().and_then(|l| {
                graph
                    .strings()
                    .get(l)
                    .map(traverse_core::graph::types::LabelId)
            });
            let from_slot = symtab.slot(from).unwrap();
            let to_slot = symtab.slot(to).unwrap();
            let mut result = Vec::new();
            let mut global_seen: FxHashSet<NodeId> = FxHashSet::default();

            let tf_ref = target_filter.as_ref().map(|e| (e, &*symtab));

            if let Some(sync_graph) = graph.as_sync_ref() {
                for frame in &frames {
                    let start_id = get_node_id_frame(frame, from_slot)?;
                    traversal_par::distinct_expand_par(
                        sync_graph,
                        start_id,
                        *min_hops,
                        *max_hops,
                        *direction,
                        &type_filter,
                        label_id,
                        tf_ref,
                        if *global_dedup {
                            Some(&mut global_seen)
                        } else {
                            None
                        },
                        frame,
                        to_slot,
                        &mut result,
                    )?;
                }
            } else {
                // reusable scratch frame for filter evaluation
                let mut scratch = if target_filter.is_some() {
                    Some(crate::frame::new_frame(symtab))
                } else {
                    None
                };
                for frame in &frames {
                    let start_id = get_node_id_frame(frame, from_slot)?;
                    let mut frontier: FxHashSet<NodeId> = FxHashSet::default();
                    frontier.insert(start_id);
                    let mut collected: FxHashSet<NodeId> = FxHashSet::default();

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
                        if let Some(lid) = label_id
                            && !graph.has_label(nid, lid).unwrap_or(false)
                        {
                            continue;
                        }
                        // target_filter before allocating a frame
                        if let Some(filter_expr) = target_filter {
                            let s = scratch.as_mut().unwrap();
                            s[to_slot] = SlotValue::NodeRef(nid);
                            let val = executor::eval_expr(filter_expr, s, &*graph, symtab)?;
                            if !executor::is_truthy(&val) {
                                continue;
                            }
                        }
                        let mut new_frame = frame.clone();
                        new_frame[to_slot] = SlotValue::NodeRef(nid);
                        result.push(new_frame);
                    }
                }
            }
            self.buffer = Some(crate::memory::RowBuffer::from_vec(result)?);
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

// -- ExpandFilterOp -----------------------------------------------------
// Fused Expand + Filter: the predicate runs during single-hop expansion, so
// non-matching neighbors get no frame. Variable-length paths fall back to
// expand then filter (frames are more complex there).

pub(crate) struct ExpandFilterOp<'p> {
    expand_plan: &'p LogicalPlan,
    predicate: &'p Expr,
    direction: Direction,
    buffer: Option<crate::memory::RowBuffer>,
    pos: usize,
    compiled: Option<Option<CompiledExpr>>,
}

impl<'p> ExpandFilterOp<'p> {
    pub(crate) fn new(
        expand_plan: &'p LogicalPlan,
        predicate: &'p Expr,
        direction: Direction,
    ) -> Self {
        Self {
            expand_plan,
            predicate,
            direction,
            buffer: None,
            pos: 0,
            compiled: None,
        }
    }
}

impl<'p> Operator for ExpandFilterOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            // compile the filter lazily
            if self.compiled.is_none() {
                self.compiled = Some(bytecode::try_compile(self.predicate, symtab));
            }

            let (input, from, rel_var, to, edge_types, range, prior_edge_vars) =
                match self.expand_plan {
                    LogicalPlan::ExpandOut {
                        input,
                        from,
                        rel_var,
                        to,
                        edge_types,
                        range,
                        prior_edge_vars,
                    }
                    | LogicalPlan::ExpandIn {
                        input,
                        from,
                        rel_var,
                        to,
                        edge_types,
                        range,
                        prior_edge_vars,
                    }
                    | LogicalPlan::ExpandBoth {
                        input,
                        from,
                        rel_var,
                        to,
                        edge_types,
                        range,
                        prior_edge_vars,
                    } => (input, from, rel_var, to, edge_types, range, prior_edge_vars),
                    _ => unreachable!(),
                };

            // variable length: expand, then filter
            if range.is_some() {
                let frames = exec_expand(
                    input,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    range,
                    prior_edge_vars,
                    self.direction,
                    graph,
                    None,
                    symtab,
                )?;
                let compiled = self.compiled.as_ref().unwrap();
                let mut result = crate::memory::RowBuffer::new();
                for frame in frames {
                    let pass = if let Some(bc) = compiled {
                        bytecode::eval_bytecode(bc, &frame, &*graph)?
                    } else {
                        executor::is_truthy(&executor::eval_expr(
                            self.predicate,
                            &frame,
                            &*graph,
                            symtab,
                        )?)
                    };
                    if pass {
                        result.push(frame)?;
                    }
                }
                self.buffer = Some(result);
            } else {
                // single hop: filter before cloning
                let result = exec_expand_with_filter(
                    input,
                    from,
                    rel_var,
                    to,
                    edge_types,
                    prior_edge_vars,
                    self.direction,
                    self.predicate,
                    self.compiled.as_ref().unwrap().as_ref(),
                    graph,
                    symtab,
                )?;
                self.buffer = Some(result);
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

/// Collect AND-conjuncts from an expression tree into a flat list.
#[recursive::recursive]
fn flatten_and_conjuncts<'a>(expr: &'a Expr) -> Vec<&'a Expr> {
    match expr {
        Expr::BinaryOp(left, BinOp::And, right) => {
            let mut out = flatten_and_conjuncts(left);
            out.extend(flatten_and_conjuncts(right));
            out
        }
        _ => vec![expr],
    }
}

/// Inputs of an edge-value bitvec, cached per thread for views with a
/// [`data_version`](GraphView::data_version): the batches of one query build it once.
#[derive(PartialEq, Eq)]
struct BitvecKey {
    version: (usize, u64),
    edge_types: Vec<String>,
    prop_key: String,
    /// Sorted.
    values: Vec<String>,
}

/// Bitvecs kept per thread (a query can carry an IN and a NOT IN filter at once).
const BITVEC_CACHE_SLOTS: usize = 4;

thread_local! {
    static BITVEC_CACHE: RefCell<Vec<(BitvecKey, Rc<Vec<u64>>)>> = const { RefCell::new(Vec::new()) };
}

/// Edge-id bitvec of the edges of `edge_types` whose string `prop_key` is one of `values`, from
/// the edge property indexes (as of the view). `None` when an untyped expand or a missing index
/// rules the index out; callers then read the property per edge.
fn build_value_bitvec(
    edge_types: &[String],
    prop_key: &str,
    values: &FxHashSet<String>,
    graph: &dyn GraphView,
) -> Option<Rc<Vec<u64>>> {
    if edge_types.is_empty() {
        return None;
    }
    let key = graph.data_version().map(|version| {
        let mut sorted: Vec<String> = values.iter().cloned().collect();
        sorted.sort_unstable();
        BitvecKey {
            version,
            edge_types: edge_types.to_vec(),
            prop_key: prop_key.to_string(),
            values: sorted,
        }
    });
    if let Some(key) = &key {
        let hit = BITVEC_CACHE.with(|c| {
            c.borrow()
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, bv)| Rc::clone(bv))
        });
        if hit.is_some() {
            return hit;
        }
    }

    let strings = graph.strings();
    let pkid = PropertyKeyId(strings.get(prop_key)?);
    let mut ids: Vec<EdgeId> = Vec::new();
    for edge_type in edge_types {
        let etid = EdgeTypeId(strings.get(edge_type)?);
        if !graph.edge_index_catalog().has_prop_index(etid, pkid) {
            return None;
        }
        for value in values {
            ids.extend(graph.edge_index_lookup(
                edge_type,
                prop_key,
                &IndexKey::String(value.clone()),
            )?);
        }
    }
    // sized by the highest id found: ids can exceed the edge count after deletions
    let words = ids.iter().map(|e| e.0 as usize / 64 + 1).max().unwrap_or(0);
    let mut bitvec = vec![0u64; words];
    for e in ids {
        let i = e.0 as usize;
        bitvec[i / 64] |= 1u64 << (i % 64);
    }
    let rc = Rc::new(bitvec);
    if let Some(key) = key {
        BITVEC_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() == BITVEC_CACHE_SLOTS {
                c.remove(0);
            }
            c.push((key, Rc::clone(&rc)));
        });
    }
    Some(rc)
}

/// Bitvec of the edges a `NOT rel.prop IN [str_values]` filter excludes (see
/// [`build_value_bitvec`]).
pub(crate) fn build_not_in_bitvec(
    edge_types: &[String],
    prop_key: &str,
    excluded_values: &FxHashSet<String>,
    graph: &dyn GraphView,
) -> Option<Rc<Vec<u64>>> {
    build_value_bitvec(edge_types, prop_key, excluded_values, graph)
}

/// Bitvec of the edges a `rel.prop IN [str_values]` filter keeps (see [`build_value_bitvec`]).
pub(crate) fn build_in_bitvec(
    edge_types: &[String],
    prop_key: &str,
    included_values: &FxHashSet<String>,
    graph: &dyn GraphView,
) -> Option<Rc<Vec<u64>>> {
    build_value_bitvec(edge_types, prop_key, included_values, graph)
}

#[inline(always)]
pub(crate) fn bitvec_test(bitvec: &[u64], edge_id: EdgeId) -> bool {
    let idx = edge_id.0 as usize;
    let word = idx / 64;
    word < bitvec.len() && (bitvec[word] & (1u64 << (idx % 64))) != 0
}

/// Handles the pattern even when nested inside AND expressions (e.g.,
/// `has_label(c, "Foo") AND NOT r.prop IN [...]`). Also resolves query
/// parameters in the string list.
///
/// Returns `(exclusion_set, remaining_predicates)` if an index exists for every
/// edge type, or `None` to fall back to the normal filter path.
pub(crate) fn build_edge_exclusion_set(
    rel_var: &str,
    edge_types: &[String],
    preds: &[&Expr],
    graph: &dyn GraphView,
) -> Option<(FxHashSet<EdgeId>, Vec<Expr>)> {
    // Can't use edge indexes if no specific edge types are given.
    if edge_types.is_empty() {
        return None;
    }

    for (i, pred) in preds.iter().enumerate() {
        // Decompose each predicate into AND-conjuncts and search for NOT IN.
        let conjuncts = flatten_and_conjuncts(pred);
        for (ci, conjunct) in conjuncts.iter().enumerate() {
            if let Some((var, prop_key, string_values)) =
                bytecode::extract_not_in_str_list(conjunct)
                && var == rel_var
            {
                // Build exclusion set: for each edge type x each string value.
                let mut exclusion_set = FxHashSet::default();
                for edge_type in edge_types {
                    for s in &string_values {
                        let ids = graph.edge_index_lookup(
                            edge_type,
                            prop_key,
                            &IndexKey::String(s.clone()),
                        );
                        {
                            let id_vec = ids?;
                            exclusion_set.extend(id_vec)
                        }
                    }
                }

                // Build remaining predicates with NOT IN conjunct removed.
                let remaining: Vec<Expr> = if conjuncts.len() == 1 {
                    // NOT IN was the entire predicate - remove it entirely.
                    preds
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .map(|(_, p)| (*p).clone())
                        .collect()
                } else {
                    // NOT IN inside AND - rebuild pred from other conjuncts.
                    let other: Vec<&Expr> = conjuncts
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != ci)
                        .map(|(_, c)| *c)
                        .collect();
                    let mut rebuilt = other[0].clone();
                    for c in &other[1..] {
                        rebuilt =
                            Expr::BinaryOp(Box::new(rebuilt), BinOp::And, Box::new((*c).clone()));
                    }
                    // Replace the i-th pred with rebuilt, keep all others.
                    preds
                        .iter()
                        .enumerate()
                        .map(|(j, p)| {
                            if j == i {
                                rebuilt.clone()
                            } else {
                                (*p).clone()
                            }
                        })
                        .collect()
                };

                return Some((exclusion_set, remaining));
            }
        }
    }

    None
}

/// Single-hop expand with inline filter: for each input frame x each neighbor,
/// evaluate the filter before cloning the frame. Saves ~90% of frame allocations
/// for selective predicates like `WHERE n.age >= 18`.
#[inline(never)]
fn exec_expand_with_filter(
    input: &LogicalPlan,
    from: &str,
    rel_var: &str,
    to: &str,
    edge_types: &[String],
    prior_edge_vars: &[String],
    direction: Direction,
    predicate: &Expr,
    compiled: Option<&CompiledExpr>,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<crate::memory::RowBuffer, CypherError> {
    let frames = super::exec_via_operators(input, graph, symtab)?;
    let from_slot = symtab.slot(from).unwrap();
    let rel_slot = symtab.slot(rel_var).unwrap();
    let to_slot = symtab.slot(to).unwrap();
    let prior_edge_slots: Vec<usize> = prior_edge_vars
        .iter()
        .filter_map(|v| symtab.slot(v))
        .collect();
    let mut result = crate::memory::RowBuffer::new();

    // Extract has_label(to_var), NOT IN rel.prop, and to_var.prop <> literal
    // from the predicate for inline evaluation (avoids bytecode per edge).
    let conjuncts = flatten_and_conjuncts(predicate);
    let mut remaining_conjuncts: Vec<&Expr> = Vec::new();
    let mut to_label_names: Vec<String> = Vec::new();
    let mut not_in_rel: Option<(String, FxHashSet<String>)> = None;
    let mut in_rel: Option<(String, FxHashSet<String>)> = None;
    let mut excluded_nodes: FxHashSet<NodeId> = FxHashSet::default();

    for conj in &conjuncts {
        let mut consumed = false;
        // Extract has_label(to_var, "Label").
        if let Expr::FunctionCall { name, args, .. } = conj
            && name == "__has_label"
            && args.len() == 2
            && let (Expr::Variable(var), Expr::Literal(Literal::String(label))) =
                (&args[0], &args[1])
            && var == to
        {
            to_label_names.push(label.clone());
            consumed = true;
        }
        // Extract NOT rel.prop IN [values].
        if !consumed
            && not_in_rel.is_none()
            && let Some((var, prop_key, values)) = bytecode::extract_not_in_str_list(conj)
            && var == rel_var
        {
            not_in_rel = Some((prop_key.to_string(), values.into_iter().collect()));
            consumed = true;
        }
        // Extract positive rel.prop IN [values] or OR-chain equivalent.
        if !consumed
            && in_rel.is_none()
            && let Some((var, prop_key, values)) = bytecode::extract_in_str_list(conj)
                .or_else(|| bytecode::extract_or_eq_as_in_str_list(conj))
            && var == rel_var
        {
            in_rel = Some((prop_key.to_string(), values.into_iter().collect()));
            consumed = true;
        }
        // Extract to_var.prop <> literal (node exclusion via index).
        if !consumed
            && let Expr::Comparison(lhs, cmps) = conj
            && cmps.len() == 1
            && cmps[0].0 == CmpOp::Neq
            && let Expr::Property(base, prop_key) = lhs.as_ref()
            && let Expr::Variable(var) = base.as_ref()
            && var == to
        {
            let idx_key = match &cmps[0].1 {
                Expr::Literal(Literal::Integer(v)) => Some(IndexKey::Int(*v)),
                Expr::Literal(Literal::String(v)) => Some(IndexKey::String(v.clone())),
                Expr::Parameter(name) => match get_query_param(name) {
                    Some(CypherValue::Integer(v)) => Some(IndexKey::Int(v)),
                    Some(CypherValue::String(v)) => Some(IndexKey::String(v)),
                    _ => None,
                },
                _ => None,
            };
            if let Some(key) = idx_key {
                let mut resolved = false;
                for label in &to_label_names {
                    if let Some(ids) = graph.index_lookup(label, prop_key, &key) {
                        excluded_nodes.extend(ids);
                        resolved = true;
                    }
                }
                if resolved {
                    consumed = true;
                }
            }
        }
        if !consumed {
            remaining_conjuncts.push(conj);
        }
    }

    // Resolve has_label to LabelIds.
    let to_label_ids: Vec<LabelId> = to_label_names
        .iter()
        .filter_map(|l| graph.strings().get(l).map(LabelId))
        .collect();

    // Rebuild the remaining predicate from unconsumed conjuncts.
    let remaining_pred: Option<Expr> = if remaining_conjuncts.is_empty() {
        None
    } else {
        let mut combined = remaining_conjuncts[0].clone();
        for c in &remaining_conjuncts[1..] {
            combined = Expr::BinaryOp(Box::new(combined), BinOp::And, Box::new((*c).clone()));
        }
        Some(combined)
    };
    let remaining_compiled = remaining_pred
        .as_ref()
        .and_then(|p| bytecode::try_compile(p, symtab));

    // Try to build a bitvec from edge indexes for the extracted NOT IN filter.
    // This replaces per-edge property reads with O(1) bit checks.
    let not_in_bitvec: Option<Rc<Vec<u64>>> = not_in_rel
        .as_ref()
        .and_then(|(pk, vals)| build_not_in_bitvec(edge_types, pk, vals, &*graph));

    // Try to build a bitvec from edge indexes for the extracted positive IN filter.
    let in_bitvec: Option<Rc<Vec<u64>>> = in_rel
        .as_ref()
        .and_then(|(pk, vals)| build_in_bitvec(edge_types, pk, vals, &*graph));

    // Try to build edge exclusion set from index (NOT rel.prop IN [str_list]).
    // Only build when enough input frames to amortize the index scan cost.
    // Note: only applies to remaining conjuncts (inline NOT IN already extracted above).
    let exclusion_opt = if frames.len() >= 50 {
        build_edge_exclusion_set(
            rel_var,
            edge_types,
            remaining_conjuncts.to_vec().to_vec().as_slice(),
            &*graph,
        )
    } else {
        None
    };
    // Determine effective predicate/compiled for filter evaluation.
    let excl_remaining_compiled = exclusion_opt
        .as_ref()
        .and_then(|(_, rem)| rem.first().and_then(|p| bytecode::try_compile(p, symtab)));
    let (eff_pred, eff_compiled): (Option<&Expr>, Option<&CompiledExpr>) = match &exclusion_opt {
        Some((_, remaining)) if remaining.is_empty() => (None, None),
        Some((_, remaining)) => (Some(&remaining[0]), excl_remaining_compiled.as_ref()),
        None => (remaining_pred.as_ref(), remaining_compiled.as_ref()),
    };
    let skip_filter = eff_pred.is_none();

    // Reusable "probe" frame - we mutate slots in-place, evaluate the
    // predicate, and only clone into `result` when it passes.
    for frame in &frames {
        let from_id = get_node_id_frame(frame, from_slot)?;

        // Check if target is already bound (bound-target path).
        let bound_target = match &frame[to_slot] {
            SlotValue::NodeRef(id) => Some(*id),
            SlotValue::Heap(cv) => match cv.as_ref() {
                CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => Some(*id),
                _ => None,
            },
            _ => None,
        };

        if let Some(target) = bound_target {
            // Bound-target: typically 0-2 matches, overhead of filter is negligible.
            // Fall back to expand then filter.
            let used_edges = collect_prior_edge_ids(frame, &prior_edge_slots);
            exec_expand_bound_target_filtered(
                from_id,
                target,
                direction,
                edge_types,
                &used_edges,
                rel_slot,
                to_slot,
                frame,
                predicate,
                compiled,
                graph,
                symtab,
                &mut result,
            )?;
        } else {
            // Unbound target - this is the hot path where filter fusion matters.
            let used_edges = collect_prior_edge_ids(frame, &prior_edge_slots);
            let type_filters: Vec<Option<&str>> = if edge_types.is_empty() {
                vec![None]
            } else {
                edge_types.iter().map(|t| Some(t.as_str())).collect()
            };
            let mut seen_edge_ids = FxHashSet::default();

            // Use a probe frame that we mutate for predicate evaluation.
            let mut probe = frame.clone();

            for type_filter in &type_filters {
                let endpoints = graph
                    .edges_with_neighbor(from_id, direction, *type_filter)
                    .map_err(CypherError::from)?;
                for ep in endpoints {
                    if !seen_edge_ids.insert(ep.edge_id) {
                        continue;
                    }
                    if used_edges.contains(&ep.edge_id) {
                        continue;
                    }
                    // Edge exclusion set: skip edges matching NOT IN values.
                    if let Some((ref exclusion_set, _)) = exclusion_opt
                        && exclusion_set.contains(&ep.edge_id)
                    {
                        continue;
                    }
                    // Inline has_label check (extracted from filter).
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
                    // Inline excluded-node check (from to_var.prop <> literal).
                    if !excluded_nodes.is_empty() && excluded_nodes.contains(&ep.neighbor) {
                        continue;
                    }
                    // Inline NOT IN rel property check: bitvec (index-backed) or per-edge property
                    // read.
                    if let Some(ref bv) = not_in_bitvec {
                        if bitvec_test(bv, ep.edge_id) {
                            continue;
                        }
                    } else if let Some((ref prop_key, ref excluded_vals)) = not_in_rel {
                        let pv = graph
                            .get_edge_property(ep.edge_id, prop_key)
                            .map_err(CypherError::from)?;
                        if let Some(traverse_core::graph::property::PropertyValue::String(ref s)) =
                            pv
                            && excluded_vals.contains(s.as_str())
                        {
                            continue;
                        }
                    }
                    // Inline positive IN rel property check: bitvec (index-backed) or per-edge
                    // property read.
                    if let Some(ref bv) = in_bitvec {
                        if !bitvec_test(bv, ep.edge_id) {
                            continue;
                        }
                    } else if let Some((ref prop_key, ref included_vals)) = in_rel {
                        let pv = graph
                            .get_edge_property(ep.edge_id, prop_key)
                            .map_err(CypherError::from)?;
                        let included = if let Some(
                            traverse_core::graph::property::PropertyValue::String(ref s),
                        ) = pv
                        {
                            included_vals.contains(s.as_str())
                        } else {
                            false
                        };
                        if !included {
                            continue;
                        }
                    }
                    // Set the target and rel slots in the probe frame.
                    probe[rel_slot] = SlotValue::EdgeRef {
                        id: ep.edge_id,
                        edge_type_id: ep.edge_type,
                        source: ep.source,
                        target: ep.target,
                    };
                    probe[to_slot] = SlotValue::NodeRef(ep.neighbor);

                    // Evaluate the remaining filter predicate on the probe
                    // frame (skip if fully consumed by exclusion set).
                    let pass = if skip_filter {
                        true
                    } else if let Some(bc) = eff_compiled {
                        bytecode::eval_bytecode(bc, &probe, &*graph)?
                    } else if let Some(rp) = eff_pred {
                        executor::is_truthy(&executor::eval_expr(rp, &probe, &*graph, symtab)?)
                    } else {
                        true
                    };

                    if pass {
                        result.push(probe.clone())?;
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Bound-target expand with inline filter.
#[inline(never)]
fn exec_expand_bound_target_filtered(
    from_id: NodeId,
    target_id: NodeId,
    direction: Direction,
    edge_types: &[String],
    used_edges: &FxHashSet<EdgeId>,
    rel_slot: usize,
    to_slot: usize,
    frame: &Frame,
    predicate: &Expr,
    compiled: Option<&CompiledExpr>,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
    result: &mut crate::memory::RowBuffer,
) -> Result<(), CypherError> {
    // Same logic as exec_expand_bound_target but with inline filter.
    let (expand_from, expand_dir, check_neighbor) = if direction != Direction::Both {
        let from_deg = graph.degree(from_id, direction).unwrap_or(0);
        let rev_dir = if direction == Direction::Outgoing {
            Direction::Incoming
        } else {
            Direction::Outgoing
        };
        let to_deg = graph.degree(target_id, rev_dir).unwrap_or(0);
        if to_deg < from_deg {
            (target_id, rev_dir, from_id)
        } else {
            (from_id, direction, target_id)
        }
    } else if from_id == target_id {
        (from_id, Direction::Outgoing, target_id)
    } else {
        (from_id, direction, target_id)
    };

    let type_filter = resolve_edge_type_filter(graph, edge_types);
    let edge_ids = graph
        .edges_between(expand_from, check_neighbor, expand_dir, &type_filter)
        .map_err(CypherError::from)?;
    for eid in edge_ids {
        if used_edges.contains(&eid) {
            continue;
        }
        let edge = graph.get_edge(eid).map_err(CypherError::from)?;
        let mut new_frame = frame.clone();
        new_frame[rel_slot] = SlotValue::EdgeRef {
            id: eid,
            edge_type_id: edge.edge_type,
            source: edge.source,
            target: edge.target,
        };
        new_frame[to_slot] = SlotValue::NodeRef(target_id);

        let pass = if let Some(bc) = compiled {
            bytecode::eval_bytecode(bc, &new_frame, &*graph)?
        } else {
            executor::is_truthy(&executor::eval_expr(
                predicate, &new_frame, &*graph, symtab,
            )?)
        };
        if pass {
            result.push(new_frame)?;
        }
    }
    Ok(())
}
