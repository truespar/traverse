//! Path operators: ShortestPath, AllShortestPaths, WeightedShortestPath.

use traverse_core::graph::types::Direction;
use traverse_core::tx::view::GraphView;

use crate::ast::RelDirection;
use crate::error::CypherError;
use crate::executor::{
    bfs_all_shortest, bfs_k_groups, bfs_k_shortest, bfs_single_shortest, dijkstra_single_shortest,
};
use crate::frame::{Frame, SlotValue, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Operator;

// -- ShortestPathOp -----------------------------------------------------

pub(crate) struct ShortestPathOp<'p> {
    plan: &'p LogicalPlan,
    /// Streaming child, pulled one frame at a time; with unbound endpoints
    /// (`shortestPath((a)-[*]-(b))`) the input is an N^2 cross product before the first BFS, so it's
    /// never materialized.
    child: Option<Box<dyn Operator + 'p>>,
    /// Result frames for the current input frame, drained FIFO.
    pending: std::collections::VecDeque<Frame>,
}

impl<'p> ShortestPathOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            child: None,
            pending: std::collections::VecDeque::new(),
        }
    }
}

impl<'p> Operator for ShortestPathOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        let LogicalPlan::ShortestPath {
            input,
            from,
            to,
            edge_types,
            direction,
            min_depth,
            max_depth,
            path_var,
            rel_var,
            all,
            k,
            k_groups,
        } = self.plan
        else {
            unreachable!();
        };
        let dir = match direction {
            RelDirection::Outgoing => Direction::Outgoing,
            RelDirection::Incoming => Direction::Incoming,
            RelDirection::Both => Direction::Both,
        };
        if self.child.is_none() {
            self.child = Some(super::build_operator_tree(input));
        }
        let from_slot = symtab.slot(from).unwrap();
        let to_slot = symtab.slot(to).unwrap();

        loop {
            if let Some(f) = self.pending.pop_front() {
                return Ok(Some(f));
            }
            let Some(frame) = self.child.as_mut().unwrap().next(graph, symtab)? else {
                return Ok(None);
            };
            let frame = &frame;
            let mut result = Vec::new();
            {
                // both endpoints unbound = N^2 sweep over node pairs; honor the query deadline
                crate::query::check()?;
                let start_id = match &frame[from_slot] {
                    SlotValue::NodeRef(id) => *id,
                    SlotValue::Heap(cv) => match cv.as_ref() {
                        CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => *id,
                        _ => continue,
                    },
                    _ => continue,
                };
                let target_id = match &frame[to_slot] {
                    SlotValue::NodeRef(id) => *id,
                    SlotValue::Heap(cv) => match cv.as_ref() {
                        CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => *id,
                        _ => continue,
                    },
                    _ => continue,
                };

                // same node at both ends with min length >= 1: no such path, skip the pair (0 rows,
                // as for bound endpoints); the streaming child + deadline keep the sweep safe (see
                // the shortest_k_safety tests)
                if k.is_none() && start_id == target_id && *min_depth >= 1 {
                    continue;
                }

                let paths = if let Some(kk) = k {
                    if *k_groups {
                        // GQL section 14.6 SHORTEST k GROUPS: every loopless path whose length is among
                        // the k smallest distinct lengths between (source, target)
                        bfs_k_groups(
                            graph, start_id, target_id, dir, edge_types, *min_depth, *max_depth,
                            *kk,
                        )?
                    } else {
                        // GQL section 14.6 SHORTEST k: top-k loopless paths by ascending length, Yen-style
                        bfs_k_shortest(
                            graph, start_id, target_id, dir, edge_types, *min_depth, *max_depth,
                            *kk,
                        )?
                    }
                } else if *all {
                    bfs_all_shortest(
                        graph, start_id, target_id, dir, edge_types, *min_depth, *max_depth,
                    )?
                } else {
                    bfs_single_shortest(
                        graph, start_id, target_id, dir, edge_types, *min_depth, *max_depth,
                    )?
                };

                for edge_path in &paths {
                    let mut new_frame = frame.clone();

                    let mut path_nodes: Vec<CypherValue> = Vec::new();
                    let mut path_edges: Vec<CypherValue> = Vec::new();

                    // start node
                    path_nodes.push(frame[from_slot].to_cypher_value());

                    let mut current_id = start_id;
                    for eid in edge_path {
                        let edge = graph.get_edge(*eid).map_err(CypherError::from)?;
                        path_edges.push(CypherValue::EdgeRef {
                            id: *eid,
                            edge_type_id: edge.edge_type,
                            source: edge.source,
                            target: edge.target,
                        });

                        let next_id = match dir {
                            Direction::Outgoing => edge.target,
                            Direction::Incoming => edge.source,
                            Direction::Both => {
                                if edge.source == current_id {
                                    edge.target
                                } else {
                                    edge.source
                                }
                            }
                        };
                        current_id = next_id;

                        path_nodes.push(CypherValue::NodeRef(next_id));
                    }

                    // relationship variable as a list
                    if let Some(rv) = rel_var
                        && let Some(slot) = symtab.slot(rv)
                    {
                        new_frame[slot] = CypherValue::List(path_edges.clone()).into();
                    }

                    // path variable
                    if let Some(pv) = path_var
                        && let Some(slot) = symtab.slot(pv)
                    {
                        new_frame[slot] = CypherValue::Path {
                            nodes: path_nodes,
                            edges: path_edges,
                        }
                        .into();
                    }

                    result.push(new_frame);
                }
            }
            self.pending.extend(result);
        }
    }
}

// -- WeightedShortestPathOp ---------------------------------------------

pub(crate) struct WeightedShortestPathOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> WeightedShortestPathOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for WeightedShortestPathOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::WeightedShortestPath {
                input,
                from,
                to,
                edge_types,
                direction,
                weight_property,
                path_var,
                rel_var,
                cost_var,
            } = self.plan
            else {
                unreachable!();
            };
            let dir = match direction {
                RelDirection::Outgoing => Direction::Outgoing,
                RelDirection::Incoming => Direction::Incoming,
                RelDirection::Both => Direction::Both,
            };
            let frames = super::exec_via_operators(input, graph, symtab)?;
            let from_slot = symtab.slot(from).unwrap();
            let to_slot = symtab.slot(to).unwrap();
            let mut result = Vec::new();

            for frame in &frames {
                // both endpoints unbound = N^2 sweep over node pairs; honor the query deadline
                crate::query::check()?;
                let start_id = match &frame[from_slot] {
                    SlotValue::NodeRef(id) => *id,
                    SlotValue::Heap(cv) => match cv.as_ref() {
                        CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => *id,
                        _ => continue,
                    },
                    _ => continue,
                };
                let target_id = match &frame[to_slot] {
                    SlotValue::NodeRef(id) => *id,
                    SlotValue::Heap(cv) => match cv.as_ref() {
                        CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => *id,
                        _ => continue,
                    },
                    _ => continue,
                };

                if let Some((edge_path, total_cost)) = dijkstra_single_shortest(
                    graph,
                    start_id,
                    target_id,
                    dir,
                    edge_types,
                    weight_property,
                )? {
                    let mut new_frame = frame.clone();

                    let mut path_nodes: Vec<CypherValue> = Vec::new();
                    let mut path_edges: Vec<CypherValue> = Vec::new();

                    // start node
                    path_nodes.push(frame[from_slot].to_cypher_value());

                    let mut current_id = start_id;
                    for eid in &edge_path {
                        let edge = graph.get_edge(*eid).map_err(CypherError::from)?;
                        path_edges.push(CypherValue::EdgeRef {
                            id: *eid,
                            edge_type_id: edge.edge_type,
                            source: edge.source,
                            target: edge.target,
                        });

                        let next_id = match dir {
                            Direction::Outgoing => edge.target,
                            Direction::Incoming => edge.source,
                            Direction::Both => {
                                if edge.source == current_id {
                                    edge.target
                                } else {
                                    edge.source
                                }
                            }
                        };
                        current_id = next_id;

                        path_nodes.push(CypherValue::NodeRef(next_id));
                    }

                    // relationship variable as a list
                    if let Some(rv) = rel_var
                        && let Some(slot) = symtab.slot(rv)
                    {
                        new_frame[slot] = CypherValue::List(path_edges.clone()).into();
                    }

                    // path variable
                    if let Some(pv) = path_var
                        && let Some(slot) = symtab.slot(pv)
                    {
                        new_frame[slot] = CypherValue::Path {
                            nodes: path_nodes,
                            edges: path_edges,
                        }
                        .into();
                    }

                    // cost variable
                    if let Some(cv) = cost_var
                        && let Some(slot) = symtab.slot(cv)
                    {
                        new_frame[slot] = SlotValue::Float(total_cost);
                    }

                    result.push(new_frame);
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
