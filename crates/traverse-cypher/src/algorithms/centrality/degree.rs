//! Degree centrality.
//!
//! One `f64` per node: incident edges in the projection's
//! [`Orientation`](super::super::Orientation). The simplest algorithm; it also
//! exercises the whole chain (projection -> kernel -> procedure adapter ->
//! property write).
//!
//! * `NATURAL`    -> out-degree
//! * `REVERSE`    -> in-degree
//! * `UNDIRECTED` -> out + in (self-loops counted once via the projection's dedup)
//!
//! O(N): `out_degree` / `in_degree` are O(1) after the projection's degree pass
//! (one walk of the packed adjacency per node, parallel across nodes).

use std::collections::BTreeMap;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeScalarResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, GraphAlgorithm, OutputKind, Stability,
};

/// Degree centrality (`traverse.degree.*`). Stateless.
pub struct Degree;

impl GraphAlgorithm for Degree {
    type Config = ();
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "degree",
            display_name: "Degree Centrality",
            category: Category::Centrality,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Beta,
            description: "Count of incident edges per node in the projection's orientation.",
            params: Vec::new(),
            output_columns: OutputKind::NodeScalar.default_columns().unwrap(),
            stats_columns: OutputKind::NodeScalar.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeScalar.default_write_columns().unwrap(),
        }
    }

    fn parse_config(&self, _params: &BTreeMap<String, CypherValue>) -> Result<(), AlgorithmError> {
        // nothing beyond the base config (orientation, label filter, type filter)
        Ok(())
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        _cfg: &(),
        cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        use rayon::prelude::*;
        let cap = graph.highest_node_id() as usize;
        cancel.check()?;
        // active mask up front so par_chunks_mut over the result skips inactive slots in O(1)
        let nodes = graph.active_nodes();
        let mut active_mask = vec![false; cap];
        for &n in &nodes {
            active_mask[n.0 as usize] = true;
        }
        let active_mask_ref: &[bool] = &active_mask;
        let mut values = vec![f64::NAN; cap];
        values
            .par_chunks_mut(1)
            .enumerate()
            .for_each(|(idx, slot)| {
                if !active_mask_ref[idx] {
                    return; // already NaN
                }
                let v = traverse_core::graph::types::NodeId(idx as u64);
                slot[0] = graph.out_degree(v) as f64;
            });
        Ok(NodeScalarResult::new("score", values))
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(Degree);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use crate::algorithms::{BaseConfig, Orientation};
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn ring_of_three_each_has_degree_one() {
        // a -> b, b -> c, c -> a: every out-degree is 1
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(c, a, "T", Default::default()).unwrap();

        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = Degree.run(&proj, &(), &CancellationToken::none()).unwrap();
        assert_eq!(r.values[a.0 as usize] as i64, 1);
        assert_eq!(r.values[b.0 as usize] as i64, 1);
        assert_eq!(r.values[c.0 as usize] as i64, 1);
    }

    #[test]
    fn star_center_has_high_degree_leaves_have_low() {
        // center -> 5 leaves
        let mut g = Graph::new();
        let center = g.create_node(&["N"], Default::default());
        let mut leaves = vec![];
        for _ in 0..5 {
            let l = g.create_node(&["N"], Default::default());
            g.create_edge(center, l, "T", Default::default()).unwrap();
            leaves.push(l);
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = Degree.run(&proj, &(), &CancellationToken::none()).unwrap();
        assert_eq!(r.values[center.0 as usize] as i64, 5);
        for l in leaves {
            assert_eq!(r.values[l.0 as usize] as i64, 0);
        }
    }

    #[test]
    fn reverse_orientation_swaps_in_for_out() {
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        let cfg = BaseConfig {
            orientation: Orientation::Reverse,
            ..Default::default()
        };
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::new(view, &cfg).unwrap();
        let r = Degree.run(&proj, &(), &CancellationToken::none()).unwrap();
        // reversed: a has 0 (no incoming), b has 1
        assert_eq!(r.values[a.0 as usize] as i64, 0);
        assert_eq!(r.values[b.0 as usize] as i64, 1);
    }
}
