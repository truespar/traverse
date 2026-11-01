//! Node-classification dataset shared by the training pipeline, the Cypher procedures and the
//! WASM inference bindings.

use crate::graph::CsrGraph;

/// Node-classification dataset: graph + per-node features + labels + train/val/test masks.
#[derive(Debug)]
pub struct ClassificationDataset {
    /// Sparse adjacency, already symmetrically normalized with self-loops (ready for a GCN).
    pub graph: CsrGraph,
    /// Flat row-major node features, `[num_nodes x feat_dim]`.
    pub features: Vec<f32>,
    /// Feature columns.
    pub feat_dim: usize,
    /// Class label per node (0..num_classes), `-1` for unlabeled nodes (masked out of
    /// training/evaluation).
    pub labels: Vec<i32>,
    /// `train_mask[i]`: node `i` is in the training set.
    pub train_mask: Vec<bool>,
    /// Validation set membership.
    pub val_mask: Vec<bool>,
    /// Test set membership.
    pub test_mask: Vec<bool>,
    /// Distinct classes (labels in `0..num_classes`).
    pub num_classes: usize,
}

impl ClassificationDataset {
    pub fn num_nodes(&self) -> usize {
        self.labels.len()
    }
}
