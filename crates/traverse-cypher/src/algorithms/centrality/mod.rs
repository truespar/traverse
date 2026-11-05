//! Centrality: Degree, PageRank, ArticleRank, Eigenvector, Betweenness, Closeness, Harmonic, HITS,
//! CELF.

pub mod article_rank;
pub mod betweenness;
pub mod celf;
pub mod closeness;
pub mod degree;
pub mod eigenvector;
pub mod harmonic;
pub mod hits;
pub mod pagerank;

use traverse_core::graph::adjacency::AdjEntry;
use traverse_core::graph::types::NodeId;

use super::projection::{AlgorithmGraph, DenseAdjacency};

/// The pull step of the rank iterations (PageRank, ArticleRank): sums per-node values over each
/// node's in-neighbors (as `for_each_in` visits them), by the cheapest read the projection offers:
/// its dense CSR arrays, then the packed adjacency, then the callback.
pub(super) struct InNeighbors<'a> {
    graph: &'a dyn AlgorithmGraph,
    /// The dense arrays, every id in them checked below `len` once, here.
    dense: Option<(DenseAdjacency<'a>, Option<DenseAdjacency<'a>>)>,
    len: usize,
}

impl<'a> InNeighbors<'a> {
    /// For sums over value slices of `len` entries (one per `0..highest_node_id()`).
    pub(super) fn new(graph: &'a dyn AlgorithmGraph, len: usize) -> Self {
        let in_range = |a: &DenseAdjacency<'_>| a.targets.iter().all(|&t| (t as usize) < len);
        let dense = graph
            .in_adjacency()
            .filter(|(first, second)| in_range(first) && second.as_ref().is_none_or(in_range));
        Self { graph, dense, len }
    }

    /// `f(i, sum)` for each active node `i` in `range`, `sum` being `values[u]` summed over its
    /// in-neighbors `u`. One loop per read, so the dense one carries nothing of the others.
    #[inline]
    pub(super) fn sums(
        &self,
        values: &[f64],
        active: &[bool],
        range: std::ops::Range<usize>,
        mut f: impl FnMut(usize, f64),
    ) {
        let active = &active[range.clone()];
        let nodes = range.zip(active).filter(|&(_, &a)| a).map(|(i, _)| i);
        match &self.dense {
            Some((first, second)) => {
                assert!(values.len() >= self.len);
                let row_sum = |adj: &DenseAdjacency<'_>, i: usize| {
                    // SAFETY: `new` checked every id in both adjacencies below `self.len`, and
                    // `values` holds at least that many entries
                    unsafe { sum_at(values, adj.neighbors(i)) }
                };
                match second {
                    None => nodes.for_each(|i| f(i, row_sum(first, i))),
                    Some(second) => {
                        nodes.for_each(|i| f(i, row_sum(first, i) + row_sum(second, i)))
                    }
                }
            }
            None => nodes.for_each(|i| {
                let v = NodeId(i as u64);
                let acc = match self.graph.incoming_adj_slice(v) {
                    Some(slice) => sum_entries(values, slice),
                    None => {
                        let mut acc = 0.0;
                        self.graph
                            .for_each_in(v, &mut |u, _, _| acc += values[u.0 as usize]);
                        acc
                    }
                };
                f(i, acc)
            }),
        }
    }
}

/// `values[u]` summed over the neighbors of a packed adjacency slice, into four accumulators (see
/// [`sum_at`]).
#[inline]
fn sum_entries(values: &[f64], slice: &[AdjEntry]) -> f64 {
    let mut acc = [0.0f64; 4];
    let (blocks, rest) = slice.as_chunks::<4>();
    for b in blocks {
        for k in 0..4 {
            acc[k] += values[b[k].neighbor().0 as usize];
        }
    }
    for (k, ep) in rest.iter().enumerate() {
        acc[k] += values[ep.neighbor().0 as usize];
    }
    (acc[0] + acc[1]) + (acc[2] + acc[3])
}

/// Nodes per parallel task of a rank step: a few tasks per pool thread, and enough nodes that a
/// task outweighs its scheduling.
pub(super) fn node_chunk(cap: usize) -> usize {
    cap.div_ceil(rayon::current_num_threads() * 8).max(256)
}

/// `values[i]` summed over `ids`, into eight accumulators: one chain of dependent adds would wait
/// out each add's latency, and eight keep both add units busy. Reads without bounds checks, which
/// would double the instructions per relationship.
///
/// # Safety
///
/// Every id in `ids` is below `values.len()`.
#[inline(always)]
unsafe fn sum_at(values: &[f64], ids: &[u32]) -> f64 {
    // SAFETY: the caller's contract
    let at = |i: u32| unsafe { *values.get_unchecked(i as usize) };
    let mut acc = [0.0f64; 8];
    let (blocks, rest) = ids.as_chunks::<8>();
    for b in blocks {
        for k in 0..8 {
            acc[k] += at(b[k]);
        }
    }
    for (k, &i) in rest.iter().enumerate() {
        acc[k] += at(i);
    }
    ((acc[0] + acc[1]) + (acc[2] + acc[3])) + ((acc[4] + acc[5]) + (acc[6] + acc[7]))
}

/// Registers every centrality algorithm in the current thread's registry.
pub fn register() {
    degree::register();
    pagerank::register();
    article_rank::register();
    eigenvector::register();
    hits::register();
    closeness::register();
    harmonic::register();
    betweenness::register();
    celf::register();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::config::{BaseConfig, Orientation};
    use crate::algorithms::materialized::MaterializedGraph;
    use crate::algorithms::projection::AlgorithmGraphView;
    use traverse_core::graph::Graph;
    use traverse_core::tx::view::GraphView;

    /// Rows of every length around the eight-wide blocks (0..=19 in-neighbors), a self-loop and a
    /// parallel pair.
    fn irregular_graph() -> Graph {
        let mut g = Graph::new();
        let n: Vec<_> = (0..24)
            .map(|_| g.create_node(&["N"], Default::default()))
            .collect();
        for (v, &target) in n.iter().enumerate().take(20) {
            for u in 0..v {
                g.create_edge(n[(u * 7 + v) % 24], target, "T", Default::default())
                    .unwrap();
            }
        }
        g.create_edge(n[21], n[21], "T", Default::default())
            .unwrap();
        g.create_edge(n[22], n[23], "T", Default::default())
            .unwrap();
        g.create_edge(n[22], n[23], "T", Default::default())
            .unwrap();
        g
    }

    /// `InNeighbors::sums` against a plain `for_each_in` sum, node by node.
    fn check_sums(graph: &dyn AlgorithmGraph) {
        let cap = graph.highest_node_id() as usize;
        let values: Vec<f64> = (0..cap).map(|i| 1.0 + i as f64 * 0.37).collect();
        let mut active = vec![false; cap];
        for n in graph.active_nodes() {
            active[n.0 as usize] = true;
        }
        let mut got = vec![None; cap];
        InNeighbors::new(graph, cap).sums(&values, &active, 0..cap, |i, s| got[i] = Some(s));
        for n in graph.active_nodes() {
            let mut want = 0.0;
            graph.for_each_in(n, &mut |u, _, _| want += values[u.0 as usize]);
            let got = got[n.0 as usize].expect("every active node summed");
            assert!((got - want).abs() < 1e-9, "node {}: {got} != {want}", n.0);
        }
    }

    #[test]
    fn in_neighbor_sums_match_for_each_in() {
        let g = irregular_graph();
        let view: &(dyn GraphView + Sync) = &g;
        for orientation in [Orientation::Natural, Orientation::Undirected] {
            let live = AlgorithmGraphView::new(
                view,
                &BaseConfig {
                    orientation,
                    ..Default::default()
                },
            )
            .unwrap();
            check_sums(&live);
        }
        for undirected in [false, true] {
            let mat = MaterializedGraph::build(view, &[], &[], None, undirected).unwrap();
            assert!(mat.in_adjacency().is_some());
            check_sums(&mat);
        }
    }
}
