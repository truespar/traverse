//! Topological sort: linear order of a DAG via Kahn's algorithm.
//!
//! # Algorithm
//!
//! 1. In-degree per node.
//! 2. Queue every zero-in-degree node.
//! 3. Pop a node, append it to the order, decrement each out-neighbor's
//!    in-degree; enqueue any that reach zero.
//! 4. Fewer than `n_active` nodes in the order means a cycle: error.
//!
//! # Notes
//!
//! * **Cycle detection for free**: a cyclic remnant never reaches zero
//!   in-degree. Comparing order length with `n_active` gives a clear
//!   `AlgorithmError::Other` telling users to project a DAG (e.g. `NATURAL`
//!   orientation over forward-only edges).
//! * **Dense state**: `in_degree: Vec<u32>`, frontier in a `VecDeque<NodeId>`,
//!   no HashMap.
//! * **Deterministic**: neighbors in adjacency-list order, so ties between
//!   zero-in-degree nodes resolve by NodeId.

use std::collections::{BTreeMap, VecDeque};

use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeScalarResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, GraphAlgorithm, OutputKind, Stability,
};

/// Topological order of a DAG by Kahn's algorithm (`traverse.topologicalSort.*`). Stateless.
pub struct TopologicalSort;

impl GraphAlgorithm for TopologicalSort {
    type Config = ();
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "topologicalSort",
            display_name: "Topological Sort",
            category: Category::Dag,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "Linear ordering of a DAG via Kahn's algorithm. Returns each node's \
                 zero-based position; fails if the projection contains a cycle.",
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
            return Ok(NodeScalarResult::new("position", vec![f64::NAN; cap]));
        }
        cancel.check()?;

        // 1. in-degree per active node, within the projection
        let mut in_deg: Vec<u32> = vec![0; cap];
        for &v in &nodes {
            in_deg[v.0 as usize] = graph.in_degree(v);
        }

        // 2. queue the zero-in-degree active nodes
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        for &v in &nodes {
            if in_deg[v.0 as usize] == 0 {
                queue.push_back(v);
            }
        }

        // 3. drain: each pop emits a position, each out-edge decrements a successor's in-degree and
        //    may enqueue it
        let mut position: Vec<f64> = vec![f64::NAN; cap];
        let mut count: u64 = 0;
        while let Some(u) = queue.pop_front() {
            cancel.check()?;
            position[u.0 as usize] = count as f64;
            count += 1;
            if let Some(slice) = graph.outgoing_adj_slice(u) {
                for ep in slice {
                    let vi = ep.neighbor().0 as usize;
                    if vi >= cap {
                        continue;
                    }
                    let d = in_deg[vi];
                    if d == 0 {
                        continue;
                    }
                    let new_d = d - 1;
                    in_deg[vi] = new_d;
                    if new_d == 0 {
                        queue.push_back(NodeId(vi as u64));
                    }
                }
            } else {
                graph.for_each_out(u, &mut |v, _eid, _w| {
                    let vi = v.0 as usize;
                    if vi >= cap {
                        return;
                    }
                    let d = in_deg[vi];
                    if d == 0 {
                        return;
                    }
                    let new_d = d - 1;
                    in_deg[vi] = new_d;
                    if new_d == 0 {
                        queue.push_back(v);
                    }
                });
            }
        }

        // 4. any active node not emitted => cycle
        if (count as usize) < n_active {
            return Err(AlgorithmError::Other(format!(
                "topologicalSort requires a DAG; cycle detected ({} of {} nodes ordered)",
                count, n_active,
            )));
        }
        Ok(NodeScalarResult::new("position", position))
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(TopologicalSort);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn linear_chain_is_ordered() {
        // a -> b -> c -> d
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
        let r = TopologicalSort
            .run(&proj, &(), &CancellationToken::none())
            .unwrap();
        for i in 0..4 {
            assert_eq!(r.values[n[i].0 as usize], i as f64);
        }
    }

    #[test]
    fn cycle_is_rejected() {
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, a, "T", Default::default()).unwrap();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let res = TopologicalSort.run(&proj, &(), &CancellationToken::none());
        match res {
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("cycle"), "unexpected error: {msg}");
            }
            Ok(_) => panic!("expected cycle error"),
        }
    }
}
