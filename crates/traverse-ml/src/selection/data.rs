//! Training data: node and link sets, feature standardization, and the holdout / k-fold
//! splits.

use std::collections::{BTreeMap, HashSet};
use std::io::{Error, ErrorKind, Result};

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;

use crate::encoder::GraphInputs;

// --- data --------------------------------------------------------

/// Node-level training data (classification and regression).
#[derive(Debug, Clone)]
pub struct NodeData {
    pub inputs: GraphInputs,
    /// Row-major `[N x feat_dim]`, unscaled.
    pub features: Vec<f32>,
    pub feat_dim: usize,
}

/// Link-prediction training data. Node pairs are undirected.
#[derive(Debug, Clone)]
pub struct LinkData {
    pub num_nodes: usize,
    /// Row-major `[N x feat_dim]`, unscaled.
    pub features: Vec<f32>,
    pub feat_dim: usize,
    /// Message-passing edges that are never prediction targets, with their relation index.
    pub context: Vec<(u32, u32, u16)>,
    /// Relation types R-GCN sees, the target relation included.
    pub num_relations: usize,
    /// Relation index of the target edges.
    pub target_relation: u16,
    /// Edges to learn to predict, each pair once.
    pub positives: Vec<(u32, u32)>,
    /// Allowed pair endpoints (`sourceNodeLabel` / `targetNodeLabel`); `None` = any node.
    pub source_nodes: Option<Vec<u32>>,
    pub target_nodes: Option<Vec<u32>>,
}

impl LinkData {
    /// Message-passing inputs: context edges plus `targets` on the target relation.
    pub fn inputs(&self, targets: &[(u32, u32)]) -> GraphInputs {
        let mut edges: Vec<(u32, u32)> = Vec::with_capacity(self.context.len() + targets.len());
        let mut rel: Vec<u16> = Vec::with_capacity(edges.capacity());
        for &(u, v, r) in &self.context {
            edges.push((u, v));
            rel.push(r);
        }
        for &(u, v) in targets {
            edges.push((u, v));
            rel.push(self.target_relation);
        }
        GraphInputs::from_edges(self.num_nodes, &edges, &rel, self.num_relations)
    }
}

/// Per-feature mean and standard deviation (1 where a feature is constant).
pub fn feature_stats(features: &[f32], n: usize, d: usize) -> (Vec<f32>, Vec<f32>) {
    let mut mean = vec![0.0f64; d];
    let mut sq = vec![0.0f64; d];
    for i in 0..n {
        for j in 0..d {
            let v = features[i * d + j] as f64;
            mean[j] += v;
            sq[j] += v * v;
        }
    }
    let nf = n.max(1) as f64;
    let means: Vec<f32> = mean.iter().map(|m| (m / nf) as f32).collect();
    let stds: Vec<f32> = (0..d)
        .map(|j| {
            let var = (sq[j] / nf - (mean[j] / nf).powi(2)).max(0.0);
            let s = var.sqrt() as f32;
            if s > 1e-12 { s } else { 1.0 }
        })
        .collect();
    (means, stds)
}

pub(super) fn standardize(features: &[f32], d: usize, means: &[f32], stds: &[f32]) -> Vec<f32> {
    features
        .chunks(d.max(1))
        .flat_map(|row| {
            row.iter()
                .enumerate()
                .map(|(j, v)| (v - means[j]) / stds[j])
        })
        .collect()
}

pub(super) fn join_floats(v: &[f32]) -> String {
    v.iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn parse_floats(s: &str) -> Vec<f32> {
    s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
}

// --- splits ------------------------------------------------------

/// `(train, test)` over the labelled nodes, stratified per class.
pub(super) fn stratified_holdout(
    labels: &[i32],
    test_fraction: f64,
    seed: u64,
) -> (Vec<u32>, Vec<u32>) {
    let mut per_class: BTreeMap<i32, Vec<u32>> = BTreeMap::new();
    for (i, &c) in labels.iter().enumerate() {
        if c >= 0 {
            per_class.entry(c).or_default().push(i as u32);
        }
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let (mut train, mut test) = (Vec::new(), Vec::new());
    for (_, mut idx) in per_class {
        idx.shuffle(&mut rng);
        // keep at least one example of each class for training
        let n_test = ((idx.len() as f64 * test_fraction).round() as usize).min(idx.len() - 1);
        test.extend_from_slice(&idx[..n_test]);
        train.extend_from_slice(&idx[n_test..]);
    }
    train.sort_unstable();
    test.sort_unstable();
    (train, test)
}

/// `k` folds of `items`, round-robin per class so every fold keeps the class mix.
pub(super) fn stratified_folds(
    items: &[u32],
    labels: &[i32],
    k: usize,
    seed: u64,
) -> Vec<Vec<u32>> {
    let mut per_class: BTreeMap<i32, Vec<u32>> = BTreeMap::new();
    for &i in items {
        per_class.entry(labels[i as usize]).or_default().push(i);
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let mut folds = vec![Vec::new(); k];
    let mut next = 0usize;
    for (_, mut idx) in per_class {
        idx.shuffle(&mut rng);
        for i in idx {
            folds[next % k].push(i);
            next += 1;
        }
    }
    folds
}

pub(super) fn plain_split<T: Copy>(items: &[T], test_fraction: f64, seed: u64) -> (Vec<T>, Vec<T>) {
    let mut v = items.to_vec();
    v.shuffle(&mut StdRng::seed_from_u64(seed));
    let n_test = ((v.len() as f64 * test_fraction).round() as usize).min(v.len().saturating_sub(1));
    let test = v[..n_test].to_vec();
    let train = v[n_test..].to_vec();
    (train, test)
}

pub(super) fn plain_folds<T: Copy>(items: &[T], k: usize, seed: u64) -> Vec<Vec<T>> {
    let mut v = items.to_vec();
    v.shuffle(&mut StdRng::seed_from_u64(seed));
    let mut folds = vec![Vec::new(); k];
    for (i, x) in v.into_iter().enumerate() {
        folds[i % k].push(x);
    }
    folds
}

pub(super) fn without<T: Copy + PartialEq + std::hash::Hash + Eq>(all: &[T], drop: &[T]) -> Vec<T> {
    let d: HashSet<T> = drop.iter().copied().collect();
    all.iter().copied().filter(|x| !d.contains(x)).collect()
}

/// Whether to cross-validate: needs at least `k` training examples. A single trial has nothing to
/// select, so a training set too small for the folds just trains once (no validation metrics);
/// several trials need the folds.
pub(super) fn use_folds(train: usize, k: usize, trials: usize, what: &str) -> Result<bool> {
    if k < 2 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "validationFolds must be at least 2",
        ));
    }
    if train >= k {
        return Ok(true);
    }
    if trials == 1 {
        return Ok(false);
    }
    Err(Error::new(
        ErrorKind::InvalidInput,
        format!(
            "{train} training {what} can't fill {k} validation folds to compare {trials} trials; \
             label more data, lower validationFolds or keep one candidate"
        ),
    ))
}
