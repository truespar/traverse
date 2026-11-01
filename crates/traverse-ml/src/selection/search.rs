//! Model selection: the parameter space, search strategies, trial planning and results.

use std::collections::BTreeMap;
use std::io::{Error, ErrorKind, Result};

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::cancel::CancellationToken;
use crate::model::Model;
use crate::pipeline::{ModelChoice, ModelKind, TaskType};

use super::fit::Batching;
use super::metrics::{ClassReport, Metric};

// --- configuration -----------------------------------------------

/// A hyperparameter: fixed, or a range auto-tuning samples from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ParamSpace<T> {
    Fixed(T),
    /// Inclusive bounds. Learning rates sample log-uniformly, integers uniformly.
    Range(T, T),
}

impl<T: Copy> ParamSpace<T> {
    fn is_fixed(&self) -> bool {
        matches!(self, ParamSpace::Fixed(_))
    }

    fn fixed_or(&self, fallback: T) -> T {
        match self {
            ParamSpace::Fixed(v) => *v,
            ParamSpace::Range(..) => fallback,
        }
    }
}

/// One model candidate of a pipeline's parameter space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateSpec {
    pub kind: ModelKind,
    /// Hidden width (per head for GAT).
    pub hidden: ParamSpace<usize>,
    /// Attention heads (GAT only).
    pub heads: ParamSpace<usize>,
    pub learning_rate: ParamSpace<f64>,
}

impl CandidateSpec {
    /// A concrete candidate.
    pub fn fixed(kind: ModelKind, hidden: usize, learning_rate: f64) -> Self {
        Self {
            kind,
            hidden: ParamSpace::Fixed(hidden),
            heads: ParamSpace::Fixed(4),
            learning_rate: ParamSpace::Fixed(learning_rate),
        }
    }

    /// No ranges: one trial, as written.
    pub fn is_concrete(&self) -> bool {
        self.hidden.is_fixed() && self.heads.is_fixed() && self.learning_rate.is_fixed()
    }

    fn sample(&self, rng: &mut StdRng) -> (ModelChoice, f64) {
        let int = |p: &ParamSpace<usize>, rng: &mut StdRng| match *p {
            ParamSpace::Fixed(v) => v,
            ParamSpace::Range(a, b) => rng.random_range(a.min(b)..=a.max(b)),
        };
        let hidden = int(&self.hidden, rng).max(1);
        let heads = int(&self.heads, rng).max(1);
        let lr = match self.learning_rate {
            ParamSpace::Fixed(v) => v,
            ParamSpace::Range(a, b) => {
                let (lo, hi) = (a.min(b).max(1e-12).ln(), a.max(b).max(1e-12).ln());
                rng.random_range(lo..=hi).exp()
            }
        };
        (self.kind.to_choice(hidden, heads), lr)
    }

    fn concrete(&self) -> (ModelChoice, f64) {
        (
            self.kind
                .to_choice(self.hidden.fixed_or(64), self.heads.fixed_or(4)),
            self.learning_rate.fixed_or(1e-2),
        )
    }
}

/// How trials are chosen and budgeted.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SearchStrategy {
    /// Every trial trains the full epoch budget.
    RandomSearch,
    /// Rungs of growing epoch budgets (`min_epochs`, `x eta`, ..., full); the best `1 / eta` of each
    /// rung advance.
    SuccessiveHalving { eta: usize, min_epochs: usize },
}

/// Settings of a training run with model selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionConfig {
    /// Share of the labelled examples (edges for link prediction) held out for the final test.
    pub test_fraction: f64,
    /// Cross-validation folds on the rest; at least 2.
    pub validation_folds: usize,
    /// Trial budget when candidates declare ranges.
    pub max_trials: usize,
    pub strategy: SearchStrategy,
    /// Evaluation metrics, the first selecting the winner.
    pub metrics: Vec<Metric>,
    pub epochs: usize,
    pub patience: Option<usize>,
    pub weight_decay: f64,
    /// Link prediction: negatives per positive, for training and evaluation.
    pub negative_sampling_ratio: f64,
    /// Link prediction: embedding width the encoder outputs.
    pub embedding_dim: usize,
    pub seed: u64,
    /// Full-graph steps or mini-batches.
    pub batching: Batching,
}

impl SelectionConfig {
    /// GDS defaults for `task`.
    pub fn defaults(task: TaskType) -> Self {
        Self {
            test_fraction: if task == TaskType::LinkPrediction {
                0.1
            } else {
                0.3
            },
            validation_folds: 3,
            max_trials: 10,
            strategy: SearchStrategy::RandomSearch,
            metrics: Metric::defaults(task),
            epochs: 100,
            patience: Some(20),
            weight_decay: 5e-4,
            negative_sampling_ratio: 1.0,
            embedding_dim: 64,
            seed: 42,
            batching: Batching::Auto,
        }
    }

    fn selection_metric(&self) -> Metric {
        self.metrics.first().copied().unwrap_or(Metric::Accuracy)
    }
}

// --- results -----------------------------------------------------

/// Average / min / max of a metric over the folds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MetricStats {
    pub avg: f64,
    pub min: f64,
    pub max: f64,
}

impl MetricStats {
    fn of(values: &[f64]) -> Self {
        let n = values.len().max(1) as f64;
        Self {
            avg: values.iter().sum::<f64>() / n,
            min: values.iter().copied().fold(f64::INFINITY, f64::min),
            max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        }
    }
}

/// One evaluated trial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrialResult {
    pub candidate: ModelChoice,
    pub learning_rate: f64,
    /// Epoch budget of this evaluation (successive halving runs a trial at several budgets).
    pub epochs: usize,
    pub validation: BTreeMap<Metric, MetricStats>,
    pub train: BTreeMap<Metric, MetricStats>,
    /// Mean best epoch over the folds.
    pub best_epoch: f64,
}

/// Outcome of a training run with model selection.
#[derive(Debug, Clone)]
pub struct SelectionResult {
    pub trials: Vec<TrialResult>,
    pub best_trial: usize,
    /// Winner retrained on the whole training set, scored on the test set.
    pub test: BTreeMap<Metric, f64>,
    /// ...and on the training set.
    pub outer_train: BTreeMap<Metric, f64>,
    /// Per-class report on the test set (classification only).
    pub class_report: Vec<ClassReport>,
    pub train_size: usize,
    pub test_size: usize,
    /// How the final model was trained, `Auto` decided.
    pub batching: Batching,
    /// The trained model, ready for the catalog.
    pub model: Model,
}

impl SelectionResult {
    pub fn best(&self) -> &TrialResult {
        &self.trials[self.best_trial]
    }
}

// --- trial planning ----------------------------------------------

#[derive(Debug, Clone)]
pub(super) struct TrialPlan {
    pub(super) choice: ModelChoice,
    pub(super) learning_rate: f64,
}

/// Concrete candidates first, then samples from ranged ones until `max_trials`.
pub(super) fn plan_trials(
    candidates: &[CandidateSpec],
    cfg: &SelectionConfig,
) -> Result<Vec<TrialPlan>> {
    if candidates.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "no model candidates; add at least one",
        ));
    }
    let mut plans: Vec<TrialPlan> = candidates
        .iter()
        .filter(|c| c.is_concrete())
        .map(|c| {
            let (choice, learning_rate) = c.concrete();
            TrialPlan {
                choice,
                learning_rate,
            }
        })
        .collect();
    let ranged: Vec<&CandidateSpec> = candidates.iter().filter(|c| !c.is_concrete()).collect();
    let mut rng = StdRng::seed_from_u64(cfg.seed ^ 0x7E57);
    let mut k = 0usize;
    while !ranged.is_empty() && plans.len() < cfg.max_trials.max(1) {
        let (choice, learning_rate) = ranged[k % ranged.len()].sample(&mut rng);
        plans.push(TrialPlan {
            choice,
            learning_rate,
        });
        k += 1;
    }
    Ok(plans)
}

/// Runs `evaluate(plan, epochs)` per the search strategy; returns every evaluation and the winner.
pub(super) fn search(
    plans: Vec<TrialPlan>,
    cfg: &SelectionConfig,
    cancel: &CancellationToken,
    progress: &dyn Fn(usize, usize),
    mut evaluate: impl FnMut(&TrialPlan, usize) -> Result<TrialResult>,
) -> Result<(Vec<TrialResult>, usize)> {
    let metric = cfg.selection_metric();
    let score = |t: &TrialResult| metric.score(t.validation.get(&metric).map_or(0.0, |s| s.avg));
    let mut results: Vec<TrialResult> = Vec::new();
    match cfg.strategy {
        SearchStrategy::RandomSearch => {
            let total = plans.len();
            for (i, plan) in plans.iter().enumerate() {
                if cancel.is_cancelled() {
                    return Err(Error::new(ErrorKind::Interrupted, "training cancelled"));
                }
                results.push(evaluate(plan, cfg.epochs)?);
                progress(i + 1, total);
            }
            let best = (0..results.len())
                .max_by(|&a, &b| {
                    score(&results[a])
                        .total_cmp(&score(&results[b]))
                        .then(b.cmp(&a))
                })
                .unwrap_or(0);
            Ok((results, best))
        }
        SearchStrategy::SuccessiveHalving { eta, min_epochs } => {
            let eta = eta.max(2);
            let mut budget = min_epochs.clamp(1, cfg.epochs.max(1));
            let mut active: Vec<TrialPlan> = plans;
            // rough total for progress reporting
            let mut total = 0usize;
            let (mut n, mut b) = (active.len(), budget);
            loop {
                total += n;
                if n <= 1 || b >= cfg.epochs {
                    break;
                }
                n = n.div_ceil(eta);
                b = (b * eta).min(cfg.epochs);
            }
            let mut done = 0usize;
            loop {
                if active.len() == 1 {
                    // nothing left to compare: the survivor gets the full budget
                    budget = cfg.epochs.max(1);
                }
                let mut rung: Vec<(usize, f64)> = Vec::with_capacity(active.len());
                for plan in &active {
                    if cancel.is_cancelled() {
                        return Err(Error::new(ErrorKind::Interrupted, "training cancelled"));
                    }
                    let r = evaluate(plan, budget)?;
                    rung.push((results.len(), score(&r)));
                    results.push(r);
                    done += 1;
                    progress(done, total.max(done));
                }
                if active.len() <= 1 || budget >= cfg.epochs {
                    // final rung: best of it wins
                    let best = rung
                        .iter()
                        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
                        .map(|r| r.0)
                        .unwrap_or(0);
                    return Ok((results, best));
                }
                let mut order: Vec<usize> = (0..active.len()).collect();
                order.sort_by(|&a, &b| rung[b].1.total_cmp(&rung[a].1).then(a.cmp(&b)));
                let keep = active.len().div_ceil(eta);
                active = order[..keep].iter().map(|&i| active[i].clone()).collect();
                budget = (budget * eta).min(cfg.epochs);
            }
        }
    }
}

pub(super) fn fold_stats(
    per_fold: &[BTreeMap<Metric, f64>],
    metrics: &[Metric],
) -> BTreeMap<Metric, MetricStats> {
    if per_fold.is_empty() {
        return BTreeMap::new();
    }
    metrics
        .iter()
        .map(|&m| {
            let v: Vec<f64> = per_fold.iter().filter_map(|f| f.get(&m).copied()).collect();
            (m, MetricStats::of(&v))
        })
        .collect()
}

pub(super) fn pick(all: &BTreeMap<Metric, f64>, metrics: &[Metric]) -> BTreeMap<Metric, f64> {
    metrics
        .iter()
        .filter_map(|m| all.get(m).map(|v| (*m, *v)))
        .collect()
}
