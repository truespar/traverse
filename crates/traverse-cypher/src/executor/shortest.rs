//! Shortest paths: bidirectional BFS, all-shortest, k-shortest and Dijkstra.

use rustc_hash::{FxHashMap, FxHashSet};
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{Direction, EdgeId, EdgeTypeId, NodeId};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;

/// `(EdgeId, neighbor)` pairs of `node` in `direction`, filtered by type.
fn expand_node(
    graph: &dyn GraphView,
    node: NodeId,
    direction: Direction,
    type_filter: &Option<FxHashSet<EdgeTypeId>>,
) -> Result<Vec<(EdgeId, NodeId)>, CypherError> {
    graph
        .edge_endpoints_filtered(node, direction, type_filter)
        .map_err(CypherError::from)
}

/// Edge path straight from the bidirectional BFS parent maps of `(EdgeId, NodeId)`; no
/// `edges_between` post-pass.
fn reconstruct_bidirectional_edges(
    fwd_parent: &FxHashMap<NodeId, (EdgeId, NodeId)>,
    bwd_parent: &FxHashMap<NodeId, (EdgeId, NodeId)>,
    meeting: NodeId,
) -> Vec<EdgeId> {
    // forward half: meeting -> ... -> start, then reverse
    let mut fwd_edges = Vec::new();
    let mut cur = meeting;
    while let Some(&(eid, prev)) = fwd_parent.get(&cur) {
        fwd_edges.push(eid);
        cur = prev;
    }
    fwd_edges.reverse();

    // backward half: meeting -> ... -> target
    cur = meeting;
    while let Some(&(eid, prev)) = bwd_parent.get(&cur) {
        fwd_edges.push(eid);
        cur = prev;
    }
    fwd_edges
}

/// Direction for the backward BFS: Out<->In, Both stays.
pub(super) fn reverse_direction(dir: Direction) -> Direction {
    match dir {
        Direction::Outgoing => Direction::Incoming,
        Direction::Incoming => Direction::Outgoing,
        Direction::Both => Direction::Both,
    }
}

/// Bidirectional parent-pointer BFS for `shortestPath` (at most one path). Parent maps hold
/// `(EdgeId, NodeId)`, so edges come straight out without `edges_between`.
pub(crate) fn bfs_single_shortest(
    graph: &dyn GraphView,
    start: NodeId,
    target: NodeId,
    direction: Direction,
    edge_types: &[String],
    min_depth: usize,
    max_depth: usize,
) -> Result<Vec<Vec<EdgeId>>, CypherError> {
    if start == target && min_depth == 0 {
        return Ok(vec![vec![]]);
    }

    let type_filter = resolve_edge_type_filter(graph, edge_types);

    // start == target (cycles need unidirectional BFS) or min_depth > 1 (rare, would
    // need depth gating): unidirectional multi-parent BFS. Otherwise bidirectional
    // works since any path between distinct nodes has length >= 1.
    if start == target || min_depth > 1 {
        let mut paths = bfs_all_shortest_inner(
            graph,
            start,
            target,
            direction,
            &type_filter,
            min_depth,
            max_depth,
        )?;
        paths.truncate(1);
        return Ok(paths);
    }

    // -- bidirectional BFS (start != target, min_depth <= 1) --
    let rev_dir = reverse_direction(direction);

    // parent[node] = (edge_id, previous_node)
    let mut fwd_parent: FxHashMap<NodeId, (EdgeId, NodeId)> = FxHashMap::default();
    let mut bwd_parent: FxHashMap<NodeId, (EdgeId, NodeId)> = FxHashMap::default();

    let mut fwd_frontier: Vec<NodeId> = vec![start];
    let mut bwd_frontier: Vec<NodeId> = vec![target];

    let mut fwd_visited: FxHashSet<NodeId> = FxHashSet::default();
    let mut bwd_visited: FxHashSet<NodeId> = FxHashSet::default();
    fwd_visited.insert(start);
    bwd_visited.insert(target);

    let mut depth = 0;

    while !fwd_frontier.is_empty() && !bwd_frontier.is_empty() && depth < max_depth {
        depth += 1;

        // expand the smaller frontier
        let expand_forward = fwd_frontier.len() <= bwd_frontier.len();

        if expand_forward {
            let mut next_frontier = Vec::new();
            for &node in &fwd_frontier {
                for (eid, neighbor) in graph
                    .edge_endpoints_filtered(node, direction, &type_filter)
                    .map_err(CypherError::from)?
                {
                    if fwd_visited.contains(&neighbor) {
                        continue;
                    }
                    if fwd_parent.contains_key(&neighbor) {
                        continue; // already found at this level
                    }
                    fwd_parent.insert(neighbor, (eid, node));

                    if bwd_visited.contains(&neighbor) {
                        // met: rebuild the edge path
                        let edges =
                            reconstruct_bidirectional_edges(&fwd_parent, &bwd_parent, neighbor);
                        return Ok(vec![edges]);
                    }

                    next_frontier.push(neighbor);
                }
            }
            for &n in &next_frontier {
                fwd_visited.insert(n);
            }
            fwd_frontier = next_frontier;
        } else {
            let mut next_frontier = Vec::new();
            for &node in &bwd_frontier {
                for (eid, neighbor) in graph
                    .edge_endpoints_filtered(node, rev_dir, &type_filter)
                    .map_err(CypherError::from)?
                {
                    if bwd_visited.contains(&neighbor) {
                        continue;
                    }
                    if bwd_parent.contains_key(&neighbor) {
                        continue;
                    }
                    bwd_parent.insert(neighbor, (eid, node));

                    if fwd_visited.contains(&neighbor) {
                        let edges =
                            reconstruct_bidirectional_edges(&fwd_parent, &bwd_parent, neighbor);
                        return Ok(vec![edges]);
                    }

                    next_frontier.push(neighbor);
                }
            }
            for &n in &next_frontier {
                bwd_visited.insert(n);
            }
            bwd_frontier = next_frontier;
        }
    }

    Ok(vec![])
}

/// All paths from `start` to `node` via DFS over the multi-parent map, as edge-id paths.
#[recursive::recursive]
fn enumerate_paths(
    parents: &FxHashMap<NodeId, Vec<(EdgeId, NodeId)>>,
    start: NodeId,
    node: NodeId,
) -> Vec<Vec<EdgeId>> {
    if node == start {
        return vec![vec![]];
    }
    let mut result = Vec::new();
    if let Some(plist) = parents.get(&node) {
        for &(eid, prev) in plist {
            for mut prefix in enumerate_paths(parents, start, prev) {
                prefix.push(eid);
                result.push(prefix);
            }
        }
    }
    result
}

/// Unidirectional multi-parent BFS.
fn bfs_all_shortest_inner(
    graph: &dyn GraphView,
    start: NodeId,
    target: NodeId,
    direction: Direction,
    type_filter: &Option<FxHashSet<EdgeTypeId>>,
    min_depth: usize,
    max_depth: usize,
) -> Result<Vec<Vec<EdgeId>>, CypherError> {
    if start == target && min_depth == 0 {
        return Ok(vec![vec![]]);
    }

    // parents[node] = (edge, predecessor) at the shortest depth
    let mut parents: FxHashMap<NodeId, Vec<(EdgeId, NodeId)>> = FxHashMap::default();
    // depth where each node was first found
    let mut depth_of: FxHashMap<NodeId, usize> = FxHashMap::default();
    depth_of.insert(start, 0);

    let mut frontier: Vec<NodeId> = vec![start];
    let mut depth: usize = 0;
    let mut target_found = false;

    while !frontier.is_empty() && depth < max_depth && !target_found {
        depth += 1;
        let mut next_frontier: Vec<NodeId> = Vec::new();

        for &node in &frontier {
            for (eid, neighbor) in expand_node(graph, node, direction, type_filter)? {
                // skip if found at an earlier depth
                if let Some(&d) = depth_of.get(&neighbor)
                    && d < depth
                {
                    continue;
                }

                parents.entry(neighbor).or_default().push((eid, node));

                if depth_of.contains_key(&neighbor) {
                    continue; // already in this depth's frontier
                }
                depth_of.insert(neighbor, depth);

                if neighbor == target {
                    target_found = true;
                    // target stays out of the next frontier: we stop after this level
                } else {
                    next_frontier.push(neighbor);
                }
            }
        }

        frontier = next_frontier;
    }

    if !target_found {
        return Ok(vec![]);
    }

    if depth < min_depth {
        return Ok(vec![]);
    }

    Ok(enumerate_paths(&parents, start, target))
}

/// Unidirectional multi-parent BFS for `allShortestPaths`. Each node appears once per depth with
/// all its parent edges, so the frontier stays O(V), not exponential.
pub(crate) fn bfs_all_shortest(
    graph: &dyn GraphView,
    start: NodeId,
    target: NodeId,
    direction: Direction,
    edge_types: &[String],
    min_depth: usize,
    max_depth: usize,
) -> Result<Vec<Vec<EdgeId>>, CypherError> {
    let type_filter = resolve_edge_type_filter(graph, edge_types);
    bfs_all_shortest_inner(
        graph,
        start,
        target,
        direction,
        &type_filter,
        min_depth,
        max_depth,
    )
}

/// Constrained single-source BFS, the inner shortest-path step of `bfs_k_shortest`
/// (Yen). `forbidden_edges` / `forbidden_nodes` are skipped; the start may be
/// forbidden only if it equals `target` with `min_depth == 0` (Yen never spurs
/// from a forbidden node). Loopless shortest edge path, or `None`.
fn bfs_constrained_unidirectional(
    graph: &dyn GraphView,
    start: NodeId,
    target: NodeId,
    direction: Direction,
    type_filter: &Option<FxHashSet<EdgeTypeId>>,
    min_depth: usize,
    max_depth: usize,
    forbidden_edges: &FxHashSet<EdgeId>,
    forbidden_nodes: &FxHashSet<NodeId>,
) -> Result<Option<Vec<EdgeId>>, CypherError> {
    if forbidden_nodes.contains(&target) {
        return Ok(None);
    }
    if start == target && min_depth == 0 {
        return Ok(Some(vec![]));
    }
    let mut parent: FxHashMap<NodeId, (EdgeId, NodeId)> = FxHashMap::default();
    let mut visited: FxHashSet<NodeId> = FxHashSet::default();
    visited.insert(start);
    let mut frontier: Vec<NodeId> = vec![start];
    let mut depth = 0;
    while !frontier.is_empty() && depth < max_depth {
        depth += 1;
        let mut next_frontier: Vec<NodeId> = Vec::new();
        for &node in &frontier {
            for (eid, neighbor) in expand_node(graph, node, direction, type_filter)? {
                if forbidden_edges.contains(&eid) {
                    continue;
                }
                if forbidden_nodes.contains(&neighbor) {
                    continue;
                }
                if visited.contains(&neighbor) {
                    continue;
                }
                parent.insert(neighbor, (eid, node));
                if neighbor == target {
                    if depth < min_depth {
                        // first reach is below min: keep going deeper; mark visited so we don't
                        // come back at this depth or shallower
                        visited.insert(neighbor);
                        next_frontier.push(neighbor);
                        continue;
                    }
                    // rebuild the edge path
                    let mut path = Vec::new();
                    let mut cur = target;
                    while cur != start {
                        let (eid, prev) = parent[&cur];
                        path.push(eid);
                        cur = prev;
                    }
                    path.reverse();
                    return Ok(Some(path));
                }
                visited.insert(neighbor);
                next_frontier.push(neighbor);
            }
        }
        frontier = next_frontier;
    }
    Ok(None)
}

/// GQL section 14.6 `SHORTEST k GROUPS`: every loopless path from `start` to `target`
/// whose length is among the `k` smallest distinct lengths (`k = 1` = ALL SHORTEST).
///
/// DFS enumerates every loopless edge path within `max_depth` (TRAIL: a
/// `used_edges` stack), then buckets by length and returns the first `k` buckets.
///
/// `ENUMERATION_CAP` bounds the total path count. Past it enumeration stops
/// early; DFS tends to find shorter paths first, so the result is usually a
/// prefix of the right answer, but only best effort (use a smaller `max_depth`
/// for strict results).
pub(crate) fn bfs_k_groups(
    graph: &dyn GraphView,
    start: NodeId,
    target: NodeId,
    direction: Direction,
    edge_types: &[String],
    min_depth: usize,
    max_depth: usize,
    k_groups: usize,
) -> Result<Vec<Vec<EdgeId>>, CypherError> {
    use std::collections::BTreeMap;
    /// Cap on paths explored end to end: SHORTEST 5 GROUPS on dense small graphs completes, very
    /// dense graphs can't run away. Documented in CONFORMANCE.md.
    const ENUMERATION_CAP: usize = 10_000;
    if k_groups == 0 {
        return Ok(Vec::new());
    }
    // a trail can't be longer than the edge count, so cap unbounded `[*]` (~ u32::MAX) there:
    // exact, loses no path, stops runaway DFS on cyclic graphs
    let max_depth = max_depth.min(graph.edge_count().saturating_add(1));
    let type_filter = resolve_edge_type_filter(graph, edge_types);

    let mut all_paths: Vec<Vec<EdgeId>> = Vec::new();
    let mut used_edges: FxHashSet<EdgeId> = FxHashSet::default();
    let mut current_edges: Vec<EdgeId> = Vec::with_capacity(max_depth.min(64));
    let mut steps: u64 = 0;

    // iterative DFS (no recursion limits on deep paths); each frame materializes its
    // node's neighbours plus a cursor. Hits are recorded on *arrival* at the target
    // (right after pushing the edge), not per iteration of that frame, else a target
    // with N outgoing edges would be recorded N times.
    type Frame = (Vec<(EdgeId, NodeId)>, usize); // (neighbours, cursor)
    let initial_neighbours = expand_node(graph, start, direction, &type_filter)?;
    let mut stack: Vec<Frame> = Vec::with_capacity(max_depth.min(64));
    stack.push((initial_neighbours, 0));

    while let Some((neighbours, cursor)) = stack.last_mut() {
        if all_paths.len() >= ENUMERATION_CAP {
            break;
        }
        steps = steps.wrapping_add(1);
        if steps & 0x3FF == 0 {
            crate::query::check()?;
        }
        // backtrack when neighbours are exhausted or the depth budget is spent
        if *cursor >= neighbours.len() || current_edges.len() >= max_depth {
            stack.pop();
            if let Some(eid) = current_edges.pop() {
                used_edges.remove(&eid);
            }
            continue;
        }
        let (eid, neighbour) = neighbours[*cursor];
        *cursor += 1;
        if used_edges.contains(&eid) {
            continue;
        }
        used_edges.insert(eid);
        current_edges.push(eid);
        // just arrived at `neighbour` via `eid`: record a target hit once per arrival, even though
        // DFS may go deeper for longer paths
        if neighbour == target && current_edges.len() >= min_depth {
            all_paths.push(current_edges.clone());
        }
        let neighbour_neighbours = expand_node(graph, neighbour, direction, &type_filter)?;
        stack.push((neighbour_neighbours, 0));
    }

    if all_paths.is_empty() {
        return Ok(all_paths);
    }

    // group by length; BTreeMap yields lengths ascending, push keeps order within a bucket
    let mut by_length: BTreeMap<usize, Vec<Vec<EdgeId>>> = BTreeMap::new();
    for p in all_paths {
        by_length.entry(p.len()).or_default().push(p);
    }

    // first `k_groups` buckets, flattened
    let mut out: Vec<Vec<EdgeId>> = Vec::new();
    for (_len, bucket) in by_length.into_iter().take(k_groups) {
        out.extend(bucket);
    }
    Ok(out)
}

/// GQL section 14.6 `SHORTEST k`: top-k loopless paths from `start` to `target` by
/// ascending length. Yen's algorithm with BFS as the inner shortest path
/// (unit weights).
///
/// Keeps accepted paths and a min-heap of candidates by length. For each prefix
/// node of the last accepted path, a spur BFS runs with:
///
///   * **Edge forbids**: edge `accepted[j][i]` for every accepted path whose
///     first `i+1` nodes match the current root prefix, so no path repeats both
///     prefix and next edge.
///   * **Node forbids**: every prefix node except the spur, so the spur can't
///     revisit the prefix and the concatenation stays loopless (TRAIL: node
///     uniqueness implies edge uniqueness per path).
///
/// Up to `k` paths, shortest first, deduplicated by edge sequence; fewer if
/// fewer exist within `max_depth`.
pub(crate) fn bfs_k_shortest(
    graph: &dyn GraphView,
    start: NodeId,
    target: NodeId,
    direction: Direction,
    edge_types: &[String],
    min_depth: usize,
    max_depth: usize,
    k: usize,
) -> Result<Vec<Vec<EdgeId>>, CypherError> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    if k == 0 {
        return Ok(Vec::new());
    }
    // a loopless path visits each node at most once, so cap unbounded `[*]` (~ u32::MAX) at the
    // node count: exact, keeps spur searches bounded
    let max_depth = max_depth.min(graph.node_count().saturating_add(1));
    let type_filter = resolve_edge_type_filter(graph, edge_types);

    let empty_e: FxHashSet<EdgeId> = FxHashSet::default();
    let empty_n: FxHashSet<NodeId> = FxHashSet::default();
    let first = match bfs_constrained_unidirectional(
        graph,
        start,
        target,
        direction,
        &type_filter,
        min_depth,
        max_depth,
        &empty_e,
        &empty_n,
    )? {
        Some(p) => p,
        None => return Ok(Vec::new()),
    };
    let mut accepted: Vec<Vec<EdgeId>> = Vec::with_capacity(k);
    accepted.push(first);

    // node sequence of an edge path; uses the outer BFS direction so undirected walks pick the
    // right endpoint per hop
    let nodes_along = |path: &[EdgeId]| -> Result<Vec<NodeId>, CypherError> {
        let mut nodes = vec![start];
        let mut cur = start;
        for &eid in path {
            let edge = graph.get_edge(eid).map_err(CypherError::from)?;
            let next = match direction {
                Direction::Outgoing => edge.target,
                Direction::Incoming => edge.source,
                Direction::Both => {
                    if edge.source == cur {
                        edge.target
                    } else {
                        edge.source
                    }
                }
            };
            nodes.push(next);
            cur = next;
        }
        Ok(nodes)
    };

    // min-heap by length; ties broken by an increasing counter in the key, so it's deterministic
    let mut candidates: BinaryHeap<Reverse<(usize, u64, Vec<EdgeId>)>> = BinaryHeap::new();
    let mut next_id: u64 = 0;
    let mut seen: FxHashSet<Vec<EdgeId>> = FxHashSet::default();
    seen.insert(accepted[0].clone());

    while accepted.len() < k {
        crate::query::check()?;
        let last = accepted.last().unwrap().clone();
        let last_nodes = nodes_along(&last)?;

        for i in 0..last.len() {
            let spur_node = last_nodes[i];
            let root_path_nodes = &last_nodes[..=i];
            let root_path_edges = &last[..i];

            // edge mask: every edge following the same prefix in an accepted path
            let mut fe: FxHashSet<EdgeId> = FxHashSet::default();
            for p in &accepted {
                if p.len() > i {
                    let p_nodes = nodes_along(p)?;
                    if p_nodes.len() > i && &p_nodes[..=i] == root_path_nodes {
                        fe.insert(p[i]);
                    }
                }
            }

            // no revisiting prefix nodes except the spur
            let mut fn_mask: FxHashSet<NodeId> = FxHashSet::default();
            for &n in &root_path_nodes[..root_path_nodes.len() - 1] {
                fn_mask.insert(n);
            }

            // length budget left for the spur
            let remaining = max_depth.saturating_sub(i);
            if remaining == 0 {
                continue;
            }

            // spur BFS: shortest spur -> target under the constraints; min_depth 0 here, the
            // combined path is re-checked against the outer min_depth
            let spur = match bfs_constrained_unidirectional(
                graph,
                spur_node,
                target,
                direction,
                &type_filter,
                0,
                remaining,
                &fe,
                &fn_mask,
            )? {
                Some(p) => p,
                None => continue,
            };

            // combined = root prefix edges ++ spur
            let mut combined: Vec<EdgeId> = root_path_edges.to_vec();
            combined.extend(&spur);

            // re-check outer min_depth, dedupe
            if combined.len() < min_depth {
                continue;
            }
            if seen.contains(&combined) {
                continue;
            }
            seen.insert(combined.clone());

            let len = combined.len();
            candidates.push(Reverse((len, next_id, combined)));
            next_id += 1;
        }

        // best candidate becomes the next accepted path
        match candidates.pop() {
            Some(Reverse((_, _, path))) => accepted.push(path),
            None => break,
        }
    }

    Ok(accepted)
}

// -- Weighted shortest path (Dijkstra) ---------------------------------

/// f64 with total_cmp ordering, for BinaryHeap.
#[derive(Clone, Copy, PartialEq)]
struct OrdF64(f64);
impl Eq for OrdF64 {}
impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Edge weight as f64.
fn get_edge_weight(
    graph: &dyn GraphView,
    edge_id: EdgeId,
    property: &str,
) -> Result<Option<f64>, CypherError> {
    match graph
        .get_edge_property(edge_id, property)
        .map_err(CypherError::from)?
    {
        Some(PropertyValue::Int(i)) => Ok(Some(i as f64)),
        Some(PropertyValue::Float(f)) => Ok(Some(f)),
        Some(_) => Err(CypherError::type_error(format!(
            "Weight property '{property}' must be numeric"
        ))),
        None => Ok(None), // no weight: skip edge
    }
}

/// Dijkstra single shortest path: `Some((edge_path, total_cost))`, `None` if unreachable.
pub(crate) fn dijkstra_single_shortest(
    graph: &dyn GraphView,
    start: NodeId,
    target: NodeId,
    direction: Direction,
    edge_types: &[String],
    weight_property: &str,
) -> Result<Option<(Vec<EdgeId>, f64)>, CypherError> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    if start == target {
        return Ok(Some((vec![], 0.0)));
    }

    let type_filter = resolve_edge_type_filter(graph, edge_types);

    // min-heap: (cost, node)
    let mut heap: BinaryHeap<Reverse<(OrdF64, NodeId)>> = BinaryHeap::new();
    // best known cost to reach each node
    let mut best_cost: FxHashMap<NodeId, f64> = FxHashMap::default();
    // node -> (edge_id, prev_node), for path reconstruction
    let mut parent: FxHashMap<NodeId, (EdgeId, NodeId)> = FxHashMap::default();

    heap.push(Reverse((OrdF64(0.0), start)));
    best_cost.insert(start, 0.0);

    while let Some(Reverse((OrdF64(cost), node))) = heap.pop() {
        if node == target {
            let mut edges = Vec::new();
            let mut cur = target;
            while let Some(&(eid, prev)) = parent.get(&cur) {
                edges.push(eid);
                cur = prev;
            }
            edges.reverse();
            return Ok(Some((edges, cost)));
        }

        // already found a cheaper path to this node
        if cost > *best_cost.get(&node).unwrap_or(&f64::INFINITY) {
            continue;
        }

        // neighbors via adjacency (no full get_edge)
        for (eid, neighbor) in graph
            .edge_endpoints_filtered(node, direction, &type_filter)
            .map_err(CypherError::from)?
        {
            // targeted weight lookup, no edge clone
            let w = match get_edge_weight(graph, eid, weight_property)? {
                Some(w) => {
                    if w < 0.0 {
                        return Err(CypherError::semantic(format!(
                            "Negative edge weight {w} on property '{weight_property}'"
                        )));
                    }
                    w
                }
                None => continue, // no weight property: skip edge
            };

            let new_cost = cost + w;
            if new_cost < *best_cost.get(&neighbor).unwrap_or(&f64::INFINITY) {
                best_cost.insert(neighbor, new_cost);
                parent.insert(neighbor, (eid, node));
                heap.push(Reverse((OrdF64(new_cost), neighbor)));
            }
        }
    }

    Ok(None) // no path
}

/// Edge-type names -> interned ids; `None` for no filter. A name the graph never interned
/// matches nothing.
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

#[cfg(test)]
mod shortest_k_safety_tests {
    use super::*;
    use std::collections::BTreeMap;
    use traverse_core::graph::Graph;
    use traverse_core::graph::types::{Direction, NodeId};

    // ring of `n` nodes + 2 chords: many loopless paths and cycles
    fn dense(n: usize) -> Graph {
        let mut g = Graph::new();
        let ids: Vec<NodeId> = (0..n)
            .map(|_| g.create_node(&["N"], BTreeMap::new()))
            .collect();
        for i in 0..n {
            g.create_edge(ids[i], ids[(i + 1) % n], "R", BTreeMap::new())
                .unwrap();
            g.create_edge(ids[i], ids[(i + 2) % n], "R", BTreeMap::new())
                .unwrap();
        }
        g
    }

    // unbounded `[*]` means max_depth = u32::MAX; the depth caps (loopless <= nodes, trail <=
    // edges) + enumeration cap must keep these bounded
    #[test]
    fn k_groups_unbounded_terminates() {
        let g = dense(10);
        let paths = bfs_k_groups(
            &g,
            NodeId(0),
            NodeId(5),
            Direction::Outgoing,
            &[],
            1,
            u32::MAX as usize,
            2,
        )
        .unwrap();
        assert!(!paths.is_empty(), "should find paths");
        assert!(paths.len() <= 10_000, "bounded by the enumeration cap");
    }

    #[test]
    fn k_shortest_unbounded_terminates_and_orders() {
        let g = dense(10);
        let paths = bfs_k_shortest(
            &g,
            NodeId(0),
            NodeId(5),
            Direction::Outgoing,
            &[],
            1,
            u32::MAX as usize,
            3,
        )
        .unwrap();
        assert!(!paths.is_empty());
        assert!(paths.len() <= 3);
        for w in paths.windows(2) {
            assert!(w[0].len() <= w[1].len(), "ascending length");
        }
    }

    #[test]
    fn deadline_aborts_k_shortest() {
        let g = dense(10);
        let query = crate::query::QueryContext::new(crate::query::QueryOptions {
            timeout: Some(std::time::Duration::ZERO), // already past
            ..Default::default()
        });
        let scope = query.enter();
        let res = bfs_k_shortest(
            &g,
            NodeId(0),
            NodeId(5),
            Direction::Outgoing,
            &[],
            1,
            u32::MAX as usize,
            100,
        );
        drop(scope);
        assert!(res.is_err(), "must abort at the deadline");
        assert!(
            res.unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("timed out")
        );
    }
}
