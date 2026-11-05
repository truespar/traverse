//! Shared helpers for similarity and topological link prediction.
//!
//! Everything here works on **undirected neighbor sets** built once from the
//! projection: a sorted `Vec<u32>` of node ids per active node, without
//! self-loops and multi-edges.
//!
//! # Pair enumeration
//!
//! The natural candidates are the two-hop pairs, (u, v) sharing at least one
//! neighbor. The **common-neighbor walk** enumerates them: for each node `w`,
//! every pair (u, v) subset of N(w) is a candidate and `w` adds to its intersection.
//! Cost sum_w |N(w)|^2 instead of n^2 over all pairs; the standard approach for
//! Jaccard NodeSimilarity.

use super::super::projection::AlgorithmGraph;

/// Sorted, deduped undirected neighbor sets for every active node, indexed by `NodeId.0 as usize`;
/// inactive slots are empty. Parallel per-row build with a slice fast path; every similarity
/// algorithm (Jaccard, cosine, kNN, ...) sits on this.
pub(super) fn neighbor_sets(graph: &dyn AlgorithmGraph) -> Vec<Vec<u32>> {
    neighbor_sets_oriented(graph, crate::algorithms::config::Orientation::Undirected)
}

/// Orientation-aware neighbor sets: NATURAL = out-neighbors, REVERSE = in-neighbors, UNDIRECTED =
/// out union in. GDS's nodeSimilarity uses the projection's orientation (NATURAL unless stated), which
/// the gds.* adapter passes through.
pub(super) fn neighbor_sets_oriented(
    graph: &dyn AlgorithmGraph,
    orientation: crate::algorithms::config::Orientation,
) -> Vec<Vec<u32>> {
    use rayon::prelude::*;
    let cap = graph.highest_node_id() as usize;
    let nodes = graph.active_nodes();
    let mut adj: Vec<Vec<u32>> = vec![Vec::new(); cap];
    let rows: Vec<(usize, Vec<u32>)> = nodes
        .par_iter()
        .map(|&v| {
            use crate::algorithms::config::Orientation;
            let (want_out, want_in) = match orientation {
                Orientation::Natural => (true, false),
                Orientation::Reverse => (false, true),
                Orientation::Undirected => (true, true),
            };
            let mut list: Vec<u32> = Vec::with_capacity(32);
            match (graph.outgoing_adj_slice(v), graph.incoming_adj_slice(v)) {
                (Some(out), Some(inc)) => {
                    if want_out {
                        for ep in out {
                            let u = ep.neighbor().0;
                            if u != v.0 {
                                list.push(u as u32);
                            }
                        }
                    }
                    if want_in {
                        for ep in inc {
                            let u = ep.neighbor().0;
                            if u != v.0 {
                                list.push(u as u32);
                            }
                        }
                    }
                }
                _ => {
                    if want_out {
                        graph.for_each_out(v, &mut |u, _eid, _w| {
                            if u != v {
                                list.push(u.0 as u32);
                            }
                        });
                    }
                    // an undirected projection lists both directions as out already
                    if want_in && !(want_out && graph.is_undirected()) {
                        graph.for_each_in(v, &mut |u, _eid, _w| {
                            if u != v {
                                list.push(u.0 as u32);
                            }
                        });
                    }
                }
            }
            list.sort_unstable();
            list.dedup();
            (v.0 as usize, list)
        })
        .collect();
    for (idx, list) in rows {
        adj[idx] = list;
    }
    adj
}

/// Sorted intersection size, O(|a| + |b|).
pub(super) fn intersect_size(a: &[u32], b: &[u32]) -> u32 {
    let (mut i, mut j, mut k) = (0usize, 0usize, 0u32);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                k += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    k
}

/// Calls `f(w_idx)` once per shared neighbor `w` of the pair (u, v), for the "sum over common
/// neighbors" LP scores. `f` gets only `w`; the caller knows u and v.
pub(super) fn for_each_common_neighbor(a: &[u32], b: &[u32], mut f: impl FnMut(u32)) {
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                f(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
}
