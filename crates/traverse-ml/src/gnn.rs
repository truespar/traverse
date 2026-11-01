//! GNN architectures: GCN, GraphSAGE, R-GCN and multi-head GAT, two message-passing layers each.
//!
//! Every model computes a layer for one [`RowBlock`] of nodes at a time, from the previous layer's
//! output for all nodes. A block over every row is the plain forward pass (training, sampled
//! mini-batches); several blocks keep inference on a large graph within a memory budget
//! (`AnyEncoder::forward_blocked`). It's the same arithmetic either way.

use crate::graph::CsrGraph;
use crate::sparse::RowBlock;
use burn::module::Module;
use burn::nn::{Linear, LinearConfig};
use burn::tensor::activation::{leaky_relu, relu};
use burn::tensor::{IndexingUpdateOp, Tensor, backend::Backend};

/// `block.spmm(h)`, masked when a mask (the block's slice) is given.
fn aggregate<B: Backend>(
    block: &RowBlock<B>,
    h: Tensor<B, 2>,
    mask: Option<Tensor<B, 1>>,
) -> Tensor<B, 2> {
    match mask {
        Some(m) => block.spmm_masked(h, m),
        None => block.spmm(h),
    }
}

/// The block's own rows of `h` (a block holds at least one row).
fn own_rows<B: Backend>(h: Tensor<B, 2>, block: &RowBlock<B>) -> Tensor<B, 2> {
    h.narrow(0, block.start, block.len)
}

/// 2-layer Graph Convolutional Network (Kipf & Welling 2017): `forward(x, adj) = A_norm @
/// relu(A_norm @ x @ W1) @ W2`, `A_norm` being the symmetrically normalized adjacency `D^(-1/2)
/// (A + I) D^(-1/2)` (the caller applies `add_self_loops` + `symmetric_normalize` first). Memory is
/// O(E*D), not O(N^2).
#[derive(Module, Debug)]
pub struct Gcn<B: Backend> {
    pub layer1: Linear<B>,
    pub layer2: Linear<B>,
}

/// [`Gcn`] config.
pub struct GcnConfig {
    pub in_dim: usize,
    pub hidden: usize,
    pub out_dim: usize,
}

impl GcnConfig {
    pub fn new(in_dim: usize, hidden: usize, out_dim: usize) -> Self {
        Self {
            in_dim,
            hidden,
            out_dim,
        }
    }

    pub fn init<B: Backend>(&self, device: &B::Device) -> Gcn<B> {
        Gcn {
            layer1: LinearConfig::new(self.in_dim, self.hidden).init(device),
            layer2: LinearConfig::new(self.hidden, self.out_dim).init(device),
        }
    }
}

impl<B: Backend> Gcn<B> {
    /// Forward: `A_norm @ relu(A_norm @ X @ W1) @ W2`.
    pub fn forward(&self, x: Tensor<B, 2>, adj: &CsrGraph) -> Tensor<B, 2> {
        let block = RowBlock::all(adj, &x.device());
        let h = relu(self.layer(0, x, &block, None));
        self.layer(1, h, &block, None)
    }

    /// Forward with a per-edge soft mask `edge_mask: [E]` (CSR order): each entry's weight is
    /// `adj.values[e] * edge_mask[e]`, the GNN on a modified adjacency. The mask carries
    /// gradients, so PGExplainer (or any edge-importance method) trains against a frozen GNN
    /// through this.
    pub fn forward_masked(
        &self,
        x: Tensor<B, 2>,
        adj: &CsrGraph,
        edge_mask: Tensor<B, 1>,
    ) -> Tensor<B, 2> {
        let block = RowBlock::all(adj, &x.device());
        let h = relu(self.layer(0, x, &block, Some(edge_mask.clone())));
        self.layer(1, h, &block, Some(edge_mask))
    }

    /// Layer `l` (0 or 1, before any activation) for `block`'s rows, from the previous layer's
    /// output `h` for every node. `mask` covers the block's entries.
    pub fn layer(
        &self,
        l: usize,
        h: Tensor<B, 2>,
        block: &RowBlock<B>,
        mask: Option<Tensor<B, 1>>,
    ) -> Tensor<B, 2> {
        let linear = if l == 0 { &self.layer1 } else { &self.layer2 };
        linear.forward(aggregate(block, h, mask))
    }
}

/// 2-layer GraphSAGE, mean aggregation with self-concat:
///
/// `h_v^{k+1} = ReLU(W^k * CONCAT(h_v^k, MEAN({h_u^k : u in N(v)})))`
///
/// Unlike [`Gcn`]:
///
/// 1. **Mean aggregation**: neighbor contributions are averaged, not weighted
///    by `D^(-1/2) (A+I) D^(-1/2)`; the caller supplies a row-normalized
///    adjacency (`CsrGraph::row_normalize()`).
/// 2. **Self-concat**: each layer concatenates the node's own embedding with
///    the aggregate before the linear, so layer input is `2 x prev_dim`.
#[derive(Module, Debug)]
pub struct GraphSage<B: Backend> {
    pub layer1: Linear<B>,
    pub layer2: Linear<B>,
}

/// [`GraphSage`] config.
pub struct GraphSageConfig {
    pub in_dim: usize,
    pub hidden: usize,
    pub out_dim: usize,
}

impl GraphSageConfig {
    pub fn new(in_dim: usize, hidden: usize, out_dim: usize) -> Self {
        Self {
            in_dim,
            hidden,
            out_dim,
        }
    }

    /// Layer inputs are doubled by the self-concat: `layer1` `2*in_dim -> hidden`, `layer2`
    /// `2*hidden -> out_dim`.
    pub fn init<B: Backend>(&self, device: &B::Device) -> GraphSage<B> {
        GraphSage {
            layer1: LinearConfig::new(2 * self.in_dim, self.hidden).init(device),
            layer2: LinearConfig::new(2 * self.hidden, self.out_dim).init(device),
        }
    }
}

impl<B: Backend> GraphSage<B> {
    /// Forward: per layer, mean-aggregate neighbors, concat with the node's own embedding, linear.
    /// Expects a *row-normalized* adjacency (`graph.row_normalize()`).
    pub fn forward(&self, x: Tensor<B, 2>, adj: &CsrGraph) -> Tensor<B, 2> {
        let block = RowBlock::all(adj, &x.device());
        let h = relu(self.layer(0, x, &block, None));
        self.layer(1, h, &block, None)
    }

    /// [`forward`](Self::forward) with a per-edge soft mask `[E]` scaling each neighbor's
    /// contribution (CSR order of `adj`), for edge-importance explainers.
    pub fn forward_masked(
        &self,
        x: Tensor<B, 2>,
        adj: &CsrGraph,
        edge_mask: Tensor<B, 1>,
    ) -> Tensor<B, 2> {
        let block = RowBlock::all(adj, &x.device());
        let h = relu(self.layer(0, x, &block, Some(edge_mask.clone())));
        self.layer(1, h, &block, Some(edge_mask))
    }

    /// Layer `l` for `block`'s rows; see [`Gcn::layer`].
    pub fn layer(
        &self,
        l: usize,
        h: Tensor<B, 2>,
        block: &RowBlock<B>,
        mask: Option<Tensor<B, 1>>,
    ) -> Tensor<B, 2> {
        let linear = if l == 0 { &self.layer1 } else { &self.layer2 };
        let neighbors = aggregate(block, h.clone(), mask);
        linear.forward(Tensor::cat(vec![own_rows(h, block), neighbors], 1))
    }
}

/// 2-layer Relational GCN (Schlichtkrull et al. 2018, *Modeling Relational
/// Data with Graph Convolutional Networks*): [`Gcn`] for several edge types.
/// Each layer has a self-transform `W_self` plus one `W_r` per relation `r`:
///
/// `h_v^{k+1} = ReLU(W_self^k * h_v^k + sum_r W_r^k * (A_r * h^k)_v)`
///
/// One row-normalized [`CsrGraph`] per relation (`graph.row_normalize()`, no
/// `add_self_loops`: `W_self` covers that); the slice length must equal the
/// config's `num_relations`.
///
/// No basis decomposition (the paper's compression for many-relation KGs):
/// direct per-relation matrices are fine for a handful of edge types.
#[derive(Module, Debug)]
pub struct RGcn<B: Backend> {
    pub layer1_self: Linear<B>,
    pub layer1_rels: Vec<Linear<B>>,
    pub layer2_self: Linear<B>,
    pub layer2_rels: Vec<Linear<B>>,
}

/// [`RGcn`] config.
pub struct RGcnConfig {
    pub in_dim: usize,
    pub hidden: usize,
    pub out_dim: usize,
    pub num_relations: usize,
}

impl RGcnConfig {
    pub fn new(in_dim: usize, hidden: usize, out_dim: usize, num_relations: usize) -> Self {
        Self {
            in_dim,
            hidden,
            out_dim,
            num_relations,
        }
    }

    pub fn init<B: Backend>(&self, device: &B::Device) -> RGcn<B> {
        let layer1_self = LinearConfig::new(self.in_dim, self.hidden).init(device);
        let layer1_rels = (0..self.num_relations)
            .map(|_| LinearConfig::new(self.in_dim, self.hidden).init(device))
            .collect();
        let layer2_self = LinearConfig::new(self.hidden, self.out_dim).init(device);
        let layer2_rels = (0..self.num_relations)
            .map(|_| LinearConfig::new(self.hidden, self.out_dim).init(device))
            .collect();
        RGcn {
            layer1_self,
            layer1_rels,
            layer2_self,
            layer2_rels,
        }
    }
}

impl<B: Backend> RGcn<B> {
    /// Forward. `rel_graphs[r]` is relation `r`'s adjacency, already row-normalized; the length
    /// must equal the config's `num_relations`.
    pub fn forward(&self, x: Tensor<B, 2>, rel_graphs: &[&CsrGraph]) -> Tensor<B, 2> {
        let device = x.device();
        let blocks: Vec<RowBlock<B>> = rel_graphs
            .iter()
            .map(|g| RowBlock::all(g, &device))
            .collect();
        let h = relu(self.layer(0, x, &blocks));
        self.layer(1, h, &blocks)
    }

    /// Layer `l` for the rows of `blocks`, one block per relation over the same rows; see
    /// [`Gcn::layer`].
    pub fn layer(&self, l: usize, h: Tensor<B, 2>, blocks: &[RowBlock<B>]) -> Tensor<B, 2> {
        let (self_linear, rel_linears) = if l == 0 {
            (&self.layer1_self, &self.layer1_rels)
        } else {
            (&self.layer2_self, &self.layer2_rels)
        };
        assert_eq!(
            blocks.len(),
            rel_linears.len(),
            "expected {} per-relation adjacencies, got {}",
            rel_linears.len(),
            blocks.len()
        );
        let mut out = self_linear.forward(own_rows(h.clone(), &blocks[0]));
        for (linear, block) in rel_linears.iter().zip(blocks) {
            out = out + linear.forward(block.spmm(h.clone()));
        }
        out
    }
}

// --- GAT ---------------------------------------------------------
//
// Graph Attention Network (Veličković et al. 2018). One head of one layer:
//
//   Wh = W * X                          # [N, out]
//   e_ij = LeakyReLU(a_src * Wh_i + a_dst * Wh_j)
//   alpha_ij = softmax_j(e_ij)              # over i's stored entries
//   h_i' = sum_j alpha_ij * Wh_j
//
// Split attention: `a_src` and `a_dst` are separate `Linear<out, 1>` projections instead of one
// over the concatenation `[Wh_i || Wh_j]`; identical when `a = [a_src; a_dst]`, but each node's
// score contribution is one scalar, computed once and added per edge. Edge weights in the CSR are
// ignored: attention replaces them.
//
// Published GAT results use K = 4 to 8 heads: richer representations, and the variance across
// heads acts as a regularizer. Layer 1 concatenates its heads (hidden width `heads x head_dim`),
// layer 2 averages them (width `out_dim`).

/// One attention head's parameters (`W`, `a_src`, `a_dst`).
#[derive(Module, Debug)]
pub struct GatHeadParams<B: Backend> {
    pub w: Linear<B>,
    pub a_src: Linear<B>,
    pub a_dst: Linear<B>,
}

/// 2-layer multi-head GAT: layer 1 concatenates its heads, layer 2 averages them.
#[derive(Module, Debug)]
pub struct GatMultiHead<B: Backend> {
    pub layer1_heads: Vec<GatHeadParams<B>>,
    pub layer2_heads: Vec<GatHeadParams<B>>,
}

/// [`GatMultiHead`] config.
pub struct GatMultiHeadConfig {
    pub in_dim: usize,
    pub head_dim: usize,
    pub out_dim: usize,
    pub num_heads_hidden: usize,
    pub num_heads_out: usize,
}

impl GatMultiHeadConfig {
    /// The GAT paper's 8-head hidden / 1-head output setup; assign fields for other head counts.
    pub fn new(in_dim: usize, head_dim: usize, out_dim: usize) -> Self {
        Self {
            in_dim,
            head_dim,
            out_dim,
            num_heads_hidden: 8,
            num_heads_out: 1,
        }
    }

    pub fn with_num_heads_hidden(mut self, k: usize) -> Self {
        self.num_heads_hidden = k;
        self
    }

    pub fn with_num_heads_out(mut self, k: usize) -> Self {
        self.num_heads_out = k;
        self
    }

    /// Attention projections have no bias: raw additive score contributions.
    pub fn init<B: Backend>(&self, device: &B::Device) -> GatMultiHead<B> {
        let make_head = |in_d: usize, out_d: usize| GatHeadParams {
            w: LinearConfig::new(in_d, out_d).init(device),
            a_src: LinearConfig::new(out_d, 1).with_bias(false).init(device),
            a_dst: LinearConfig::new(out_d, 1).with_bias(false).init(device),
        };
        let layer1_heads: Vec<GatHeadParams<B>> = (0..self.num_heads_hidden)
            .map(|_| make_head(self.in_dim, self.head_dim))
            .collect();
        // layer 2 input: layer 1 heads concatenated
        let hidden_dim = self.num_heads_hidden * self.head_dim;
        let layer2_heads: Vec<GatHeadParams<B>> = (0..self.num_heads_out)
            .map(|_| make_head(hidden_dim, self.out_dim))
            .collect();
        GatMultiHead {
            layer1_heads,
            layer2_heads,
        }
    }
}

/// One layer's per-head projections for every node, computed once and shared by all blocks:
/// `(Wh [N, out], a_src*Wh [N], a_dst*Wh [N])` per head.
pub struct GatProjection<B: Backend> {
    heads: Vec<(Tensor<B, 2>, Tensor<B, 1>, Tensor<B, 1>)>,
}

impl<B: Backend> GatMultiHead<B> {
    pub fn forward(&self, x: Tensor<B, 2>, adj: &CsrGraph) -> Tensor<B, 2> {
        self.forward_inner(x, adj, None)
    }

    /// [`forward`](Self::forward) with a per-edge soft mask `[E]` on the attention weights (CSR
    /// order of `adj`), for edge-importance explainers.
    pub fn forward_masked(
        &self,
        x: Tensor<B, 2>,
        adj: &CsrGraph,
        edge_mask: Tensor<B, 1>,
    ) -> Tensor<B, 2> {
        self.forward_inner(x, adj, Some(edge_mask))
    }

    fn forward_inner(
        &self,
        x: Tensor<B, 2>,
        adj: &CsrGraph,
        mask: Option<Tensor<B, 1>>,
    ) -> Tensor<B, 2> {
        let block = RowBlock::all(adj, &x.device());
        let p = self.project(0, x);
        let h = relu(self.attend(0, &p, &block, mask.clone()));
        let p = self.project(1, h);
        self.attend(1, &p, &block, mask)
    }

    /// Layer `l`'s projections of its input `h` (every node).
    pub fn project(&self, l: usize, h: Tensor<B, 2>) -> GatProjection<B> {
        let heads = if l == 0 {
            &self.layer1_heads
        } else {
            &self.layer2_heads
        };
        GatProjection {
            heads: heads
                .iter()
                .map(|head| {
                    let wh = head.w.forward(h.clone());
                    let src = head.a_src.forward(wh.clone()).squeeze_dim::<1>(1);
                    let dst = head.a_dst.forward(wh.clone()).squeeze_dim::<1>(1);
                    (wh, src, dst)
                })
                .collect(),
        }
    }

    /// Layer `l` for `block`'s rows from its [`project`](Self::project)ion: heads concatenated
    /// (layer 0) or averaged (layer 1), before any activation. `mask` covers the block's entries.
    pub fn attend(
        &self,
        l: usize,
        p: &GatProjection<B>,
        block: &RowBlock<B>,
        mask: Option<Tensor<B, 1>>,
    ) -> Tensor<B, 2> {
        let outs: Vec<Tensor<B, 2>> = p
            .heads
            .iter()
            .map(|(wh, src, dst)| attend_head(wh, src, dst, block, mask.clone()))
            .collect();
        if l == 0 {
            Tensor::cat(outs, 1)
        } else {
            let k = outs.len().max(1) as f32;
            outs.into_iter()
                .reduce(|a, b| a + b)
                .expect("at least one head")
                / k
        }
    }
}

/// One head's attention-weighted aggregation for `block`'s rows: a softmax over each row's stored
/// entries. The scores are shifted by their (detached) maximum first, which leaves every row's
/// softmax unchanged and keeps `exp` from overflowing.
fn attend_head<B: Backend>(
    wh: &Tensor<B, 2>,
    src: &Tensor<B, 1>,
    dst: &Tensor<B, 1>,
    block: &RowBlock<B>,
    mask: Option<Tensor<B, 1>>,
) -> Tensor<B, 2> {
    let [_, out_dim] = wh.dims();
    let device = wh.device();
    let zeros = Tensor::<B, 2>::zeros([block.len, out_dim], &device);
    let Some((out_idx, in_idx)) = block.indices() else {
        return zeros;
    };
    let own = src.clone().narrow(0, block.start, block.len);
    let score = own.select(0, out_idx.clone()) + dst.clone().select(0, in_idx.clone());
    let score = leaky_relu(score, 0.2);
    let score = score.clone() - score.clone().detach().max();
    let exp = score.exp();
    let sums = Tensor::<B, 1>::zeros([block.len], &device).select_assign(
        0,
        out_idx.clone(),
        exp.clone(),
        IndexingUpdateOp::Add,
    );
    let alpha = exp / sums.select(0, out_idx.clone());
    let alpha = match mask {
        Some(m) => alpha * m,
        None => alpha,
    };
    let messages =
        wh.clone().select(0, in_idx) * alpha.unsqueeze_dim::<2>(1).expand([block.entries, out_dim]);
    zeros.select_assign(0, out_idx, messages, IndexingUpdateOp::Add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CsrGraph;

    type B = burn::backend::NdArray;

    #[test]
    fn graphsage_forward_produces_expected_shape() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0), (0, 2, 1.0)])
            .add_self_loops()
            .row_normalize();
        let in_dim = 4;
        let hidden = 8;
        let out_dim = 2;
        let model: GraphSage<B> = GraphSageConfig::new(in_dim, hidden, out_dim).init(&device);

        let x_data: Vec<f32> = (0..(3 * in_dim)).map(|i| (i as f32) * 0.1).collect();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats(x_data.as_slice(), &device).reshape([3, in_dim]);

        let y = model.forward(x, &adj);
        assert_eq!(y.dims(), [3, out_dim]);
    }

    #[test]
    fn graphsage_gradients_flow() {
        // autograd check: SpMM + Tensor::cat + Linear in one backward pass; both Linear weights must
        // get a gradient
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0), (0, 2, 1.0)])
            .add_self_loops()
            .row_normalize();
        let model: GraphSage<BA> = GraphSageConfig::new(4, 8, 2).init(&device);
        let x_data: Vec<f32> = (0..(3 * 4)).map(|i| (i as f32) * 0.1).collect();
        let x: Tensor<BA, 2> =
            Tensor::<BA, 1>::from_floats(x_data.as_slice(), &device).reshape([3, 4]);

        let y = model.forward(x, &adj);
        let loss = y.sum();
        let grads = loss.backward();

        assert!(
            model.layer1.weight.grad(&grads).is_some(),
            "layer1.weight should have a gradient"
        );
        assert!(
            model.layer2.weight.grad(&grads).is_some(),
            "layer2.weight should have a gradient"
        );
    }

    #[test]
    fn rgcn_forward_produces_expected_shape() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        // two relations, one graph each
        let rel_a = CsrGraph::from_edges(4, &[(0, 1, 1.0), (2, 3, 1.0)]).row_normalize();
        let rel_b = CsrGraph::from_edges(4, &[(0, 2, 1.0), (1, 3, 1.0)]).row_normalize();
        let rel_graphs: Vec<&CsrGraph> = vec![&rel_a, &rel_b];

        let model: RGcn<B> =
            RGcnConfig::new(/*in*/ 3, /*h*/ 6, /*out*/ 2, /*rels*/ 2).init(&device);
        let x_data: Vec<f32> = (0..(4 * 3)).map(|i| (i as f32) * 0.1).collect();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats(x_data.as_slice(), &device).reshape([4, 3]);

        let y = model.forward(x, &rel_graphs);
        assert_eq!(y.dims(), [4, 2]);
    }

    #[test]
    fn rgcn_gradients_flow() {
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let rel_a = CsrGraph::from_edges(4, &[(0, 1, 1.0), (2, 3, 1.0)]).row_normalize();
        let rel_b = CsrGraph::from_edges(4, &[(0, 2, 1.0), (1, 3, 1.0)]).row_normalize();
        let rel_graphs: Vec<&CsrGraph> = vec![&rel_a, &rel_b];

        let model: RGcn<BA> = RGcnConfig::new(3, 6, 2, 2).init(&device);
        let x_data: Vec<f32> = (0..(4 * 3)).map(|i| (i as f32) * 0.1).collect();
        let x: Tensor<BA, 2> =
            Tensor::<BA, 1>::from_floats(x_data.as_slice(), &device).reshape([4, 3]);

        let loss = model.forward(x, &rel_graphs).sum();
        let grads = loss.backward();

        // self transforms get gradients
        assert!(
            model.layer1_self.weight.grad(&grads).is_some(),
            "layer1_self should have a gradient"
        );
        assert!(
            model.layer2_self.weight.grad(&grads).is_some(),
            "layer2_self should have a gradient"
        );
        // every per-relation linear gets a gradient
        for (r, lin) in model.layer1_rels.iter().enumerate() {
            assert!(
                lin.weight.grad(&grads).is_some(),
                "layer1_rels[{r}] should have a gradient"
            );
        }
        for (r, lin) in model.layer2_rels.iter().enumerate() {
            assert!(
                lin.weight.grad(&grads).is_some(),
                "layer2_rels[{r}] should have a gradient"
            );
        }
    }

    #[test]
    fn rgcn_learns_relation_signal() {
        // 30 nodes, two communities of 15; relation 0 is intra-cluster-A only, relation 1
        // intra-cluster-B only. Random Gaussian features carry no class signal, so beating 50%
        // requires the per-relation routing: if the per-relation weights were accidentally tied,
        // nothing could be learned from edge type and the loss wouldn't fall
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        use burn::optim::{AdamConfig, GradientsParams, Optimizer};
        use burn::prelude::ElementConversion;
        use burn::tensor::{Distribution, Int, activation::log_softmax};

        let device = burn::backend::ndarray::NdArrayDevice::default();
        let n = 30;
        let half = n / 2;

        let mut rel_a_edges = Vec::new();
        for i in 0..half {
            for j in (i + 1)..half {
                rel_a_edges.push((i as u32, j as u32, 1.0));
            }
        }
        let mut rel_b_edges = Vec::new();
        for i in half..n {
            for j in (i + 1)..n {
                rel_b_edges.push((i as u32, j as u32, 1.0));
            }
        }
        let rel_a = CsrGraph::from_edges(n, &rel_a_edges).row_normalize();
        let rel_b = CsrGraph::from_edges(n, &rel_b_edges).row_normalize();
        let rel_graphs: Vec<&CsrGraph> = vec![&rel_a, &rel_b];

        let labels: Vec<i32> = (0..n).map(|i| if i < half { 0 } else { 1 }).collect();
        let labels_t: Tensor<BA, 1, Int> = Tensor::from_ints(labels.as_slice(), &device);

        let in_dim = 8;
        let x: Tensor<BA, 2> = Tensor::random([n, in_dim], Distribution::Normal(0.0, 1.0), &device);

        let mut model: RGcn<BA> = RGcnConfig::new(in_dim, 16, 2, 2).init(&device);
        let mut optim = AdamConfig::new().init();
        let mut first_loss = 0.0f32;
        let mut last_loss = 0.0f32;
        for epoch in 1..=80 {
            let logits = model.forward(x.clone(), &rel_graphs);
            let log_probs = log_softmax(logits, 1);
            let nll = log_probs
                .gather(1, labels_t.clone().unsqueeze_dim(1))
                .neg()
                .mean();
            let loss_val: f32 = nll.clone().into_scalar().elem();
            if epoch == 1 {
                first_loss = loss_val;
            }
            last_loss = loss_val;
            let grads = nll.backward();
            let grads = GradientsParams::from_grads(grads, &model);
            model = optim.step(1e-2, model, grads);
        }
        assert!(
            last_loss < first_loss * 0.5,
            "R-GCN didn't learn relational signal: first={first_loss:.4}, last={last_loss:.4}"
        );
    }

    #[test]
    fn gat_forward_produces_expected_shape() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        // no row normalization: GAT computes its own attention; CSR edge weights are unused
        let adj = CsrGraph::from_edges(4, &[(0, 1, 1.0), (1, 2, 1.0), (2, 3, 1.0), (0, 3, 1.0)])
            .add_self_loops();

        let model: GatMultiHead<B> = GatMultiHeadConfig::new(3, 8, 2)
            .with_num_heads_hidden(1)
            .init(&device);
        let x_data: Vec<f32> = (0..(4 * 3)).map(|i| (i as f32) * 0.1).collect();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats(x_data.as_slice(), &device).reshape([4, 3]);

        let y = model.forward(x, &adj);
        assert_eq!(y.dims(), [4, 2]);
    }

    #[test]
    fn gat_gradients_flow() {
        // select + select_assign (twice each: softmax denominators and the weighted aggregate) with
        // Linear + LeakyReLU + exp + element-wise mul; every projection must get a gradient
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(4, &[(0, 1, 1.0), (1, 2, 1.0), (2, 3, 1.0), (0, 3, 1.0)])
            .add_self_loops();
        let model: GatMultiHead<BA> = GatMultiHeadConfig::new(3, 8, 2)
            .with_num_heads_hidden(1)
            .init(&device);
        let x_data: Vec<f32> = (0..(4 * 3)).map(|i| (i as f32) * 0.1).collect();
        let x: Tensor<BA, 2> =
            Tensor::<BA, 1>::from_floats(x_data.as_slice(), &device).reshape([4, 3]);

        let loss = model.forward(x, &adj).sum();
        let grads = loss.backward();

        let (l1, l2) = (&model.layer1_heads[0], &model.layer2_heads[0]);
        for (name, w) in [
            ("layer1 w", &l1.w.weight),
            ("layer1 a_src", &l1.a_src.weight),
            ("layer1 a_dst", &l1.a_dst.weight),
            ("layer2 w", &l2.w.weight),
            ("layer2 a_src", &l2.a_src.weight),
            ("layer2 a_dst", &l2.a_dst.weight),
        ] {
            assert!(w.grad(&grads).is_some(), "{name} should have a gradient");
        }
    }

    #[test]
    fn gat_trains_on_sbm_smoke() {
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        use burn::optim::{AdamConfig, GradientsParams, Optimizer};
        use burn::prelude::ElementConversion;
        use burn::tensor::{Distribution, Int, activation::log_softmax};

        let device = burn::backend::ndarray::NdArrayDevice::default();
        let n = 60;
        let half = n / 2;
        let mut edges = Vec::new();
        for i in 0..half {
            for j in (i + 1)..half {
                edges.push((i as u32, j as u32, 1.0));
            }
        }
        for i in half..n {
            for j in (i + 1)..n {
                edges.push((i as u32, j as u32, 1.0));
            }
        }
        edges.push((0u32, half as u32, 1.0));
        let adj = CsrGraph::from_edges(n, &edges).add_self_loops();

        let labels: Vec<i32> = (0..n).map(|i| if i < half { 0 } else { 1 }).collect();
        let labels_t: Tensor<BA, 1, Int> = Tensor::from_ints(labels.as_slice(), &device);

        let in_dim = 8;
        let x: Tensor<BA, 2> = Tensor::random([n, in_dim], Distribution::Normal(0.0, 1.0), &device);

        let mut model: GatMultiHead<BA> = GatMultiHeadConfig::new(in_dim, 16, 2)
            .with_num_heads_hidden(1)
            .init(&device);
        let mut optim = AdamConfig::new().init();
        let mut first_loss = 0.0f32;
        let mut last_loss = 0.0f32;
        for epoch in 1..=80 {
            let logits = model.forward(x.clone(), &adj);
            let log_probs = log_softmax(logits, 1);
            let nll = log_probs
                .gather(1, labels_t.clone().unsqueeze_dim(1))
                .neg()
                .mean();
            let loss_val: f32 = nll.clone().into_scalar().elem();
            if epoch == 1 {
                first_loss = loss_val;
            }
            last_loss = loss_val;
            let grads = nll.backward();
            let grads = GradientsParams::from_grads(grads, &model);
            model = optim.step(5e-3, model, grads);
        }
        assert!(
            last_loss < first_loss * 0.5,
            "GAT didn't learn: first={first_loss:.4}, last={last_loss:.4}"
        );
    }

    #[test]
    fn graphsage_trains_on_sbm_smoke() {
        // short training run on an SBM-like graph with random features, 2 communities: the loss
        // must fall over 50 epochs (catches a forward pass that ignores its input, e.g. all zeros
        // from a shape mismatch)
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        use burn::optim::{AdamConfig, GradientsParams, Optimizer};
        use burn::prelude::ElementConversion;
        use burn::tensor::{Distribution, Int, activation::log_softmax};

        let device = burn::backend::ndarray::NdArrayDevice::default();
        let n = 60;
        let half = n / 2;
        // two cliques joined by one edge
        let mut edges = Vec::new();
        for i in 0..half {
            for j in (i + 1)..half {
                edges.push((i as u32, j as u32, 1.0));
            }
        }
        for i in half..n {
            for j in (i + 1)..n {
                edges.push((i as u32, j as u32, 1.0));
            }
        }
        edges.push((0u32, half as u32, 1.0));

        let adj = CsrGraph::from_edges(n, &edges)
            .add_self_loops()
            .row_normalize();
        let labels: Vec<i32> = (0..n).map(|i| if i < half { 0 } else { 1 }).collect();
        let labels_t: Tensor<BA, 1, Int> = Tensor::from_ints(labels.as_slice(), &device);

        let in_dim = 8;
        let x: Tensor<BA, 2> = Tensor::random([n, in_dim], Distribution::Normal(0.0, 1.0), &device);

        let mut model: GraphSage<BA> = GraphSageConfig::new(in_dim, 16, 2).init(&device);
        let mut optim = AdamConfig::new().init();
        let mut first_loss = 0.0f32;
        let mut last_loss = 0.0f32;
        for epoch in 1..=50 {
            let logits = model.forward(x.clone(), &adj);
            let log_probs = log_softmax(logits, 1);
            let nll = log_probs
                .gather(1, labels_t.clone().unsqueeze_dim(1))
                .neg()
                .mean();
            let loss_val: f32 = nll.clone().into_scalar().elem();
            if epoch == 1 {
                first_loss = loss_val;
            }
            last_loss = loss_val;
            let grads = nll.backward();
            let grads = GradientsParams::from_grads(grads, &model);
            model = optim.step(1e-2, model, grads);
        }
        assert!(
            last_loss < first_loss * 0.5,
            "GraphSAGE didn't learn: first={first_loss:.4}, last={last_loss:.4}"
        );
    }

    // --- multi-head GAT tests ------------------------------------

    #[test]
    fn gat_multi_head_forward_produces_expected_shape() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(
            5,
            &[
                (0, 1, 1.0),
                (1, 2, 1.0),
                (2, 3, 1.0),
                (3, 4, 1.0),
                (0, 4, 1.0),
            ],
        )
        .add_self_loops();
        // 4 hidden heads of dim 8 => hidden_dim 32; 1 output head keeps out_dim
        let model: GatMultiHead<B> = GatMultiHeadConfig::new(3, 8, 2)
            .with_num_heads_hidden(4)
            .with_num_heads_out(1)
            .init(&device);

        let x_data: Vec<f32> = (0..(5 * 3)).map(|i| (i as f32) * 0.1).collect();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats(x_data.as_slice(), &device).reshape([5, 3]);
        let y = model.forward(x, &adj);
        assert_eq!(y.dims(), [5, 2], "multi-head GAT should preserve out_dim");
    }

    #[test]
    fn gat_multi_head_with_multiple_output_heads_averages() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj =
            CsrGraph::from_edges(4, &[(0, 1, 1.0), (1, 2, 1.0), (2, 3, 1.0)]).add_self_loops();
        // 2 output heads still give [N, out_dim] (mean)
        let model: GatMultiHead<B> = GatMultiHeadConfig::new(2, 4, 3)
            .with_num_heads_hidden(2)
            .with_num_heads_out(2)
            .init(&device);
        let x: Tensor<B, 2> = Tensor::<B, 1>::from_floats(
            [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8].as_slice(),
            &device,
        )
        .reshape([4, 2]);
        let y = model.forward(x, &adj);
        assert_eq!(y.dims(), [4, 3]);
    }

    #[test]
    fn gat_multi_head_gradients_flow() {
        // every head must get gradients through `W`, `a_src` and `a_dst`; checked on layer 1 (8
        // heads x 3 tensors = 24 params)
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(4, &[(0, 1, 1.0), (1, 2, 1.0), (2, 3, 1.0), (0, 3, 1.0)])
            .add_self_loops();
        let model: GatMultiHead<BA> = GatMultiHeadConfig::new(2, 4, 2)
            .with_num_heads_hidden(3)
            .with_num_heads_out(1)
            .init(&device);
        let x: Tensor<BA, 2> = Tensor::<BA, 1>::from_floats(
            [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8].as_slice(),
            &device,
        )
        .reshape([4, 2]);
        let y = model.forward(x, &adj);
        let loss = y.sum();
        let grads = loss.backward();
        // every layer-1 head's W has a gradient
        for (i, head) in model.layer1_heads.iter().enumerate() {
            let g = head.w.weight.grad(&grads);
            assert!(
                g.is_some(),
                "layer1 head {i} W received no gradient - multi-head dispatch broken"
            );
        }
        // every layer-2 head's W has a gradient
        for (i, head) in model.layer2_heads.iter().enumerate() {
            let g = head.w.weight.grad(&grads);
            assert!(g.is_some(), "layer2 head {i} W received no gradient");
        }
    }

    #[test]
    fn gat_multi_head_learns_on_sbm_smoke() {
        // the one-head GAT test's SBM with 4 hidden heads; the loss must drop >= 50% as with one
        // head
        use burn::optim::{AdamConfig, GradientsParams, Optimizer};
        use burn::prelude::ElementConversion;
        use burn::tensor::Int;
        use burn::tensor::activation::log_softmax;
        use rand::{RngExt, SeedableRng};

        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let mut rng = rand::rngs::StdRng::seed_from_u64(91);
        let block_size = 16;
        let n = block_size * 2;
        let mut edges: Vec<(u32, u32, f32)> = Vec::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let same = (i / block_size) == (j / block_size);
                let p = if same { 0.15 } else { 0.01 };
                if rng.random::<f32>() < p {
                    edges.push((i as u32, j as u32, 1.0));
                }
            }
        }
        let adj = CsrGraph::from_edges(n, &edges).add_self_loops();
        let feat_dim = 4;
        let mut features = vec![0.0f32; n * feat_dim];
        for i in 0..n {
            let block = (i / block_size) as f32;
            features[i * feat_dim] = block;
            for f in 1..feat_dim {
                features[i * feat_dim + f] = rng.random::<f32>() * 0.05;
            }
        }
        let labels: Vec<i32> = (0..n).map(|i| (i / block_size) as i32).collect();
        let labels_t = Tensor::<BA, 1, Int>::from_ints(labels.as_slice(), &device);
        let x: Tensor<BA, 2> =
            Tensor::<BA, 1>::from_floats(features.as_slice(), &device).reshape([n, feat_dim]);

        let mut model: GatMultiHead<BA> = GatMultiHeadConfig::new(feat_dim, 6, 2)
            .with_num_heads_hidden(4)
            .with_num_heads_out(1)
            .init(&device);
        let mut optim = AdamConfig::new().init();
        let mut first_loss = f32::NAN;
        let mut last_loss = f32::NAN;
        for epoch in 1..=60 {
            let logits = model.forward(x.clone(), &adj);
            let log_probs = log_softmax(logits, 1);
            let nll = log_probs
                .gather(1, labels_t.clone().unsqueeze_dim(1))
                .neg()
                .mean();
            let loss_val: f32 = nll.clone().into_scalar().elem();
            if epoch == 1 {
                first_loss = loss_val;
            }
            last_loss = loss_val;
            let grads = nll.backward();
            let grads = GradientsParams::from_grads(grads, &model);
            model = optim.step(1e-2, model, grads);
        }
        assert!(
            last_loss < first_loss * 0.5,
            "multi-head GAT didn't learn: first={first_loss:.4}, last={last_loss:.4}"
        );
    }
}
