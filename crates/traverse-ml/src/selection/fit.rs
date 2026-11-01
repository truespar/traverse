//! The training loop: full-graph or mini-batch, early stopping, cancellation and divergence
//! checks.

use std::collections::HashSet;
use std::io::{Error, ErrorKind, Result};

use burn::module::AutodiffModule;
use burn::optim::{AdamWConfig, GradientsParams, Optimizer};
use burn::prelude::ElementConversion;
use burn::tensor::activation::log_softmax;
use burn::tensor::backend::{AutodiffBackend, Backend};
use burn::tensor::{Int, Tensor};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::cancel::CancellationToken;
use crate::encoder::{AnyEncoder, GraphInputs, InputBlock, Reads};
use crate::graph::CsrGraph;
use crate::pipeline::ModelChoice;
use crate::sampling::{Batch, Fanout, Sampler};

/// How a fit sees the graph each optimizer step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Batching {
    /// The whole graph when a step's messages fit in [`FULL_GRAPH_VALUES`]; otherwise mini-batches
    /// of [`DEFAULT_BATCH_SIZE`] with [`DEFAULT_FANOUTS`].
    #[default]
    Auto,
    /// The whole graph every step: one step per epoch.
    FullGraph,
    /// `batch_size` training examples per step (nodes, or positive pairs for link prediction,
    /// each with its negatives), every one with its sampled 2-hop neighborhood: `fanouts[0]`
    /// neighbors per node, `fanouts[1]` per neighbor (see [`crate::sampling`]).
    MiniBatch {
        batch_size: usize,
        fanouts: [Fanout; 2],
    },
}

/// Training examples per mini-batch step unless configured.
pub const DEFAULT_BATCH_SIZE: usize = 512;
/// Neighbors drawn per layer unless configured (GraphSAGE's 25 and 10).
pub const DEFAULT_FANOUTS: [Fanout; 2] = [Fanout::Sample(25), Fanout::Sample(10)];
/// Message values (`stored entries x widest message`) a full-graph step may hold under
/// [`Batching::Auto`]: 128 Mi floats, 512 MiB per tensor. A backward pass keeps a few such, so a
/// step peaks at a few GiB; larger graphs train in mini-batches.
pub const FULL_GRAPH_VALUES: usize = 1 << 27;
/// Validation examples a mini-batch fit scores after each epoch for early stopping. The folds'
/// metrics still use every example, with exact inference.
const EARLY_STOP_EXAMPLES: usize = 4096;

impl Batching {
    /// `Auto` decided for a graph whose adjacency holds `entries` stored entries, for an encoder
    /// passing messages `width` values wide.
    pub fn resolve(self, entries: usize, width: usize) -> Batching {
        match self {
            Batching::Auto if entries.saturating_mul(width) <= FULL_GRAPH_VALUES => {
                Batching::FullGraph
            }
            Batching::Auto => Batching::MiniBatch {
                batch_size: DEFAULT_BATCH_SIZE,
                fanouts: DEFAULT_FANOUTS,
            },
            other => other,
        }
    }
}

/// Widest message an encoder of `choice` passes: GCN, GraphSAGE and R-GCN aggregate the inputs,
/// then the hidden layer; GAT aggregates projections, every head at once.
fn message_width(choice: &ModelChoice, in_dim: usize, out_dim: usize) -> usize {
    let w = match *choice {
        ModelChoice::Gat { hidden, heads } => (hidden * heads.max(1)).max(out_dim),
        _ => in_dim.max(choice.hidden()),
    };
    w.max(1)
}

/// Stored entries of the adjacency `reads` names.
fn adjacency_entries(g: &GraphInputs, reads: Reads) -> usize {
    match reads {
        Reads::Sym | Reads::Row => g.sym.num_edges(),
        Reads::Relations => g.relations.iter().map(CsrGraph::num_edges).sum(),
    }
}

/// One model fit.
#[derive(Debug, Clone)]
pub(crate) struct FitParams {
    pub choice: ModelChoice,
    pub learning_rate: f64,
    pub epochs: usize,
    /// Early-stopping patience on the validation set; needs a non-empty validation set.
    pub patience: Option<usize>,
    pub weight_decay: f64,
    pub seed: u64,
    pub batching: Batching,
}

/// Uniform negative sampler for link prediction, avoiding self-pairs and known positives.
#[derive(Debug, Clone)]
pub(crate) struct NegativeSampler {
    pub num_nodes: u32,
    pub sources: Option<Vec<u32>>,
    pub targets: Option<Vec<u32>>,
    /// Known positives as `(min, max)` pairs.
    pub known: HashSet<(u32, u32)>,
}

impl NegativeSampler {
    fn pick(&self, pool: &Option<Vec<u32>>, rng: &mut StdRng) -> u32 {
        match pool {
            Some(p) if !p.is_empty() => p[rng.random_range(0..p.len())],
            _ => rng.random_range(0..self.num_nodes.max(1)),
        }
    }

    pub fn sample(&self, count: usize, rng: &mut StdRng) -> Vec<(u32, u32)> {
        let mut out = Vec::with_capacity(count);
        // dense graphs can run out of negatives; bound the attempts
        let mut attempts = 0usize;
        while out.len() < count && attempts < count * 20 + 100 {
            attempts += 1;
            let (u, v) = (self.pick(&self.sources, rng), self.pick(&self.targets, rng));
            if u != v && !self.known.contains(&(u.min(v), u.max(v))) {
                out.push((u, v));
            }
        }
        out
    }
}

/// What a fit optimizes. Indices are node indices (node tasks) or node pairs (links).
pub(crate) enum Objective<'a> {
    Classify {
        labels: &'a [i32],
        train: &'a [u32],
        val: &'a [u32],
    },
    Regress {
        targets: &'a [f32],
        train: &'a [u32],
        val: &'a [u32],
    },
    Link {
        train_pos: &'a [(u32, u32)],
        val_pos: &'a [(u32, u32)],
        val_neg: &'a [(u32, u32)],
        negative_ratio: f64,
        sampler: &'a NegativeSampler,
    },
}

impl Objective<'_> {
    fn has_validation(&self) -> bool {
        match self {
            Objective::Classify { val, .. } | Objective::Regress { val, .. } => !val.is_empty(),
            Objective::Link {
                val_pos, val_neg, ..
            } => !val_pos.is_empty() && !val_neg.is_empty(),
        }
    }

    fn train_len(&self) -> usize {
        match self {
            Objective::Classify { train, .. } | Objective::Regress { train, .. } => train.len(),
            Objective::Link { train_pos, .. } => train_pos.len(),
        }
    }
}

/// Result of one fit: the encoder (best validation epoch when there's a validation set).
pub(crate) struct Fitted<B: Backend> {
    pub encoder: AnyEncoder<B>,
    pub epochs_run: usize,
    /// Epochs of training behind `encoder`.
    pub best_epoch: usize,
    /// Mean training loss of the last epoch.
    pub final_loss: f32,
    /// How the fit batched, `Auto` decided.
    pub batching: Batching,
}

fn index_tensor<B: Backend>(idx: &[u32], device: &B::Device) -> Tensor<B, 1, Int> {
    let v: Vec<i32> = idx.iter().map(|&i| i as i32).collect();
    Tensor::from_ints(v.as_slice(), device)
}

/// `z[u] * z[v]` per pair.
pub(super) fn pair_scores<B: Backend>(
    z: Tensor<B, 2>,
    pairs: &[(u32, u32)],
    device: &B::Device,
) -> Tensor<B, 1> {
    let src: Vec<u32> = pairs.iter().map(|p| p.0).collect();
    let dst: Vec<u32> = pairs.iter().map(|p| p.1).collect();
    let zs = z.clone().select(0, index_tensor::<B>(&src, device));
    let zd = z.select(0, index_tensor::<B>(&dst, device));
    (zs * zd).sum_dim(1).squeeze_dim::<1>(1)
}

/// `log(1 + e^x)` without overflow.
fn softplus<B: Backend>(x: Tensor<B, 1>) -> Tensor<B, 1> {
    x.clone().clamp_min(0.0) + x.abs().neg().exp().log1p()
}

pub(crate) fn tensor_to_vec<B: Backend, const D: usize>(t: Tensor<B, D>) -> Vec<f32> {
    t.into_data().iter::<f32>().collect()
}

/// Host-side argmax per row of a row-major `[n, c]` buffer.
pub(crate) fn argmax_rows(flat: &[f32], c: usize) -> Vec<i32> {
    flat.chunks(c.max(1))
        .map(|row| {
            row.iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i as i32)
                .unwrap_or(0)
        })
        .collect()
}

/// `log(1 + e^x)` on the host, overflow-free.
fn softplus_f64(x: f64) -> f64 {
    x.max(0.0) + (-x.abs()).exp().ln_1p()
}

/// Cross-entropy of class logits `row` against class `label`, on the host.
fn nll(row: &[f32], label: i32) -> f64 {
    let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64;
    let lse = max
        + row
            .iter()
            .map(|&v| (v as f64 - max).exp())
            .sum::<f64>()
            .ln();
    lse - row[label as usize] as f64
}

/// Early-stopping signal, larger = better: the negative validation loss (cross-entropy, MSE or
/// link BCE). Loss keeps improving after accuracy-style metrics plateau on small validation sets,
/// so the best epoch it picks doesn't stop training too early.
fn validation_score<B: Backend>(
    out: Tensor<B, 2>,
    obj: &Objective,
    out_dim: usize,
    device: &B::Device,
) -> f64 {
    match obj {
        Objective::Classify { labels, val, .. } => {
            let rows = tensor_to_vec(out.select(0, index_tensor::<B>(val, device)));
            let total: f64 = val
                .iter()
                .zip(rows.chunks(out_dim.max(1)))
                .map(|(i, row)| nll(row, labels[*i as usize]))
                .sum();
            -total / val.len().max(1) as f64
        }
        Objective::Regress { targets, val, .. } => {
            let rows = tensor_to_vec(out.select(0, index_tensor::<B>(val, device)));
            let mse: f64 = val
                .iter()
                .zip(&rows)
                .map(|(i, p)| {
                    let d = (*p - targets[*i as usize]) as f64;
                    d * d
                })
                .sum::<f64>()
                / val.len().max(1) as f64;
            -mse
        }
        Objective::Link {
            val_pos, val_neg, ..
        } => {
            let pos = tensor_to_vec(pair_scores(out.clone(), val_pos, device));
            let neg = tensor_to_vec(pair_scores(out, val_neg, device));
            let mean = |v: &[f32], sign: f64| {
                v.iter()
                    .map(|&x| softplus_f64(sign * x as f64))
                    .sum::<f64>()
                    / v.len().max(1) as f64
            };
            -(mean(&pos, -1.0) + mean(&neg, 1.0))
        }
    }
}

fn classification_loss<B: Backend>(rows: Tensor<B, 2>, labels: Tensor<B, 1, Int>) -> Tensor<B, 1> {
    log_softmax(rows, 1)
        .gather(1, labels.unsqueeze_dim(1))
        .squeeze_dim::<1>(1)
        .mean()
        .neg()
}

fn regression_loss<B: Backend>(rows: Tensor<B, 2>, targets: Tensor<B, 1>) -> Tensor<B, 1> {
    let diff = rows.squeeze_dim::<1>(1) - targets;
    (diff.clone() * diff).mean()
}

fn link_loss<B: Backend>(pos: Tensor<B, 1>, neg: Option<Tensor<B, 1>>) -> Tensor<B, 1> {
    let loss = softplus(pos.neg()).mean();
    match neg {
        Some(n) => loss + softplus(n).mean(),
        None => loss,
    }
}

/// Fits one encoder. `x` is the (standardized) `[N, in_dim]` feature tensor; regression targets
/// in `obj` are expected standardized too.
pub(crate) fn fit<BA: AutodiffBackend>(
    params: &FitParams,
    in_dim: usize,
    out_dim: usize,
    g: &GraphInputs,
    x: &Tensor<BA, 2>,
    obj: &Objective,
    device: &BA::Device,
    cancel: &CancellationToken,
) -> Result<Fitted<BA::InnerBackend>> {
    let reads = Reads::of(&params.choice);
    let batching = params.batching.resolve(
        adjacency_entries(g, reads),
        message_width(&params.choice, in_dim, out_dim),
    );
    // seeded weight init: the same seed gives the same model (the backend RNG is process-wide, so
    // fits running at the same time can still interleave)
    BA::seed(device, params.seed);
    let model: AnyEncoder<BA> =
        AnyEncoder::init(&params.choice, in_dim, out_dim, g.num_relations(), device);
    let mut fitted = match batching {
        Batching::MiniBatch {
            batch_size,
            fanouts,
        } => fit_mini_batch(
            params,
            model,
            g,
            x,
            obj,
            out_dim,
            device,
            cancel,
            batch_size.max(1),
            fanouts,
        ),
        _ => fit_full_graph(params, model, g, x, obj, out_dim, device, cancel),
    }?;
    fitted.batching = batching;
    Ok(fitted)
}

/// AdamW with gradient-norm clipping.
fn optimizer<B: AutodiffBackend>(params: &FitParams) -> impl Optimizer<AnyEncoder<B>, B> + use<B> {
    AdamWConfig::new()
        .with_weight_decay(params.weight_decay as f32)
        .with_grad_clipping(Some(burn::grad_clipping::GradientClippingConfig::Norm(5.0)))
        .init::<B, AnyEncoder<B>>()
}

fn cancelled() -> Error {
    Error::new(ErrorKind::Interrupted, "training cancelled")
}

fn diverged(epoch: usize, loss: f32) -> Error {
    Error::other(format!(
        "training diverged at epoch {epoch} (loss {loss}); lower the learning rate"
    ))
}

/// Every step on the whole graph. The adjacency goes to the device once, before the first epoch.
fn fit_full_graph<BA: AutodiffBackend>(
    params: &FitParams,
    mut model: AnyEncoder<BA>,
    g: &GraphInputs,
    x: &Tensor<BA, 2>,
    obj: &Objective,
    out_dim: usize,
    device: &BA::Device,
    cancel: &CancellationToken,
) -> Result<Fitted<BA::InnerBackend>> {
    let mut optim = optimizer::<BA>(params);
    let mut rng = StdRng::seed_from_u64(params.seed);
    let blocks = [InputBlock::<BA>::all(g, model.reads(), device)];

    // targets fixed for the whole fit
    let (cls_train, cls_labels) = match obj {
        Objective::Classify { labels, train, .. } => {
            let l: Vec<i32> = train.iter().map(|&i| labels[i as usize]).collect();
            (
                Some(index_tensor::<BA>(train, device)),
                Some(Tensor::<BA, 1, Int>::from_ints(l.as_slice(), device)),
            )
        }
        _ => (None, None),
    };
    let (reg_train, reg_targets) = match obj {
        Objective::Regress { targets, train, .. } => {
            let t: Vec<f32> = train.iter().map(|&i| targets[i as usize]).collect();
            (
                Some(index_tensor::<BA>(train, device)),
                Some(Tensor::<BA, 1>::from_floats(t.as_slice(), device)),
            )
        }
        _ => (None, None),
    };

    let use_val = obj.has_validation();
    let mut best: Option<(f64, usize, AnyEncoder<BA::InnerBackend>)> = None;
    let mut stale = 0usize;
    let mut epochs_run = 0usize;
    let mut final_loss = f32::NAN;
    for epoch in 1..=params.epochs {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let out = model.forward_blocks(x.clone(), &blocks);
        if use_val {
            // the output before this epoch's step: the model after `epoch - 1` epochs
            let score =
                validation_score::<BA::InnerBackend>(out.clone().inner(), obj, out_dim, device);
            if best.as_ref().is_none_or(|b| score > b.0 + 1e-9) {
                best = Some((score, epoch - 1, model.valid()));
                stale = 0;
            } else {
                stale += 1;
                if params.patience.is_some_and(|p| stale >= p) {
                    break;
                }
            }
        }
        let loss = match obj {
            Objective::Classify { .. } => classification_loss(
                out.select(0, cls_train.clone().expect("classification targets")),
                cls_labels.clone().expect("classification targets"),
            ),
            Objective::Regress { .. } => regression_loss(
                out.select(0, reg_train.clone().expect("regression targets")),
                reg_targets.clone().expect("regression targets"),
            ),
            Objective::Link {
                train_pos,
                negative_ratio,
                sampler,
                ..
            } => {
                let count = ((train_pos.len() as f64) * negative_ratio).ceil().max(1.0) as usize;
                let neg = sampler.sample(count, &mut rng);
                let pos = pair_scores(out.clone(), train_pos, device);
                let neg = (!neg.is_empty()).then(|| pair_scores(out, &neg, device));
                link_loss(pos, neg)
            }
        };
        let value: f32 = loss.clone().into_scalar().elem();
        final_loss = value;
        if !value.is_finite() {
            return Err(diverged(epoch, value));
        }
        let grads = GradientsParams::from_grads(loss.backward(), &model);
        model = optim.step(params.learning_rate, model, grads);
        epochs_run = epoch;
    }
    Ok(match best {
        // a best epoch of 0 is the untrained model; the retrain still takes one step
        Some((_, best_epoch, encoder)) => Fitted {
            encoder,
            epochs_run,
            best_epoch: best_epoch.max(1),
            final_loss,
            batching: Batching::FullGraph,
        },
        None => Fitted {
            encoder: model.valid(),
            epochs_run,
            best_epoch: epochs_run,
            final_loss,
            batching: Batching::FullGraph,
        },
    })
}

/// A training mini-batch's output for its seeds: the sampled subgraph's features and adjacency go
/// to the device, the encoder runs over the subgraph.
fn forward_batch<B: Backend>(
    model: &AnyEncoder<B>,
    x: &Tensor<B, 2>,
    batch: &Batch,
    device: &B::Device,
) -> Tensor<B, 2> {
    let xb = x.clone().select(0, index_tensor::<B>(&batch.nodes, device));
    let block = InputBlock::all(&batch.inputs, model.reads(), device);
    model.forward_blocks(xb, std::slice::from_ref(&block))
}

/// What a validation batch's seeds stand for.
enum Scored {
    /// Node tasks: the graph node behind each seed row.
    Nodes(Vec<u32>),
    /// Links: `(row u, row v)` per pair, the first `positives` of them positive.
    Pairs {
        pairs: Vec<(u32, u32)>,
        positives: usize,
    },
}

/// Up to [`EARLY_STOP_EXAMPLES`] validation examples with their neighborhoods drawn once, so every
/// epoch's early-stopping score sees the same subgraphs.
struct EarlyStop<B: Backend> {
    batches: Vec<(Tensor<B, 2>, InputBlock<B>, Vec<u32>, Scored)>,
}

impl<B: Backend> EarlyStop<B> {
    fn new(
        obj: &Objective,
        sampler: &mut Sampler,
        x: &Tensor<B, 2>,
        reads: Reads,
        batch_size: usize,
        seed: u64,
        device: &B::Device,
    ) -> Option<Self> {
        if !obj.has_validation() {
            return None;
        }
        let mut rng = StdRng::seed_from_u64(seed ^ 0xE57A);
        let mut build = |seeds: &[u32], scored: Scored| {
            let b = sampler.sample(seeds, &mut rng);
            let xb = x.clone().select(0, index_tensor::<B>(&b.nodes, device));
            (
                xb,
                InputBlock::all(&b.inputs, reads, device),
                b.seed_index,
                scored,
            )
        };
        let mut batches = Vec::new();
        match obj {
            Objective::Classify { val, .. } | Objective::Regress { val, .. } => {
                let mut pick = val.to_vec();
                pick.shuffle(&mut StdRng::seed_from_u64(seed));
                pick.truncate(EARLY_STOP_EXAMPLES);
                for chunk in pick.chunks(batch_size) {
                    batches.push(build(chunk, Scored::Nodes(chunk.to_vec())));
                }
            }
            Objective::Link {
                val_pos, val_neg, ..
            } => {
                let take = |v: &[(u32, u32)]| {
                    let mut p = v.to_vec();
                    p.shuffle(&mut StdRng::seed_from_u64(seed));
                    p.truncate(EARLY_STOP_EXAMPLES / 2);
                    p
                };
                let (pos, neg) = (take(val_pos), take(val_neg));
                let tagged: Vec<((u32, u32), bool)> = pos
                    .iter()
                    .map(|&p| (p, true))
                    .chain(neg.iter().map(|&p| (p, false)))
                    .collect();
                for chunk in tagged.chunks(batch_size) {
                    let seeds: Vec<u32> = chunk.iter().flat_map(|((u, v), _)| [*u, *v]).collect();
                    // positives first within the batch
                    let mut order: Vec<usize> = (0..chunk.len()).collect();
                    order.sort_by_key(|&i| !chunk[i].1);
                    let positives = chunk.iter().filter(|c| c.1).count();
                    let pairs: Vec<(u32, u32)> = order
                        .iter()
                        .map(|&i| (2 * i as u32, 2 * i as u32 + 1))
                        .collect();
                    batches.push(build(&seeds, Scored::Pairs { pairs, positives }));
                }
            }
        }
        Some(Self { batches })
    }

    /// The early-stopping signal for `model`: negative validation loss, as [`validation_score`].
    fn score(&self, model: &AnyEncoder<B>, obj: &Objective, out_dim: usize) -> f64 {
        let (mut sum, mut count) = (0.0f64, 0usize);
        let (mut pos_sum, mut pos_n, mut neg_sum, mut neg_n) = (0.0f64, 0usize, 0.0f64, 0usize);
        for (x, block, seed_index, scored) in &self.batches {
            let out = model.forward_blocks(x.clone(), std::slice::from_ref(block));
            let device = out.device();
            match scored {
                Scored::Nodes(nodes) => {
                    let rows = tensor_to_vec(out.select(0, index_tensor::<B>(seed_index, &device)));
                    for (k, &node) in nodes.iter().enumerate() {
                        let row = &rows[k * out_dim..(k + 1) * out_dim];
                        sum += match obj {
                            Objective::Classify { labels, .. } => nll(row, labels[node as usize]),
                            Objective::Regress { targets, .. } => {
                                let d = (row[0] - targets[node as usize]) as f64;
                                d * d
                            }
                            Objective::Link { .. } => 0.0,
                        };
                        count += 1;
                    }
                }
                Scored::Pairs { pairs, positives } => {
                    let rows: Vec<(u32, u32)> = pairs
                        .iter()
                        .map(|&(a, b)| (seed_index[a as usize], seed_index[b as usize]))
                        .collect();
                    let s = tensor_to_vec(pair_scores(out, &rows, &device));
                    for (k, &v) in s.iter().enumerate() {
                        if k < *positives {
                            pos_sum += softplus_f64(-(v as f64));
                            pos_n += 1;
                        } else {
                            neg_sum += softplus_f64(v as f64);
                            neg_n += 1;
                        }
                    }
                }
            }
        }
        match obj {
            Objective::Link { .. } => {
                -(pos_sum / pos_n.max(1) as f64 + neg_sum / neg_n.max(1) as f64)
            }
            _ => -sum / count.max(1) as f64,
        }
    }
}

/// `batch_size` training examples per step, each with its sampled neighborhood; early stopping
/// scores a fixed validation sample after each epoch.
fn fit_mini_batch<BA: AutodiffBackend>(
    params: &FitParams,
    mut model: AnyEncoder<BA>,
    g: &GraphInputs,
    x: &Tensor<BA, 2>,
    obj: &Objective,
    out_dim: usize,
    device: &BA::Device,
    cancel: &CancellationToken,
    batch_size: usize,
    fanouts: [Fanout; 2],
) -> Result<Fitted<BA::InnerBackend>> {
    let reads = model.reads();
    let mut optim = optimizer::<BA>(params);
    let mut rng = StdRng::seed_from_u64(params.seed);
    let mut sampler = Sampler::new(g, reads, &fanouts);
    let early_stop = EarlyStop::<BA::InnerBackend>::new(
        obj,
        &mut sampler,
        &x.clone().inner(),
        reads,
        batch_size,
        params.seed,
        device,
    );
    let mut order: Vec<usize> = (0..obj.train_len()).collect();
    let mut best: Option<(f64, usize, AnyEncoder<BA::InnerBackend>)> = None;
    let mut stale = 0usize;
    let mut epochs_run = 0usize;
    let mut final_loss = f32::NAN;
    for epoch in 1..=params.epochs {
        order.shuffle(&mut rng);
        let (mut loss_sum, mut steps) = (0.0f64, 0usize);
        for chunk in order.chunks(batch_size) {
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            let loss = match obj {
                Objective::Classify { labels, train, .. } => {
                    let seeds: Vec<u32> = chunk.iter().map(|&i| train[i]).collect();
                    let batch = sampler.sample(&seeds, &mut rng);
                    let out = forward_batch(&model, x, &batch, device);
                    let l: Vec<i32> = seeds.iter().map(|&s| labels[s as usize]).collect();
                    classification_loss(
                        out.select(0, index_tensor::<BA>(&batch.seed_index, device)),
                        Tensor::from_ints(l.as_slice(), device),
                    )
                }
                Objective::Regress { targets, train, .. } => {
                    let seeds: Vec<u32> = chunk.iter().map(|&i| train[i]).collect();
                    let batch = sampler.sample(&seeds, &mut rng);
                    let out = forward_batch(&model, x, &batch, device);
                    let t: Vec<f32> = seeds.iter().map(|&s| targets[s as usize]).collect();
                    regression_loss(
                        out.select(0, index_tensor::<BA>(&batch.seed_index, device)),
                        Tensor::from_floats(t.as_slice(), device),
                    )
                }
                Objective::Link {
                    train_pos,
                    negative_ratio,
                    sampler: negatives,
                    ..
                } => {
                    let pos: Vec<(u32, u32)> = chunk.iter().map(|&i| train_pos[i]).collect();
                    let count = ((pos.len() as f64) * negative_ratio).ceil().max(1.0) as usize;
                    let neg = negatives.sample(count, &mut rng);
                    let seeds: Vec<u32> =
                        pos.iter().chain(&neg).flat_map(|&(u, v)| [u, v]).collect();
                    let batch = sampler.sample(&seeds, &mut rng);
                    let out = forward_batch(&model, x, &batch, device);
                    let rows: Vec<(u32, u32)> = (0..pos.len() + neg.len())
                        .map(|i| (batch.seed_index[2 * i], batch.seed_index[2 * i + 1]))
                        .collect();
                    let (p, n) = rows.split_at(pos.len());
                    let n = (!n.is_empty()).then(|| pair_scores(out.clone(), n, device));
                    link_loss(pair_scores(out, p, device), n)
                }
            };
            let value: f32 = loss.clone().into_scalar().elem();
            if !value.is_finite() {
                return Err(diverged(epoch, value));
            }
            loss_sum += value as f64;
            steps += 1;
            let grads = GradientsParams::from_grads(loss.backward(), &model);
            model = optim.step(params.learning_rate, model, grads);
        }
        epochs_run = epoch;
        final_loss = (loss_sum / steps.max(1) as f64) as f32;
        if let Some(es) = &early_stop {
            let current = model.valid();
            let score = es.score(&current, obj, out_dim);
            if best.as_ref().is_none_or(|b| score > b.0 + 1e-9) {
                best = Some((score, epoch, current));
                stale = 0;
            } else {
                stale += 1;
                if params.patience.is_some_and(|p| stale >= p) {
                    break;
                }
            }
        }
    }
    let batching = Batching::MiniBatch {
        batch_size,
        fanouts,
    };
    Ok(match best {
        Some((_, best_epoch, encoder)) => Fitted {
            encoder,
            epochs_run,
            best_epoch,
            final_loss,
            batching,
        },
        None => Fitted {
            encoder: model.valid(),
            epochs_run,
            best_epoch: epochs_run,
            final_loss,
            batching,
        },
    })
}

/// Every node's encoder output, exactly, a block of rows at a time (see
/// [`AnyEncoder::forward_blocked`]): the evaluation and prediction path, whatever the graph's size.
pub fn infer<B: Backend>(enc: &AnyEncoder<B>, x: Tensor<B, 2>, g: &GraphInputs) -> Tensor<B, 2> {
    enc.forward_blocked(x, g, enc.block_entries())
}
