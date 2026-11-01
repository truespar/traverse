//! Parallel traversal operators: the parallel DistinctExpand frontier expansion. BFS shortest-path
//! operators stay sequential in the executor (their frontiers are usually small, parallelism would
//! only add overhead).

use rustc_hash::FxHashSet;

use traverse_core::graph::types::{Direction, EdgeTypeId, LabelId, NodeId};
use traverse_core::tx::view::GraphView;

use crate::ast::Expr;
use crate::error::CypherError;
use crate::executor::{eval_expr, is_truthy};
use crate::frame::{Frame, SlotValue, SymbolTable};

/// Frontier size where parallel expansion kicks in.
const PAR_FRONTIER_THRESHOLD: usize = 64;

/// Frontier nodes per rayon task.
const PAR_CHUNK_SIZE: usize = 32;

// -- parallel DistinctExpand -------------------------------------------

/// Parallel level-synchronous BFS for DistinctExpand: same algorithm as the inline code in
/// `DistinctExpandOp::next()`, but large frontiers expand on rayon.
#[inline(never)]
pub(crate) fn distinct_expand_par(
    graph: &(dyn GraphView + Sync),
    start_id: NodeId,
    min_hops: usize,
    max_hops: usize,
    direction: Direction,
    type_filter: &Option<FxHashSet<EdgeTypeId>>,
    label_id: Option<LabelId>,
    target_filter: Option<(&Expr, &SymbolTable)>,
    mut global_seen: Option<&mut FxHashSet<NodeId>>,
    frame: &Frame,
    to_slot: usize,
    result: &mut Vec<Frame>,
) -> Result<(), CypherError> {
    let mut frontier: FxHashSet<NodeId> = FxHashSet::default();
    frontier.insert(start_id);
    let mut collected: FxHashSet<NodeId> = FxHashSet::default();
    let mut frontier_vec: Vec<NodeId> = Vec::new();

    for depth in 1..=max_hops {
        frontier_vec.clear();
        frontier_vec.extend(frontier.iter().copied());
        let next = if frontier.len() >= PAR_FRONTIER_THRESHOLD {
            expand_frontier_par(graph, &frontier_vec, direction, type_filter)?
        } else {
            expand_frontier_seq(graph, &frontier_vec, direction, type_filter)?
        };
        frontier = next;
        if depth >= min_hops {
            collected.extend(&frontier);
        }
    }

    // reusable scratch frame for filter evaluation (no clone per candidate)
    let mut scratch = if target_filter.is_some() {
        Some(frame.clone())
    } else {
        None
    };

    for &nid in &collected {
        if let Some(ref mut seen) = global_seen
            && !seen.insert(nid)
        {
            continue;
        }
        if let Some(lid) = label_id
            && !graph.has_label(nid, lid).unwrap_or(false)
        {
            continue;
        }
        // target_filter before frame allocation
        if let Some((filter_expr, symtab)) = &target_filter {
            let s = scratch.as_mut().unwrap();
            s[to_slot] = SlotValue::NodeRef(nid);
            // eval_expr wants &dyn GraphView, upcast from &(dyn GraphView + Sync)
            let val = eval_expr(filter_expr, s, graph, symtab)?;
            if !is_truthy(&val) {
                continue;
            }
        }
        let mut new_frame = frame.clone();
        new_frame[to_slot] = SlotValue::NodeRef(nid);
        result.push(new_frame);
    }
    Ok(())
}

/// Parallel frontier expansion: chunks expanded in parallel via bulk `expand_frontier`, hash sets
/// merged.
fn expand_frontier_par(
    graph: &(dyn GraphView + Sync),
    nodes: &[NodeId],
    direction: Direction,
    type_filter: &Option<FxHashSet<EdgeTypeId>>,
) -> Result<FxHashSet<NodeId>, CypherError> {
    use rayon::prelude::*;

    let chunk_sets: Result<Vec<FxHashSet<NodeId>>, CypherError> = nodes
        .par_chunks(PAR_CHUNK_SIZE)
        .map(|chunk| {
            let mut local = FxHashSet::default();
            graph
                .expand_frontier(chunk, direction, type_filter, &mut local)
                .map_err(CypherError::from)?;
            Ok(local)
        })
        .collect();

    let mut sets = chunk_sets?;
    if sets.is_empty() {
        return Ok(FxHashSet::default());
    }
    // start from the largest set to minimize re-hashing
    sets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut merged = sets.swap_remove(0);
    for s in sets {
        merged.extend(s);
    }
    Ok(merged)
}

/// Sequential frontier expansion (below the threshold).
fn expand_frontier_seq(
    graph: &(dyn GraphView + Sync),
    nodes: &[NodeId],
    direction: Direction,
    type_filter: &Option<FxHashSet<EdgeTypeId>>,
) -> Result<FxHashSet<NodeId>, CypherError> {
    let mut next: FxHashSet<NodeId> = FxHashSet::default();
    graph
        .expand_frontier(nodes, direction, type_filter, &mut next)
        .map_err(CypherError::from)?;
    Ok(next)
}
