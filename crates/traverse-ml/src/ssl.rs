//! Self-supervised pretraining of node features: GraphMAE (Hou et al., KDD
//! 2022, *GraphMAE: Self-Supervised Masked Graph Autoencoders*), for the common
//! "many nodes, few labels" case.
//!
//! 1. Pick a random `mask_ratio` share of the nodes.
//! 2. Replace their feature rows with a **learnable mask token** (one vector
//!    shared by all masked nodes).
//! 3. Run the encoder GNN over the masked features -> hidden representations.
//! 4. Linear decoder back to feature dim -> reconstructed features.
//! 5. SCE loss `(1 - cos(reconstructed, original))^gamma` on masked positions only.
//!
//! The encoder learns to fill in masked features from *neighbors*, exactly the
//! inductive bias a GNN backbone should have before supervised fine-tuning.
//!
//! Large graphs train in mini-batches (see [`crate::selection::Batching`]): each step samples a
//! batch of nodes' 2-hop neighborhoods, masks a share of the sampled nodes and scores the masked
//! seeds. The embeddings come from exact inference over the whole graph either way.

use crate::encoder::{AnyEncoder, GraphInputs, Reads};
use crate::gnn::{Gcn, GcnConfig};
use crate::graph::CsrGraph;
use crate::sampling::Sampler;
use crate::selection::Batching;
use burn::module::{Module, Param};
use burn::nn::{Linear, LinearConfig};
use burn::tensor::{Bool, Int, Tensor, backend::Backend};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;

/// Masked graph autoencoder. `encoder`: 2-layer GCN, `feat_dim -> hidden -> hidden`. `decoder`:
/// Linear `hidden -> feat_dim` (a single-layer decoder is the paper's cheap, effective default; the
/// encoder does the heavy lifting). `mask_token`: learnable `[feat_dim]` vector broadcast to every
/// masked row before encoding.
#[derive(Module, Debug)]
pub struct GraphMae<B: Backend> {
    pub encoder: Gcn<B>,
    pub decoder: Linear<B>,
    pub mask_token: Param<Tensor<B, 1>>,
}

/// [`GraphMae`] config.
pub struct GraphMaeConfig {
    pub feat_dim: usize,
    pub hidden: usize,
    pub mask_ratio: f64,
    pub sce_gamma: f64,
}

impl GraphMaeConfig {
    /// Defaults `mask_ratio = 0.5`, `sce_gamma = 3.0` (the GraphMAE recipe).
    pub fn new(feat_dim: usize, hidden: usize) -> Self {
        Self {
            feat_dim,
            hidden,
            mask_ratio: 0.5,
            sce_gamma: 3.0,
        }
    }

    pub fn with_mask_ratio(mut self, r: f64) -> Self {
        self.mask_ratio = r;
        self
    }

    pub fn with_sce_gamma(mut self, g: f64) -> Self {
        self.sce_gamma = g;
        self
    }

    pub fn init<B: Backend>(&self, device: &B::Device) -> GraphMae<B> {
        GraphMae {
            encoder: GcnConfig::new(self.feat_dim, self.hidden, self.hidden).init(device),
            decoder: LinearConfig::new(self.hidden, self.feat_dim).init(device),
            mask_token: Param::from_tensor(Tensor::zeros([self.feat_dim], device)),
        }
    }
}

impl<B: Backend> GraphMae<B> {
    /// Replaces the rows of `x` selected by `mask_2d` with the mask token. `mask_2d` is an `[N,
    /// feat_dim]` Bool tensor, all `true` in masked rows and all `false` elsewhere;
    /// [`build_node_mask_2d`] builds it from a per-node mask.
    pub fn apply_mask(&self, x: Tensor<B, 2>, mask_2d: Tensor<B, 2, Bool>) -> Tensor<B, 2> {
        let [n, d] = x.dims();
        let token_2d = self.mask_token.val().unsqueeze::<2>().expand([n, d]);
        x.mask_where(mask_2d, token_2d)
    }

    /// Encoder + decoder: the reconstructed `[N, feat_dim]` tensor; the loss against the original
    /// is the caller's (see [`sce_loss`]).
    pub fn forward(&self, x_masked: Tensor<B, 2>, adj: &CsrGraph) -> Tensor<B, 2> {
        let h = self.encoder.forward(x_masked, adj);
        self.decoder.forward(h)
    }
}

/// `floor(N * mask_ratio)` distinct mask indices, sorted, deterministic per `seed`.
pub fn random_mask_indices(n: usize, mask_ratio: f64, seed: u64) -> Vec<i32> {
    let m = ((n as f64 * mask_ratio) as usize).min(n);
    let mut rng = StdRng::seed_from_u64(seed);
    let mut indices: Vec<i32> = (0..n as i32).collect();
    indices.shuffle(&mut rng);
    indices.truncate(m);
    indices.sort_unstable();
    indices
}

/// `[N, feat_dim]` Bool mask: rows in `masked` all `true`, others all `false`. Input for
/// [`GraphMae::apply_mask`].
pub fn build_node_mask_2d<B: Backend>(
    masked: &[i32],
    n: usize,
    feat_dim: usize,
    device: &B::Device,
) -> Tensor<B, 2, Bool> {
    let mut flags: Vec<i32> = vec![0; n];
    for &i in masked {
        flags[i as usize] = 1;
    }
    let per_node: Tensor<B, 1, Int> = Tensor::from_ints(flags.as_slice(), device);
    // Bool [N] -> [N, 1] -> [N, feat_dim] via expand
    per_node.bool().unsqueeze_dim::<2>(1).expand([n, feat_dim])
}

/// Scaled cosine error, `SCE(reconstructed, original) = mean((1 - cos(x_i, y_i))^gamma)`, cosine per
/// row. Inputs are usually narrowed to the masked rows first (see this module's smoke-train test),
/// so gradient only reaches positions the encoder had to reconstruct.
pub fn sce_loss<B: Backend>(
    reconstructed: Tensor<B, 2>,
    original: Tensor<B, 2>,
    gamma: f64,
) -> Tensor<B, 1> {
    // per-row L2 norm via sum of squares + sqrt, with a small eps before the sqrt so zero rows
    // don't give NaN gradients
    let eps = 1e-8f32;
    let x_norm = reconstructed
        .clone()
        .powi_scalar(2)
        .sum_dim(1)
        .add_scalar(eps)
        .sqrt();
    let y_norm = original
        .clone()
        .powi_scalar(2)
        .sum_dim(1)
        .add_scalar(eps)
        .sqrt();
    let x_unit = reconstructed / x_norm;
    let y_unit = original / y_norm;
    // per-row cosine = sum_d (x_unit_d * y_unit_d)
    let cos = (x_unit * y_unit).sum_dim(1);
    // (1 - cos)^gamma, mean over rows
    let loss = cos.neg().add_scalar(1.0).powf_scalar(gamma as f32);
    loss.mean()
}

/// The `mask_indices` rows of an `[N, D]` tensor, for the SCE-loss call sites.
pub fn gather_rows<B: Backend>(
    x: Tensor<B, 2>,
    mask_indices: &[i32],
    device: &B::Device,
) -> Tensor<B, 2> {
    let [_n, d] = x.dims();
    let m = mask_indices.len();
    let idx_t: Tensor<B, 1, Int> = Tensor::from_ints(mask_indices, device);
    let idx_2d: Tensor<B, 2, Int> = idx_t.unsqueeze_dim::<2>(1).expand([m, d]);
    x.gather(0, idx_2d)
}

/// [`train_graph_mae`] settings; defaults follow the GraphMAE paper's node-level setup.
#[derive(Debug, Clone)]
pub struct GraphMaeOptions {
    /// Width of the learned embeddings (the encoder's hidden and output dim).
    pub embedding_dim: usize,
    /// Share of nodes masked per epoch.
    pub mask_ratio: f64,
    /// SCE exponent; larger focuses the loss on badly reconstructed nodes.
    pub sce_gamma: f64,
    pub epochs: usize,
    pub learning_rate: f64,
    pub weight_decay: f64,
    pub seed: u64,
    /// Full-graph steps or mini-batches.
    pub batching: Batching,
}

impl Default for GraphMaeOptions {
    fn default() -> Self {
        Self {
            embedding_dim: 64,
            mask_ratio: 0.5,
            sce_gamma: 3.0,
            epochs: 100,
            learning_rate: 1e-3,
            weight_decay: 2e-4,
            seed: 42,
            batching: Batching::Auto,
        }
    }
}

/// Self-supervised node embeddings: trains a [`GraphMae`] to reconstruct masked feature rows from
/// their neighborhood, then encodes the unmasked graph. `features` is row-major `[N x feat_dim]`
/// (standardized here per column); the encoder propagates over `g.sym`. Returns the row-major
/// `[N x embedding_dim]` embeddings, the mean loss per epoch, and how it batched.
pub fn train_graph_mae<BA: burn::tensor::backend::AutodiffBackend>(
    features: &[f32],
    feat_dim: usize,
    g: &GraphInputs,
    opts: &GraphMaeOptions,
    device: &BA::Device,
    cancel: &crate::cancel::CancellationToken,
) -> std::io::Result<(Vec<f32>, Vec<f32>, Batching)> {
    use burn::module::AutodiffModule;
    use burn::optim::{AdamWConfig, GradientsParams, Optimizer};
    use burn::prelude::ElementConversion;

    let adj = &g.sym;
    let n = adj.num_nodes();
    if n < 2 || feat_dim == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "GraphMAE needs at least 2 nodes and 1 feature",
        ));
    }
    let (means, stds) = crate::selection::feature_stats(features, n, feat_dim);
    let scaled: Vec<f32> = features
        .chunks(feat_dim)
        .flat_map(|row| {
            row.iter()
                .enumerate()
                .map(|(j, v)| (v - means[j]) / stds[j])
        })
        .collect();
    let x = Tensor::<BA, 1>::from_floats(scaled.as_slice(), device).reshape([n, feat_dim]);
    let mut model: GraphMae<BA> = GraphMaeConfig::new(feat_dim, opts.embedding_dim.max(1))
        .with_mask_ratio(opts.mask_ratio)
        .with_sce_gamma(opts.sce_gamma)
        .init(device);
    let mut optim = AdamWConfig::new()
        .with_weight_decay(opts.weight_decay as f32)
        .init();
    let cancelled = || std::io::Error::new(std::io::ErrorKind::Interrupted, "training cancelled");
    let hidden = opts.embedding_dim.max(1);
    let batching = opts.batching.resolve(adj.num_edges(), feat_dim.max(hidden));
    let mut losses = Vec::with_capacity(opts.epochs);
    match batching {
        Batching::MiniBatch {
            batch_size,
            fanouts,
        } => {
            let mut sampler = Sampler::new(g, Reads::Sym, &fanouts);
            let mut rng = StdRng::seed_from_u64(opts.seed);
            let mut order: Vec<u32> = (0..n as u32).collect();
            for epoch in 0..opts.epochs {
                order.shuffle(&mut rng);
                let (mut sum, mut steps) = (0.0f64, 0usize);
                for seeds in order.chunks(batch_size.max(1)) {
                    if cancel.is_cancelled() {
                        return Err(cancelled());
                    }
                    let batch = sampler.sample(seeds, &mut rng);
                    let m = batch.nodes.len();
                    // a share of every sampled node is masked; the loss scores the masked seeds,
                    // whose 2-hop neighborhoods are complete samples
                    let step_seed = opts
                        .seed
                        .wrapping_add((epoch * n + steps * batch_size) as u64);
                    let masked = random_mask_indices(m, opts.mask_ratio, step_seed);
                    let mut scored: Vec<i32> = masked
                        .iter()
                        .copied()
                        .filter(|&i| (i as usize) < seeds.len())
                        .collect();
                    if scored.is_empty() {
                        scored.push(0);
                    }
                    let mask_2d = build_node_mask_2d::<BA>(&masked, m, feat_dim, device);
                    let idx: Vec<i32> = batch.nodes.iter().map(|&v| v as i32).collect();
                    let xb = x
                        .clone()
                        .select(0, Tensor::<BA, 1, Int>::from_ints(idx.as_slice(), device));
                    let recon =
                        model.forward(model.apply_mask(xb.clone(), mask_2d), &batch.inputs.sym);
                    let loss = sce_loss(
                        gather_rows(recon, &scored, device),
                        gather_rows(xb, &scored, device),
                        opts.sce_gamma,
                    );
                    sum += loss.clone().into_scalar().elem::<f32>() as f64;
                    steps += 1;
                    let grads = GradientsParams::from_grads(loss.backward(), &model);
                    model = optim.step(opts.learning_rate, model, grads);
                }
                losses.push((sum / steps.max(1) as f64) as f32);
            }
        }
        _ => {
            for epoch in 0..opts.epochs {
                if cancel.is_cancelled() {
                    return Err(cancelled());
                }
                let mut masked =
                    random_mask_indices(n, opts.mask_ratio, opts.seed.wrapping_add(epoch as u64));
                if masked.is_empty() {
                    masked.push((epoch % n) as i32);
                }
                let mask_2d = build_node_mask_2d::<BA>(&masked, n, feat_dim, device);
                let recon = model.forward(model.apply_mask(x.clone(), mask_2d), adj);
                let loss = sce_loss(
                    gather_rows(recon, &masked, device),
                    gather_rows(x.clone(), &masked, device),
                    opts.sce_gamma,
                );
                losses.push(loss.clone().into_scalar().elem::<f32>());
                let grads = GradientsParams::from_grads(loss.backward(), &model);
                model = optim.step(opts.learning_rate, model, grads);
            }
        }
    }
    let encoder = AnyEncoder::Gcn(model.valid().encoder);
    let z = crate::selection::infer(&encoder, x.inner(), g);
    Ok((z.into_data().iter::<f32>().collect(), losses, batching))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CsrGraph;

    type B = burn::backend::NdArray;

    #[test]
    fn graph_mae_embeddings_separate_communities() {
        type BA = burn::backend::Autodiff<B>;
        // two 10-cliques joined by one edge; features carry the community, noisily
        let mut edges = Vec::new();
        for c in 0..2u32 {
            for i in 0..10u32 {
                for j in (i + 1)..10 {
                    edges.push((c * 10 + i, c * 10 + j, 1.0));
                }
            }
        }
        edges.push((0, 10, 1.0));
        let g = GraphInputs::new(
            CsrGraph::from_edges(20, &edges)
                .add_self_loops()
                .symmetric_normalize(),
            &[],
        );
        let features: Vec<f32> = (0..20)
            .flat_map(|i| {
                let c = (i / 10) as f32;
                [
                    c + 0.1 * ((i * 7 % 5) as f32),
                    1.0 - c,
                    (i % 3) as f32 * 0.05,
                ]
            })
            .collect();
        for batching in [
            Batching::Auto,
            Batching::MiniBatch {
                batch_size: 6,
                fanouts: [
                    crate::sampling::Fanout::Sample(4),
                    crate::sampling::Fanout::Sample(3),
                ],
            },
        ] {
            let opts = GraphMaeOptions {
                embedding_dim: 8,
                epochs: 60,
                learning_rate: 1e-2,
                batching,
                ..Default::default()
            };
            let (z, losses, used) = train_graph_mae::<BA>(
                &features,
                3,
                &g,
                &opts,
                &Default::default(),
                &crate::cancel::CancellationToken::none(),
            )
            .unwrap();
            assert_eq!(used == Batching::FullGraph, batching == Batching::Auto);
            assert_eq!(z.len(), 20 * 8);
            assert!(
                losses.last().unwrap() < losses.first().unwrap(),
                "{batching:?}: {losses:?}"
            );
            // nearest neighbor (cosine) of every node lies in its own community
            let row = |i: usize| &z[i * 8..(i + 1) * 8];
            let cos = |a: &[f32], b: &[f32]| {
                let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
                let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
                let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
                d / (na * nb + 1e-9)
            };
            let mut same = 0;
            for i in 0..20 {
                let nn = (0..20)
                    .filter(|&j| j != i)
                    .max_by(|&a, &b| cos(row(i), row(a)).total_cmp(&cos(row(i), row(b))))
                    .unwrap();
                if nn / 10 == i / 10 {
                    same += 1;
                }
            }
            assert!(
                same >= 18,
                "{batching:?}: {same}/20 nearest neighbors in the same community"
            );
        }
    }

    #[test]
    fn random_mask_indices_are_deterministic_and_sized() {
        let m1 = random_mask_indices(100, 0.3, 42);
        let m2 = random_mask_indices(100, 0.3, 42);
        let m3 = random_mask_indices(100, 0.3, 43);
        assert_eq!(m1, m2, "same seed should yield same indices");
        assert_ne!(m1, m3, "different seed should change the selection");
        assert_eq!(m1.len(), 30);
        // distinct, sorted, in range
        for w in m1.windows(2) {
            assert!(w[0] < w[1], "indices should be sorted and unique");
        }
        assert!(m1.iter().all(|&i| (0..100).contains(&i)));
    }

    #[test]
    fn build_mask_2d_marks_only_selected_rows() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let masked = [1i32, 3];
        let mask_2d: Tensor<B, 2, Bool> = build_node_mask_2d(&masked, 4, 3, &device);
        // bool -> int -> Vec<i32> to inspect
        let flat: Vec<i32> = mask_2d.int().into_data().iter::<i32>().collect();
        // row 0 all 0, row 1 all 1, row 2 all 0, row 3 all 1
        assert_eq!(flat, vec![0, 0, 0, 1, 1, 1, 0, 0, 0, 1, 1, 1]);
    }

    #[test]
    fn apply_mask_replaces_only_selected_rows() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        // 3 nodes, 2-dim features, mask token zero at init
        let model: GraphMae<B> = GraphMaeConfig::new(2, 4).init(&device);
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats([1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0].as_slice(), &device)
                .reshape([3, 2]);
        // mask only row 1
        let mask_2d: Tensor<B, 2, Bool> = build_node_mask_2d(&[1], 3, 2, &device);
        let masked = model.apply_mask(x, mask_2d);
        let data: Vec<f32> = masked.into_data().iter::<f32>().collect();
        // rows 0 and 2 untouched, row 1 zero (initial mask token)
        assert_eq!(data, vec![1.0, 2.0, 0.0, 0.0, 5.0, 6.0]);
    }

    #[test]
    fn sce_loss_is_zero_on_identical_inputs() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats([1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0].as_slice(), &device)
                .reshape([3, 2]);
        let loss = sce_loss(x.clone(), x, 3.0);
        let v: f32 = loss.into_data().iter::<f32>().next().unwrap();
        // (1 - cos(x, x))^gamma = (1 - 1)^gamma = 0
        assert!(
            v.abs() < 1e-5,
            "SCE on identical inputs should be ~0, got {v}"
        );
    }

    #[test]
    fn sce_loss_is_positive_on_orthogonal_inputs() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        // orthogonal x and y: cos = 0, (1 - 0)^gamma = 1
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats([1.0f32, 0.0, 1.0, 0.0].as_slice(), &device)
                .reshape([2, 2]);
        let y: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats([0.0f32, 1.0, 0.0, 1.0].as_slice(), &device)
                .reshape([2, 2]);
        let loss = sce_loss(x, y, 3.0);
        let v: f32 = loss.into_data().iter::<f32>().next().unwrap();
        assert!(
            (v - 1.0).abs() < 1e-4,
            "SCE on orthogonal inputs should be ~1, got {v}"
        );
    }

    #[test]
    fn graphmae_gradients_flow() {
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let adj = CsrGraph::from_edges(4, &[(0, 1, 1.0), (1, 2, 1.0), (2, 3, 1.0), (0, 3, 1.0)])
            .add_self_loops()
            .symmetric_normalize();
        let model: GraphMae<BA> = GraphMaeConfig::new(3, 8).init(&device);
        let x_data: Vec<f32> = (0..(4 * 3)).map(|i| (i as f32) * 0.1).collect();
        let x: Tensor<BA, 2> =
            Tensor::<BA, 1>::from_floats(x_data.as_slice(), &device).reshape([4, 3]);
        let mask: Vec<i32> = random_mask_indices(4, 0.5, 7);
        let mask_2d: Tensor<BA, 2, Bool> = build_node_mask_2d(&mask, 4, 3, &device);
        let x_masked = model.apply_mask(x.clone(), mask_2d);
        let recon = model.forward(x_masked, &adj);
        let recon_at_mask = gather_rows(recon, &mask, &device);
        let orig_at_mask = gather_rows(x, &mask, &device);
        let loss = sce_loss(recon_at_mask, orig_at_mask, 3.0);
        let grads = loss.backward();

        // encoder weights, decoder weight and mask_token all get gradients
        assert!(
            model.encoder.layer1.weight.grad(&grads).is_some(),
            "encoder.layer1 should have gradient"
        );
        assert!(
            model.encoder.layer2.weight.grad(&grads).is_some(),
            "encoder.layer2 should have gradient"
        );
        assert!(
            model.decoder.weight.grad(&grads).is_some(),
            "decoder should have gradient"
        );
        assert!(
            model.mask_token.grad(&grads).is_some(),
            "mask_token should have gradient"
        );
    }

    #[test]
    fn graphmae_pretrains_on_correlated_sbm() {
        // SBM whose clusters share a feature pattern, so masked reconstruction is solvable by
        // averaging neighbors; with GraphMAE wired correctly the SCE loss falls over 100 epochs,
        // anything broken (gradient sign flip, detached mask path, SCE blowing up to NaN) fails the
        // assertion
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        use burn::optim::{AdamConfig, GradientsParams, Optimizer};
        use burn::prelude::ElementConversion;

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
        let adj = CsrGraph::from_edges(n, &edges)
            .add_self_loops()
            .symmetric_normalize();

        // cluster-correlated features: A ~ [1, 0, ...], B ~ [0, 1, ...], plus noise
        let feat_dim = 8;
        let mut feature_buf: Vec<f32> = vec![0.0; n * feat_dim];
        use rand::RngExt;
        let mut rng = StdRng::seed_from_u64(11);
        for i in 0..n {
            let cluster_anchor = if i < half { 0 } else { 1 };
            for d in 0..feat_dim {
                let signal = if d == cluster_anchor { 1.0 } else { 0.0 };
                let noise: f32 = rng.random::<f32>() * 0.2;
                feature_buf[i * feat_dim + d] = signal + noise;
            }
        }
        let x: Tensor<BA, 2> =
            Tensor::<BA, 1>::from_floats(feature_buf.as_slice(), &device).reshape([n, feat_dim]);

        let mut model: GraphMae<BA> = GraphMaeConfig::new(feat_dim, 16)
            .with_mask_ratio(0.5)
            .with_sce_gamma(3.0)
            .init(&device);
        let mut optim = AdamConfig::new().init();

        let mut first_loss = 0.0f32;
        let mut last_loss = 0.0f32;
        for epoch in 1..=100 {
            // fresh mask each epoch (standard SSL practice)
            let mask = random_mask_indices(n, 0.5, 1000 + epoch as u64);
            let mask_2d: Tensor<BA, 2, Bool> = build_node_mask_2d(&mask, n, feat_dim, &device);
            let x_masked = model.apply_mask(x.clone(), mask_2d);
            let recon = model.forward(x_masked, &adj);
            let recon_at_mask = gather_rows(recon, &mask, &device);
            let orig_at_mask = gather_rows(x.clone(), &mask, &device);
            let loss = sce_loss(recon_at_mask, orig_at_mask, 3.0);
            let loss_val: f32 = loss.clone().into_scalar().elem();
            if epoch == 1 {
                first_loss = loss_val;
            }
            last_loss = loss_val;
            let grads = loss.backward();
            let grads = GradientsParams::from_grads(grads, &model);
            model = optim.step(1e-2, model, grads);
        }
        assert!(
            last_loss < first_loss * 0.5,
            "GraphMAE didn't learn: first={first_loss:.4}, last={last_loss:.4}"
        );
    }
}
