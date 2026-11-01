//! Explainability for trained GNNs.
//!
//! **PGExplainer** (Luo et al., NeurIPS 2020, *Parameterized Explainer for Graph
//! Neural Networks*) is amortized at inference: once the explainer is trained,
//! per-prediction edge-importance scores for any node are one forward pass
//! through a small MLP.
//!
//! ## Math
//!
//! Given a trained GNN `f` and node embeddings `z = encoder(X)`, per edge
//! `(u -> v)`:
//!
//! ```text
//! omega_{uv} = MLP(concat(z_u, z_v))      # raw score, scalar per edge
//! mu_{uv} = sigmoid(omega_{uv} / tau)        # soft mask in [0, 1]
//! ```
//!
//! Training updates only the MLP (the GNN `f` stays **frozen**) to minimize:
//!
//! ```text
//! L = CE(f(X, A * mu)[target], pred_class(target))    +
//!     lambda_size * mean(mu)                                +
//!     lambda_ent  * H(mu)                                  # entropy reg
//! ```
//!
//! Afterwards `edge_scores(z, src, dst)` gives an `[E]` tensor of raw
//! (temperature-free) scores to sort into a ranked explanation.
//!
//! ## Contents
//!
//! * [`PgExplainer`] / [`PgExplainerConfig`]: the 2-layer MLP, optional bias
//!   on the second layer.
//! * [`edge_scores`](PgExplainer::edge_scores): gather endpoint embeddings,
//!   concat, MLP -> `[E]` raw scores.
//! * [`apply_temperature`](PgExplainer::apply_temperature): raw scores ->
//!   `[0, 1]` masks via `sigmoid(omega / tau)`.
//! * [`csr_edge_indices`]: per-edge `src` / `dst` index tensors from a CSR
//!   (useful to SpMM callers too).
//!
//! [`train_pg_explainer`] runs the training loop against any edge-maskable
//! [`AnyEncoder`] (GCN, GraphSAGE, GAT); [`pg_explain`] ranks a node's edges.
//! The counterfactual and audit explainers below take the same encoders.

use crate::cancel::CancellationToken;
use crate::encoder::{AnyEncoder, GraphInputs};
use crate::graph::CsrGraph;
use burn::module::Module;
use burn::nn::{Linear, LinearConfig};
use burn::tensor::activation::relu;
use burn::tensor::{Int, Tensor, backend::Backend};
use std::io::{Error, ErrorKind, Result};

/// 2-layer MLP from edge-endpoint embeddings to a per-edge score. Input dim `2 * emb_dim`
/// (`z_src` ++ `z_dst`), output 1 (squeezed to `[E]` by `edge_scores`).
#[derive(Module, Debug)]
pub struct PgExplainer<B: Backend> {
    pub layer1: Linear<B>,
    pub layer2: Linear<B>,
}

/// [`PgExplainer`] config.
pub struct PgExplainerConfig {
    /// Width of each node embedding fed to the explainer (`emb_dim`, not `2 * emb_dim`; the config
    /// handles the concat).
    pub emb_dim: usize,
    /// Hidden layer width.
    pub hidden: usize,
}

impl PgExplainerConfig {
    pub fn new(emb_dim: usize, hidden: usize) -> Self {
        Self { emb_dim, hidden }
    }

    pub fn init<B: Backend>(&self, device: &B::Device) -> PgExplainer<B> {
        PgExplainer {
            // input: two emb_dim vectors concatenated
            layer1: LinearConfig::new(2 * self.emb_dim, self.hidden).init(device),
            // output: one scalar per edge
            layer2: LinearConfig::new(self.hidden, 1).init(device),
        }
    }
}

impl<B: Backend> PgExplainer<B> {
    /// Raw per-edge scores `omega_{uv} = MLP(concat(z_u, z_v))`.
    ///
    /// * `z`: node embeddings `[N, emb_dim]` from the frozen GNN encoder.
    /// * `src_per_edge`, `dst_per_edge`: `[E]` Int tensors, usually from
    ///   [`csr_edge_indices`].
    ///
    /// Returns `[E]`; [`apply_temperature`](Self::apply_temperature) turns it into
    /// the `[0, 1]` mask for `Gcn::forward_masked`.
    pub fn edge_scores(
        &self,
        z: Tensor<B, 2>,
        src_per_edge: Tensor<B, 1, Int>,
        dst_per_edge: Tensor<B, 1, Int>,
    ) -> Tensor<B, 1> {
        let [_n, emb_dim] = z.dims();
        let num_edges = src_per_edge.dims()[0];
        let src_idx_2d: Tensor<B, 2, Int> = src_per_edge
            .unsqueeze_dim::<2>(1)
            .expand([num_edges, emb_dim]);
        let dst_idx_2d: Tensor<B, 2, Int> = dst_per_edge
            .unsqueeze_dim::<2>(1)
            .expand([num_edges, emb_dim]);
        let z_src = z.clone().gather(0, src_idx_2d); // [E, emb_dim]
        let z_dst = z.gather(0, dst_idx_2d); // [E, emb_dim]
        let edge_emb = Tensor::cat(vec![z_src, z_dst], 1); // [E, 2*emb_dim]
        let h = relu(self.layer1.forward(edge_emb));
        self.layer2.forward(h).squeeze_dim::<1>(1) // [E]
    }

    /// Squashes raw scores `omega` into a `[0, 1]` soft mask, `sigmoid(omega / tau)`. Smaller `tau` => harder
    /// mask (near 0/1), larger => softer (near 0.5). The paper starts around `tau` ~ 5.0 and anneals
    /// down.
    pub fn apply_temperature(scores: Tensor<B, 1>, tau: f32) -> Tensor<B, 1> {
        burn::tensor::activation::sigmoid(scores.div_scalar(tau))
    }
}

/// Per-edge `(src, dst)` Int index tensors from a CSR, in `graph.raw().col_idx` order (edge `e`'s
/// source is the row owning it, its destination `col_idx[e]`). Every SpMM in this crate uses this
/// order, so mask values line up 1:1 with the CSR entries `Gcn::forward_masked` reads.
pub fn csr_edge_indices<B: Backend>(
    graph: &CsrGraph,
    device: &B::Device,
) -> (Tensor<B, 1, Int>, Tensor<B, 1, Int>) {
    let raw = graph.raw();
    let num_edges = raw.col_idx.len();
    let mut src_per_edge: Vec<i32> = Vec::with_capacity(num_edges);
    for i in 0..raw.num_nodes {
        let start = raw.row_ptr[i] as usize;
        let end = raw.row_ptr[i + 1] as usize;
        for _ in start..end {
            src_per_edge.push(i as i32);
        }
    }
    let dst_per_edge: Vec<i32> = raw.col_idx.iter().map(|&c| c as i32).collect();
    let src_t = Tensor::from_ints(src_per_edge.as_slice(), device);
    let dst_t = Tensor::from_ints(dst_per_edge.as_slice(), device);
    (src_t, dst_t)
}

// --- counterfactual explainer (CF-GNNExplainer-style) -----------
//
// PGExplainer asks "which edges support the prediction", minimizing the
// divergence between masked and original output. The counterfactual question
// is the *opposite*: "what's the smallest edge set whose **removal** flips
// the prediction". Different object, different loss:
//
// ```text
// L_flip   = P(predicted_class | masked)          (push this to 0)
// L_sparse = lambda * mean(1 - mask)                    (penalise removal)
// L        = L_flip + L_sparse
// ```
//
// `mask[e] = 1` keeps edge `e`, `0` removes it; the optimizer balances the flip
// incentive against the cost of removing edges.
//
// The mask is a Burn Module, so the usual `AdamConfig::init` +
// `GradientsParams::from_grads` pattern works (see `train_counterfactual` and
// its smoke test).

/// Trainable per-edge mask for the counterfactual objective: a `Linear` from 1 -> `num_edges` (input
/// the constant `[1.0]`, output the edge logits; Kaiming-uniform init puts the initial sigmoid near
/// 0.5).
#[derive(Module, Debug)]
pub struct CounterfactualMask<B: Backend> {
    pub logits: Linear<B>,
}

/// [`CounterfactualMask`] config.
pub struct CounterfactualMaskConfig {
    pub num_edges: usize,
}

impl CounterfactualMaskConfig {
    pub fn new(num_edges: usize) -> Self {
        Self { num_edges }
    }

    pub fn init<B: Backend>(&self, device: &B::Device) -> CounterfactualMask<B> {
        CounterfactualMask {
            logits: LinearConfig::new(1, self.num_edges)
                .with_bias(false)
                .init(device),
        }
    }
}

impl<B: Backend> CounterfactualMask<B> {
    /// The `[E]` soft mask, `sigmoid(weights)`.
    pub fn mask(&self, device: &B::Device) -> Tensor<B, 1> {
        let one = Tensor::<B, 1>::ones([1], device).reshape([1, 1]);
        let out: Tensor<B, 2> = self.logits.forward(one); // [1, E]
        let [_, e] = out.dims();
        let flat: Tensor<B, 1> = out.reshape([e]);
        burn::tensor::activation::sigmoid(flat)
    }
}

/// [`train_counterfactual`] result.
#[derive(Debug, Clone)]
pub struct CounterfactualOutcome {
    /// Final per-edge mask over the whole graph (`mask[e] in [0, 1]`, 1 outside the receptive
    /// field).
    pub mask: Vec<f32>,
    /// Probability of the originally predicted class on the soft-masked graph; nearer 0 = more
    /// flipped.
    pub flipped_prob: f32,
    /// Unmasked probability of the predicted class.
    pub original_prob: f32,
    /// sum `1 - mask[e]`: the total removal the optimizer spent.
    pub removal_budget: f32,
    /// Class predicted on the unmodified graph.
    pub original_class: i32,
    /// Class predicted once the edges with `mask < 0.5` are removed outright.
    pub counterfactual_class: i32,
    /// CSR edge indices removed (`mask < 0.5`), ascending.
    pub removed_edges: Vec<usize>,
}

fn cancelled() -> Error {
    Error::new(ErrorKind::Interrupted, "explanation cancelled")
}

/// Probability of `class` at `node`, from `[N, C]` logits.
fn class_prob<B: Backend>(logits: Tensor<B, 2>, node: usize, class: usize) -> Tensor<B, 2> {
    burn::tensor::activation::softmax(logits, 1).slice([node..node + 1, class..class + 1])
}

/// CF-GNNExplainer-style counterfactual (Lucic et al. 2022) for any edge-maskable encoder: learns a
/// soft mask over `candidate_edges` (usually [`receptive_field_edges`]) that flips `target_node`'s
/// predicted class while removing as few edges as possible. Edges outside the candidates stay.
/// `cancel` is polled every epoch.
pub fn train_counterfactual<BA: burn::tensor::backend::AutodiffBackend>(
    encoder: &AnyEncoder<BA>,
    x: Tensor<BA, 2>,
    g: &GraphInputs,
    target_node: usize,
    candidate_edges: &[usize],
    epochs: usize,
    lr: f64,
    sparsity_lambda: f32,
    device: &BA::Device,
    cancel: &CancellationToken,
) -> Result<CounterfactualOutcome> {
    use burn::optim::{AdamConfig, GradientsParams, Optimizer};
    use burn::prelude::ElementConversion;
    use burn::tensor::IndexingUpdateOp;

    let num_edges = g.sym.num_edges();
    let orig_logits = encoder.forward(x.clone(), g).detach();
    let original_class: i32 = orig_logits
        .clone()
        .argmax(1)
        .squeeze_dim::<1>(1)
        .slice(target_node..target_node + 1)
        .into_scalar()
        .elem();
    let class = original_class as usize;
    let original_prob: f32 = class_prob(orig_logits, target_node, class)
        .into_scalar()
        .elem();

    let k = candidate_edges.len();
    let idx: Vec<i32> = candidate_edges.iter().map(|&e| e as i32).collect();
    let idx_t = Tensor::<BA, 1, Int>::from_ints(idx.as_slice(), device);
    let full = |local: Tensor<BA, 1>| {
        // ones everywhere, `local` on the candidate edges
        Tensor::<BA, 1>::ones([num_edges], device).scatter(
            0,
            idx_t.clone(),
            local.sub_scalar(1.0),
            IndexingUpdateOp::Add,
        )
    };

    let mut mask_module: CounterfactualMask<BA> = CounterfactualMaskConfig::new(k).init(device);
    let mut optim = AdamConfig::new().init();
    let mut last_flipped = original_prob;
    for _ in 0..epochs.max(1) {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let local = mask_module.mask(device);
        let logits = encoder.forward_masked(x.clone(), g, full(local.clone()))?;
        let flip_loss = class_prob(logits, target_node, class).mean();
        let removal = local.neg().add_scalar(1.0).mean();
        let loss = flip_loss.clone() + removal.mul_scalar(sparsity_lambda);
        last_flipped = flip_loss.into_scalar().elem();
        let grads = GradientsParams::from_grads(loss.backward(), &mask_module);
        mask_module = optim.step(lr, mask_module, grads);
    }

    let local: Vec<f32> = mask_module.mask(device).into_data().iter::<f32>().collect();
    let mut mask = vec![1.0f32; num_edges];
    let mut removed_edges = Vec::new();
    for (&e, &m) in candidate_edges.iter().zip(&local) {
        mask[e] = m;
        if m < 0.5 {
            removed_edges.push(e);
        }
    }
    removed_edges.sort_unstable();
    let removal_budget = local.iter().map(|m| 1.0 - m).sum();
    let mut hard = vec![1.0f32; num_edges];
    for &e in &removed_edges {
        hard[e] = 0.0;
    }
    let hard_t = Tensor::<BA, 1>::from_floats(hard.as_slice(), device);
    let counterfactual_class: i32 = encoder
        .forward_masked(x, g, hard_t)?
        .argmax(1)
        .squeeze_dim::<1>(1)
        .slice(target_node..target_node + 1)
        .into_scalar()
        .elem();
    Ok(CounterfactualOutcome {
        mask,
        flipped_prob: last_flipped,
        original_prob,
        removal_budget,
        original_class,
        counterfactual_class,
        removed_edges,
    })
}

// --- SubgraphX audit mode (deterministic greedy) ----------------
//
// SubgraphX (Yuan et al., ICML 2021) searches for the most explanatory
// subgraph with Monte Carlo Tree Search guided by Shapley values: random
// rollouts, so non-deterministic, and expensive (~10k forwards per
// explanation).
//
// **Audit mode** gives up the optimality bound for what auditors need:
//
// 1. **Determinism**: same input => same explanation, for reproducible
//    compliance records.
// 2. **Bounded cost**: `O((E_rf - k) * E_rf)` forwards over the receptive
//    field, no rollouts.
//
// Greedy backward elimination: start with every receptive-field edge, drop
// at each step the edge whose removal *least* lowers
// `P(predicted_class | retained edges)`, stop at `k_max` edges. The retained
// set is the explanation.

/// L-hop receptive-field edges of `target_node` in a CSR, as CSR edge-array indices (compatible
/// with `Gcn::forward_masked`'s per-edge mask).
pub fn receptive_field_edges(graph: &CsrGraph, target_node: usize, num_hops: usize) -> Vec<usize> {
    let raw = graph.raw();
    let n = raw.num_nodes;
    let mut in_field = vec![false; n];
    in_field[target_node] = true;
    let mut frontier = vec![target_node];
    for _ in 0..num_hops {
        let mut next: Vec<usize> = Vec::new();
        for &u in &frontier {
            let s = raw.row_ptr[u] as usize;
            let e = raw.row_ptr[u + 1] as usize;
            for &v in &raw.col_idx[s..e] {
                if !in_field[v as usize] {
                    in_field[v as usize] = true;
                    next.push(v as usize);
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    // an edge is in the receptive field iff *both* endpoints are
    let mut edges = Vec::new();
    for u in 0..n {
        let s = raw.row_ptr[u] as usize;
        let e = raw.row_ptr[u + 1] as usize;
        for (offset, &v) in raw.col_idx[s..e].iter().enumerate() {
            if in_field[u] && in_field[v as usize] {
                edges.push(s + offset);
            }
        }
    }
    edges
}

/// [`subgraphx_audit`] result.
#[derive(Debug, Clone)]
pub struct SubgraphXAuditOutcome {
    /// CSR edge-array indices of the edges kept by greedy elimination, ascending.
    pub retained_edges: Vec<usize>,
    /// P(predicted_class) on the retained subgraph only.
    pub final_prob: f32,
    /// P(predicted_class) on the full graph.
    pub baseline_prob: f32,
    /// Originally predicted class (the audit target).
    pub target_class: i32,
}

/// Deterministic greedy backward elimination over `candidate_edges` (usually the encoder-depth
/// [`receptive_field_edges`] of `target_node`), stopping at `k_max` edges; the retained set is the
/// explanation. Edges in `keep_edges` (self-loops, say) stay on throughout and aren't counted;
/// every other edge is off. Costs `O((E - k) * E)` forwards, so keep the candidate set small;
/// `cancel` is polled before each one.
pub fn subgraphx_audit<BI: Backend>(
    encoder: &AnyEncoder<BI>,
    x: Tensor<BI, 2>,
    g: &GraphInputs,
    target_node: usize,
    k_max: usize,
    candidate_edges: &[usize],
    keep_edges: &[usize],
    device: &BI::Device,
    cancel: &CancellationToken,
) -> Result<SubgraphXAuditOutcome> {
    use burn::prelude::ElementConversion;

    let num_edges = g.sym.num_edges();
    let base_logits = encoder.forward(x.clone(), g);
    let target_class: i32 = base_logits
        .clone()
        .argmax(1)
        .squeeze_dim::<1>(1)
        .slice(target_node..target_node + 1)
        .into_scalar()
        .elem();
    let class = target_class as usize;
    let baseline_prob: f32 = class_prob(base_logits, target_node, class)
        .into_scalar()
        .elem();
    let mut kept = vec![false; num_edges];
    for &e in keep_edges {
        if e < num_edges {
            kept[e] = true;
        }
    }
    let prob_with = |active: &[bool]| -> Result<f32> {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let mask: Vec<f32> = active
            .iter()
            .zip(&kept)
            .map(|(&a, &k)| if a || k { 1.0 } else { 0.0 })
            .collect();
        let mask_t = Tensor::<BI, 1>::from_floats(mask.as_slice(), device);
        let logits = encoder.forward_masked(x.clone(), g, mask_t)?;
        Ok(class_prob(logits, target_node, class).into_scalar().elem())
    };

    // only the candidates start active; everything else stays removed
    let mut active = vec![false; num_edges];
    for &e in candidate_edges {
        if e < num_edges && !kept[e] {
            active[e] = true;
        }
    }
    let mut retained = active.iter().filter(|&&b| b).count();
    while retained > k_max {
        // drop the edge whose removal keeps P(target_class) highest: the least important
        let mut best: Option<(usize, f32)> = None;
        for e in 0..num_edges {
            if !active[e] {
                continue;
            }
            active[e] = false;
            let p = prob_with(&active)?;
            active[e] = true;
            if best.is_none_or(|(_, bp)| p > bp) {
                best = Some((e, p));
            }
        }
        let Some((e, _)) = best else { break };
        active[e] = false;
        retained -= 1;
    }
    let final_prob = prob_with(&active)?;
    let retained_edges: Vec<usize> = (0..num_edges).filter(|&e| active[e]).collect();
    Ok(SubgraphXAuditOutcome {
        retained_edges,
        final_prob,
        baseline_prob,
        target_class,
    })
}

// --- PGExplainer training ---------------------------------------

/// [`train_pg_explainer`] settings; defaults follow the paper's node-classification setup.
#[derive(Debug, Clone)]
pub struct PgExplainerOptions {
    pub epochs: usize,
    pub learning_rate: f64,
    /// Hidden width of the explainer MLP.
    pub hidden: usize,
    /// Temperature annealed from `.0` to `.1` (exponentially) over the epochs.
    pub temperature: (f32, f32),
    /// Weight of the mask-size penalty (sparse explanations).
    pub size_coef: f32,
    /// Weight of the mask-entropy penalty (near-binary masks).
    pub entropy_coef: f32,
}

impl Default for PgExplainerOptions {
    fn default() -> Self {
        Self {
            epochs: 30,
            learning_rate: 3e-3,
            hidden: 64,
            temperature: (5.0, 1.0),
            size_coef: 0.005,
            entropy_coef: 1.0,
        }
    }
}

/// Trains a PGExplainer against a frozen encoder: one explainer amortized over `targets`, so
/// explaining a node afterwards is one MLP pass ([`pg_explain`]). Loss per epoch: cross-entropy of
/// the masked prediction against the encoder's own predicted class, plus size and entropy
/// penalties on the mask. `z` holds the encoder's output for every node of `g`, which may be the
/// targets' computation subgraph, with `z` taken from the whole graph. `cancel` is polled every
/// epoch. Returns the explainer and the loss per epoch.
pub fn train_pg_explainer<BA: burn::tensor::backend::AutodiffBackend>(
    encoder: &AnyEncoder<BA>,
    x: Tensor<BA, 2>,
    g: &GraphInputs,
    z: Tensor<BA, 2>,
    targets: &[usize],
    opts: &PgExplainerOptions,
    device: &BA::Device,
    cancel: &CancellationToken,
) -> Result<(PgExplainer<BA::InnerBackend>, Vec<f32>)> {
    use burn::module::AutodiffModule;
    use burn::optim::{AdamConfig, GradientsParams, Optimizer};
    use burn::prelude::ElementConversion;
    use burn::tensor::activation::log_softmax;

    if targets.is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "no nodes to explain"));
    }
    let z = z.detach();
    let emb_dim = z.dims()[1];
    let target_idx: Vec<i32> = targets.iter().map(|&t| t as i32).collect();
    let target_t = Tensor::<BA, 1, Int>::from_ints(target_idx.as_slice(), device);
    let classes = z
        .clone()
        .select(0, target_t.clone())
        .argmax(1)
        .squeeze_dim::<1>(1);
    let (src_t, dst_t) = csr_edge_indices::<BA>(&g.sym, device);

    let mut explainer: PgExplainer<BA> = PgExplainerConfig::new(emb_dim, opts.hidden).init(device);
    let mut optim = AdamConfig::new().init();
    let (t0, t1) = opts.temperature;
    let mut losses = Vec::with_capacity(opts.epochs);
    for epoch in 0..opts.epochs.max(1) {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let frac = epoch as f32 / opts.epochs.max(2).saturating_sub(1) as f32;
        let tau = t0 * (t1 / t0).powf(frac);
        let scores = explainer.edge_scores(z.clone(), src_t.clone(), dst_t.clone());
        let mask = PgExplainer::<BA>::apply_temperature(scores, tau);
        let logits = encoder.forward_masked(x.clone(), g, mask.clone())?;
        let ce = log_softmax(logits.select(0, target_t.clone()), 1)
            .gather(1, classes.clone().unsqueeze_dim(1))
            .mean()
            .neg();
        let eps = 1e-6;
        let m = mask.clamp(eps, 1.0 - eps);
        let one_minus = m.clone().neg().add_scalar(1.0);
        let entropy = (m.clone() * m.clone().log() + one_minus.clone() * one_minus.log())
            .neg()
            .mean();
        let loss = ce + m.mean().mul_scalar(opts.size_coef) + entropy.mul_scalar(opts.entropy_coef);
        losses.push(loss.clone().into_scalar().elem());
        let grads = GradientsParams::from_grads(loss.backward(), &explainer);
        explainer = optim.step(opts.learning_rate, explainer, grads);
    }
    Ok((explainer.valid(), losses))
}

/// Edge importances for `node` from a trained PGExplainer: `(csr_edge_index, importance)` over the
/// node's `num_hops` receptive field, most important first. Importance is the temperature-free
/// mask value `sigmoid(omega)`. `z` holds the encoder's output for every node of `g`, as in
/// [`train_pg_explainer`].
pub fn pg_explain<B: Backend>(
    explainer: &PgExplainer<B>,
    z: Tensor<B, 2>,
    g: &GraphInputs,
    node: usize,
    num_hops: usize,
    device: &B::Device,
) -> Vec<(usize, f32)> {
    let edges = receptive_field_edges(&g.sym, node, num_hops);
    if edges.is_empty() {
        return Vec::new();
    }
    let (src_all, dst_all) = csr_edge_indices::<B>(&g.sym, device);
    let idx: Vec<i32> = edges.iter().map(|&e| e as i32).collect();
    let idx_t = Tensor::<B, 1, Int>::from_ints(idx.as_slice(), device);
    let scores = explainer.edge_scores(
        z,
        src_all.select(0, idx_t.clone()),
        dst_all.select(0, idx_t),
    );
    let imp: Vec<f32> = PgExplainer::<B>::apply_temperature(scores, 1.0)
        .into_data()
        .iter::<f32>()
        .collect();
    let mut out: Vec<(usize, f32)> = edges.into_iter().zip(imp).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gnn::{Gcn, GcnConfig};
    use crate::graph::CsrGraph;

    type B = burn::backend::NdArray;

    #[test]
    fn csr_edge_indices_produce_per_edge_pairs() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        // triangle (0,1), (1,2), (0,2); with self-loops: (0,0), (0,1), (0,2), (1,0), (1,1), (1,2),
        // (2,0), (2,1), (2,2)
        let g = CsrGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, 1.0), (0, 2, 1.0)]).add_self_loops();
        let (src_t, dst_t) = csr_edge_indices::<B>(&g, &device);
        let src: Vec<i32> = src_t.into_data().iter::<i32>().collect();
        let dst: Vec<i32> = dst_t.into_data().iter::<i32>().collect();
        assert_eq!(src.len(), 9);
        assert_eq!(dst.len(), 9);
        // edge count matches the raw view
        assert_eq!(g.num_edges(), 9);
        // each (src, dst) appears among its source's neighbors
        for i in 0..3 {
            let mut listed: Vec<i32> = g.neighbors(i).map(|(c, _)| c as i32).collect();
            listed.sort();
            let mut from_idx: Vec<i32> = src
                .iter()
                .zip(dst.iter())
                .filter_map(|(&s, &d)| if s == i as i32 { Some(d) } else { None })
                .collect();
            from_idx.sort();
            assert_eq!(listed, from_idx, "edge ordering mismatch for node {i}");
        }
    }

    #[test]
    fn pgexplainer_edge_scores_have_per_edge_shape() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let g = CsrGraph::from_edges(4, &[(0, 1, 1.0), (1, 2, 1.0), (2, 3, 1.0), (0, 3, 1.0)])
            .add_self_loops();
        let (src_t, dst_t) = csr_edge_indices::<B>(&g, &device);
        let num_edges = src_t.dims()[0];

        let emb_dim = 6;
        let explainer: PgExplainer<B> = PgExplainerConfig::new(emb_dim, 8).init(&device);
        let z: Tensor<B, 2> = Tensor::random(
            [4, emb_dim],
            burn::tensor::Distribution::Normal(0.0, 1.0),
            &device,
        );
        let scores = explainer.edge_scores(z, src_t, dst_t);
        assert_eq!(scores.dims(), [num_edges]);
    }

    #[test]
    fn pgexplainer_temperature_produces_unit_interval() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        // hand-picked scores over a wide range
        let scores: Tensor<B, 1> =
            Tensor::<B, 1>::from_floats([-10.0f32, -1.0, 0.0, 1.0, 10.0].as_slice(), &device);
        let mask = PgExplainer::<B>::apply_temperature(scores, 1.0);
        let vals: Vec<f32> = mask.into_data().iter::<f32>().collect();
        for v in &vals {
            assert!((0.0..=1.0).contains(v), "mask value {v} outside [0,1]");
        }
        // sigmoid(0) = 0.5
        assert!(
            (vals[2] - 0.5).abs() < 1e-6,
            "sigmoid(0/tau) should be 0.5, got {}",
            vals[2]
        );
    }

    #[test]
    fn pgexplainer_training_step_reduces_loss() {
        // smoke test for the full PGExplainer training loop:
        // 1. train a Gcn on a synthetic SBM (the 2-cluster pattern of gnn::tests::*);
        // 2. freeze it;
        // 3. compute node embeddings;
        // 4. train a PGExplainer for an edge mask under which the masked GNN
        //    reproduces the original prediction for every node.
        //
        // The loss is cross-entropy of `gcn.forward_masked(X, adj, sigmoid(omega/tau))`
        // against the frozen Gcn's unmasked predictions, plus an L1 sparsity penalty.
        // A falling loss shows that spmm_csr_sparse_weighted carries gradients through
        // the mask, that the PgExplainer MLP receives them, and that the frozen Gcn
        // stays differentiable without being updated (gradients computed, but its
        // params aren't in the optimizer).
        type BA = burn::backend::Autodiff<burn::backend::NdArray>;
        use burn::optim::{AdamConfig, GradientsParams, Optimizer};
        use burn::prelude::ElementConversion;
        use burn::tensor::{Distribution, Int, activation::log_softmax};

        let device = burn::backend::ndarray::NdArrayDevice::default();
        let n = 40;
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
        let adj = CsrGraph::from_edges(n, &edges)
            .add_self_loops()
            .symmetric_normalize();

        // -- 1. train a Gcn briefly so its predictions mean something
        let labels: Vec<i32> = (0..n).map(|i| if i < half { 0 } else { 1 }).collect();
        let labels_t: Tensor<BA, 1, Int> = Tensor::from_ints(labels.as_slice(), &device);
        let in_dim = 8;
        let x: Tensor<BA, 2> = Tensor::random([n, in_dim], Distribution::Normal(0.0, 1.0), &device);
        let mut gcn: Gcn<BA> = GcnConfig::new(in_dim, 16, 2).init(&device);
        let mut optim = AdamConfig::new().init();
        for _ in 1..=40 {
            let logits = gcn.forward(x.clone(), &adj);
            let log_probs = log_softmax(logits, 1);
            let nll = log_probs
                .gather(1, labels_t.clone().unsqueeze_dim(1))
                .neg()
                .mean();
            let grads = nll.backward();
            let grads = GradientsParams::from_grads(grads, &gcn);
            gcn = optim.step(1e-2, gcn, grads);
        }
        // frozen from here: used, never updated

        // -- 2. frozen-GNN predictions (target distribution)
        let target_logits = gcn.forward(x.clone(), &adj);
        let target_class: Tensor<BA, 1, Int> = target_logits.clone().argmax(1).squeeze_dim::<1>(1);

        // -- 3. GNN output as node embeddings, recomputed so autograd flows into the explainer (gcn
        // weights are still autodiff params, just not in the optimizer)
        let z = gcn.forward(x.clone(), &adj); // [N, 2]
        let (src_t, dst_t) = csr_edge_indices::<BA>(&adj, &device);
        let _num_edges = src_t.dims()[0];

        // -- 4. train the explainer
        let mut explainer: PgExplainer<BA> = PgExplainerConfig::new(2, 16).init(&device);
        let mut explainer_optim = AdamConfig::new().init();
        let tau = 5.0f32;
        let sparsity_lambda = 0.01f32;
        let mut first_loss = 0.0f32;
        let mut last_loss = 0.0f32;
        for epoch in 1..=30 {
            let scores = explainer.edge_scores(z.clone(), src_t.clone(), dst_t.clone());
            let mask = PgExplainer::<BA>::apply_temperature(scores, tau);
            let masked_logits = gcn.forward_masked(x.clone(), &adj, mask.clone());
            // cross-entropy against the frozen GNN's predicted class
            let log_probs = log_softmax(masked_logits, 1);
            let ce = log_probs
                .gather(1, target_class.clone().unsqueeze_dim(1))
                .neg()
                .mean();
            // L1 sparsity (small mask)
            let sparsity: Tensor<BA, 1> = mask.mean();
            let loss = ce + sparsity.mul_scalar(sparsity_lambda);
            let loss_val: f32 = loss.clone().into_scalar().elem();
            if epoch == 1 {
                first_loss = loss_val;
            }
            last_loss = loss_val;
            let grads = loss.backward();
            // update only the explainer's params
            let grads = GradientsParams::from_grads(grads, &explainer);
            explainer = explainer_optim.step(1e-2, explainer, grads);
        }
        assert!(
            last_loss < first_loss,
            "explainer didn't make progress: first={first_loss:.4}, last={last_loss:.4}"
        );
    }

    #[test]
    fn library_pg_explainer_trains_and_ranks_receptive_field_edges() {
        let (gcn_ab, features, n, feat_dim, graph, device) = train_small_gcn();
        let x: Tensor<AB, 2> =
            Tensor::<AB, 1>::from_floats(features.as_slice(), &device).reshape([n, feat_dim]);
        let g = GraphInputs::new(graph.clone(), &[]);
        let encoder = AnyEncoder::Gcn(gcn_ab);
        let targets: Vec<usize> = (0..n).collect();
        let z = encoder.forward(x.clone(), &g);
        let (explainer, losses) = train_pg_explainer::<AB>(
            &encoder,
            x.clone(),
            &g,
            z.clone(),
            &targets,
            &PgExplainerOptions::default(),
            &device,
            &CancellationToken::none(),
        )
        .unwrap();
        assert!(
            losses.last().unwrap() < losses.first().unwrap(),
            "loss didn't fall: {losses:?}"
        );
        let ranked = pg_explain::<B>(&explainer, z.inner(), &g, 3, 2, &device);
        let rf = receptive_field_edges(&graph, 3, 2);
        assert_eq!(ranked.len(), rf.len());
        assert!(ranked.windows(2).all(|w| w[0].1 >= w[1].1), "not sorted");
        assert!(
            ranked
                .iter()
                .all(|(e, v)| rf.contains(e) && (0.0..=1.0).contains(v))
        );
    }

    // --- counterfactual + SubgraphX tests -------------------------

    type AB = burn::backend::Autodiff<B>;

    /// Small SBM with a 2-class GCN trained under the autodiff backend. Returns the trained model,
    /// the features (as a Vec, for rebuilding) and the adjacency; tests wanting the inner-backend
    /// forward call `model.valid()`.
    fn train_small_gcn() -> (
        Gcn<AB>,
        Vec<f32>,
        usize,
        usize,
        CsrGraph,
        burn::backend::ndarray::NdArrayDevice,
    ) {
        use burn::optim::{AdamConfig, GradientsParams, Optimizer};
        use burn::tensor::Int;
        use burn::tensor::activation::log_softmax;
        use rand::{RngExt, SeedableRng};

        let device = burn::backend::ndarray::NdArrayDevice::default();
        let mut rng = rand::rngs::StdRng::seed_from_u64(53);
        let block_size = 12;
        let n = block_size * 2;
        let mut edges: Vec<(u32, u32, f32)> = Vec::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let same = (i / block_size) == (j / block_size);
                let p = if same { 0.30 } else { 0.02 };
                if rng.random::<f32>() < p {
                    edges.push((i as u32, j as u32, 1.0));
                }
            }
        }
        let graph = CsrGraph::from_edges(n, &edges)
            .add_self_loops()
            .symmetric_normalize();

        let feat_dim = 3;
        let mut features = vec![0.0f32; n * feat_dim];
        for i in 0..n {
            let block = (i / block_size) as f32;
            features[i * feat_dim] = block;
            features[i * feat_dim + 1] = rng.random::<f32>() * 0.05;
            features[i * feat_dim + 2] = rng.random::<f32>() * 0.05;
        }
        let labels: Vec<i32> = (0..n).map(|i| (i / block_size) as i32).collect();
        let labels_t = Tensor::<AB, 1, Int>::from_ints(labels.as_slice(), &device);
        let x: Tensor<AB, 2> =
            Tensor::<AB, 1>::from_floats(features.as_slice(), &device).reshape([n, feat_dim]);

        let mut model: Gcn<AB> = GcnConfig::new(feat_dim, 12, 2).init(&device);
        let mut optim = AdamConfig::new().init();
        for _ in 0..80 {
            let logits = model.forward(x.clone(), &graph);
            let log_probs = log_softmax(logits, 1);
            let target_lp = log_probs.gather(1, labels_t.clone().unsqueeze_dim(1));
            let loss = target_lp.neg().mean();
            let grads = loss.backward();
            let grads = GradientsParams::from_grads(grads, &model);
            model = optim.step(1e-2, model, grads);
        }
        (model, features, n, feat_dim, graph, device)
    }

    #[test]
    fn receptive_field_edges_are_endpoint_filtered() {
        // line graph 0-1-2-3 with a self-loop at each node + (0,1),(1,0),(1,2),(2,1),(2,3),(3,2)
        let g = CsrGraph::from_edges(4, &[(0, 1, 1.0), (1, 2, 1.0), (2, 3, 1.0)]).add_self_loops();

        // node 1's 1-hop field is {0, 1, 2}: every edge with both endpoints in it
        let rf1 = receptive_field_edges(&g, 1, 1);
        // 0-loop, 0-1, 1-0, 1-loop, 1-2, 2-1, 2-loop = 7 edges
        assert_eq!(rf1.len(), 7);

        // 2 hops cover the whole graph (4 nodes)
        let rf2 = receptive_field_edges(&g, 1, 2);
        assert_eq!(rf2.len(), g.num_edges());

        // 0 hops: just node 1's self-loop
        let rf0 = receptive_field_edges(&g, 1, 0);
        assert_eq!(rf0.len(), 1);
    }

    #[test]
    fn counterfactual_mask_module_initialises_near_half() {
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let m: CounterfactualMask<B> = CounterfactualMaskConfig::new(20).init(&device);
        let mask = m.mask(&device);
        assert_eq!(mask.dims()[0], 20);
        let vals: Vec<f32> = mask.into_data().iter::<f32>().collect();
        // Kaiming-uniform weights: sigmoid outputs around [0.3, 0.7] for small fan-in, mean
        // mid-range
        let mean = vals.iter().sum::<f32>() / vals.len() as f32;
        assert!(
            (0.3..=0.7).contains(&mean),
            "initial CF mask mean = {mean}, expected mid-range"
        );
    }

    #[test]
    fn counterfactual_training_pushes_target_prob_down() {
        let (gcn_ab, features, n, feat_dim, graph, device) = train_small_gcn();
        let x: Tensor<AB, 2> =
            Tensor::<AB, 1>::from_floats(features.as_slice(), &device).reshape([n, feat_dim]);

        // a node known to be in block 1 (index >= 12)
        let target_node = 18;

        let g = GraphInputs::new(graph.clone(), &[]);
        let encoder = AnyEncoder::Gcn(gcn_ab);
        let rf = receptive_field_edges(&graph, target_node, 2);
        let outcome = train_counterfactual::<AB>(
            &encoder,
            x,
            &g,
            target_node,
            &rf,
            /* epochs */ 60,
            /* lr */ 5e-2,
            /* sparsity_lambda */ 0.01,
            &device,
            &CancellationToken::none(),
        )
        .unwrap();
        // outcome sanity
        assert_eq!(outcome.mask.len(), graph.num_edges());
        // edges outside the receptive field are never touched
        for e in 0..graph.num_edges() {
            if !rf.contains(&e) {
                assert_eq!(outcome.mask[e], 1.0);
            }
        }
        // the predicted class's probability dropped
        assert!(
            outcome.flipped_prob < outcome.original_prob,
            "CF should reduce target-class probability: orig={}, flipped={}",
            outcome.original_prob,
            outcome.flipped_prob
        );
        // positive removal budget: some edges pushed below 1
        assert!(
            outcome.removal_budget > 0.5,
            "expected some edges to be removed; budget={}",
            outcome.removal_budget
        );
    }

    #[test]
    fn subgraphx_audit_retains_exactly_k_edges() {
        use burn::module::AutodiffModule;
        let (gcn_ab, features, n, feat_dim, graph, device) = train_small_gcn();
        let gcn: Gcn<B> = gcn_ab.valid();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats(features.as_slice(), &device).reshape([n, feat_dim]);
        // node 5's 2-hop receptive field (the 2-layer GCN's depth)
        let rf = receptive_field_edges(&graph, 5, 2);
        let k_max = 6.min(rf.len());
        let g = GraphInputs::new(graph.clone(), &[]);
        let outcome = subgraphx_audit::<B>(
            &AnyEncoder::Gcn(gcn),
            x,
            &g,
            5,
            k_max,
            &rf,
            &[],
            &device,
            &CancellationToken::none(),
        )
        .unwrap();
        assert_eq!(
            outcome.retained_edges.len(),
            k_max,
            "audit must terminate with exactly k_max edges"
        );
        // retained edges subset of candidates
        for &e in &outcome.retained_edges {
            assert!(rf.contains(&e), "retained edge {e} outside receptive field");
        }
    }

    #[test]
    fn subgraphx_audit_beats_random_subgraph() {
        use burn::module::AutodiffModule;
        use burn::prelude::ElementConversion;
        use burn::tensor::activation::softmax;
        use rand::SeedableRng;

        let (gcn_ab, features, n, feat_dim, graph, device) = train_small_gcn();
        let gcn: Gcn<B> = gcn_ab.valid();
        let x: Tensor<B, 2> =
            Tensor::<B, 1>::from_floats(features.as_slice(), &device).reshape([n, feat_dim]);
        let target_node = 5;
        let rf = receptive_field_edges(&graph, target_node, 2);
        let k_max = (rf.len() / 4).max(3);

        let g = GraphInputs::new(graph.clone(), &[]);
        let encoder = AnyEncoder::Gcn(gcn.clone());
        let audit_outcome = subgraphx_audit::<B>(
            &encoder,
            x.clone(),
            &g,
            target_node,
            k_max,
            &rf,
            &[],
            &device,
            &CancellationToken::none(),
        )
        .unwrap();

        // vs a random k-edge subset of the receptive field: the greedy audit's retained-set
        // probability is >= the random baseline
        let mut rng = rand::rngs::StdRng::seed_from_u64(123);
        let num_edges = graph.num_edges();
        let mut sum_rand_p = 0.0f32;
        let trials = 5;
        for _ in 0..trials {
            use rand::seq::SliceRandom;
            let mut shuffled = rf.clone();
            shuffled.shuffle(&mut rng);
            let chosen: Vec<usize> = shuffled.into_iter().take(k_max).collect();
            let mut mask = vec![0.0f32; num_edges];
            for &e in &chosen {
                mask[e] = 1.0;
            }
            let mask_t = Tensor::<B, 1>::from_floats(mask.as_slice(), &device);
            let logits = gcn.forward_masked(x.clone(), &graph, mask_t);
            let probs = softmax(logits, 1);
            let p: f32 = probs
                .slice([
                    target_node..target_node + 1,
                    audit_outcome.target_class as usize..audit_outcome.target_class as usize + 1,
                ])
                .into_scalar()
                .elem();
            sum_rand_p += p;
        }
        let mean_rand_p = sum_rand_p / trials as f32;
        // the audit's retained subgraph is at least as confident as random for the predicted class,
        // with a little slack (greedy, not optimal)
        assert!(
            audit_outcome.final_prob >= mean_rand_p - 0.02,
            "audit final_prob = {}, random mean = {} (audit should be >= random)",
            audit_outcome.final_prob,
            mean_rand_p
        );
    }
}
