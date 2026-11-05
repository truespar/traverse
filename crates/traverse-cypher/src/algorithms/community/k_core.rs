//! K-core decomposition (Batagelj & Zaversnik, 2003).
//!
//! # Definition
//!
//! A node's *coreness* is the largest k such that it belongs to a subgraph where
//! every node has degree >= k; nodes with coreness >= k form the k-core.
//!
//! # Algorithm
//!
//! Bin-sort peeling, O(n + m):
//!
//! 1. projected degree of each node;
//! 2. bucket-sort nodes into degree bins;
//! 3. peel: pop the lowest-degree node, its coreness is its current degree, then
//!    decrement each neighbor's degree (moving it one bin down).
//!
//! # Notes
//!
//! * O(n + m) worst case, unlike repeatedly removing all degree-<=k nodes.
//! * **No HashMap**: bins are dense `Vec<Vec<NodeId>>`; `pos[node]` gives O(1)
//!   removal from the middle of a bin (swap-remove).
//! * **Single-threaded**: each removal changes the degrees, so peeling is
//!   sequential; O(n + m) keeps it fast anyway.
//! * Treats the projection as **undirected** (incoming + outgoing neighbors).

use std::collections::BTreeMap;

use traverse_core::graph::types::NodeId;

use crate::types::CypherValue;

use super::super::cancellation::CancellationToken;
use super::super::projection::AlgorithmGraph;
use super::super::result::NodeCommunityResult;
use super::super::{
    AlgorithmError, AlgorithmMetadata, Category, GraphAlgorithm, OutputKind, Stability,
};

/// K-core decomposition: each node's coreness, the largest `k` such that it belongs to a subgraph
/// where every node has degree >= `k` (`traverse.kCore.*`). Stateless.
pub struct KCore;

impl GraphAlgorithm for KCore {
    type Config = ();
    type Output = NodeCommunityResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "kCore",
            display_name: "K-Core Decomposition",
            category: Category::Community,
            output_kind: OutputKind::NodeCommunity,
            stability: Stability::Beta,
            description: "Per-node coreness via Batagelj-Zaversnik bucket-sort peeling. \
                 Treats the projection as undirected.",
            params: Vec::new(),
            output_columns: OutputKind::NodeCommunity.default_columns().unwrap(),
            stats_columns: OutputKind::NodeCommunity.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeCommunity.default_write_columns().unwrap(),
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
    ) -> Result<NodeCommunityResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let nodes = graph.active_nodes();
        if nodes.is_empty() {
            return Ok(NodeCommunityResult::new("coreness", vec![u64::MAX; cap]));
        }
        cancel.check()?;

        // step 1: per-node adjacency in parallel (out + in neighbors; slice fast path
        // when unfiltered).
        //
        // Mutual edges are NOT deduplicated: Neo4j GDS's kCore degree is
        // `out_degree + in_degree`, so a mutual edge counts 2 (a mutual K_5, all pairs
        // both ways, has coreness 8 = 2 x (k-1) in GDS). UNDIRECTED projections already
        // count mutuals twice in `out_degree`, so raw counts match both GDS and
        // `traverse.degree.stream({UNDIRECTED})`. Graphs without mutual edges are
        // unaffected. Self-loops are excluded: they never add to coreness.
        use rayon::prelude::*;
        let mut adj: Vec<Vec<u32>> = vec![Vec::new(); cap];
        let rows: Vec<(usize, Vec<u32>)> = nodes
            .par_iter()
            .map(|&v| {
                let vp = v.0;
                let mut list: Vec<u32> = Vec::with_capacity(32);
                match (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
                    (Some(out), Some(inc)) => {
                        for ep in out {
                            let n = ep.neighbor().0;
                            if n != vp {
                                list.push(n as u32);
                            }
                        }
                        for ep in inc {
                            let n = ep.neighbor().0;
                            if n != vp {
                                list.push(n as u32);
                            }
                        }
                    }
                    _ => {
                        graph.for_each_out(v, &mut |u, _eid, _w| {
                            if u.0 != vp {
                                list.push(u.0 as u32);
                            }
                        });
                        // an undirected projection lists both directions as out already
                        if !graph.is_undirected() {
                            graph.for_each_in(v, &mut |u, _eid, _w| {
                                if u.0 != vp {
                                    list.push(u.0 as u32);
                                }
                            });
                        }
                    }
                }
                (v.0 as usize, list)
            })
            .collect();
        let mut degree: Vec<u32> = vec![0; cap];
        for (idx, list) in rows {
            degree[idx] = list.len() as u32;
            adj[idx] = list;
        }
        cancel.check()?;

        // step 2: bucket-sort nodes by degree
        let max_deg = degree.iter().copied().max().unwrap_or(0) as usize;
        let mut bins: Vec<Vec<NodeId>> = vec![Vec::new(); max_deg + 1];
        let mut pos: Vec<u32> = vec![u32::MAX; cap]; // position in bin
        let mut bin_of: Vec<u32> = vec![u32::MAX; cap]; // current bin
        for &v in &nodes {
            let d = degree[v.0 as usize] as usize;
            pos[v.0 as usize] = bins[d].len() as u32;
            bin_of[v.0 as usize] = d as u32;
            bins[d].push(v);
        }

        // step 3: peel in degree order; the popped node's coreness is its current degree
        let mut coreness: Vec<u64> = vec![u64::MAX; cap];
        for d in 0..=max_deg {
            // bins[d] grows as neighbors of removed nodes drop into it, so iterate by index
            let mut i = 0;
            while i < bins[d].len() {
                let v = bins[d][i];
                i += 1;
                let vi = v.0 as usize;
                coreness[vi] = d as u64;
                // v leaves re-bucketing for good
                bin_of[vi] = u32::MAX;
                // take v's neighbor list so `bins` can be borrowed mutably meanwhile
                let neigh = std::mem::take(&mut adj[vi]);
                for &u_raw in &neigh {
                    let ui = u_raw as usize;
                    // skip neighbors already peeled
                    if bin_of[ui] == u32::MAX {
                        continue;
                    }
                    let old_bin = bin_of[ui] as usize;
                    if old_bin <= d {
                        // at or below the current peel level: coreness is set on its turn
                        continue;
                    }
                    // O(1) swap-remove of u from `old_bin`, fixing `pos` of the swapped node
                    let u_pos = pos[ui] as usize;
                    let last = bins[old_bin].len() - 1;
                    if u_pos != last {
                        let swapped = bins[old_bin][last];
                        bins[old_bin][u_pos] = swapped;
                        pos[swapped.0 as usize] = u_pos as u32;
                    }
                    bins[old_bin].pop();
                    // insert u into `old_bin - 1`
                    let new_bin = old_bin - 1;
                    pos[ui] = bins[new_bin].len() as u32;
                    bin_of[ui] = new_bin as u32;
                    bins[new_bin].push(NodeId(u_raw as u64));
                }
            }
            cancel.check()?;
        }

        Ok(NodeCommunityResult::new("coreness", coreness))
    }
}

/// Adds this algorithm to the current thread's registry.
pub fn register() {
    super::super::procedure::register(KCore);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    #[test]
    fn k4_has_coreness_three() {
        // K_4: every node has degree 3, all in the 3-core
        let mut g = Graph::new();
        let n: Vec<_> = (0..4)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..4 {
            for j in (i + 1)..4 {
                g.create_edge(n[i], n[j], "T", Default::default()).unwrap();
            }
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = KCore.run(&proj, &(), &CancellationToken::none()).unwrap();
        for ni in &n {
            assert_eq!(r.values[ni.0 as usize], 3);
        }
    }

    #[test]
    fn mutual_edges_double_coreness_matching_neo4j() {
        // K_4 with MUTUAL edges (a->b AND b->a per pair): 3 mutual neighbors give effective degree 6,
        // and GDS reports coreness 6, not 3
        let mut g = Graph::new();
        let n: Vec<_> = (0..4)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for i in 0..4 {
            for j in 0..4 {
                if i != j {
                    g.create_edge(n[i], n[j], "T", Default::default()).unwrap();
                }
            }
        }
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = KCore.run(&proj, &(), &CancellationToken::none()).unwrap();
        for ni in &n {
            // coreness 6 = 6 incident edges (3 mutual pairs x 2), the gds.kcore convention
            assert_eq!(
                r.values[ni.0 as usize], 6,
                "expected coreness 6 with mutual edges (Neo4j-compat),  \
                 got {} for node {:?}",
                r.values[ni.0 as usize], ni
            );
        }
    }

    #[test]
    fn star_leaves_have_coreness_one_center_one() {
        // star: center + 5 leaves; coreness 1 everywhere (the graph is its 1-core, the 2-core is
        // empty)
        let mut g = Graph::new();
        let center = g.create_node(&["N"], Default::default());
        let leaves: Vec<_> = (0..5)
            .map(|_| {
                let l = g.create_node(&["N"], Default::default());
                g.create_edge(center, l, "T", Default::default()).unwrap();
                l
            })
            .collect();
        let view: &(dyn GraphView + Sync) = &g;
        let proj = AlgorithmGraphView::open(view).unwrap();
        let r = KCore.run(&proj, &(), &CancellationToken::none()).unwrap();
        assert_eq!(r.values[center.0 as usize], 1);
        for l in leaves {
            assert_eq!(r.values[l.0 as usize], 1);
        }
    }
}
