//! Bind operators: paths, path and edge endpoints, variable-length lists and QPP groups.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::exec_bind_path_endpoints;
use crate::frame::{self, Frame, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Operator;

/// Path binding over frames: fills the path slot from the node/edge slots. Shared by BindPathOp and
/// exec_via_operators_seeded.
pub(crate) fn bind_path_frames(
    frames: Vec<Frame>,
    variable: &str,
    node_vars: &[String],
    edge_vars: &[String],
    symtab: &SymbolTable,
) -> Vec<Frame> {
    let path_slot = symtab.slot(variable).unwrap();
    let node_slots: Vec<Option<usize>> = node_vars.iter().map(|v| symtab.slot(v)).collect();
    let edge_slots: Vec<Option<usize>> = edge_vars.iter().map(|v| symtab.slot(v)).collect();
    let is_single_hop = node_slots.len() == 2 && edge_slots.len() == 1;
    let mut result = Vec::with_capacity(frames.len());
    for mut frame in frames {
        if is_single_hop {
            let start_slot = node_slots[0];
            let end_slot = node_slots[1];
            let edge_slot = edge_slots[0];
            if let (Some(ss), Some(es), Some(esl)) = (start_slot, end_slot, edge_slot) {
                let start_ref = &frame[ss];
                let end_ref = &frame[es];
                let edge_ref = &frame[esl];
                if let (
                    frame::SlotValue::NodeRef(start_id),
                    frame::SlotValue::NodeRef(end_id),
                    frame::SlotValue::EdgeRef {
                        id,
                        edge_type_id,
                        source,
                        ..
                    },
                ) = (start_ref, end_ref, edge_ref)
                {
                    frame[path_slot] = frame::SlotValue::SingleHopPath {
                        start: *start_id,
                        end: *end_id,
                        edge_id: *id,
                        edge_type: *edge_type_id,
                        outgoing: *source == *start_id,
                    };
                    result.push(frame);
                    continue;
                }
            }
        }
        let mut nodes: Vec<CypherValue> = Vec::new();
        let mut edges: Vec<CypherValue> = Vec::new();
        if let Some(Some(slot)) = node_slots.first() {
            nodes.push(frame[*slot].to_cypher_value());
        }
        for (i, edge_slot) in edge_slots.iter().enumerate() {
            let val = match edge_slot {
                Some(s) => frame[*s].to_cypher_value(),
                None => CypherValue::Null,
            };
            if let CypherValue::List(items) = val {
                if items.is_empty() {
                    // zero-length path: start already added, no end
                } else {
                    let item_count = items.len();
                    let mut current_id = match nodes.last() {
                        Some(CypherValue::Node { id, .. }) | Some(CypherValue::NodeRef(id)) => *id,
                        _ => traverse_core::graph::types::NodeId(0),
                    };
                    for (j, edge_val) in items.into_iter().enumerate() {
                        let (src, tgt) = match &edge_val {
                            CypherValue::Edge { source, target, .. }
                            | CypherValue::EdgeRef { source, target, .. } => {
                                (Some(*source), Some(*target))
                            }
                            _ => (None, None),
                        };
                        edges.push(edge_val);
                        if let (Some(src), Some(tgt)) = (src, tgt) {
                            let next_id = if src == current_id { tgt } else { src };
                            current_id = next_id;
                            if j < item_count - 1 {
                                nodes.push(CypherValue::NodeRef(next_id));
                            }
                        }
                    }
                    if let Some(Some(slot)) = node_slots.get(i + 1) {
                        nodes.push(frame[*slot].to_cypher_value());
                    }
                }
            } else {
                edges.push(val);
                if let Some(Some(slot)) = node_slots.get(i + 1) {
                    nodes.push(frame[*slot].to_cypher_value());
                }
            }
        }
        frame[path_slot] = CypherValue::Path { nodes, edges }.into();
        result.push(frame);
    }
    result
}

// -- BindPathOp --------------------------------------------------------

pub(crate) struct BindPathOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> BindPathOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for BindPathOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::BindPath {
                input,
                variable,
                node_vars,
                edge_vars,
            } = self.plan
            else {
                unreachable!();
            };
            let frames = super::exec_via_operators(input, graph, symtab)?.into_vec();
            self.buffer = Some(bind_path_frames(
                frames, variable, node_vars, edge_vars, symtab,
            ));
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

// -- BindQppGroupsOp --------------------------------------------------

/// Binds the ISO section 16.11 group variables of a quantified group `((x)-[r]->(y)){m,n}`:
/// rebuilds the traversal from the anchor and the var-length edge list and binds
/// `x`/`y` to the per-hop source/target lists. Zero iterations bind empty lists; a
/// null anchor or edge list (OPTIONAL MATCH) binds nulls.
pub(crate) fn bind_qpp_group_frames(
    frames: Vec<Frame>,
    anchor_var: &str,
    rel_var: &str,
    src_var: Option<&str>,
    dst_var: Option<&str>,
    symtab: &SymbolTable,
) -> Vec<Frame> {
    use traverse_core::graph::types::NodeId;

    let anchor_slot = symtab.slot(anchor_var);
    let rel_slot = symtab.slot(rel_var);
    let src_slot = src_var.and_then(|v| symtab.slot(v));
    let dst_slot = dst_var.and_then(|v| symtab.slot(v));

    let node_id_of = |sv: &frame::SlotValue| -> Option<NodeId> {
        match sv {
            frame::SlotValue::NodeRef(id) => Some(*id),
            frame::SlotValue::Heap(cv) => match cv.as_ref() {
                CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => Some(*id),
                _ => None,
            },
            _ => None,
        }
    };

    let mut result = Vec::with_capacity(frames.len());
    for mut frame in frames {
        let (Some(asl), Some(rsl)) = (anchor_slot, rel_slot) else {
            result.push(frame);
            continue;
        };
        let anchor = node_id_of(&frame[asl]);
        // the var-length rel var holds a LIST of EdgeRefs in traversal order (a lone EdgeRef can
        // come from fused single-hop paths)
        let edges: Option<Vec<(NodeId, NodeId)>> = match &frame[rsl] {
            frame::SlotValue::Heap(cv) => match cv.as_ref() {
                CypherValue::List(items) => items
                    .iter()
                    .map(|e| match e {
                        CypherValue::EdgeRef { source, target, .. } => Some((*source, *target)),
                        CypherValue::Edge { .. } => None,
                        _ => None,
                    })
                    .collect(),
                _ => None,
            },
            frame::SlotValue::EdgeRef { source, target, .. } => Some(vec![(*source, *target)]),
            _ => None,
        };

        match (anchor, edges) {
            (Some(anchor), Some(edges)) => {
                // walk from the anchor; each hop's far end is whichever side isn't the current node
                // (handles Both)
                let mut cur = anchor;
                let mut srcs = Vec::with_capacity(edges.len());
                let mut dsts = Vec::with_capacity(edges.len());
                for (s, t) in edges {
                    let next = if s == cur { t } else { s };
                    srcs.push(CypherValue::NodeRef(cur));
                    dsts.push(CypherValue::NodeRef(next));
                    cur = next;
                }
                if let Some(sl) = src_slot {
                    frame[sl] = CypherValue::List(srcs).into();
                }
                if let Some(sl) = dst_slot {
                    frame[sl] = CypherValue::List(dsts).into();
                }
            }
            _ => {
                // null anchor / rel (e.g. OPTIONAL MATCH miss): bind nulls
                if let Some(sl) = src_slot {
                    frame[sl] = frame::SlotValue::Null;
                }
                if let Some(sl) = dst_slot {
                    frame[sl] = frame::SlotValue::Null;
                }
            }
        }
        result.push(frame);
    }
    result
}

/// Binds each `(target, members)` group to the LIST of the members' slot values (ISO section 16.11 group
/// variables from the GQL multi-step unroll). Missing / null members give NULL; no members give an
/// empty LIST.
pub(crate) fn bind_var_list_frames(
    frames: Vec<Frame>,
    groups: &[(String, Vec<String>)],
    symtab: &SymbolTable,
) -> Vec<Frame> {
    let resolved: Vec<(Option<usize>, Vec<Option<usize>>)> = groups
        .iter()
        .map(|(target, members)| {
            (
                symtab.slot(target),
                members.iter().map(|m| symtab.slot(m)).collect(),
            )
        })
        .collect();
    let mut result = Vec::with_capacity(frames.len());
    for mut frame in frames {
        for (target_slot, member_slots) in &resolved {
            let Some(target_slot) = target_slot else {
                continue;
            };
            let items: Vec<CypherValue> = member_slots
                .iter()
                .map(|m| match m {
                    Some(sl) => frame[*sl].to_cypher_value(),
                    None => CypherValue::Null,
                })
                .collect();
            frame[*target_slot] = CypherValue::List(items).into();
        }
        result.push(frame);
    }
    result
}

pub(crate) struct BindVarListsOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> BindVarListsOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for BindVarListsOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::BindVarLists { input, groups } = self.plan else {
                unreachable!();
            };
            let frames = super::exec_via_operators(input, graph, symtab)?.into_vec();
            self.buffer = Some(bind_var_list_frames(frames, groups, symtab));
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

pub(crate) struct BindQppGroupsOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> BindQppGroupsOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for BindQppGroupsOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::BindQppGroups {
                input,
                anchor_var,
                rel_var,
                src_var,
                dst_var,
            } = self.plan
            else {
                unreachable!();
            };
            let frames = super::exec_via_operators(input, graph, symtab)?.into_vec();
            self.buffer = Some(bind_qpp_group_frames(
                frames,
                anchor_var,
                rel_var,
                src_var.as_deref(),
                dst_var.as_deref(),
                symtab,
            ));
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

// -- BindPathEndpointsOp ----------------------------------------------

pub(crate) struct BindPathEndpointsOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> BindPathEndpointsOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for BindPathEndpointsOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::BindPathEndpoints {
                input,
                rel_var,
                from_var,
                to_var,
                direction,
            } = self.plan
            else {
                unreachable!();
            };
            let records =
                exec_bind_path_endpoints(input, rel_var, from_var, to_var, direction, graph, None)?;
            self.buffer = Some(
                records
                    .into_iter()
                    .map(|r| frame::record_to_frame(&r, symtab))
                    .collect(),
            );
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

// -- BindEdgeEndpointsOp ----------------------------------------------

pub(crate) struct BindEdgeEndpointsOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> BindEdgeEndpointsOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for BindEdgeEndpointsOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::BindEdgeEndpoints {
                input,
                edge_var,
                from_var,
                to_var,
                direction,
                edge_types,
            } = self.plan
            else {
                unreachable!();
            };

            let input_frames = super::exec_via_operators(input, graph, symtab)?;
            let edge_slot = symtab.slot(edge_var).ok_or_else(|| {
                CypherError::semantic(format!(
                    "BindEdgeEndpoints: unknown edge variable `{edge_var}`"
                ))
            })?;
            let from_slot = symtab.slot(from_var).ok_or_else(|| {
                CypherError::semantic(format!("BindEdgeEndpoints: unknown variable `{from_var}`"))
            })?;
            let to_slot = symtab.slot(to_var).ok_or_else(|| {
                CypherError::semantic(format!("BindEdgeEndpoints: unknown variable `{to_var}`"))
            })?;

            // type filter as interned ids; a required type missing from the interner means no edge
            // can match
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
                    self.buffer = Some(Vec::new());
                    let buf = self.buffer.as_mut().unwrap();
                    if self.pos < buf.len() {
                        let frame = std::mem::take(&mut buf[self.pos]);
                        self.pos += 1;
                        return Ok(Some(frame));
                    }
                    return Ok(None);
                }
                Some(ids)
            };

            let mut result = Vec::with_capacity(input_frames.len());
            for frame in input_frames {
                let edge_info = extract_edge_endpoints(&frame[edge_slot], graph);
                let (etid_u32, src, tgt) = match edge_info {
                    Some(v) => v,
                    None => continue,
                };
                if let Some(ref ids) = type_ids
                    && !ids.contains(&etid_u32)
                {
                    continue;
                }
                match direction {
                    crate::ast::RelDirection::Outgoing => {
                        if let Some(f) = bind_oriented(&frame, from_slot, to_slot, src, tgt) {
                            result.push(f);
                        }
                    }
                    crate::ast::RelDirection::Incoming => {
                        if let Some(f) = bind_oriented(&frame, from_slot, to_slot, tgt, src) {
                            result.push(f);
                        }
                    }
                    crate::ast::RelDirection::Both => {
                        if let Some(f) = bind_oriented(&frame, from_slot, to_slot, src, tgt) {
                            result.push(f);
                        }
                        if src != tgt
                            && let Some(f) = bind_oriented(&frame, from_slot, to_slot, tgt, src)
                        {
                            result.push(f);
                        }
                    }
                }
            }
            self.buffer = Some(result);
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

/// (interned edge_type_id, source, target) from a slot value, or None if not an edge.
/// `CypherValue::Edge` stores the type as a String: look up its id, None if never interned (no such
/// edge exists).
fn extract_edge_endpoints(
    slot: &crate::frame::SlotValue,
    graph: &dyn GraphView,
) -> Option<(
    u32,
    traverse_core::graph::types::NodeId,
    traverse_core::graph::types::NodeId,
)> {
    use crate::frame::SlotValue;
    match slot {
        SlotValue::EdgeRef {
            edge_type_id,
            source,
            target,
            ..
        } => Some((edge_type_id.0, *source, *target)),
        SlotValue::Heap(cv) => match cv.as_ref() {
            CypherValue::EdgeRef {
                edge_type_id,
                source,
                target,
                ..
            } => Some((edge_type_id.0, *source, *target)),
            CypherValue::Edge {
                edge_type,
                source,
                target,
                ..
            } => graph
                .strings()
                .get(edge_type)
                .map(|tid| (tid, *source, *target)),
            _ => None,
        },
        _ => None,
    }
}

/// Binds from/to slots to the given nodes, checking existing bindings; None if a pre-bound endpoint
/// contradicts the orientation.
fn bind_oriented(
    frame: &Frame,
    from_slot: usize,
    to_slot: usize,
    from_id: traverse_core::graph::types::NodeId,
    to_id: traverse_core::graph::types::NodeId,
) -> Option<Frame> {
    if !slot_matches_node(&frame[from_slot], from_id) {
        return None;
    }
    if !slot_matches_node(&frame[to_slot], to_id) {
        return None;
    }
    let mut new_frame = frame.clone();
    new_frame[from_slot] = crate::frame::SlotValue::NodeRef(from_id);
    new_frame[to_slot] = crate::frame::SlotValue::NodeRef(to_id);
    Some(new_frame)
}

fn slot_matches_node(
    slot: &crate::frame::SlotValue,
    node_id: traverse_core::graph::types::NodeId,
) -> bool {
    use crate::frame::SlotValue;
    match slot {
        SlotValue::Null => true,
        SlotValue::NodeRef(id) => *id == node_id,
        SlotValue::Heap(cv) => match cv.as_ref() {
            CypherValue::Null => true,
            CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => *id == node_id,
            _ => false,
        },
        _ => false,
    }
}
