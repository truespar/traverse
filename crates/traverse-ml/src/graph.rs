//! Sparse graph in Compressed Sparse Row (CSR) format: O(1) access to a node's
//! neighbor list in O(N + E) memory. For N nodes and E directed edges:
//!
//! * `row_ptr: [N+1]`: cumulative edge counts; node `i`'s neighbors are
//!   `col_idx[row_ptr[i] .. row_ptr[i+1]]`, `row_ptr[0] = 0`, `row_ptr[N] = E`.
//! * `col_idx: [E]`: destination indices, sorted within each row.
//! * `values:  [E]`: edge weights (usually 1.0 unweighted, or
//!   `1 / sqrt(deg(i) * deg(j))` after `symmetric_normalize`).
//!
//! Storage only; the math (SpMM, scatter-add, ...) is in `crate::sparse`.

use burn::tensor::{Tensor, backend::Backend};

/// Undirected, weighted graph in CSR, built from an edge list via [`CsrGraph::from_edges`]. Edges
/// are bidirectional: `(i, j)` stores both `(i, j)` and `(j, i)`. Self-loops are kept as given;
/// [`add_self_loops`](Self::add_self_loops) ensures every node has one.
#[derive(Debug, Clone)]
pub struct CsrGraph {
    num_nodes: usize,
    row_ptr: Vec<u32>,
    col_idx: Vec<u32>,
    values: Vec<f32>,
}

impl CsrGraph {
    /// Number of nodes.
    pub fn num_nodes(&self) -> usize {
        self.num_nodes
    }

    /// Directed edge count (both directions of each undirected edge; self-loops once).
    pub fn num_edges(&self) -> usize {
        self.col_idx.len()
    }

    /// `node`'s neighbors as `(target, weight)`.
    pub fn neighbors(&self, node: usize) -> impl Iterator<Item = (u32, f32)> + '_ {
        let start = self.row_ptr[node] as usize;
        let end = self.row_ptr[node + 1] as usize;
        self.col_idx[start..end]
            .iter()
            .copied()
            .zip(self.values[start..end].iter().copied())
    }

    /// `node`'s out-degree.
    pub fn degree(&self, node: usize) -> usize {
        (self.row_ptr[node + 1] - self.row_ptr[node]) as usize
    }

    /// CSR from an undirected edge list: each `(src, dst, w)` gives *two* directed entries,
    /// `(src->dst, w)` and `(dst->src, w)`; a self-loop (`src == dst`) gives one. Nodes must be in
    /// `0..num_nodes`.
    pub fn from_edges(num_nodes: usize, edges: &[(u32, u32, f32)]) -> Self {
        let mut degrees = vec![0u32; num_nodes];
        for &(src, dst, _) in edges {
            assert!(
                (src as usize) < num_nodes && (dst as usize) < num_nodes,
                "edge ({src}, {dst}) out of range for {num_nodes}-node graph"
            );
            degrees[src as usize] += 1;
            if src != dst {
                degrees[dst as usize] += 1;
            }
        }

        // row_ptr from cumulative degree
        let mut row_ptr = Vec::with_capacity(num_nodes + 1);
        row_ptr.push(0);
        let mut sum = 0u32;
        for &d in &degrees {
            sum += d;
            row_ptr.push(sum);
        }
        let total = sum as usize;

        // fill col_idx + values with a per-row cursor
        let mut col_idx = vec![0u32; total];
        let mut values = vec![0.0f32; total];
        let mut cursor: Vec<u32> = row_ptr[..num_nodes].to_vec();
        for &(src, dst, w) in edges {
            let s = src as usize;
            let pos = cursor[s] as usize;
            col_idx[pos] = dst;
            values[pos] = w;
            cursor[s] += 1;
            if src != dst {
                let d = dst as usize;
                let pos = cursor[d] as usize;
                col_idx[pos] = src;
                values[pos] = w;
                cursor[d] += 1;
            }
        }

        // sort each row by column index: a canonical CSR, for deterministic equality and faster
        // kernels
        for i in 0..num_nodes {
            let start = row_ptr[i] as usize;
            let end = row_ptr[i + 1] as usize;
            let mut pairs: Vec<(u32, f32)> = col_idx[start..end]
                .iter()
                .copied()
                .zip(values[start..end].iter().copied())
                .collect();
            pairs.sort_by_key(|&(c, _)| c);
            for (k, (c, v)) in pairs.into_iter().enumerate() {
                col_idx[start + k] = c;
                values[start + k] = v;
            }
        }

        Self {
            num_nodes,
            row_ptr,
            col_idx,
            values,
        }
    }

    /// CSR from directed entries `(row, col, weight)`: row `row` reads column `col`. Unlike
    /// [`from_edges`](Self::from_edges) nothing is mirrored. Rows come out sorted by column, ties
    /// in the order given, so two calls with the same `(row, col)` sequence share a layout.
    pub fn from_entries(num_nodes: usize, entries: &[(u32, u32, f32)]) -> Self {
        let mut sorted: Vec<(u32, u32, f32)> = entries.to_vec();
        for &(r, c, _) in &sorted {
            assert!(
                (r as usize) < num_nodes && (c as usize) < num_nodes,
                "entry ({r}, {c}) out of range for {num_nodes}-node graph"
            );
        }
        sorted.sort_by_key(|&(r, c, _)| (r, c));
        let mut row_ptr = vec![0u32; num_nodes + 1];
        for &(r, _, _) in &sorted {
            row_ptr[r as usize + 1] += 1;
        }
        for i in 0..num_nodes {
            row_ptr[i + 1] += row_ptr[i];
        }
        Self {
            num_nodes,
            row_ptr,
            col_idx: sorted.iter().map(|e| e.1).collect(),
            values: sorted.iter().map(|e| e.2).collect(),
        }
    }

    /// Adds a unit self-loop to every node lacking one: the standard GCN preprocessing (Kipf &
    /// Welling 2017 section 2.1); without it a GCN can't propagate a node's own features.
    pub fn add_self_loops(self) -> Self {
        let n = self.num_nodes;
        // walk the CSR, inserting a missing self-loop at its sorted position
        let mut new_row_ptr = Vec::with_capacity(n + 1);
        let mut new_col_idx: Vec<u32> = Vec::with_capacity(self.col_idx.len() + n);
        let mut new_values: Vec<f32> = Vec::with_capacity(self.values.len() + n);
        new_row_ptr.push(0);
        for i in 0..n {
            let start = self.row_ptr[i] as usize;
            let end = self.row_ptr[i + 1] as usize;
            let mut inserted = false;
            for k in start..end {
                let c = self.col_idx[k];
                if !inserted && c >= i as u32 {
                    if c != i as u32 {
                        new_col_idx.push(i as u32);
                        new_values.push(1.0);
                    }
                    inserted = true;
                }
                new_col_idx.push(c);
                new_values.push(self.values[k]);
            }
            if !inserted {
                new_col_idx.push(i as u32);
                new_values.push(1.0);
            }
            new_row_ptr.push(new_col_idx.len() as u32);
        }
        Self {
            num_nodes: n,
            row_ptr: new_row_ptr,
            col_idx: new_col_idx,
            values: new_values,
        }
    }

    /// Row normalization: each row sums to 1 (`A[i,j] / row_sum(i)`); unweighted that's
    /// `1/degree(i)` per edge, so SpMM with it is **mean pooling** over neighbors, the
    /// GraphSAGE-mean aggregator. Isolated nodes (row sum 0) stay zero, so SpMM gives them the zero
    /// vector, GraphSAGE's documented behavior for nodes left without neighbors.
    pub fn row_normalize(mut self) -> Self {
        let n = self.num_nodes;
        for i in 0..n {
            let s = self.row_ptr[i] as usize;
            let e = self.row_ptr[i + 1] as usize;
            let row_sum: f32 = self.values[s..e].iter().sum();
            if row_sum == 0.0 {
                continue;
            }
            let inv = 1.0 / row_sum;
            for k in s..e {
                self.values[k] *= inv;
            }
        }
        self
    }

    /// Symmetric normalization: `A[i,j]` becomes `A[i,j] / sqrt(deg(i) * deg(j))`, `deg(i)` being
    /// `i`'s row sum *in the current CSR* (so call it **after** `add_self_loops` for a Kipf-style
    /// GCN).
    pub fn symmetric_normalize(mut self) -> Self {
        let n = self.num_nodes;
        // row sums: weight sums for weighted graphs, not edge counts (the standard A_norm formula)
        let mut row_sum = vec![0.0f32; n];
        for i in 0..n {
            let s = self.row_ptr[i] as usize;
            let e = self.row_ptr[i + 1] as usize;
            row_sum[i] = self.values[s..e].iter().sum();
        }
        let d_inv_sqrt: Vec<f32> = row_sum
            .iter()
            .map(|&d| if d > 0.0 { 1.0 / d.sqrt() } else { 0.0 })
            .collect();
        for i in 0..n {
            let s = self.row_ptr[i] as usize;
            let e = self.row_ptr[i + 1] as usize;
            for k in s..e {
                let j = self.col_idx[k] as usize;
                self.values[k] *= d_inv_sqrt[i] * d_inv_sqrt[j];
            }
        }
        self
    }

    /// The CSR as a dense `[N, N]` tensor. **O(N^2) memory**: for tests and the naive SpMM baseline
    /// only; real workloads use `crate::sparse`.
    pub fn to_dense_tensor<B: Backend>(&self, device: &B::Device) -> Tensor<B, 2> {
        let n = self.num_nodes;
        let mut dense = vec![0.0f32; n * n];
        for i in 0..n {
            let s = self.row_ptr[i] as usize;
            let e = self.row_ptr[i + 1] as usize;
            for k in s..e {
                let j = self.col_idx[k] as usize;
                dense[i * n + j] = self.values[k];
            }
        }
        Tensor::<B, 1>::from_floats(dense.as_slice(), device).reshape([n, n])
    }

    /// Raw CSR storage, for sparse kernels.
    pub fn raw(&self) -> CsrRaw<'_> {
        CsrRaw {
            num_nodes: self.num_nodes,
            row_ptr: &self.row_ptr,
            col_idx: &self.col_idx,
            values: &self.values,
        }
    }
}

/// Borrowed CSR storage view for sparse kernels.
#[derive(Debug, Clone, Copy)]
pub struct CsrRaw<'a> {
    pub num_nodes: usize,
    pub row_ptr: &'a [u32],
    pub col_idx: &'a [u32],
    pub values: &'a [f32],
}

#[cfg(test)]
mod tests {
    use super::*;

    type B = burn::backend::NdArray;

    /// Triangle 0-1, 1-2, 0-2; every node has degree 2.
    fn triangle() -> CsrGraph {
        CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0), (0, 2, 1.0)])
    }

    #[test]
    fn from_edges_basic() {
        let g = triangle();
        assert_eq!(g.num_nodes(), 3);
        assert_eq!(g.num_edges(), 6); // 3 undirected -> 6 directed
        for i in 0..3 {
            assert_eq!(g.degree(i), 2);
        }
        // node 0's neighbors are {1, 2}, both weight 1.0
        let nbrs: Vec<_> = g.neighbors(0).collect();
        assert_eq!(nbrs, vec![(1, 1.0), (2, 1.0)]);
    }

    #[test]
    fn entries_are_directed_and_sorted() {
        let g = CsrGraph::from_entries(3, &[(2, 0, 0.5), (0, 2, 1.0), (0, 1, 2.0), (2, 0, 0.25)]);
        assert_eq!(g.num_edges(), 4);
        assert_eq!(g.neighbors(0).collect::<Vec<_>>(), vec![(1, 2.0), (2, 1.0)]);
        assert_eq!(g.degree(1), 0);
        // duplicates keep their order
        assert_eq!(
            g.neighbors(2).collect::<Vec<_>>(),
            vec![(0, 0.5), (0, 0.25)]
        );
    }

    #[test]
    fn neighbours_sorted_by_column() {
        // unsorted input; the CSR sorts it
        let g = CsrGraph::from_edges(4, &[(0, 3, 1.0), (0, 1, 1.0), (0, 2, 1.0)]);
        let nbrs: Vec<u32> = g.neighbors(0).map(|(c, _)| c).collect();
        assert_eq!(nbrs, vec![1, 2, 3]);
    }

    #[test]
    fn self_loops_added_once() {
        let g = triangle().add_self_loops();
        for i in 0..3 {
            let nbrs: Vec<u32> = g.neighbors(i).map(|(c, _)| c).collect();
            assert!(nbrs.contains(&(i as u32)));
            assert_eq!(g.degree(i), 3); // 2 original + 1 self-loop
        }
        // idempotent: a second call adds no second self-loop
        let g2 = g.clone().add_self_loops();
        for i in 0..3 {
            assert_eq!(g2.degree(i), 3);
        }
    }

    #[test]
    fn symmetric_normalize_triangle_with_self_loops() {
        // triangle + self-loops: degree 3 everywhere, so every weight is 1 / sqrt(3 * 3) = 1/3
        let g = triangle().add_self_loops().symmetric_normalize();
        for i in 0..3 {
            for (_, w) in g.neighbors(i) {
                assert!((w - 1.0 / 3.0).abs() < 1e-6, "expected 1/3, got {w}");
            }
        }
    }

    #[test]
    fn row_normalize_triangle_gives_uniform_thirds() {
        // triangle + self-loops: row sum 3 (two neighbors + self), so all weights are 1/3 and SpMM
        // is the mean of the three rows
        let g = triangle().add_self_loops().row_normalize();
        for i in 0..3 {
            // rows sum to 1
            let row_sum: f32 = g.neighbors(i).map(|(_, w)| w).sum();
            assert!((row_sum - 1.0).abs() < 1e-6, "row {i} sums to {row_sum}");
            // each value 1/3 (3 out-edges per node)
            for (_, w) in g.neighbors(i) {
                assert!((w - 1.0 / 3.0).abs() < 1e-6, "expected 1/3, got {w}");
            }
        }
    }

    #[test]
    fn row_normalize_isolated_node_stays_zero() {
        // the stranded third node has row sum 0: no divide-by-zero panic, its (nonexistent) weights
        // stay zero
        let g = CsrGraph::from_edges(3, &[(0, 1, 1.0)]).row_normalize();
        assert_eq!(g.degree(2), 0);
        for (_, w) in g.neighbors(0) {
            // node 0 has one neighbor: weight 1.0
            assert!((w - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn symmetric_normalize_isolated_node_stays_zero() {
        // one edge plus a stranded degree-0 node: normalization must not panic dividing, its
        // (nonexistent) weights stay zero
        let g = CsrGraph::from_edges(3, &[(0, 1, 1.0)]).symmetric_normalize();
        assert_eq!(g.degree(2), 0);
        for (_, w) in g.neighbors(0) {
            assert!((w - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn to_dense_tensor_matches_csr() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let g = triangle();
        let dense: Tensor<B, 2> = g.to_dense_tensor::<B>(&device);
        let data: Vec<f32> = dense.into_data().iter::<f32>().collect();
        // Symmetric adjacency for the triangle, no self-loops:
        // 0 1 1
        // 1 0 1
        // 1 1 0
        let expected = vec![
            0.0, 1.0, 1.0, //
            1.0, 0.0, 1.0, //
            1.0, 1.0, 0.0, //
        ];
        assert_eq!(data, expected);
    }
}
