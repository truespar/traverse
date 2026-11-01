//! Relationship expansion on frames: single hops, streaming chains and variable-length paths.

use rustc_hash::FxHashSet;
use traverse_core::graph::types::{Direction, EdgeEndpoint, EdgeId, EdgeTypeId, NodeId};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::frame::{Frame, SlotValue, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::shortest::reverse_direction;

// -- Streaming expand chain ---------------------------------------------

/// One hop of a chain of fixed-length expands.
pub(crate) struct ExpandHop<'a> {
    pub(crate) from: &'a str,
    pub(crate) rel_var: &'a str,
    pub(crate) to: &'a str,
    pub(crate) edge_types: &'a [String],
    pub(crate) direction: Direction,
    pub(crate) prior_edge_vars: &'a [String],
}

/// Walks down from `plan` collecting consecutive fixed-length (range=None) expands.
/// `(base_plan, hops_in_execution_order)` if there are 2+ hops.
pub(crate) fn extract_expand_chain(
    plan: &LogicalPlan,
) -> Option<(&LogicalPlan, Vec<ExpandHop<'_>>)> {
    let mut hops = Vec::new();
    let mut current = plan;
    loop {
        match current {
            LogicalPlan::ExpandOut {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } if range.is_none() => {
                hops.push(ExpandHop {
                    from,
                    rel_var,
                    to,
                    edge_types,
                    direction: Direction::Outgoing,
                    prior_edge_vars,
                });
                current = input;
            }
            LogicalPlan::ExpandIn {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } if range.is_none() => {
                hops.push(ExpandHop {
                    from,
                    rel_var,
                    to,
                    edge_types,
                    direction: Direction::Incoming,
                    prior_edge_vars,
                });
                current = input;
            }
            LogicalPlan::ExpandBoth {
                input,
                from,
                rel_var,
                to,
                edge_types,
                range,
                prior_edge_vars,
            } if range.is_none() => {
                hops.push(ExpandHop {
                    from,
                    rel_var,
                    to,
                    edge_types,
                    direction: Direction::Both,
                    prior_edge_vars,
                });
                current = input;
            }
            _ => break,
        }
    }
    if hops.len() >= 2 {
        hops.reverse(); // bottom-up -> top-down
        Some((current, hops))
    } else {
        None
    }
}

/// Depth-first chained expand with limit; no intermediate `Vec<Frame>` between hops.
pub(crate) fn exec_streaming_expand_chain(
    base: &LogicalPlan,
    hops: &[ExpandHop<'_>],
    graph: &mut dyn GraphView,
    limit: usize,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    // prior edge slots for hop[0] (edges bound before the chain)
    let base_prior_slots: Vec<usize> = hops[0]
        .prior_edge_vars
        .iter()
        .filter_map(|v| symtab.slot(v))
        .collect();
    // each hop's rel_var slot (edges within the chain)
    let hop_rel_slots: Vec<usize> = hops
        .iter()
        .map(|h| symtab.slot(h.rel_var).unwrap())
        .collect();

    let base_frames = crate::operators::exec_via_operators(base, graph, symtab)?;

    // last hop into an already-bound slot (cycle)? Checked once here so expand_chain_dfs stays lean
    let last_to_slot = symtab.slot(hops.last().unwrap().to).unwrap();
    let has_bound_last = base_frames.first().is_some_and(|f| match &f[last_to_slot] {
        SlotValue::NodeRef(_) => true,
        SlotValue::Heap(cv) => matches!(
            cv.as_ref(),
            CypherValue::Node { .. } | CypherValue::NodeRef(_)
        ),
        _ => false,
    });
    if has_bound_last {
        return exec_chain_with_bound_last(
            &base_frames,
            hops,
            &base_prior_slots,
            &hop_rel_slots,
            graph,
            limit,
            symtab,
        );
    }

    let mut result = Vec::with_capacity(limit);
    for base_frame in &base_frames {
        expand_chain_dfs(
            base_frame,
            hops,
            0,
            &base_prior_slots,
            &hop_rel_slots,
            graph,
            limit,
            symtab,
            &mut result,
        )?;
        if result.len() >= limit {
            break;
        }
    }
    Ok(result)
}

/// Cold path: chain whose last hop targets a bound node (cycle). DFS over all but
/// the last hop, then `exec_expand_bound_target`, keeping that code out of
/// `expand_chain_dfs`.
#[inline(never)]
fn exec_chain_with_bound_last(
    base_frames: &[Frame],
    hops: &[ExpandHop<'_>],
    base_prior_slots: &[usize],
    hop_rel_slots: &[usize],
    graph: &mut dyn GraphView,
    limit: usize,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    let last_hop = hops.last().unwrap();
    let last_rel_slot = *hop_rel_slots.last().unwrap();
    let last_from_slot = symtab.slot(last_hop.from).unwrap();
    let last_to_slot = symtab.slot(last_hop.to).unwrap();

    let mut result = Vec::with_capacity(limit);

    // prefix hops via chain DFS, then bound-target for the last
    let prefix_hops = &hops[..hops.len() - 1];
    let mut intermediate = Vec::new();
    for base_frame in base_frames {
        expand_chain_dfs(
            base_frame,
            prefix_hops,
            0,
            base_prior_slots,
            hop_rel_slots,
            graph,
            usize::MAX,
            symtab,
            &mut intermediate,
        )?;
    }

    for frame in &intermediate {
        let from_id = get_node_id_frame(frame, last_from_slot)?;
        let target_id = get_node_id_frame(frame, last_to_slot)?;
        let used_edges =
            collect_prior_edge_ids_chain(frame, base_prior_slots, hop_rel_slots, hops.len() - 1);
        exec_expand_bound_target(
            from_id,
            target_id,
            last_hop.direction,
            last_hop.edge_types,
            &used_edges,
            last_rel_slot,
            last_to_slot,
            frame,
            graph,
            Some(limit),
            &mut result,
        )?;
        if result.len() >= limit {
            break;
        }
    }
    Ok(result)
}

#[recursive::recursive]
fn expand_chain_dfs(
    frame: &Frame,
    hops: &[ExpandHop<'_>],
    hop_idx: usize,
    base_prior_slots: &[usize],
    hop_rel_slots: &[usize],
    graph: &mut dyn GraphView,
    limit: usize,
    symtab: &SymbolTable,
    result: &mut Vec<Frame>,
) -> Result<(), CypherError> {
    if hop_idx >= hops.len() {
        result.push(frame.clone());
        return Ok(());
    }
    let hop = &hops[hop_idx];
    let from_slot = symtab.slot(hop.from).unwrap();
    let rel_slot = hop_rel_slots[hop_idx];
    let to_slot = symtab.slot(hop.to).unwrap();
    let from_id = get_node_id_frame(frame, from_slot)?;
    // edge uniqueness: base prior slots + chain hops before this one
    let used_edges = collect_prior_edge_ids_chain(frame, base_prior_slots, hop_rel_slots, hop_idx);

    let type_filters: Vec<Option<&str>> = if hop.edge_types.is_empty() {
        vec![None]
    } else {
        hop.edge_types.iter().map(|t| Some(t.as_str())).collect()
    };

    let mut seen = FxHashSet::default();
    for tf in &type_filters {
        let endpoints = graph
            .edges_with_neighbor(from_id, hop.direction, *tf)
            .map_err(CypherError::from)?;
        for ep in endpoints {
            if !seen.insert(ep.edge_id) {
                continue;
            }
            if used_edges.contains(&ep.edge_id) {
                continue;
            }

            let mut new_frame = frame.clone();
            new_frame[rel_slot] = SlotValue::EdgeRef {
                id: ep.edge_id,
                edge_type_id: ep.edge_type,
                source: ep.source,
                target: ep.target,
            };
            new_frame[to_slot] = SlotValue::NodeRef(ep.neighbor);

            expand_chain_dfs(
                &new_frame,
                hops,
                hop_idx + 1,
                base_prior_slots,
                hop_rel_slots,
                graph,
                limit,
                symtab,
                result,
            )?;
            if result.len() >= limit {
                return Ok(());
            }
        }
    }
    Ok(())
}

// -- Direction reversal for bound targets ------------------------------

/// Expands between two known endpoints from the lower-degree side.
fn exec_expand_bound_target(
    from_id: NodeId,
    target_id: NodeId,
    direction: Direction,
    edge_types: &[String],
    used_edges: &FxHashSet<EdgeId>,
    rel_slot: usize,
    to_slot: usize,
    frame: &Frame,
    graph: &mut dyn GraphView,
    limit: Option<usize>,
    result: &mut Vec<Frame>,
) -> Result<(), CypherError> {
    // compare degrees to pick the side (not for Both, no gain)
    let (expand_from, expand_dir, check_neighbor) = if direction != Direction::Both {
        let from_deg = graph.degree(from_id, direction).unwrap_or(0);
        let rev_dir = reverse_direction(direction);
        let to_deg = graph.degree(target_id, rev_dir).unwrap_or(0);
        if to_deg < from_deg {
            (target_id, rev_dir, from_id)
        } else {
            (from_id, direction, target_id)
        }
    } else if from_id == target_id {
        // self-loop: outgoing only, else edges_between(Both) duplicates
        (from_id, Direction::Outgoing, target_id)
    } else {
        (from_id, direction, target_id)
    };

    // edges_between gives only edges joining the two endpoints, no full neighbor list
    let type_filter = resolve_edge_type_filter(graph, edge_types);
    let edge_ids = graph
        .edges_between(expand_from, check_neighbor, expand_dir, &type_filter)
        .map_err(CypherError::from)?;
    for eid in edge_ids {
        if used_edges.contains(&eid) {
            continue;
        }
        // edge for source/target/type (usually 0-2 matches)
        let edge = graph.get_edge(eid).map_err(CypherError::from)?;
        let mut new_frame = frame.clone();
        new_frame[rel_slot] = SlotValue::EdgeRef {
            id: eid,
            edge_type_id: edge.edge_type,
            source: edge.source,
            target: edge.target,
        };
        new_frame[to_slot] = SlotValue::NodeRef(target_id);
        result.push(new_frame);
        if limit.is_some_and(|l| result.len() >= l) {
            return Ok(());
        }
    }
    Ok(())
}

pub(crate) fn exec_expand(
    input: &LogicalPlan,
    from: &str,
    rel_var: &str,
    to: &str,
    edge_types: &[String],
    range: &Option<(Option<u32>, Option<u32>)>,
    prior_edge_vars: &[String],
    direction: Direction,
    graph: &mut dyn GraphView,
    limit: Option<usize>,
    symtab: &SymbolTable,
) -> Result<crate::memory::RowBuffer, CypherError> {
    // with a limit, pull input frames one by one instead of materializing the whole scan; without
    // one, materialize up front
    let mut input_op;
    let materialized;
    if limit.is_some() {
        input_op = Some(crate::operators::build_operator_tree(input));
        materialized = crate::memory::RowBuffer::new();
    } else {
        input_op = None;
        materialized = crate::operators::exec_via_operators(input, graph, symtab)?;
    }
    let from_slot = symtab.slot(from).unwrap();
    let rel_slot = symtab.slot(rel_var).unwrap();
    let to_slot = symtab.slot(to).unwrap();
    // prior edge slots, once before the loop
    let prior_edge_slots: Vec<usize> = prior_edge_vars
        .iter()
        .filter_map(|v| symtab.slot(v))
        .collect();
    // every expansion is materialized here; charge as built so a var-length explosion fails the
    // query, not the process
    let mut result = Vec::new();
    let mut charge = crate::memory::Charge::new();
    let mut mat_pos = 0;

    loop {
        // next input frame, from the streaming operator or the materialized Vec
        let frame = if let Some(ref mut op) = input_op {
            match op.next(graph, symtab)? {
                Some(f) => f,
                None => break,
            }
        } else {
            if mat_pos >= materialized.len() {
                break;
            }
            let f = materialized[mat_pos].clone();
            mat_pos += 1;
            f
        };
        // skip null sources (e.g. from OPTIONAL MATCH)
        let from_id = match get_node_id_frame(&frame, from_slot) {
            Ok(id) => id,
            Err(_) if matches!(frame[from_slot], SlotValue::Null) => continue,
            Err(e) => return Err(e),
        };
        // relationship isomorphism: targeted slot reads, no full frame scan
        let used_edges = collect_prior_edge_ids(&frame, &prior_edge_slots);

        if let Some((min_opt, max_opt)) = range {
            // variable length
            let min = min_opt.unwrap_or(1) as usize;
            let max = max_opt.unwrap_or(30) as usize; // cap at 30
            let paths =
                find_variable_length_paths(graph, from_id, direction, edge_types, min, max)?;
            for (end_node_id, path_edges) in paths {
                // relationship isomorphism: skip paths reusing bound edges
                if path_edges.iter().any(|ep| used_edges.contains(&ep.edge_id)) {
                    continue;
                }
                let mut new_frame = frame.clone();
                let edge_list: Vec<CypherValue> = path_edges
                    .iter()
                    .map(|ep| CypherValue::EdgeRef {
                        id: ep.edge_id,
                        edge_type_id: ep.edge_type,
                        source: ep.source,
                        target: ep.target,
                    })
                    .collect();
                new_frame[rel_slot] = CypherValue::List(edge_list).into();
                new_frame[to_slot] = SlotValue::NodeRef(end_node_id);
                charge.add(crate::memory::frame_bytes(&new_frame))?;
                result.push(new_frame);
                if limit.is_some_and(|l| result.len() >= l) {
                    break;
                }
            }
        } else {
            // fixed length 1: target already bound? then direction reversal
            let bound_target = match &frame[to_slot] {
                SlotValue::NodeRef(id) => Some(*id),
                SlotValue::Heap(cv) => match cv.as_ref() {
                    CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => Some(*id),
                    _ => None,
                },
                _ => None,
            };

            if let Some(target_id) = bound_target {
                // both endpoints known: expand from the lower-degree side
                let before = result.len();
                exec_expand_bound_target(
                    from_id,
                    target_id,
                    direction,
                    edge_types,
                    &used_edges,
                    rel_slot,
                    to_slot,
                    &frame,
                    graph,
                    limit,
                    &mut result,
                )?;
                for f in &result[before..] {
                    charge.add(crate::memory::frame_bytes(f))?;
                }
            } else {
                // single hop via edges_with_neighbor (no property clones)
                let type_filters: Vec<Option<&str>> = if edge_types.is_empty() {
                    vec![None] // no filter: one call
                } else {
                    edge_types.iter().map(|t| Some(t.as_str())).collect()
                };
                let mut seen_edge_ids = FxHashSet::default();
                'type_loop: for type_filter in &type_filters {
                    let endpoints = graph
                        .edges_with_neighbor(from_id, direction, *type_filter)
                        .map_err(CypherError::from)?;
                    for ep in endpoints {
                        // dedup: self-loops show up twice for Both, and across type filters
                        if !seen_edge_ids.insert(ep.edge_id) {
                            continue;
                        }
                        // relationship isomorphism: skip edges already bound
                        if used_edges.contains(&ep.edge_id) {
                            continue;
                        }
                        let mut new_frame = frame.clone();
                        new_frame[rel_slot] = SlotValue::EdgeRef {
                            id: ep.edge_id,
                            edge_type_id: ep.edge_type,
                            source: ep.source,
                            target: ep.target,
                        };
                        new_frame[to_slot] = SlotValue::NodeRef(ep.neighbor);
                        charge.add(crate::memory::frame_bytes(&new_frame))?;
                        result.push(new_frame);
                        if limit.is_some_and(|l| result.len() >= l) {
                            break 'type_loop;
                        }
                    }
                }
            }
        }
        if limit.is_some_and(|l| result.len() >= l) {
            break;
        }
    }
    Ok(crate::memory::RowBuffer::from_parts(result, charge))
}

/// `exec_expand` over pre-resolved input frames instead of a plan; lets `exec_via_operators_seeded`
/// skip re-materializing an input it already has (e.g. OPTIONAL MATCH).
pub(crate) fn exec_expand_frames(
    input_frames: &[Frame],
    from: &str,
    rel_var: &str,
    to: &str,
    edge_types: &[String],
    range: &Option<(Option<u32>, Option<u32>)>,
    prior_edge_vars: &[String],
    direction: Direction,
    graph: &mut dyn GraphView,
    limit: Option<usize>,
    symtab: &SymbolTable,
) -> Result<Vec<Frame>, CypherError> {
    let from_slot = symtab.slot(from).unwrap();
    let rel_slot = symtab.slot(rel_var).unwrap();
    let to_slot = symtab.slot(to).unwrap();
    let prior_edge_slots: Vec<usize> = prior_edge_vars
        .iter()
        .filter_map(|v| symtab.slot(v))
        .collect();
    let mut result = Vec::new();

    let mut limit_count: u32 = 0;
    for frame in input_frames {
        limit_count = limit_count.wrapping_add(1);
        if limit_count & 0xFFF == 0 {
            crate::query::check()?;
        }
        let from_id = match get_node_id_frame(frame, from_slot) {
            Ok(id) => id,
            Err(_) if matches!(frame[from_slot], SlotValue::Null) => continue,
            Err(e) => return Err(e),
        };
        let used_edges = collect_prior_edge_ids(frame, &prior_edge_slots);

        // target already bound (e.g. OPTIONAL MATCH where the outer MATCH set it)?
        let bound_to = match &frame[to_slot] {
            SlotValue::NodeRef(id) => Some(*id),
            SlotValue::Heap(cv) => match cv.as_ref() {
                CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => Some(*id),
                _ => None,
            },
            _ => None,
        };

        if let Some((min_opt, max_opt)) = range {
            let min = min_opt.unwrap_or(1) as usize;
            let max = max_opt.unwrap_or(30) as usize;
            let paths =
                find_variable_length_paths(graph, from_id, direction, edge_types, min, max)?;
            for (end_node_id, path_edges) in paths {
                if path_edges.iter().any(|ep| used_edges.contains(&ep.edge_id)) {
                    continue;
                }
                if let Some(bid) = bound_to
                    && end_node_id != bid
                {
                    continue;
                }
                let mut new_frame = frame.clone();
                let edge_list: Vec<CypherValue> = path_edges
                    .iter()
                    .map(|ep| CypherValue::EdgeRef {
                        id: ep.edge_id,
                        edge_type_id: ep.edge_type,
                        source: ep.source,
                        target: ep.target,
                    })
                    .collect();
                new_frame[rel_slot] = CypherValue::List(edge_list).into();
                new_frame[to_slot] = SlotValue::NodeRef(end_node_id);
                result.push(new_frame);
                if limit.is_some_and(|l| result.len() >= l) {
                    break;
                }
            }
        } else {
            if let Some(target_id) = bound_to {
                exec_expand_bound_target(
                    from_id,
                    target_id,
                    direction,
                    edge_types,
                    &used_edges,
                    rel_slot,
                    to_slot,
                    frame,
                    graph,
                    limit,
                    &mut result,
                )?;
            } else {
                let type_filters: Vec<Option<&str>> = if edge_types.is_empty() {
                    vec![None]
                } else {
                    edge_types.iter().map(|t| Some(t.as_str())).collect()
                };
                let mut seen_edge_ids = FxHashSet::default();
                'type_loop: for type_filter in &type_filters {
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
                        let mut new_frame = frame.clone();
                        new_frame[rel_slot] = SlotValue::EdgeRef {
                            id: ep.edge_id,
                            edge_type_id: ep.edge_type,
                            source: ep.source,
                            target: ep.target,
                        };
                        new_frame[to_slot] = SlotValue::NodeRef(ep.neighbor);
                        result.push(new_frame);
                        if limit.is_some_and(|l| result.len() >= l) {
                            break 'type_loop;
                        }
                    }
                }
            }
        }
        if limit.is_some_and(|l| result.len() >= l) {
            break;
        }
    }
    Ok(result)
}

pub(super) fn find_variable_length_paths(
    graph: &dyn GraphView,
    start: NodeId,
    direction: Direction,
    edge_types: &[String],
    min: usize,
    max: usize,
) -> Result<Vec<(NodeId, Vec<EdgeEndpoint>)>, CypherError> {
    // ISO section 14.5 path mode. TRAIL (default) keeps edges unique within each path, the
    // openCypher behaviour; WALK lets an edge repeat; ACYCLIC also requires unique
    // nodes. Read from a thread-local set by the GQL surface; openCypher always gets TRAIL.
    let path_mode = crate::session::current_path_mode();
    let mut results = Vec::new();
    // paths and the DFS frontier are charged to the query memory budget as built:
    // a dense neighbourhood at depth 30 is the classic way to exhaust a machine.
    // Released on return (frames built from the paths are charged by the caller).
    let mut charge = crate::memory::Charge::new();
    // TRAIL / ACYCLIC track the path's edges, ACYCLIC also its nodes; both sets ride along even
    // when unused so the inner loop stays branch-free
    let mut stack: Vec<(
        NodeId,
        Vec<EdgeEndpoint>,
        FxHashSet<EdgeId>,
        FxHashSet<NodeId>,
    )> = Vec::new();

    let mut start_nodes = FxHashSet::default();
    if matches!(path_mode, crate::ast::PathMode::Acyclic) {
        start_nodes.insert(start);
    }
    stack.push((start, Vec::new(), FxHashSet::default(), start_nodes));

    while let Some((current, path, used_edges, visited_nodes)) = stack.pop() {
        let depth = path.len();
        if depth >= min {
            charge.add(40 + depth * std::mem::size_of::<EdgeEndpoint>())?;
            results.push((current, path.clone()));
        }
        if depth >= max {
            continue;
        }

        // type filter pushed into edges_with_neighbor (no property clones)
        let type_filters: Vec<Option<&str>> = if edge_types.is_empty() {
            vec![None]
        } else {
            edge_types.iter().map(|t| Some(t.as_str())).collect()
        };
        for type_filter in &type_filters {
            let endpoints = graph
                .edges_with_neighbor(current, direction, *type_filter)
                .map_err(CypherError::from)?;
            for ep in endpoints {
                // path-mode uniqueness
                match path_mode {
                    crate::ast::PathMode::Trail => {
                        if used_edges.contains(&ep.edge_id) {
                            continue;
                        }
                    }
                    crate::ast::PathMode::Acyclic => {
                        if used_edges.contains(&ep.edge_id) || visited_nodes.contains(&ep.neighbor)
                        {
                            continue;
                        }
                    }
                    crate::ast::PathMode::Walk => {
                        // no within-path uniqueness: edges/nodes may repeat
                    }
                }
                let mut new_path = path.clone();
                new_path.push(ep);
                let new_used = if matches!(path_mode, crate::ast::PathMode::Walk) {
                    used_edges.clone()
                } else {
                    let mut s = used_edges.clone();
                    s.insert(ep.edge_id);
                    s
                };
                let new_visited = if matches!(path_mode, crate::ast::PathMode::Acyclic) {
                    let mut v = visited_nodes.clone();
                    v.insert(ep.neighbor);
                    v
                } else {
                    visited_nodes.clone()
                };
                charge.add(96 + (depth + 1) * (std::mem::size_of::<EdgeEndpoint>() + 24))?;
                stack.push((ep.neighbor, new_path, new_used, new_visited));
            }
        }
    }

    Ok(results)
}

/// Edge-type names -> `EdgeTypeId` set for O(1) checks during BFS. `None` for an empty list (all
/// types).
pub(crate) fn resolve_edge_type_filter(
    graph: &dyn GraphView,
    edge_types: &[String],
) -> Option<FxHashSet<EdgeTypeId>> {
    if edge_types.is_empty() {
        return None;
    }
    let st = graph.strings();
    Some(
        edge_types
            .iter()
            .filter_map(|t| st.get(t).map(EdgeTypeId))
            .collect(),
    )
}

/// NodeId from a frame slot.
pub(crate) fn get_node_id_frame(frame: &Frame, slot: usize) -> Result<NodeId, CypherError> {
    match &frame[slot] {
        SlotValue::NodeRef(id) => Ok(*id),
        SlotValue::Heap(cv) => match cv.as_ref() {
            CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => Ok(*id),
            _ => Err(CypherError::type_error(format!(
                "expected node, got {}",
                cv.type_name()
            ))),
        },
        SlotValue::Null => Err(CypherError::semantic("variable not bound".to_string())),
        other => Err(CypherError::type_error(format!(
            "expected node, got {}",
            other.type_name()
        ))),
    }
}

/// Edge IDs from specific frame slots (planner-directed uniqueness).
#[inline(never)]
pub(crate) fn collect_prior_edge_ids(frame: &Frame, slots: &[usize]) -> FxHashSet<EdgeId> {
    let mut ids = FxHashSet::default();
    for &slot in slots {
        match &frame[slot] {
            SlotValue::EdgeRef { id, .. } => {
                ids.insert(*id);
            }
            SlotValue::Heap(cv) => match cv.as_ref() {
                CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                    ids.insert(*id);
                }
                CypherValue::List(items) => {
                    for item in items {
                        if let CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } = item
                        {
                            ids.insert(*id);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    ids
}

/// Edge IDs for chain DFS: base prior slots + chain hops before `hop_idx`.
#[inline(never)]
fn collect_prior_edge_ids_chain(
    frame: &Frame,
    base_prior_slots: &[usize],
    hop_rel_slots: &[usize],
    hop_idx: usize,
) -> FxHashSet<EdgeId> {
    let mut ids = FxHashSet::default();
    // edges bound before the chain (planner-directed)
    for &slot in base_prior_slots {
        match &frame[slot] {
            SlotValue::EdgeRef { id, .. } => {
                ids.insert(*id);
            }
            SlotValue::Heap(cv) => match cv.as_ref() {
                CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } => {
                    ids.insert(*id);
                }
                CypherValue::List(items) => {
                    for item in items {
                        if let CypherValue::Edge { id, .. } | CypherValue::EdgeRef { id, .. } = item
                        {
                            ids.insert(*id);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    // edges from earlier hops of this chain
    for &slot in &hop_rel_slots[..hop_idx] {
        if let SlotValue::EdgeRef { id, .. } = &frame[slot] {
            ids.insert(*id);
        }
    }
    ids
}
