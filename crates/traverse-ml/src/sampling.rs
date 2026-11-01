//! Neighbor sampling: the subgraph a batch of nodes' outputs are computed on.
//!
//! A 2-layer GNN's output for a node depends only on its 2-hop neighborhood. [`Sampler::sample`]
//! builds that neighborhood for a batch of seed nodes, with up to `fanouts[0]` neighbors per seed
//! and `fanouts[1]` per neighbor, drawn uniformly without replacement (GraphSAGE, PyG's
//! `NeighborLoader`), plus the message-passing entries between them, pointing towards the seeds.
//!
//! Entry weights come from the full graph's adjacency, scaled by `n / k` when `k` of a node's `n`
//! entries were drawn, so a sampled aggregation is an unbiased estimate of the full one, and exact
//! when everything is taken. A model trained on samples is therefore the model full-graph
//! inference runs: GCN and GraphSAGE read the scaled weights, GAT attends over the drawn
//! neighbors, and R-GCN draws per relationship type (PyG's per-type fanouts). Self-loops are
//! always kept. [`Fanout::All`] at every layer gives a node's exact computation subgraph.

use std::ops::Range;

use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};

use crate::encoder::{GraphInputs, Reads};
use crate::graph::CsrGraph;

/// How many of a node's neighbors one layer draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fanout {
    /// Every neighbor: exact.
    All,
    /// Up to this many, uniformly at random.
    Sample(usize),
}

impl Fanout {
    fn take(self, n: usize) -> usize {
        match self {
            Fanout::All => n,
            Fanout::Sample(k) => k.min(n),
        }
    }
}

/// A sampled subgraph, numbered `0..nodes.len()`.
#[derive(Debug, Clone)]
pub struct Batch {
    /// The adjacency the encoder kind reads (see [`Reads`]); the others have no entries.
    pub inputs: GraphInputs,
    /// Graph node of each subgraph node. The seeds come first, in the order given, duplicates
    /// once.
    pub nodes: Vec<u32>,
    /// Subgraph index of each seed passed to [`Sampler::sample`].
    pub seed_index: Vec<u32>,
}

/// Draws [`Batch`]es from one graph for one encoder kind. Holds a slot per graph node (4 bytes
/// each) so a sample costs time in the size of the batch, not the graph.
pub struct Sampler<'g> {
    graph: &'g GraphInputs,
    reads: Reads,
    fanouts: Vec<Fanout>,
    /// Subgraph index of each graph node during a sample, `u32::MAX` otherwise.
    slot: Vec<u32>,
}

impl<'g> Sampler<'g> {
    /// One fanout per message-passing layer, the seeds' first.
    pub fn new(graph: &'g GraphInputs, reads: Reads, fanouts: &[Fanout]) -> Self {
        Self {
            graph,
            reads,
            fanouts: fanouts.to_vec(),
            slot: vec![u32::MAX; graph.num_nodes()],
        }
    }

    /// The neighborhood of `seeds`. Panics on a seed outside the graph.
    pub fn sample(&mut self, seeds: &[u32], rng: &mut StdRng) -> Batch {
        let mut nodes: Vec<u32> = Vec::new();
        let seed_index: Vec<u32> = seeds
            .iter()
            .map(|&s| include(&mut self.slot, &mut nodes, s))
            .collect();
        let relations = match self.reads {
            Reads::Sym | Reads::Row => 0,
            Reads::Relations => self.graph.num_relations(),
        };
        // (row, col, sym weight, row weight) for `Sym`/`Row`; per relation otherwise
        let mut shared: Vec<(u32, u32, f32, f32)> = Vec::new();
        let mut per_relation: Vec<Vec<(u32, u32, f32)>> = vec![Vec::new(); relations];

        let mut frontier: Range<usize> = 0..nodes.len();
        for &fanout in &self.fanouts {
            let next = nodes.len();
            for sub in frontier {
                let u = nodes[sub] as usize;
                match self.reads {
                    Reads::Sym | Reads::Row => {
                        let (sym, row) = (self.graph.sym.raw(), self.graph.row.raw());
                        for (e, scale) in draw(&self.graph.sym, u, fanout, true, rng) {
                            let v = include(&mut self.slot, &mut nodes, sym.col_idx[e]);
                            shared.push((
                                sub as u32,
                                v,
                                sym.values[e] * scale,
                                row.values[e] * scale,
                            ));
                        }
                    }
                    Reads::Relations => {
                        for (r, g) in self.graph.relations.iter().enumerate() {
                            let raw = g.raw();
                            for (e, scale) in draw(g, u, fanout, false, rng) {
                                let v = include(&mut self.slot, &mut nodes, raw.col_idx[e]);
                                per_relation[r].push((sub as u32, v, raw.values[e] * scale));
                            }
                        }
                    }
                }
            }
            frontier = next..nodes.len();
        }
        for &n in &nodes {
            self.slot[n as usize] = u32::MAX;
        }

        let m = nodes.len();
        let empty = || CsrGraph::from_entries(m, &[]);
        let inputs = match self.reads {
            Reads::Sym | Reads::Row => {
                let (sym, row): (Vec<_>, Vec<_>) = shared
                    .iter()
                    .map(|&(r, c, s, w)| ((r, c, s), (r, c, w)))
                    .unzip();
                GraphInputs {
                    sym: CsrGraph::from_entries(m, &sym),
                    row: CsrGraph::from_entries(m, &row),
                    relations: Vec::new(),
                }
            }
            Reads::Relations => GraphInputs {
                sym: empty(),
                row: empty(),
                relations: per_relation
                    .iter()
                    .map(|entries| CsrGraph::from_entries(m, entries))
                    .collect(),
            },
        };
        Batch {
            inputs,
            nodes,
            seed_index,
        }
    }
}

/// The exact computation subgraph of `seeds` for an encoder `layers` deep that reads `reads`: every
/// neighbor at every layer, so the seeds' outputs on it equal their outputs on the whole graph.
pub fn computation_subgraph(
    graph: &GraphInputs,
    reads: Reads,
    layers: usize,
    seeds: &[u32],
) -> Batch {
    let fanouts = vec![Fanout::All; layers];
    // taking everything draws nothing, so the generator is never consulted
    let mut rng = <StdRng as rand::SeedableRng>::seed_from_u64(0);
    Sampler::new(graph, reads, &fanouts).sample(seeds, &mut rng)
}

/// Subgraph index of graph node `v`, adding it if new.
fn include(slot: &mut [u32], nodes: &mut Vec<u32>, v: u32) -> u32 {
    let s = &mut slot[v as usize];
    if *s == u32::MAX {
        *s = nodes.len() as u32;
        nodes.push(v);
    }
    *s
}

/// Entries of `u`'s row to keep, with the weight scale making the sum unbiased: self-loop entries
/// (when `keep_self`) at scale 1, then up to `fanout` of the other `n` entries at `n / k`.
fn draw(
    g: &CsrGraph,
    u: usize,
    fanout: Fanout,
    keep_self: bool,
    rng: &mut StdRng,
) -> Vec<(usize, f32)> {
    let raw = g.raw();
    let (lo, hi) = (raw.row_ptr[u] as usize, raw.row_ptr[u + 1] as usize);
    let cols = &raw.col_idx[lo..hi];
    // a row is sorted by column, so its self-loop entries are one run
    let (self_lo, self_hi) = if keep_self {
        let a = cols.partition_point(|&c| (c as usize) < u);
        let b = cols.partition_point(|&c| (c as usize) <= u);
        (a, b)
    } else {
        (0, 0)
    };
    let others = cols.len() - (self_hi - self_lo);
    let k = fanout.take(others);
    let mut out: Vec<(usize, f32)> = (self_lo..self_hi).map(|i| (lo + i, 1.0)).collect();
    if k == 0 {
        return out;
    }
    let scale = others as f32 / k as f32;
    let at = |i: usize| {
        lo + if i < self_lo {
            i
        } else {
            i + (self_hi - self_lo)
        }
    };
    if k == others {
        out.extend((0..others).map(|i| (at(i), 1.0)));
    } else {
        let mut picked: Vec<usize> = rand::seq::index::sample(rng, others, k).into_vec();
        // sorted, so the batch's entry order doesn't depend on the draw order
        picked.sort_unstable();
        out.extend(picked.into_iter().map(|i| (at(i), scale)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::AnyEncoder;
    use crate::pipeline::ModelChoice;
    use burn::tensor::Tensor;
    use rand::SeedableRng;

    type B = burn::backend::NdArray;

    /// Two 6-cliques joined by one edge (5-6), alternating relationship types.
    fn graph() -> GraphInputs {
        let mut edges = Vec::new();
        for c in 0..2u32 {
            for i in 0..6 {
                for j in i + 1..6 {
                    edges.push((c * 6 + i, c * 6 + j));
                }
            }
        }
        edges.push((5, 6));
        let rel: Vec<u16> = (0..edges.len()).map(|i| (i % 2) as u16).collect();
        GraphInputs::from_edges(12, &edges, &rel, 2)
    }

    fn rows<const D: usize>(t: Tensor<B, D>) -> Vec<f32> {
        t.into_data().iter::<f32>().collect()
    }

    #[test]
    fn seeds_come_first_and_duplicates_once() {
        let g = graph();
        let mut s = Sampler::new(&g, Reads::Sym, &[Fanout::Sample(2), Fanout::Sample(2)]);
        let b = s.sample(&[3, 9, 3], &mut StdRng::seed_from_u64(1));
        assert_eq!(&b.nodes[..2], &[3, 9]);
        assert_eq!(b.seed_index, vec![0, 1, 0]);
        // every subgraph node once
        let mut sorted = b.nodes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), b.nodes.len());
        // the slots are clean for the next sample
        assert!(s.slot.iter().all(|&x| x == u32::MAX));
    }

    #[test]
    fn fanouts_bound_each_layer() {
        let g = graph();
        let mut s = Sampler::new(&g, Reads::Sym, &[Fanout::Sample(2), Fanout::Sample(1)]);
        let b = s.sample(&[0], &mut StdRng::seed_from_u64(7));
        // the seed's row: its self-loop and 2 of its 5 neighbors
        assert_eq!(b.inputs.sym.degree(0), 3);
        // each new neighbor: self-loop and 1 more
        for sub in 1..3 {
            assert_eq!(b.inputs.sym.degree(sub), 2, "node {}", b.nodes[sub]);
        }
        // 1 seed + 2 + up to 2 more
        assert!(b.nodes.len() <= 5);
        // sym and row share a layout
        assert_eq!(b.inputs.sym.raw().row_ptr, b.inputs.row.raw().row_ptr);
        assert_eq!(b.inputs.sym.raw().col_idx, b.inputs.row.raw().col_idx);
    }

    #[test]
    fn taking_every_neighbor_reproduces_the_full_forward() {
        let g = graph();
        let device = Default::default();
        let feats: Vec<f32> = (0..12 * 3).map(|i| ((i * 7) % 5) as f32 - 2.0).collect();
        let x = Tensor::<B, 1>::from_floats(feats.as_slice(), &device).reshape([12, 3]);
        for choice in [
            ModelChoice::Gcn { hidden: 6 },
            ModelChoice::GraphSage { hidden: 6 },
            ModelChoice::RGcn { hidden: 6 },
            ModelChoice::Gat {
                hidden: 3,
                heads: 2,
            },
        ] {
            let enc = AnyEncoder::<B>::init(&choice, 3, 2, g.num_relations(), &device);
            let full = rows(enc.forward(x.clone(), &g));
            let mut s = Sampler::new(&g, Reads::of(&choice), &[Fanout::All, Fanout::All]);
            let seeds = [5u32, 0, 11];
            let b = s.sample(&seeds, &mut StdRng::seed_from_u64(3));
            let idx: Vec<i32> = b.nodes.iter().map(|&n| n as i32).collect();
            let xb = x
                .clone()
                .select(0, Tensor::from_ints(idx.as_slice(), &device));
            let out = rows(enc.forward(xb, &b.inputs));
            for (j, &seed) in seeds.iter().enumerate() {
                let sub = b.seed_index[j] as usize;
                for c in 0..2 {
                    let (a, e) = (out[sub * 2 + c], full[seed as usize * 2 + c]);
                    assert!((a - e).abs() < 1e-5, "{choice:?} seed {seed}: {a} vs {e}");
                }
            }
        }
    }

    #[test]
    fn sampled_aggregation_is_unbiased() {
        // the mean over many samples of a seed's sampled GCN aggregation matches the exact one
        let g = graph();
        let device = Default::default();
        let feats: Vec<f32> = (0..12).map(|i| i as f32).collect();
        let x = Tensor::<B, 1>::from_floats(feats.as_slice(), &device).reshape([12, 1]);
        let exact = rows(crate::sparse::spmm_csr_sparse(&g.sym, x.clone()))[0];
        let mut s = Sampler::new(&g, Reads::Sym, &[Fanout::Sample(2)]);
        let mut rng = StdRng::seed_from_u64(11);
        let runs = 4000;
        let mut total = 0.0f64;
        for _ in 0..runs {
            let b = s.sample(&[0], &mut rng);
            let idx: Vec<i32> = b.nodes.iter().map(|&n| n as i32).collect();
            let xb = x
                .clone()
                .select(0, Tensor::from_ints(idx.as_slice(), &device));
            total += rows(crate::sparse::spmm_csr_sparse(&b.inputs.sym, xb))[0] as f64;
        }
        let mean = total / runs as f64;
        assert!(
            (mean - exact as f64).abs() < 0.03 * exact.abs() as f64,
            "{mean} vs {exact}"
        );
    }

    #[test]
    fn relations_are_drawn_separately() {
        let g = graph();
        let mut s = Sampler::new(&g, Reads::Relations, &[Fanout::Sample(1)]);
        let b = s.sample(&[0], &mut StdRng::seed_from_u64(5));
        // one neighbor per relationship type, no self-loops, and no shared adjacency
        for r in &b.inputs.relations {
            assert_eq!(r.degree(0), 1);
        }
        assert_eq!(b.inputs.sym.num_edges(), 0);
    }

    #[test]
    fn same_seed_same_sample() {
        let g = graph();
        let mut s = Sampler::new(&g, Reads::Sym, &[Fanout::Sample(2), Fanout::Sample(2)]);
        let a = s.sample(&[0, 7], &mut StdRng::seed_from_u64(9));
        let b = s.sample(&[0, 7], &mut StdRng::seed_from_u64(9));
        assert_eq!(a.nodes, b.nodes);
        assert_eq!(a.inputs.sym.raw().col_idx, b.inputs.sym.raw().col_idx);
    }
}
