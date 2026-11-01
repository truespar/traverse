//! Write operators: CreateNode, CreateRel, SetProperty, RemoveOp, Delete, Merge,
//! ForEach.
//!
//! `CREATE`, `SET`, `REMOVE` and `MERGE` **stream**: pull one row, apply the
//! effect, emit the row. Nothing is buffered, so imports (`UNWIND $rows AS r
//! MATCH (a {k: r.a}) MATCH (b {k: r.b}) CREATE (a)-[:R]->(b)`,
//! `LOAD CSV ... MERGE`) run in memory proportional to one row, whatever their size.
//!
//! Streaming is only correct where no read further down can see a write out of
//! clause order. The optimizer decides that (`optimizer::eager`), inserting an
//! [`Eager`](LogicalPlan::Eager) barrier exactly where read and write sets
//! overlap; these operators don't second-guess it.
//!
//! `DELETE` stays a barrier: relationships must go before nodes across *all*
//! rows (a node deleted in row 1 may still have a relationship row 2 deletes),
//! so it drains its input (charged to the query memory budget) and deletes in
//! one pass. `FOREACH` also drains its input before running the body over every
//! iteration.

use std::collections::{BTreeMap, VecDeque};

use rustc_hash::FxHashSet;

use traverse_core::graph::types::{Direction, EdgeId, NodeId};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::{
    check_all_node_constraints, edge_to_cypher_value, eval_expr, eval_property_map, exec_merge,
    exec_remove_item, exec_set_item, get_node_id, inc_stat, node_to_cypher_value,
};
use crate::frame::{self, Frame, SymbolTable};
use crate::memory::RowBuffer;
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Operator;

/// Periodic timeout / memory check, every 4096 rows.
#[inline]
fn tick(counter: &mut u32) -> Result<(), CypherError> {
    *counter = counter.wrapping_add(1);
    if *counter & 0xFFF == 0 {
        crate::query::check()?;
    }
    Ok(())
}

// -- CreateNodeOp ------------------------------------------------------

pub(crate) struct CreateNodeOp<'p> {
    plan: &'p LogicalPlan,
    input: Option<Box<dyn Operator + 'p>>,
    rows: u32,
}

impl<'p> CreateNodeOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            input: None,
            rows: 0,
        }
    }
}

impl<'p> Operator for CreateNodeOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        let LogicalPlan::CreateNode {
            input,
            variable,
            labels,
            dynamic_labels,
            properties,
        } = self.plan
        else {
            unreachable!();
        };
        let child = self
            .input
            .get_or_insert_with(|| super::build_operator_tree(input));
        let Some(frame) = child.next(graph, symtab)? else {
            return Ok(None);
        };
        tick(&mut self.rows)?;

        let rec = frame::frame_to_record(&frame, symtab);
        let props = if let Some(prop_expr) = properties {
            eval_property_map(prop_expr, &rec, graph)?
        } else {
            BTreeMap::new()
        };
        // dynamic labels resolved per frame against the current binding (expressions may use outer
        // variables like `line.label`)
        let mut resolved_dyn: Vec<String> = Vec::new();
        for de in dynamic_labels {
            let v = eval_expr(de, &frame, &*graph, symtab)?;
            match v {
                CypherValue::String(s) => resolved_dyn.push(s),
                CypherValue::Null => {}
                other => {
                    return Err(CypherError::type_error(format!(
                        "dynamic label expression must evaluate to STRING, got {}",
                        other.type_name()
                    )));
                }
            }
        }
        let mut label_strs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        for s in &resolved_dyn {
            label_strs.push(s.as_str());
        }
        let prop_count = props.len() as i64;
        let nid = graph
            .create_node(&label_strs, props)
            .map_err(CypherError::from)?;
        inc_stat(|s| s.nodes_created += 1);
        inc_stat(|s| s.labels_added += label_strs.len() as i64);
        inc_stat(|s| s.properties_set += prop_count);
        check_all_node_constraints(graph, nid)?;
        let mut new_frame = frame;
        if let Some(var) = variable {
            let node = graph.get_node(nid).map_err(CypherError::from)?;
            let cv = node_to_cypher_value(&node, graph);
            if let Some(slot) = symtab.slot(var) {
                new_frame[slot] = cv.into();
            }
        }
        Ok(Some(new_frame))
    }
}

// -- CreateRelOp -------------------------------------------------------

pub(crate) struct CreateRelOp<'p> {
    plan: &'p LogicalPlan,
    input: Option<Box<dyn Operator + 'p>>,
    rows: u32,
}

impl<'p> CreateRelOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            input: None,
            rows: 0,
        }
    }
}

impl<'p> Operator for CreateRelOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        let LogicalPlan::CreateRel {
            input,
            variable,
            from,
            to,
            edge_type,
            dynamic_edge_type,
            properties,
        } = self.plan
        else {
            unreachable!();
        };
        let child = self
            .input
            .get_or_insert_with(|| super::build_operator_tree(input));
        let Some(frame) = child.next(graph, symtab)? else {
            return Ok(None);
        };
        tick(&mut self.rows)?;

        let rec = frame::frame_to_record(&frame, symtab);
        let from_id = get_node_id(&rec, from)?;
        let to_id = get_node_id(&rec, to)?;
        let props = if let Some(prop_expr) = properties {
            eval_property_map(prop_expr, &rec, graph)?
        } else {
            BTreeMap::new()
        };
        // dynamic-type syntax: resolve the edge type at execute time, else the static name
        let resolved_type: String;
        let effective_type: &str = if let Some(de) = dynamic_edge_type {
            let v = eval_expr(de, &frame, &*graph, symtab)?;
            match v {
                CypherValue::String(s) => {
                    resolved_type = s;
                    resolved_type.as_str()
                }
                other => {
                    return Err(CypherError::type_error(format!(
                        "dynamic edge type must evaluate to STRING, got {}",
                        other.type_name()
                    )));
                }
            }
        } else {
            edge_type.as_str()
        };
        let prop_count = props.len() as i64;
        let eid = graph
            .create_edge(from_id, to_id, effective_type, props)
            .map_err(CypherError::from)?;
        crate::executor::check_all_edge_constraints(&*graph, eid)?;
        inc_stat(|s| s.relationships_created += 1);
        inc_stat(|s| s.properties_set += prop_count);
        let mut new_frame = frame;
        if let Some(var) = variable {
            let edge = graph.get_edge(eid).map_err(CypherError::from)?;
            let cv = edge_to_cypher_value(&edge, graph);
            if let Some(slot) = symtab.slot(var) {
                new_frame[slot] = cv.into();
            }
        }
        Ok(Some(new_frame))
    }
}

// -- SetPropertyOp -----------------------------------------------------

pub(crate) struct SetPropertyOp<'p> {
    plan: &'p LogicalPlan,
    input: Option<Box<dyn Operator + 'p>>,
    rows: u32,
}

impl<'p> SetPropertyOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            input: None,
            rows: 0,
        }
    }
}

impl<'p> Operator for SetPropertyOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        let LogicalPlan::SetProperty { input, items } = self.plan else {
            unreachable!();
        };
        let child = self
            .input
            .get_or_insert_with(|| super::build_operator_tree(input));
        let Some(frame) = child.next(graph, symtab)? else {
            return Ok(None);
        };
        tick(&mut self.rows)?;
        let mut rec = frame::frame_to_record(&frame, symtab);
        for item in items {
            exec_set_item(item, &mut rec, graph)?;
        }
        Ok(Some(frame::record_to_frame(&rec, symtab)))
    }
}

// -- RemoveOp ----------------------------------------------------------

pub(crate) struct RemoveOpOp<'p> {
    plan: &'p LogicalPlan,
    input: Option<Box<dyn Operator + 'p>>,
    rows: u32,
}

impl<'p> RemoveOpOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            input: None,
            rows: 0,
        }
    }
}

impl<'p> Operator for RemoveOpOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        let LogicalPlan::RemoveOp { input, items } = self.plan else {
            unreachable!();
        };
        let child = self
            .input
            .get_or_insert_with(|| super::build_operator_tree(input));
        let Some(frame) = child.next(graph, symtab)? else {
            return Ok(None);
        };
        tick(&mut self.rows)?;
        let mut rec = frame::frame_to_record(&frame, symtab);
        for item in items {
            exec_remove_item(item, &mut rec, graph)?;
        }
        Ok(Some(frame::record_to_frame(&rec, symtab)))
    }
}

// -- DeleteOp ----------------------------------------------------------

/// A barrier by necessity: every row's relationships are deleted before any node, and a node
/// deleted by one row must not fail a later row.
pub(crate) struct DeleteOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<RowBuffer>,
    pos: usize,
}

impl<'p> DeleteOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for DeleteOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::Delete {
                input,
                exprs,
                detach,
            } = self.plan
            else {
                unreachable!();
            };
            let frames = super::exec_via_operators(input, graph, symtab)?;
            // collect everything to delete first, then delete
            let mut node_ids = Vec::new();
            let mut edge_ids = Vec::new();
            for frame in frames.iter() {
                for expr in exprs {
                    let val = eval_expr(expr, frame, graph, symtab)?;
                    match val {
                        CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => {
                            node_ids.push(id)
                        }
                        CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                            edge_ids.push(id)
                        }
                        CypherValue::Path { nodes, edges } => {
                            for n in &nodes {
                                match n {
                                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => {
                                        node_ids.push(*id);
                                    }
                                    _ => {}
                                }
                            }
                            for e in &edges {
                                match e {
                                    CypherValue::Edge { id, .. }
                                    | CypherValue::EdgeRef { id, .. } => {
                                        edge_ids.push(*id);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        CypherValue::List(items) => {
                            for item in &items {
                                match item {
                                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => {
                                        node_ids.push(*id)
                                    }
                                    CypherValue::Edge { id, .. }
                                    | CypherValue::EdgeRef { id, .. } => edge_ids.push(*id),
                                    CypherValue::Null => {}
                                    _ => {
                                        return Err(CypherError::type_error(
                                            "DELETE requires node or relationship".to_string(),
                                        ));
                                    }
                                }
                            }
                        }
                        CypherValue::Null => {} // skip
                        _ => {
                            return Err(CypherError::type_error(
                                "DELETE requires node or relationship".to_string(),
                            ));
                        }
                    }
                }
            }
            // edges first
            let edge_set: FxHashSet<EdgeId> = edge_ids.into_iter().collect();
            for eid in &edge_set {
                if graph.delete_edge(*eid).is_ok() {
                    inc_stat(|s| s.relationships_deleted += 1);
                }
            }
            // then nodes
            let node_set: FxHashSet<NodeId> = node_ids.into_iter().collect();
            for nid in &node_set {
                if *detach {
                    // count edges the cascade will delete
                    if let Ok(out) = graph.edges_of(*nid, Direction::Both, None) {
                        // minus edges already deleted explicitly above
                        let cascade_count = out.iter().filter(|e| !edge_set.contains(e)).count();
                        inc_stat(|s| s.relationships_deleted += cascade_count as i64);
                    }
                    if graph.delete_node_cascading(*nid).is_ok() {
                        inc_stat(|s| s.nodes_deleted += 1);
                    }
                } else {
                    graph.delete_node(*nid).map_err(CypherError::from)?;
                    inc_stat(|s| s.nodes_deleted += 1);
                }
            }
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

// -- MergeOp -----------------------------------------------------------

/// Streams input rows through `exec_merge` one at a time. A row matching several relationships
/// yields several output rows, queued and emitted before the next input row is pulled:
/// row-sequential MERGE semantics (`UNWIND [1, 1] AS x MERGE (:P {id: x})` creates one node).
pub(crate) struct MergeOp<'p> {
    plan: &'p LogicalPlan,
    input: Option<Box<dyn Operator + 'p>>,
    pending: VecDeque<Frame>,
    rows: u32,
}

impl<'p> MergeOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            input: None,
            pending: VecDeque::new(),
            rows: 0,
        }
    }
}

impl<'p> Operator for MergeOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        let LogicalPlan::Merge {
            input,
            pattern,
            on_create,
            on_match,
        } = self.plan
        else {
            unreachable!();
        };
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
            let child = self
                .input
                .get_or_insert_with(|| super::build_operator_tree(input));
            let Some(frame) = child.next(graph, symtab)? else {
                return Ok(None);
            };
            tick(&mut self.rows)?;
            let record = frame::frame_to_record(&frame, symtab);
            let out = exec_merge(vec![record], pattern, on_create, on_match, graph)?;
            self.pending
                .extend(out.iter().map(|r| frame::record_to_frame(r, symtab)));
        }
    }
}

// -- ForEachOp ----------------------------------------------------------

/// `FOREACH (var IN list | updates)`: per input row, evaluate `list` and run the
/// body's updates once per element with `var` bound. Mutates the graph without
/// changing cardinality: input rows pass through, body output is discarded.
///
/// The body plan is rebuilt with its `EmptyRow` leaf swapped for a `SeededRows`
/// leaf of per-iteration seed frames, then drained, giving writes the seeded
/// input the read-only `exec_subquery`/`exec_via_operators_seeded` can't. The
/// body runs once over all iterations, clause by clause, after the input drained.
pub(crate) struct ForEachOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<RowBuffer>,
    pos: usize,
}

impl<'p> ForEachOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl Operator for ForEachOp<'_> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::ForEach {
                input,
                variable,
                list,
                body,
            } = self.plan
            else {
                unreachable!();
            };
            let input_frames = super::exec_via_operators(input, graph, symtab)?;
            let var_slot = symtab.slot(variable);

            // one seed row per (input row, list element)
            let mut seeds = RowBuffer::new();
            for frame in input_frames.iter() {
                let val = eval_expr(list, frame, &*graph, symtab)?;
                match val {
                    // FOREACH over null is a no-op (as Neo4j)
                    CypherValue::Null => {}
                    CypherValue::List(items) => {
                        for item in items {
                            let mut seed = frame.clone();
                            if let Some(s) = var_slot {
                                seed[s] = item.into();
                            }
                            seeds.push(seed)?;
                        }
                    }
                    // Neo4j treats a non-list as a singleton list, like UNWIND
                    other => {
                        let mut seed = frame.clone();
                        if let Some(s) = var_slot {
                            seed[s] = other.into();
                        }
                        seeds.push(seed)?;
                    }
                }
            }

            // run the body over all seed rows; only the mutations matter, output dropped
            if !seeds.is_empty() {
                let seeded_body = seed_foreach_body(body, seeds.into_vec());
                let mut op = super::build_operator_tree(&seeded_body);
                super::drain_operator_discard(&mut *op, graph, symtab)?;
            }

            // input rows pass through unchanged
            self.buffer = Some(input_frames);
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

/// Clones the FOREACH body with its `EmptyRow` leaf replaced by `SeededRows` of the per-iteration
/// seeds.
fn seed_foreach_body(body: &LogicalPlan, seeds: Vec<Frame>) -> LogicalPlan {
    let mut owned = body.clone();
    replace_leaf_with_seeds(&mut owned, seeds);
    owned
}

/// Walks the linear body chain to its leaf and swaps it for `SeededRows`. A nested `FOREACH`'s body
/// is left alone (its own ForEachOp seeds it); only its `input` (the outer chain) is followed.
#[recursive::recursive]
fn replace_leaf_with_seeds(plan: &mut LogicalPlan, seeds: Vec<Frame>) {
    match plan {
        LogicalPlan::EmptyRow => *plan = LogicalPlan::SeededRows(seeds),
        LogicalPlan::CreateNode { input, .. }
        | LogicalPlan::CreateRel { input, .. }
        | LogicalPlan::SetProperty { input, .. }
        | LogicalPlan::RemoveOp { input, .. }
        | LogicalPlan::Delete { input, .. }
        | LogicalPlan::Merge { input, .. }
        | LogicalPlan::ForEach { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Eager { input }
        | LogicalPlan::Unwind { input, .. } => replace_leaf_with_seeds(input, seeds),
        // any other leaf (shouldn't happen in an update-only body): seed it anyway so writes see
        // the per-iteration rows
        other => *other = LogicalPlan::SeededRows(seeds),
    }
}
