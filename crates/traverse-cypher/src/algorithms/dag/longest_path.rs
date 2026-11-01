//! Longest path in a DAG.
//!
//! # Algorithm
//!
//! DP over a topological order:
//!
//! 1. Kahn's topological order; a cycle rejects the projection.
//! 2. `dist[v] = 0` for every active node.
//! 3. For each `u` in order, each out-neighbor `v` with weight `w`:
//!    `dist[v] = max(dist[v], dist[u] + w)`.
//!
//! O(n + m), cheaper than Bellman-Ford or Dijkstra-with-negation on a DAG.
//!
//! # Notes
//!
//! * One linear pass over the order: no priority queue, no atomic f64.
//! * Honors the projection's `WeightConfig`: unweighted = longest chain (hops),
//!   weighted = longest-cost path.
//! * Negative weights are fine; no cycles, so no negative-cycle blow-up.

use std::collections::{BTreeMap, VecDeque};

use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeScalarResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, GraphAlgorithm, OutputKind, Stability,
};

/// Longest path to each node of a DAG, in topological order (`traverse.longestPath.*`). Stateless.
pub struct LongestPath;

impl GraphAlgorithm for LongestPath {
    type Config = ();
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "longestPath",
            display_name: "Longest Path (DAG)",
            category: Category::Dag,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "For each node, the maximum cost of any path ending at it. Computed in \
                 O(n + m) via topological-order DP. Requires a DAG projection.",
            params: Vec::new(),
            output_columns: OutputKind::NodeScalar.default_columns().unwrap(),
            stats_columns: OutputKind::NodeScalar.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeScalar.default_write_columns().unwrap(),
        }
    }

    fn parse_config(&self, _params: &BTreeMap<String, CypherValue>) -> Result<(), AlgorithmError> {
        Ok(())
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        _cfg: &(),
        cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        let n_active = nodes.len();
        if n_active == 0 {
            return Ok(NodeScalarResult::new("score", vec![f64::NAN; cap]));
        }
        cancel.check()?;

        // Kahn's topological order, as in TopologicalSort but keeping the order in a Vec
        let mut in_deg: Vec<u32> = vec![0; cap];
        for &v in &nodes {
            in_deg[v.0 as usize] = graph.in_degree(v);
        }
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        for &v in &nodes {
            if in_deg[v.0 as usize] == 0 {
                queue.push_back(v);
            }
        }
        let mut order: Vec<NodeId> = Vec::with_capacity(n_active);
        while let Some(u) = queue.pop_front() {
            order.push(u);
            if let Some(slice) = graph.outgoing_adj_slice(u) {
                for ep in slice {
                    let vi = ep.neighbor().0 as usize;
                    if vi >= cap || in_deg[vi] == 0 {
                        continue;
                    }
                    in_deg[vi] -= 1;
                    if in_deg[vi] == 0 {
                        queue.push_back(NodeId(vi as u64));
                    }
                }
            } else {
                graph.for_each_out(u, &mut |v, _eid, _w| {
                    let vi = v.0 as usize;
                    if vi >= cap || in_deg[vi] == 0 {
                        return;
                    }
                    in_deg[vi] -= 1;
                    if in_deg[vi] == 0 {
                        queue.push_back(v);
                    }
                });
            }
        }
        if order.len() < n_active {
            return Err(AlgorithmError::Other(format!(
                "longestPath requires a DAG; cycle detected ({} of {} nodes ordered)",
                order.len(),
                n_active,
            )));
        }
        cancel.check()?;

        // DP relax in topological order
        let mut dist: Vec<f64> = vec![0.0; cap];
        // projection members; everything else becomes NaN after the relax
        let mut active_mask: Vec<bool> = vec![false; cap];
        for &v in &nodes {
            active_mask[v.0 as usize] = true;
        }

        let is_unit = graph.is_unit_weighted();
        for &u in &order {
            let du = dist[u.0 as usize];
            match graph.outgoing_adj_slice(u) {
                Some(slice) if is_unit => {
                    for ep in slice {
                        let vi = ep.neighbor().0 as usize;
                        if !active_mask[vi] {
                            continue;
                        }
                        let nd = du + 1.0;
                        if nd > dist[vi] {
                            dist[vi] = nd;
                        }
                    }
                }
                _ => {
                    graph.for_each_out(u, &mut |v, _eid, w| {
                        let vi = v.0 as usize;
                        if !active_mask[vi] {
                            return;
                        }
                        let nd = du + w;
                        if nd > dist[vi] {
                            dist[vi] = nd;
                        }
                    });
                }
            }
        }

        // mask inactive slots
        for i in 0..cap {
            if !active_mask[i] {
                dist[i] = f64::NAN;
            }
        }
        Ok(NodeScalarResult::new("score", dist))
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(LongestPath);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn chain_distances_grow_monotonically() {
        // a -> b -> c -> d, unweighted (cost 1 per edge)
        let mut g = Graph::new();
        let n: Vec<_> = (0..4)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..3 {
            g.create_edge(n[i], n[i + 1], "T", Default::default())
                .unwrap();
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = LongestPath
            .run(&proj, &(), &CancellationToken::none())
            .unwrap();
        for i in 0..4 {
            assert_eq!(r.values[n[i].0 as usize], i as f64);
        }
    }

    #[test]
    fn diamond_picks_longer_branch() {
        // a -> b -> d (cost 2) and a -> d (cost 1)
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let d = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, d, "T", Default::default()).unwrap();
        g.create_edge(a, d, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = LongestPath
            .run(&proj, &(), &CancellationToken::none())
            .unwrap();
        // d takes the via-b path (2), not the direct one (1)
        assert_eq!(r.values[d.0 as usize], 2.0);
    }
}
