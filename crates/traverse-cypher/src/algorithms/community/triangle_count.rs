//! Triangle counting + local clustering coefficient (LCC).
//!
//! # Algorithm
//!
//! Schank/Wagner forward algorithm with NodeId ordering:
//!
//! 1. sorted (by NodeId) undirected adjacency per active node (each edge in
//!    both endpoints' lists);
//! 2. per edge (u, v) with u < v, intersect the two lists counting common
//!    w > v: each yields triangle {u, v, w} exactly once;
//! 3. per triangle, increment `tri_count[u]`, `tri_count[v]`, `tri_count[w]`;
//! 4. LCC(v) = `tri_count[v] / (deg(v) * (deg(v) - 1) / 2)` if `deg(v) >= 2`,
//!    else 0.
//!
//! # Complexity
//!
//! Worst case O(m^{1.5}). NodeId-ordering pruning is several times faster than
//! the naive variant on power-law graphs (few shared higher-ranked neighbors
//! per edge).
//!
//! # Output
//!
//! `OutputKind::NodeMulti`: `triangleCount` and `localClusteringCoefficient` per node.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeMultiResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, GraphAlgorithm, OutputKind, Stability,
};

/// Per-node triangle count and local clustering coefficient (`traverse.triangleCount.*`).
/// Stateless.
pub struct TriangleCount;

impl GraphAlgorithm for TriangleCount {
    type Config = ();
    type Output = NodeMultiResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "triangleCount",
            display_name: "Triangle Count + LCC",
            category: Category::Community,
            output_kind: OutputKind::NodeMulti,
            stability: Stability::Beta,
            description: "Per-node triangle count and local clustering coefficient via the \
                 forward algorithm (Schank/Wagner). Treats the projection as undirected.",
            params: Vec::new(),
            output_columns: &["nodeId", "triangleCount", "localClusteringCoefficient"],
            // NodeMulti's stats() exposes nodeCount + fieldCount
            stats_columns: OutputKind::NodeMulti.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeMulti.default_write_columns().unwrap(),
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
    ) -> Result<NodeMultiResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();

        // step 1: sorted, deduped undirected neighbors per active node, in parallel (rows
        // independent); push raw ids then sort+dedup (no hashing, contiguous, vectorized
        // sort_unstable)
        let mut sorted_adj: Vec<Vec<u64>> = vec![Vec::new(); cap];
        let rows: Vec<(usize, Vec<u64>)> = nodes
            .par_iter()
            .map(|&v| {
                let mut adj: Vec<u64> = Vec::with_capacity(32);
                match (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
                    (Some(out), Some(inc)) => {
                        for ep in out {
                            let u = ep.neighbor().0;
                            if u != v.0 {
                                adj.push(u);
                            }
                        }
                        for ep in inc {
                            let u = ep.neighbor().0;
                            if u != v.0 {
                                adj.push(u);
                            }
                        }
                    }
                    _ => {
                        graph.for_each_out(v, &mut |u, _eid, _w| {
                            if u != v {
                                adj.push(u.0);
                            }
                        });
                        // an undirected projection lists both directions as out already
                        if !graph.is_undirected() {
                            graph.for_each_in(v, &mut |u, _eid, _w| {
                                if u != v {
                                    adj.push(u.0);
                                }
                            });
                        }
                    }
                }
                adj.sort_unstable();
                adj.dedup();
                (v.0 as usize, adj)
            })
            .collect();
        for (idx, adj) in rows {
            sorted_adj[idx] = adj;
        }

        cancel.check()?;

        // step 2: per-node triangle counters (atomic for the parallel sweep)
        let tri_count: Vec<AtomicU64> = (0..cap).map(|_| AtomicU64::new(0)).collect();

        // step 3: parallel forward enumeration: for u, neighbors v > u, intersect adj[u] with
        // adj[v] keeping w > v
        nodes.par_iter().for_each(|&u_node| {
            let u = u_node.0;
            let adj_u = &sorted_adj[u as usize];
            for &v in adj_u {
                if v <= u {
                    continue;
                }
                let adj_v = &sorted_adj[v as usize];
                // single-pass intersection, counting and incrementing per w without building the
                // common Vec (hubs with huge neighbor lists made that allocation costly)
                let mut k: u64 = 0;
                sorted_intersect_above_visit(adj_u, adj_v, v, |w| {
                    k += 1;
                    tri_count[w as usize].fetch_add(1, Ordering::Relaxed);
                });
                if k > 0 {
                    tri_count[u as usize].fetch_add(k, Ordering::Relaxed);
                    tri_count[v as usize].fetch_add(k, Ordering::Relaxed);
                }
            }
        });

        cancel.check()?;

        // step 4: NodeMulti result, two fields per node
        let fields: Vec<&'static str> = vec!["triangleCount", "localClusteringCoefficient"];
        let mut values: Vec<Vec<f64>> = vec![vec![f64::NAN, f64::NAN]; cap];
        for &n in &nodes {
            let idx = n.0 as usize;
            let tri = tri_count[idx].load(Ordering::Relaxed);
            let deg = sorted_adj[idx].len() as u64;
            let lcc = if deg >= 2 {
                let pairs = (deg * (deg - 1)) as f64 / 2.0;
                tri as f64 / pairs
            } else {
                0.0
            };
            values[idx] = vec![tri as f64, lcc];
        }

        Ok(NodeMultiResult::new(fields, values))
    }
}

/// Calls `f(w)` for each member of `a intersect b` above `threshold`, without materializing the
/// intersection (the triangle loop only increments per-w counters).
#[inline]
fn sorted_intersect_above_visit(a: &[u64], b: &[u64], threshold: u64, mut f: impl FnMut(u64)) {
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        let ai = a[i];
        let bj = b[j];
        if ai <= threshold {
            i += 1;
            continue;
        }
        if bj <= threshold {
            j += 1;
            continue;
        }
        match ai.cmp(&bj) {
            std::cmp::Ordering::Equal => {
                f(ai);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(TriangleCount);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    fn run_tc(g: &Graph) -> NodeMultiResult {
        let view: &(dyn GraphView + Sync) = g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        TriangleCount
            .run(&proj, &(), &CancellationToken::none())
            .unwrap()
    }

    #[test]
    fn triangle_is_three_triangles_per_node() {
        // K_3: every node in 1 triangle
        let mut g = Graph::new();
        let a = g.create_node(&["N"], Default::default());
        let b = g.create_node(&["N"], Default::default());
        let c = g.create_node(&["N"], Default::default());
        g.create_edge(a, b, "T", Default::default()).unwrap();
        g.create_edge(b, c, "T", Default::default()).unwrap();
        g.create_edge(c, a, "T", Default::default()).unwrap();
        let r = run_tc(&g);
        for n in [a, b, c] {
            let row = &r.values[n.0 as usize];
            assert_eq!(row[0], 1.0, "{n:?} triangle count");
            assert_eq!(row[1], 1.0, "{n:?} LCC (K_3 is fully connected -> 1.0)");
        }
    }

    #[test]
    fn star_has_no_triangles() {
        // star: leaves have deg 1 -> LCC 0; the center has degree n but no leaf-leaf edges -> 0
        // triangles -> LCC 0
        let mut g = Graph::new();
        let center = g.create_node(&["N"], Default::default());
        let leaves: Vec<_> = (0..5)
            .map(|_| {
                let l = g.create_node(&["N"], Default::default());
                g.create_edge(center, l, "T", Default::default()).unwrap();
                l
            })
            .collect();
        let r = run_tc(&g);
        assert_eq!(r.values[center.0 as usize][0], 0.0);
        assert_eq!(r.values[center.0 as usize][1], 0.0);
        for l in leaves {
            assert_eq!(r.values[l.0 as usize][0], 0.0);
            assert_eq!(r.values[l.0 as usize][1], 0.0);
        }
    }

    #[test]
    fn k4_has_four_triangles_total_per_node_three() {
        // K_4: C(4,3) = 4 triangles, each vertex in C(3,2) = 3, LCC = 3 / C(3,2) = 1.0
        let mut g = Graph::new();
        let n: Vec<_> = (0..4)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..4 {
            for j in (i + 1)..4 {
                g.create_edge(n[i], n[j], "T", Default::default()).unwrap();
            }
        }
        let r = run_tc(&g);
        for ni in &n {
            let row = &r.values[ni.0 as usize];
            assert_eq!(row[0], 3.0, "every K_4 vertex is in 3 triangles");
            assert!((row[1] - 1.0).abs() < 1e-12, "K_4 LCC must be 1.0");
        }
    }

    #[test]
    fn path_has_no_triangles_lcc_zero() {
        // a -> b -> c -> d -> e
        let mut g = Graph::new();
        let n: Vec<_> = (0..5)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..4 {
            g.create_edge(n[i], n[i + 1], "T", Default::default())
                .unwrap();
        }
        let r = run_tc(&g);
        for ni in &n {
            assert_eq!(r.values[ni.0 as usize][0], 0.0);
            assert_eq!(r.values[ni.0 as usize][1], 0.0);
        }
    }
}
