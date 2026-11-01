//! One interface over the GNN architectures in [`crate::gnn`], so training, prediction and the
//! explainers treat every model kind the same way.
//!
//! * [`GraphInputs`]: the adjacencies the encoders propagate over, built once per graph.
//! * [`AnyEncoder`]: a Burn module enum over GCN, GraphSAGE, R-GCN and multi-head GAT, built from
//!   a [`ModelChoice`]. Weights serialize as the *inner* model's record, so models saved before
//!   this enum existed still load.
//! * [`InputBlock`]: a range of rows of the adjacency an encoder reads, on a device.
//!   [`AnyEncoder::forward_blocked`] computes every node's output a block at a time, so inference
//!   on a graph of any size stays within a memory budget and gives exactly what
//!   [`AnyEncoder::forward`] would.

use std::borrow::Borrow;
use std::io::{Error, ErrorKind, Result};
use std::ops::Range;

use burn::module::Module;
use burn::record::{BinBytesRecorder, FullPrecisionSettings, Recorder};
use burn::tensor::activation::relu;
use burn::tensor::{Tensor, backend::Backend};

use crate::gnn::{
    GatMultiHead, GatMultiHeadConfig, Gcn, GcnConfig, GraphSage, GraphSageConfig, RGcn, RGcnConfig,
};
use crate::graph::CsrGraph;
use crate::pipeline::ModelChoice;
use crate::sparse::RowBlock;

/// Adjacencies for every encoder kind, built once per graph.
#[derive(Debug, Clone)]
pub struct GraphInputs {
    /// `D^-1/2 (A + I) D^-1/2`: GCN propagation, and the edge set (CSR order) GAT attends over and
    /// edge masks index.
    pub sym: CsrGraph,
    /// `sym` row-normalized: GraphSAGE's mean aggregator. Same CSR layout as `sym`.
    pub row: CsrGraph,
    /// One row-normalized adjacency per relationship type, no self-loops (R-GCN's self weight
    /// covers those). A homogeneous graph is one relation.
    pub relations: Vec<CsrGraph>,
}

impl GraphInputs {
    /// From a symmetric-normalized graph with self-loops (the datasets' `graph`) and raw
    /// per-relation adjacencies. With no relations, R-GCN sees the whole graph as one.
    pub fn new(sym: CsrGraph, relations: &[CsrGraph]) -> Self {
        let row = sym.clone().row_normalize();
        let relations = if relations.is_empty() {
            vec![structure_without_self_loops(&sym).row_normalize()]
        } else {
            relations
                .iter()
                .map(|r| r.clone().row_normalize())
                .collect()
        };
        Self {
            sym,
            row,
            relations,
        }
    }

    /// From an undirected edge list (each pair once, any order) with a relation index per edge.
    pub fn from_edges(
        num_nodes: usize,
        edges: &[(u32, u32)],
        relation_of: &[u16],
        num_relations: usize,
    ) -> Self {
        let weighted: Vec<(u32, u32, f32)> = edges.iter().map(|&(u, v)| (u, v, 1.0)).collect();
        let sym = CsrGraph::from_edges(num_nodes, &weighted)
            .add_self_loops()
            .symmetric_normalize();
        let mut per_rel: Vec<Vec<(u32, u32, f32)>> = vec![Vec::new(); num_relations.max(1)];
        for (i, &(u, v)) in edges.iter().enumerate() {
            let r = relation_of.get(i).copied().unwrap_or(0) as usize;
            if u != v
                && let Some(list) = per_rel.get_mut(r)
            {
                list.push((u, v, 1.0));
            }
        }
        let relations: Vec<CsrGraph> = per_rel
            .iter()
            .map(|e| CsrGraph::from_edges(num_nodes, e))
            .collect();
        Self::new(sym, &relations)
    }

    /// Node count.
    pub fn num_nodes(&self) -> usize {
        self.sym.num_nodes()
    }

    /// Relationship types R-GCN sees.
    pub fn num_relations(&self) -> usize {
        self.relations.len()
    }

    /// Consecutive row ranges covering every node, each holding at most `max_entries` stored
    /// entries of the adjacencies `reads` names (a single row may hold more).
    pub fn plan(&self, reads: Reads, max_entries: usize) -> Vec<Range<usize>> {
        let n = self.num_nodes();
        let entries_of = |r: usize| -> usize {
            match reads {
                Reads::Sym | Reads::Row => self.sym.degree(r),
                Reads::Relations => self.relations.iter().map(|g| g.degree(r)).sum(),
            }
        };
        let mut ranges = Vec::new();
        let (mut start, mut held) = (0usize, 0usize);
        for r in 0..n {
            let e = entries_of(r);
            if r > start && held + e > max_entries {
                ranges.push(start..r);
                (start, held) = (r, 0);
            }
            held += e;
        }
        ranges.push(start..n);
        ranges
    }
}

/// `graph`'s structure as unit-weight undirected edges, self-loops dropped.
fn structure_without_self_loops(graph: &CsrGraph) -> CsrGraph {
    let n = graph.num_nodes();
    let mut edges = Vec::new();
    for u in 0..n {
        for (v, _) in graph.neighbors(u) {
            // the CSR holds both directions; keep each pair once
            if (u as u32) < v {
                edges.push((u as u32, v, 1.0));
            }
        }
    }
    CsrGraph::from_edges(n, &edges)
}

/// Which of [`GraphInputs`]' adjacencies an encoder kind propagates over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reads {
    /// `sym`: GCN, and the edge set GAT attends over.
    Sym,
    /// `row`: GraphSAGE.
    Row,
    /// `relations`: R-GCN.
    Relations,
}

impl Reads {
    pub fn of(choice: &ModelChoice) -> Self {
        match choice {
            ModelChoice::Gcn { .. } | ModelChoice::Gat { .. } => Reads::Sym,
            ModelChoice::GraphSage { .. } => Reads::Row,
            ModelChoice::RGcn { .. } => Reads::Relations,
        }
    }
}

/// Rows `rows` of the adjacency an encoder reads, on a device.
#[derive(Debug, Clone)]
pub struct InputBlock<B: Backend> {
    /// `sym` or `row` rows (GCN, GAT, GraphSAGE).
    adjacency: Option<RowBlock<B>>,
    /// One per relation, over the same rows (R-GCN).
    relations: Vec<RowBlock<B>>,
    /// Per-entry weights on `adjacency`'s entries (explainers).
    mask: Option<Tensor<B, 1>>,
}

impl<B: Backend> InputBlock<B> {
    pub fn new(g: &GraphInputs, reads: Reads, rows: Range<usize>, device: &B::Device) -> Self {
        let (adjacency, relations) = match reads {
            Reads::Sym => (Some(RowBlock::new(&g.sym, rows, device)), Vec::new()),
            Reads::Row => (Some(RowBlock::new(&g.row, rows, device)), Vec::new()),
            Reads::Relations => (
                None,
                g.relations
                    .iter()
                    .map(|r| RowBlock::new(r, rows.clone(), device))
                    .collect(),
            ),
        };
        Self {
            adjacency,
            relations,
            mask: None,
        }
    }

    /// Every row.
    pub fn all(g: &GraphInputs, reads: Reads, device: &B::Device) -> Self {
        Self::new(g, reads, 0..g.num_nodes(), device)
    }

    fn adjacency(&self) -> &RowBlock<B> {
        self.adjacency
            .as_ref()
            .expect("GCN, GraphSAGE and GAT read a single adjacency")
    }
}

/// A 2-layer GNN encoder of any supported kind. `forward` maps `[N, in_dim]` features to
/// `[N, out_dim]` (class logits, a regression value or link embeddings, per task).
#[derive(Module, Debug)]
pub enum AnyEncoder<B: Backend> {
    /// GCN.
    Gcn(Gcn<B>),
    /// GraphSAGE-mean.
    GraphSage(GraphSage<B>),
    /// Relational GCN.
    RGcn(RGcn<B>),
    /// Multi-head GAT.
    Gat(GatMultiHead<B>),
}

/// Message values one step of [`AnyEncoder::forward_blocked`] may hold (`entries x width`):
/// 64 Mi floats, 256 MiB.
pub const BLOCK_VALUES: usize = 1 << 26;

impl<B: Backend> AnyEncoder<B> {
    /// Fresh weights for `choice`. `num_relations` only matters for R-GCN.
    pub fn init(
        choice: &ModelChoice,
        in_dim: usize,
        out_dim: usize,
        num_relations: usize,
        device: &B::Device,
    ) -> Self {
        match *choice {
            ModelChoice::Gcn { hidden } => {
                Self::Gcn(GcnConfig::new(in_dim, hidden, out_dim).init(device))
            }
            ModelChoice::GraphSage { hidden } => {
                Self::GraphSage(GraphSageConfig::new(in_dim, hidden, out_dim).init(device))
            }
            ModelChoice::RGcn { hidden } => Self::RGcn(
                RGcnConfig::new(in_dim, hidden, out_dim, num_relations.max(1)).init(device),
            ),
            ModelChoice::Gat { hidden, heads } => Self::Gat(
                GatMultiHeadConfig::new(in_dim, hidden, out_dim)
                    .with_num_heads_hidden(heads.max(1))
                    .with_num_heads_out(1)
                    .init(device),
            ),
        }
    }

    /// The adjacency this encoder propagates over.
    pub fn reads(&self) -> Reads {
        match self {
            Self::Gcn(_) | Self::Gat(_) => Reads::Sym,
            Self::GraphSage(_) => Reads::Row,
            Self::RGcn(_) => Reads::Relations,
        }
    }

    /// `[N, in_dim]` -> `[N, out_dim]`.
    pub fn forward(&self, x: Tensor<B, 2>, g: &GraphInputs) -> Tensor<B, 2> {
        let block = InputBlock::all(g, self.reads(), &x.device());
        self.forward_blocks(x, std::slice::from_ref(&block))
    }

    /// Forward over `blocks`, which cover every node once, in order. Built once, they serve any
    /// number of passes (a training loop's epochs).
    pub fn forward_blocks(&self, x: Tensor<B, 2>, blocks: &[InputBlock<B>]) -> Tensor<B, 2> {
        let h = relu(self.layer(0, x, blocks));
        self.layer(1, h, blocks)
    }

    /// [`forward`](Self::forward) computed in blocks of rows holding at most `max_entries` stored
    /// entries each (see [`BLOCK_VALUES`] for a width-aware budget), so a large graph never
    /// needs all of its messages in memory at once. Blocks are built per layer and dropped after.
    pub fn forward_blocked(
        &self,
        x: Tensor<B, 2>,
        g: &GraphInputs,
        max_entries: usize,
    ) -> Tensor<B, 2> {
        let reads = self.reads();
        let plan = g.plan(reads, max_entries.max(1));
        let device = x.device();
        let blocks = || {
            plan.iter()
                .map(|rows| InputBlock::new(g, reads, rows.clone(), &device))
        };
        let h = relu(self.layer(0, x, blocks()));
        self.layer(1, h, blocks())
    }

    /// Stored entries per block for [`forward_blocked`](Self::forward_blocked) so a step holds
    /// about [`BLOCK_VALUES`] message values at this encoder's widest layer.
    pub fn block_entries(&self) -> usize {
        let width = |l: &burn::nn::Linear<B>| {
            let [i, o] = l.weight.dims();
            i.max(o)
        };
        let widest = match self {
            Self::Gcn(m) => width(&m.layer1).max(width(&m.layer2)),
            Self::GraphSage(m) => width(&m.layer1).max(width(&m.layer2)),
            Self::RGcn(m) => width(&m.layer1_self).max(width(&m.layer2_self)),
            Self::Gat(m) => m
                .layer1_heads
                .iter()
                .chain(&m.layer2_heads)
                .map(|h| width(&h.w))
                .max()
                .unwrap_or(1),
        };
        (BLOCK_VALUES / widest.max(1)).max(1)
    }

    /// Layer `l` (0 or 1) for every node, a block at a time, before any activation.
    fn layer<I>(&self, l: usize, h: Tensor<B, 2>, blocks: I) -> Tensor<B, 2>
    where
        I: IntoIterator,
        I::Item: Borrow<InputBlock<B>>,
    {
        let blocks = blocks.into_iter();
        // GAT projects every node once per layer; the blocks share it
        let projection = match self {
            Self::Gat(m) => Some(m.project(l, h.clone())),
            _ => None,
        };
        let parts: Vec<Tensor<B, 2>> = blocks
            .map(|b| {
                let b: &InputBlock<B> = b.borrow();
                match self {
                    Self::Gcn(m) => m.layer(l, h.clone(), b.adjacency(), b.mask.clone()),
                    Self::GraphSage(m) => m.layer(l, h.clone(), b.adjacency(), b.mask.clone()),
                    Self::RGcn(m) => m.layer(l, h.clone(), &b.relations),
                    Self::Gat(m) => m.attend(
                        l,
                        projection.as_ref().expect("projected above"),
                        b.adjacency(),
                        b.mask.clone(),
                    ),
                }
            })
            .collect();
        if parts.len() == 1 {
            parts.into_iter().next().expect("one part")
        } else {
            Tensor::cat(parts, 0)
        }
    }

    /// Whether [`forward_masked`](Self::forward_masked) works: every kind but R-GCN, whose
    /// per-relation graphs don't share one edge order.
    pub fn supports_edge_mask(&self) -> bool {
        !matches!(self, Self::RGcn(_))
    }

    /// Forward with a soft per-edge mask `[E]` over `g.sym`'s edges in CSR order (explainers).
    pub fn forward_masked(
        &self,
        x: Tensor<B, 2>,
        g: &GraphInputs,
        edge_mask: Tensor<B, 1>,
    ) -> Result<Tensor<B, 2>> {
        if !self.supports_edge_mask() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "edge masks need a single edge set; R-GCN models can't be explained this way",
            ));
        }
        let mut block = InputBlock::all(g, self.reads(), &x.device());
        block.mask = Some(edge_mask);
        Ok(self.forward_blocks(x, std::slice::from_ref(&block)))
    }

    /// Weights as `BinBytesRecorder` bytes of the inner model's record, the format
    /// [`from_bytes`](Self::from_bytes) and models saved by earlier versions use.
    pub fn to_bytes(self) -> Result<Vec<u8>> {
        let rec = BinBytesRecorder::<FullPrecisionSettings>::new();
        let bytes = match self {
            Self::Gcn(m) => rec.record(m.into_record(), ()),
            Self::GraphSage(m) => rec.record(m.into_record(), ()),
            Self::RGcn(m) => rec.record(m.into_record(), ()),
            Self::Gat(m) => rec.record(m.into_record(), ()),
        };
        bytes.map_err(|e| Error::other(format!("record weights: {e}")))
    }

    /// Rebuilds an encoder from [`to_bytes`](Self::to_bytes) output.
    pub fn from_bytes(
        choice: &ModelChoice,
        bytes: &[u8],
        in_dim: usize,
        out_dim: usize,
        num_relations: usize,
        device: &B::Device,
    ) -> Result<Self> {
        let rec = BinBytesRecorder::<FullPrecisionSettings>::new();
        let load_err = |e| Error::new(ErrorKind::InvalidData, format!("load weights: {e}"));
        Ok(
            match Self::init(choice, in_dim, out_dim, num_relations, device) {
                Self::Gcn(m) => {
                    Self::Gcn(m.load_record(rec.load(bytes.to_vec(), device).map_err(load_err)?))
                }
                Self::GraphSage(m) => Self::GraphSage(
                    m.load_record(rec.load(bytes.to_vec(), device).map_err(load_err)?),
                ),
                Self::RGcn(m) => {
                    Self::RGcn(m.load_record(rec.load(bytes.to_vec(), device).map_err(load_err)?))
                }
                Self::Gat(m) => {
                    Self::Gat(m.load_record(rec.load(bytes.to_vec(), device).map_err(load_err)?))
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type B = burn::backend::NdArray;

    fn triangle_plus_tail() -> GraphInputs {
        // 0-1-2 triangle (relation 0), 2-3 tail (relation 1)
        GraphInputs::from_edges(4, &[(0, 1), (1, 2), (0, 2), (2, 3)], &[0, 0, 0, 1], 2)
    }

    fn every_kind() -> [ModelChoice; 4] {
        [
            ModelChoice::Gcn { hidden: 8 },
            ModelChoice::GraphSage { hidden: 8 },
            ModelChoice::RGcn { hidden: 8 },
            ModelChoice::Gat {
                hidden: 4,
                heads: 2,
            },
        ]
    }

    #[test]
    fn every_kind_maps_to_out_dim() {
        let device = Default::default();
        let g = triangle_plus_tail();
        let x = Tensor::<B, 2>::ones([4, 3], &device);
        for choice in every_kind() {
            let enc = AnyEncoder::<B>::init(&choice, 3, 5, g.num_relations(), &device);
            assert_eq!(enc.forward(x.clone(), &g).dims(), [4, 5], "{choice:?}");
        }
    }

    #[test]
    fn blocked_inference_equals_the_full_forward() {
        let device = Default::default();
        // 40 nodes, three relationship types, uneven degrees, an isolated node
        let mut edges = Vec::new();
        let mut rel = Vec::new();
        for i in 0..38u32 {
            for j in [i + 1, (i * 7 + 3) % 38, (i * 11 + 5) % 38] {
                if j != i {
                    edges.push((i.min(j), i.max(j)));
                    rel.push((i % 3) as u16);
                }
            }
        }
        let g = GraphInputs::from_edges(40, &edges, &rel, 3);
        let feats: Vec<f32> = (0..40 * 3)
            .map(|i| ((i * 37) % 11) as f32 * 0.2 - 1.0)
            .collect();
        let x = Tensor::<B, 1>::from_floats(feats.as_slice(), &device).reshape([40, 3]);
        for choice in every_kind() {
            let enc = AnyEncoder::<B>::init(&choice, 3, 4, g.num_relations(), &device);
            let whole: Vec<f32> = enc.forward(x.clone(), &g).into_data().to_vec().unwrap();
            for max_entries in [1, 7, 50] {
                let blocked: Vec<f32> = enc
                    .forward_blocked(x.clone(), &g, max_entries)
                    .into_data()
                    .to_vec()
                    .unwrap();
                for (a, b) in whole.iter().zip(&blocked) {
                    assert!(
                        (a - b).abs() < 1e-5,
                        "{choice:?} at {max_entries}: {a} vs {b}"
                    );
                }
            }
        }
    }

    #[test]
    fn plans_cover_every_row_once_within_the_budget() {
        let g = triangle_plus_tail();
        for reads in [Reads::Sym, Reads::Row, Reads::Relations] {
            for budget in [1, 3, 4, 100] {
                let plan = g.plan(reads, budget);
                assert_eq!(plan.first().unwrap().start, 0);
                assert_eq!(plan.last().unwrap().end, 4);
                for w in plan.windows(2) {
                    assert_eq!(w[0].end, w[1].start);
                }
                for r in &plan {
                    let held: usize = r
                        .clone()
                        .map(|i| match reads {
                            Reads::Sym | Reads::Row => g.sym.degree(i),
                            Reads::Relations => g.relations.iter().map(|x| x.degree(i)).sum(),
                        })
                        .sum();
                    assert!(held <= budget || r.len() == 1, "{reads:?} {budget} {r:?}");
                }
            }
        }
    }

    #[test]
    fn weights_round_trip() {
        let device = Default::default();
        let g = triangle_plus_tail();
        let x = Tensor::<B, 2>::ones([4, 3], &device);
        for choice in [
            ModelChoice::Gcn { hidden: 8 },
            ModelChoice::RGcn { hidden: 8 },
            ModelChoice::Gat {
                hidden: 4,
                heads: 3,
            },
        ] {
            let enc = AnyEncoder::<B>::init(&choice, 3, 2, 2, &device);
            let before: Vec<f32> = enc.forward(x.clone(), &g).into_data().to_vec().unwrap();
            let bytes = enc.to_bytes().unwrap();
            let back = AnyEncoder::<B>::from_bytes(&choice, &bytes, 3, 2, 2, &device).unwrap();
            let after: Vec<f32> = back.forward(x.clone(), &g).into_data().to_vec().unwrap();
            assert_eq!(before, after, "{choice:?}");
        }
    }

    #[test]
    fn homogeneous_graph_is_one_relation() {
        let sym = CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0)])
            .add_self_loops()
            .symmetric_normalize();
        let g = GraphInputs::new(sym, &[]);
        assert_eq!(g.num_relations(), 1);
        // no self-loops in the relation graph: 2 undirected edges, both directions
        assert_eq!(g.relations[0].num_edges(), 4);
    }
}
