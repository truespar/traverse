//! Sparse-graph math kernels.
//!
//! * [`RowBlock`]: a range of CSR rows ready for message passing, holding each stored entry's
//!   output row (within the block) and input node as 1-D index tensors, plus its weight.
//!   [`RowBlock::spmm`] gathers input rows with `select` and adds them into output rows with
//!   `select_assign`: O(E*D) memory, Burn-native ops only, so autograd works on every backend.
//!   (Burn's `Backward` trait is crate-private, so a custom fused kernel couldn't be trained
//!   through.) A block over every row is the whole graph; blocks over parts of it bound the memory
//!   of one step when inferring on large graphs (`AnyEncoder::forward_blocked`).
//! * [`spmm_csr_sparse`] / [`spmm_csr_sparse_weighted`]: the whole graph as one block.
//! * [`spmm_naive`]: dense matmul, O(N^2) memory; the tests' known-good baseline.

use std::ops::Range;

use crate::graph::CsrGraph;
use burn::tensor::{IndexingUpdateOp, Int, Tensor, backend::Backend};

/// Rows `start..start + len` of a CSR adjacency, on a device. See the module docs.
#[derive(Debug, Clone)]
pub struct RowBlock<B: Backend> {
    /// First row (node) of the block.
    pub start: usize,
    /// Rows in the block.
    pub len: usize,
    /// Position of the block's first entry among the graph's stored entries (CSR order), to line
    /// up per-entry masks.
    pub entry_start: usize,
    /// Stored entries in the block.
    pub entries: usize,
    /// `(output row within the block, input node, weight)` per entry; `None` for an empty block,
    /// which needs no device memory.
    index: Option<(Tensor<B, 1, Int>, Tensor<B, 1, Int>, Tensor<B, 1>)>,
}

impl<B: Backend> RowBlock<B> {
    /// The entries of `rows`.
    pub fn new(graph: &CsrGraph, rows: Range<usize>, device: &B::Device) -> Self {
        let raw = graph.raw();
        assert!(
            rows.start <= rows.end && rows.end <= raw.num_nodes,
            "rows {rows:?} out of range for a {}-node graph",
            raw.num_nodes
        );
        let lo = raw.row_ptr[rows.start] as usize;
        let hi = raw.row_ptr[rows.end] as usize;
        let index = (hi > lo).then(|| {
            let mut out: Vec<i32> = Vec::with_capacity(hi - lo);
            for (i, r) in rows.clone().enumerate() {
                let n = (raw.row_ptr[r + 1] - raw.row_ptr[r]) as usize;
                out.extend(std::iter::repeat_n(i as i32, n));
            }
            let input: Vec<i32> = raw.col_idx[lo..hi].iter().map(|&c| c as i32).collect();
            (
                Tensor::from_ints(out.as_slice(), device),
                Tensor::from_ints(input.as_slice(), device),
                Tensor::from_floats(&raw.values[lo..hi], device),
            )
        });
        Self {
            start: rows.start,
            len: rows.len(),
            entry_start: lo,
            entries: hi - lo,
            index,
        }
    }

    /// Every row: the whole graph.
    pub fn all(graph: &CsrGraph, device: &B::Device) -> Self {
        Self::new(graph, 0..graph.num_nodes(), device)
    }

    /// The same rows and entries weighted by `graph`, which must have this block's graph's layout
    /// (a re-normalized copy of it); the index tensors are shared.
    pub fn reweighted(&self, graph: &CsrGraph, device: &B::Device) -> Self {
        let raw = graph.raw();
        let lo = raw.row_ptr[self.start] as usize;
        let hi = raw.row_ptr[self.start + self.len] as usize;
        assert!(
            lo == self.entry_start && hi - lo == self.entries,
            "reweighting needs the same CSR layout"
        );
        Self {
            index: self.index.as_ref().map(|(out, input, _)| {
                (
                    out.clone(),
                    input.clone(),
                    Tensor::from_floats(&raw.values[lo..hi], device),
                )
            }),
            ..*self
        }
    }

    /// Output row within the block and input node of every entry, `None` if there are none.
    pub fn indices(&self) -> Option<(Tensor<B, 1, Int>, Tensor<B, 1, Int>)> {
        self.index
            .as_ref()
            .map(|(out, input, _)| (out.clone(), input.clone()))
    }

    /// The block's rows of `A @ X`: `[len, D]` from `x: [N, D]`.
    pub fn spmm(&self, x: Tensor<B, 2>) -> Tensor<B, 2> {
        self.aggregate(x, None)
    }

    /// [`spmm`](Self::spmm) with each entry's weight times `mask` (`[entries]`, CSR order). The mask
    /// carries gradients, so an edge-importance model can learn it.
    pub fn spmm_masked(&self, x: Tensor<B, 2>, mask: Tensor<B, 1>) -> Tensor<B, 2> {
        self.aggregate(x, Some(mask))
    }

    fn aggregate(&self, x: Tensor<B, 2>, mask: Option<Tensor<B, 1>>) -> Tensor<B, 2> {
        let d = x.dims()[1];
        let zeros = Tensor::<B, 2>::zeros([self.len, d], &x.device());
        let Some((out, input, weights)) = &self.index else {
            return zeros;
        };
        let w = match mask {
            Some(m) => weights.clone() * m,
            None => weights.clone(),
        };
        let messages =
            x.select(0, input.clone()) * w.unsqueeze_dim::<2>(1).expand([self.entries, d]);
        zeros.select_assign(0, out.clone(), messages, IndexingUpdateOp::Add)
    }
}

/// Sparse-dense multiplication `Y = A @ X`, `A` the CSR graph, `X` a dense `[N, D]` feature matrix;
/// `Y` is `[N, D]`. One message-passing step: each output row is the weighted sum of its neighbors'
/// rows in `X`, weights from `A`'s edge values. **Naive baseline**: materializes `A` densely and
/// calls `matmul`, O(N^2) memory.
pub fn spmm_naive<B: Backend>(adj: &CsrGraph, x: Tensor<B, 2>) -> Tensor<B, 2> {
    assert_eq!(
        adj.num_nodes(),
        x.dims()[0],
        "graph has {} nodes but feature matrix has {} rows",
        adj.num_nodes(),
        x.dims()[0]
    );
    let device = x.device();
    let dense_adj = adj.to_dense_tensor::<B>(&device);
    dense_adj.matmul(x)
}

/// [`spmm_naive`]'s result in O(E*D) memory: the whole graph as one [`RowBlock`].
pub fn spmm_csr_sparse<B: Backend>(graph: &CsrGraph, x: Tensor<B, 2>) -> Tensor<B, 2> {
    check_rows(graph, &x);
    RowBlock::all(graph, &x.device()).spmm(x)
}

/// [`spmm_csr_sparse`] with each stored entry's weight times `edge_mask[e]` (`[num_edges]`, CSR
/// order), as if the adjacency values were `values[e] * edge_mask[e]`. The mask flows through
/// autograd, so an upstream model (PGExplainer's edge scorer, say) can learn it.
pub fn spmm_csr_sparse_weighted<B: Backend>(
    graph: &CsrGraph,
    x: Tensor<B, 2>,
    edge_mask: Tensor<B, 1>,
) -> Tensor<B, 2> {
    check_rows(graph, &x);
    assert_eq!(
        edge_mask.dims()[0],
        graph.num_edges(),
        "edge_mask must have one entry per CSR edge ({} expected, {} given)",
        graph.num_edges(),
        edge_mask.dims()[0]
    );
    RowBlock::all(graph, &x.device()).spmm_masked(x, edge_mask)
}

fn check_rows<B: Backend>(graph: &CsrGraph, x: &Tensor<B, 2>) {
    assert_eq!(
        graph.num_nodes(),
        x.dims()[0],
        "graph has {} nodes but feature matrix has {} rows",
        graph.num_nodes(),
        x.dims()[0]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CsrGraph;

    type B = burn::backend::NdArray;

    fn values<const D: usize>(t: Tensor<B, D>) -> Vec<f32> {
        t.into_data().iter::<f32>().collect()
    }

    #[test]
    fn spmm_triangle_against_handcomputed() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0), (0, 2, 1.0)]);
        // node features 0: [1, 0], 1: [0, 1], 2: [1, 1]
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats([1.0, 0.0, 0.0, 1.0, 1.0, 1.0], &device).reshape([3, 2]);
        // A @ X without self-loops: each node sums the other two rows
        let expected = vec![1.0, 2.0, 2.0, 1.0, 1.0, 1.0];
        assert_eq!(values(spmm_naive(&adj, x.clone())), expected);
        assert_eq!(values(spmm_csr_sparse(&adj, x)), expected);
    }

    #[test]
    fn spmm_with_self_loops_includes_self() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(2, &[(0, 1, 1.0)]).add_self_loops();
        let x: Tensor<B, 2> = Tensor::<B, 1>::from_floats([2.0, 3.0], &device).reshape([2, 1]);
        assert_eq!(values(spmm_naive(&adj, x)), vec![5.0, 5.0]);
    }

    #[test]
    fn spmm_csr_sparse_matches_naive_triangle() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0), (0, 2, 1.0)])
            .add_self_loops()
            .symmetric_normalize();
        let x_data: Vec<f32> = (0..6).map(|v| v as f32).collect();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats(x_data.as_slice(), &device).reshape([3, 2]);
        let naive = values(spmm_naive(&adj, x.clone()));
        let sparse = values(spmm_csr_sparse(&adj, x));
        for (a, b) in naive.iter().zip(&sparse) {
            assert!((a - b).abs() < 1e-5, "naive {a} vs sparse {b}");
        }
    }

    #[test]
    fn blocks_of_rows_stack_up_to_the_whole_product() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        // a 6-node graph with an isolated node (5): its block is empty
        let adj = CsrGraph::from_edges(6, &[(0, 1, 1.0), (1, 2, 2.0), (2, 3, 1.0), (0, 4, 0.5)])
            .symmetric_normalize();
        let x_data: Vec<f32> = (0..18).map(|v| (v as f32) * 0.25 - 1.0).collect();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats(x_data.as_slice(), &device).reshape([6, 3]);
        let whole = values(spmm_naive(&adj, x.clone()));
        let mut stacked = Vec::new();
        for rows in [0..2, 2..2, 2..5, 5..6] {
            let block = RowBlock::<B>::new(&adj, rows.clone(), &device);
            let part = block.spmm(x.clone());
            assert_eq!(part.dims(), [rows.len(), 3]);
            stacked.extend(values(part));
        }
        for (a, b) in whole.iter().zip(&stacked) {
            assert!((a - b).abs() < 1e-5, "whole {a} vs blocks {b}");
        }
    }

    #[test]
    fn a_masked_block_reads_its_slice_of_the_mask() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0)]);
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats([1.0, 10.0, 100.0], &device).reshape([3, 1]);
        // entries in CSR order: 0->1, 1->0, 1->2, 2->1; drop 1->2
        let mask = [1.0, 1.0, 0.0, 1.0];
        let full =
            spmm_csr_sparse_weighted(&adj, x.clone(), Tensor::<B, 1>::from_floats(mask, &device));
        assert_eq!(values(full), vec![10.0, 1.0, 10.0]);
        let block = RowBlock::<B>::new(&adj, 1..3, &device);
        let slice = &mask[block.entry_start..block.entry_start + block.entries];
        let part = block.spmm_masked(x, Tensor::<B, 1>::from_floats(slice, &device));
        assert_eq!(values(part), vec![1.0, 10.0]);
    }

    #[test]
    fn spmm_csr_sparse_autograd_gradient_flows() {
        // a backward pass through the SpMM gives every input row a gradient
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0), (0, 2, 1.0)])
            .add_self_loops()
            .symmetric_normalize();
        let x_data: Vec<f32> = (0..6).map(|v| v as f32).collect();
        let x: Tensor<BA, 2> = Tensor::<BA, 1>::from_floats(x_data.as_slice(), &device)
            .reshape([3, 2])
            .require_grad();
        let grads = spmm_csr_sparse(&adj, x.clone()).sum().backward();
        let g: Vec<f32> = x
            .grad(&grads)
            .expect("x must have a gradient")
            .into_data()
            .iter::<f32>()
            .collect();
        // all of Y is summed and every X row feeds several Y rows: no zero gradient
        assert_eq!(g.len(), 6);
        for (i, v) in g.iter().enumerate() {
            assert!(v.abs() > 1e-7, "x.grad[{i}] = {v} should be non-zero");
        }
    }

    #[test]
    fn spmm_normalized_triangle_matches_dense() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0), (0, 2, 1.0)])
            .add_self_loops()
            .symmetric_normalize();
        let x_data: Vec<f32> = (0..6).map(|v| v as f32).collect();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats(x_data.as_slice(), &device).reshape([3, 2]);
        let expected = values(adj.to_dense_tensor::<B>(&device).matmul(x.clone()));
        let got = values(spmm_naive(&adj, x));
        for (e, g) in expected.iter().zip(&got) {
            assert!((e - g).abs() < 1e-6, "mismatch: expected {e}, got {g}");
        }
    }
}
