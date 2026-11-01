//! Pipeline framework: the user-facing surface for graph ML training.
//!
//! The lower layers (`graph`, `sparse`, `gnn`, `model`, `loader`) are
//! primitives: a CSR graph, SpMM, a GCN, a catalog. Pipelines orchestrate them
//! into a "configure -> train -> save -> predict" workflow mapping onto Neo4j GDS's
//! `gds.beta.pipeline.*` procedures, for node classification, node regression
//! and link prediction. All three share [`FeatureStage`], [`SplitConfig`],
//! [`AutoTuneConfig`] and the `train`/`predict` flow; only the model heads and
//! losses differ.
//!
//! ## Design
//!
//! A pipeline is a *plan*, not a *model*: the recipe from raw data to a trained
//! model, and re-running it on a new graph gives a fresh model. Trained models
//! live in [`ModelCatalog`].
//!
//! Builder steps mirror Neo4j GDS:
//!
//! 1. [`NodeClassificationPipeline::new`]: name it.
//! 2. [`add_feature_stage`](NodeClassificationPipeline::add_feature_stage):
//!    numeric features for the model. Raw features are always included; this
//!    adds derived columns (degree, ...).
//! 3. [`add_model_candidate`](NodeClassificationPipeline::add_model_candidate):
//!    one or more architecture+hyperparameter combos; the trainer fits each and
//!    keeps the best by validation metric.
//! 4. [`with_split`](NodeClassificationPipeline::with_split) /
//!    [`with_training`](NodeClassificationPipeline::with_training): the
//!    train/val/test split and the optimizer.
//! 5. [`train_node_classification`]: runs the loop, returning a
//!    [`TrainResult`] with the winner's metrics and catalog entry name.
//! 6. [`predict_node_classification`]: loads the trained model
//!    from the catalog and applies it to a (possibly different)
//!    [`ClassificationDataset`].
//!
//! The pipeline definition is `Serialize`/`Deserialize`, so it can be stored
//! next to the trained model and re-trained later as configured.
//!
//! These trainers use one fixed split. [`crate::selection`] is the GDS-style
//! trainer the Cypher procedures use: test holdout, k-fold cross-validated model
//! selection, random search or successive halving.

use crate::cancel::CancellationToken;
use crate::encoder::GraphInputs;
use crate::graph::CsrGraph;
use crate::loader::ClassificationDataset;
use crate::model::{Model, ModelCatalog, ModelMetadata};
use crate::selection::{
    Batching, FitParams, Metric, NegativeSampler, Objective, argmax_rows, classification_metrics,
    fit, link_metrics, regression_metrics, tensor_to_vec,
};
use burn::tensor::{
    Tensor,
    backend::{AutodiffBackend, Backend},
};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::io::{Error, ErrorKind, Result};

// --- task types --------------------------------------------------

/// What the pipeline predicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskType {
    NodeClassification,
    NodeRegression,
    LinkPrediction,
}

// --- feature stages ----------------------------------------------

/// One feature-engineering step. Stages run in order; their outputs are appended column-wise to the
/// dataset's raw features.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FeatureStage {
    /// Appends `[degree(v) / max_degree]` per node. Cheap, often surprisingly effective as a
    /// structural prior.
    Degree,
    /// Appends `[log(1 + degree(v))]` per node; better for heavy-tailed degree distributions (most
    /// graphs).
    LogDegree,
    /// A numeric column already on the dataset, declared as used. The dataset's `features` are
    /// always included, so this is a marker, matching the Cypher procedure surface where users name
    /// properties individually.
    Property { name: String },
}

impl FeatureStage {
    /// The stage as a row-major `Vec<f32>` of shape `[num_nodes x out_dim]`; the caller appends it
    /// to the raw features.
    fn realize(&self, graph: &CsrGraph) -> Vec<f32> {
        let n = graph.num_nodes();
        match self {
            FeatureStage::Degree => {
                let max_deg = (0..n).map(|i| graph.degree(i)).max().unwrap_or(1).max(1) as f32;
                (0..n).map(|i| graph.degree(i) as f32 / max_deg).collect()
            }
            FeatureStage::LogDegree => (0..n)
                .map(|i| (1.0 + graph.degree(i) as f32).ln())
                .collect(),
            FeatureStage::Property { .. } => Vec::new(), // marker, nothing to add
        }
    }

    fn out_dim(&self) -> usize {
        match self {
            FeatureStage::Degree | FeatureStage::LogDegree => 1,
            FeatureStage::Property { .. } => 0,
        }
    }
}

// --- model candidates --------------------------------------------

/// One architecture + hyperparameter snapshot for the trainer; several candidates make a sweep, the
/// best by validation metric wins. All are 2-layer encoders; see [`crate::encoder::AnyEncoder`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelChoice {
    /// GCN (Kipf & Welling 2017).
    Gcn { hidden: usize },
    /// GraphSAGE-mean (Hamilton et al. 2017).
    GraphSage { hidden: usize },
    /// Relational GCN (Schlichtkrull et al. 2018): one weight matrix per relationship type.
    RGcn { hidden: usize },
    /// Multi-head GAT (Veličković et al. 2018): `heads` attention heads of width `hidden`
    /// concatenated in layer 1, one averaged output head.
    Gat { hidden: usize, heads: usize },
}

impl ModelChoice {
    /// Tag used as the `Model::kind` discriminator and in the catalog entry name when several
    /// candidates are kept.
    pub fn kind(&self) -> &'static str {
        match self {
            ModelChoice::Gcn { .. } => "gcn-2layer",
            ModelChoice::GraphSage { .. } => "graphsage-2layer",
            ModelChoice::RGcn { .. } => "rgcn-2layer",
            ModelChoice::Gat { .. } => "gat-2layer",
        }
    }

    /// Hidden width (per head for GAT).
    pub fn hidden(&self) -> usize {
        match self {
            ModelChoice::Gcn { hidden }
            | ModelChoice::GraphSage { hidden }
            | ModelChoice::RGcn { hidden }
            | ModelChoice::Gat { hidden, .. } => *hidden,
        }
    }

    /// Same architecture, another hidden width.
    pub fn with_hidden(&self, hidden: usize) -> Self {
        match self {
            ModelChoice::Gcn { .. } => ModelChoice::Gcn { hidden },
            ModelChoice::GraphSage { .. } => ModelChoice::GraphSage { hidden },
            ModelChoice::RGcn { .. } => ModelChoice::RGcn { hidden },
            ModelChoice::Gat { heads, .. } => ModelChoice::Gat {
                hidden,
                heads: *heads,
            },
        }
    }

    /// The candidate a stored model was trained as: `extra["candidate"]` (JSON) when present,
    /// else rebuilt from `kind` and the `architecture` tag (models trained before candidates were
    /// stored).
    pub fn of_model(model: &Model) -> Result<Self> {
        if let Some(json) = model.metadata.extra.get("candidate") {
            return serde_json::from_str(json)
                .map_err(|e| Error::new(ErrorKind::InvalidData, format!("candidate: {e}")));
        }
        let hidden = parse_hidden(model.metadata.extra.get("architecture")).unwrap_or(32);
        match model.kind.as_str() {
            "gcn-2layer" => Ok(ModelChoice::Gcn { hidden }),
            "graphsage-2layer" => Ok(ModelChoice::GraphSage { hidden }),
            other => Err(Error::new(
                ErrorKind::InvalidInput,
                format!("unknown model kind '{other}'"),
            )),
        }
    }
}

/// Architecture-only enum for random sampling: one-to-one with [`ModelChoice`]'s variants,
/// parameters sampled separately.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ModelKind {
    Gcn,
    GraphSage,
    RGcn,
    Gat,
}

impl ModelKind {
    /// The candidate for this architecture; `heads` only matters for GAT.
    pub fn to_choice(self, hidden: usize, heads: usize) -> ModelChoice {
        match self {
            ModelKind::Gcn => ModelChoice::Gcn { hidden },
            ModelKind::GraphSage => ModelChoice::GraphSage { hidden },
            ModelKind::RGcn => ModelChoice::RGcn { hidden },
            ModelKind::Gat => ModelChoice::Gat {
                hidden,
                heads: heads.max(1),
            },
        }
    }
}

// --- split config ------------------------------------------------

/// How labeled nodes are carved into train/validation/test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitConfig {
    pub train_fraction: f32,
    pub validation_fraction: f32,
    pub seed: u64,
}

impl SplitConfig {
    /// 60/20/20 with a fixed seed, for reproducibility.
    pub fn standard() -> Self {
        Self {
            train_fraction: 0.6,
            validation_fraction: 0.2,
            seed: 42,
        }
    }
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self::standard()
    }
}

// --- training config ---------------------------------------------

/// Optimizer and loop settings shared by all candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingConfig {
    pub epochs: usize,
    pub learning_rate: f64,
    /// Early-stopping patience on validation accuracy; `None` runs all `epochs`.
    pub patience: Option<usize>,
}

impl Default for TrainingConfig {
    fn default() -> Self {
        Self {
            epochs: 200,
            learning_rate: 1e-2,
            patience: None,
        }
    }
}

// --- auto-tuning (minimal grid) ----------------------------------

/// Tiny grid search: an empty grid uses each candidate as written; otherwise each candidate expands
/// over the cartesian product of overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutoTuneConfig {
    pub learning_rates: Vec<f64>,
    pub hidden_dims: Vec<usize>,
}

impl AutoTuneConfig {
    fn expand(&self, base_candidate: &ModelChoice, base_lr: f64) -> Vec<(ModelChoice, f64)> {
        let lrs: Vec<f64> = if self.learning_rates.is_empty() {
            vec![base_lr]
        } else {
            self.learning_rates.clone()
        };
        let hiddens: Vec<usize> = if self.hidden_dims.is_empty() {
            vec![base_candidate.hidden()]
        } else {
            self.hidden_dims.clone()
        };
        let mut out = Vec::new();
        for &lr in &lrs {
            for &h in &hiddens {
                out.push((base_candidate.with_hidden(h), lr));
            }
        }
        out
    }
}

// --- pipeline definition -----------------------------------------

/// Reusable node-classification recipe (see module docs). Fields are public so callers (e.g. the
/// Cypher procedure surface) can inspect or modify pipelines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeClassificationPipeline {
    pub name: String,
    pub task: TaskType,
    pub feature_stages: Vec<FeatureStage>,
    pub candidates: Vec<ModelChoice>,
    pub split: SplitConfig,
    pub training: TrainingConfig,
    pub auto_tune: AutoTuneConfig,
}

impl NodeClassificationPipeline {
    /// New pipeline named `name`. Defaults: 60/20/20 split, 200 epochs, lr=1e-2, no auto-tune, no
    /// feature stages, no candidates.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            task: TaskType::NodeClassification,
            feature_stages: Vec::new(),
            candidates: Vec::new(),
            split: SplitConfig::standard(),
            training: TrainingConfig::default(),
            auto_tune: AutoTuneConfig::default(),
        }
    }

    pub fn add_feature_stage(mut self, stage: FeatureStage) -> Self {
        self.feature_stages.push(stage);
        self
    }

    pub fn add_model_candidate(mut self, candidate: ModelChoice) -> Self {
        self.candidates.push(candidate);
        self
    }

    pub fn with_split(mut self, split: SplitConfig) -> Self {
        self.split = split;
        self
    }

    pub fn with_training(mut self, training: TrainingConfig) -> Self {
        self.training = training;
        self
    }

    pub fn with_auto_tune(mut self, auto_tune: AutoTuneConfig) -> Self {
        self.auto_tune = auto_tune;
        self
    }
}

// --- training result ---------------------------------------------

/// One candidate's result from a `train()` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateOutcome {
    pub model_kind: String,
    pub hidden: usize,
    pub learning_rate: f64,
    pub val_acc: f32,
    pub test_acc: f32,
    pub final_loss: f32,
    pub epochs_completed: usize,
}

/// `train()` output: the winner's catalog entry name plus every candidate's outcome (inspection /
/// metric logging).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainResult {
    /// Catalog entry of the best candidate's saved model.
    pub model_name: String,
    /// Outcomes of every candidate fitted.
    pub candidates: Vec<CandidateOutcome>,
    /// Index of the winner in `candidates`.
    pub best_index: usize,
}

impl TrainResult {
    pub fn best(&self) -> &CandidateOutcome {
        &self.candidates[self.best_index]
    }
}

// --- training ------------------------------------------------------

/// Runs a dataset through the pipeline's feature stages, giving the flat row-major feature buffer
/// for training. Public so tests and the Cypher surface can inspect feature realization separately
/// from training.
pub fn realize_features(
    pipeline: &NodeClassificationPipeline,
    ds: &ClassificationDataset,
) -> (Vec<f32>, usize) {
    let n = ds.num_nodes();
    let raw_dim = ds.feat_dim;
    let stage_dims: Vec<usize> = pipeline
        .feature_stages
        .iter()
        .map(|s| s.out_dim())
        .collect();
    let total_dim = raw_dim + stage_dims.iter().sum::<usize>();

    let mut out = vec![0.0f32; n * total_dim];
    for i in 0..n {
        let dst_row = &mut out[i * total_dim..(i + 1) * total_dim];
        let src_row = &ds.features[i * raw_dim..(i + 1) * raw_dim];
        dst_row[..raw_dim].copy_from_slice(src_row);
    }

    let mut col_offset = raw_dim;
    for (stage, &stage_dim) in pipeline.feature_stages.iter().zip(stage_dims.iter()) {
        if stage_dim == 0 {
            continue;
        }
        let realized = stage.realize(&ds.graph);
        assert_eq!(realized.len(), n * stage_dim);
        for i in 0..n {
            let dst_row = &mut out[i * total_dim..(i + 1) * total_dim];
            let src_row = &realized[i * stage_dim..(i + 1) * stage_dim];
            dst_row[col_offset..col_offset + stage_dim].copy_from_slice(src_row);
        }
        col_offset += stage_dim;
    }

    (out, total_dim)
}

/// Re-splits labeled nodes per the pipeline's [`SplitConfig`], ignoring any masks on `ds`: the
/// pipeline owns the split, so a different seed means something.
pub fn build_split_masks(
    pipeline: &NodeClassificationPipeline,
    ds: &ClassificationDataset,
) -> (Vec<bool>, Vec<bool>, Vec<bool>) {
    let n = ds.num_nodes();
    let mut labelled: Vec<u32> = (0..n as u32)
        .filter(|&i| ds.labels[i as usize] >= 0)
        .collect();
    let mut rng = StdRng::seed_from_u64(pipeline.split.seed);
    labelled.shuffle(&mut rng);

    let n_lab = labelled.len();
    let n_train = (n_lab as f32 * pipeline.split.train_fraction) as usize;
    let n_val = (n_lab as f32 * pipeline.split.validation_fraction) as usize;

    let mut train_mask = vec![false; n];
    let mut val_mask = vec![false; n];
    let mut test_mask = vec![false; n];
    for &idx in &labelled[..n_train] {
        train_mask[idx as usize] = true;
    }
    for &idx in &labelled[n_train..n_train + n_val] {
        val_mask[idx as usize] = true;
    }
    for &idx in &labelled[n_train + n_val..] {
        test_mask[idx as usize] = true;
    }
    (train_mask, val_mask, test_mask)
}

/// Training entry point, generic over the autodiff backend so one implementation serves CPU / CUDA
/// / WGPU builds (see the examples for the cfg-gated backend alias). Splits the labelled nodes per
/// the pipeline's [`SplitConfig`], fits every candidate (expanded by the auto-tune grid) and keeps
/// the best by validation accuracy. `cancel` aborts cooperatively. The catalog is updated in place;
/// `None` skips persistence (tests that only want the [`TrainResult`]). GDS-style training with
/// cross-validated model selection is [`crate::selection`].
pub fn train_node_classification<BA: AutodiffBackend>(
    pipeline: &NodeClassificationPipeline,
    ds: &ClassificationDataset,
    device: &BA::Device,
    catalog: Option<&mut ModelCatalog>,
    cancel: &CancellationToken,
) -> Result<TrainResult> {
    let masks = build_split_masks(pipeline, ds);
    train_nc_on_masks::<BA>(pipeline, ds, &masks, device, catalog, cancel)
}

/// Indices set in `mask` that pass `labelled`.
fn mask_indices(mask: &[bool], labelled: impl Fn(usize) -> bool) -> Vec<u32> {
    (0..mask.len())
        .filter(|&i| mask[i] && labelled(i))
        .map(|i| i as u32)
        .collect()
}

fn no_candidates() -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "pipeline has no model candidates - call add_model_candidate at least once",
    )
}

fn cancelled() -> Error {
    Error::new(
        ErrorKind::Interrupted,
        "training cancelled before all candidates finished",
    )
}

/// Every candidate x auto-tune setting as `(candidate, learning rate, catalog tag)`.
fn expanded_candidates(
    candidates: &[ModelChoice],
    auto_tune: &AutoTuneConfig,
    base_lr: f64,
) -> Vec<(ModelChoice, f64, String)> {
    candidates
        .iter()
        .flat_map(|c| auto_tune.expand(c, base_lr))
        .map(|(c, lr)| {
            let tag = format!("{}-h{}-lr{:.0e}", c.kind(), c.hidden(), lr);
            (c, lr, tag)
        })
        .collect()
}

/// Index of the best outcome: highest `primary`, ties broken by `secondary`.
fn best_by<T>(items: &[T], primary: impl Fn(&T) -> f32, secondary: impl Fn(&T) -> f32) -> usize {
    (0..items.len())
        .max_by(|&a, &b| {
            primary(&items[a])
                .total_cmp(&primary(&items[b]))
                .then(secondary(&items[a]).total_cmp(&secondary(&items[b])))
        })
        .unwrap_or(0)
}

fn train_nc_on_masks<BA: AutodiffBackend>(
    pipeline: &NodeClassificationPipeline,
    ds: &ClassificationDataset,
    (train_mask, val_mask, test_mask): &(Vec<bool>, Vec<bool>, Vec<bool>),
    device: &BA::Device,
    catalog: Option<&mut ModelCatalog>,
    cancel: &CancellationToken,
) -> Result<TrainResult> {
    if pipeline.candidates.is_empty() {
        return Err(no_candidates());
    }
    let (feat_buf, feat_dim) = realize_features(pipeline, ds);
    let n = ds.num_nodes();
    let inputs = GraphInputs::new(ds.graph.clone(), &[]);
    let x = Tensor::<BA, 1>::from_floats(feat_buf.as_slice(), device).reshape([n, feat_dim]);
    let labelled = |i: usize| ds.labels[i] >= 0;
    let (train, val, test) = (
        mask_indices(train_mask, labelled),
        mask_indices(val_mask, labelled),
        mask_indices(test_mask, labelled),
    );

    let mut outcomes: Vec<(CandidateOutcome, Vec<u8>, ModelChoice, String)> = Vec::new();
    for (candidate, lr, tag) in expanded_candidates(
        &pipeline.candidates,
        &pipeline.auto_tune,
        pipeline.training.learning_rate,
    ) {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let params = FitParams {
            choice: candidate.clone(),
            learning_rate: lr,
            epochs: pipeline.training.epochs,
            patience: pipeline.training.patience,
            weight_decay: 0.0,
            seed: pipeline.split.seed,
            batching: Batching::Auto,
        };
        let obj = Objective::Classify {
            labels: &ds.labels,
            train: &train,
            val: &val,
        };
        let fitted = fit::<BA>(
            &params,
            feat_dim,
            ds.num_classes,
            &inputs,
            &x,
            &obj,
            device,
            cancel,
        )?;
        let out = tensor_to_vec(fitted.encoder.forward(x.clone().inner(), &inputs));
        let preds = argmax_rows(&out, ds.num_classes);
        let acc = |idx: &[u32]| {
            classification_metrics(&ds.labels, &preds, idx, ds.num_classes).0[&Metric::Accuracy]
                as f32
        };
        outcomes.push((
            CandidateOutcome {
                model_kind: candidate.kind().to_string(),
                hidden: candidate.hidden(),
                learning_rate: lr,
                val_acc: acc(&val),
                test_acc: acc(&test),
                final_loss: fitted.final_loss,
                epochs_completed: fitted.epochs_run,
            },
            fitted.encoder.to_bytes()?,
            candidate,
            tag,
        ));
    }

    let best_index = best_by(&outcomes, |o| o.0.val_acc, |o| o.0.test_acc);
    let (winner, weights, choice, tag) = outcomes[best_index].clone();
    let model_name = format!("{}-{}", pipeline.name, tag);
    if let Some(catalog) = catalog {
        let mut meta = ModelMetadata::now();
        meta.feature_dim = feat_dim;
        meta.num_classes = ds.num_classes;
        meta.train_graph_nodes = ds.num_nodes();
        meta.train_graph_edges = ds.graph.num_edges();
        meta.training_metrics
            .insert("val_acc".into(), winner.val_acc);
        meta.training_metrics
            .insert("test_acc".into(), winner.test_acc);
        meta.training_metrics
            .insert("final_loss".into(), winner.final_loss);
        meta.extra.insert("pipeline".into(), pipeline.name.clone());
        meta.extra.insert("architecture".into(), tag);
        meta.extra.insert(
            "candidate".into(),
            serde_json::to_string(&choice).unwrap_or_default(),
        );
        meta.extra
            .insert("epochs".into(), winner.epochs_completed.to_string());
        meta.set_schema_fingerprint();
        catalog.insert(
            model_name.clone(),
            Model {
                kind: winner.model_kind.clone(),
                metadata: meta,
                weights,
            },
        );
    }
    Ok(TrainResult {
        model_name,
        candidates: outcomes.into_iter().map(|(o, ..)| o).collect(),
        best_index,
    })
}

// --- predict -----------------------------------------------------

/// Applies a trained model to a (possibly different) dataset, with the same feature realization as
/// at train time. One predicted class index per node. GNNs are inductive: the graph may have grown
/// since training, only the feature columns must match.
pub fn predict_node_classification<BI: Backend>(
    pipeline: &NodeClassificationPipeline,
    model: &Model,
    ds: &ClassificationDataset,
    device: &BI::Device,
) -> Result<Vec<i32>> {
    let (feat_buf, feat_dim) = realize_features(pipeline, ds);
    let inputs = GraphInputs::new(ds.graph.clone(), &[]);
    Ok(
        crate::selection::predict_classes::<BI>(model, &inputs, &feat_buf, feat_dim, device)?
            .into_iter()
            .map(|(class, _)| class)
            .collect(),
    )
}

fn parse_hidden(arch: Option<&String>) -> Option<usize> {
    let s = arch?;
    // architectures look like "gcn-2layer-h32-lr1e-2": find the "h" token
    let after_h = s.split('-').find_map(|tok| tok.strip_prefix('h'))?;
    after_h.parse().ok()
}

// --- node regression ---------------------------------------------

/// Per-node continuous-target dataset: like [`ClassificationDataset`], but `targets` are `f32` and
/// `NaN` (not `-1`) means "no label, exclude from train/eval".
#[derive(Debug)]
pub struct RegressionDataset {
    pub graph: CsrGraph,
    pub features: Vec<f32>,
    pub feat_dim: usize,
    pub targets: Vec<f32>,
    pub train_mask: Vec<bool>,
    pub val_mask: Vec<bool>,
    pub test_mask: Vec<bool>,
}

impl RegressionDataset {
    pub fn num_nodes(&self) -> usize {
        self.targets.len()
    }
}

/// Node-regression pipeline, shaped like [`NodeClassificationPipeline`]; only the head, loss and
/// metric differ. Candidates output one scalar per node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegressionPipeline {
    pub name: String,
    pub task: TaskType,
    pub feature_stages: Vec<FeatureStage>,
    pub candidates: Vec<ModelChoice>,
    pub split: SplitConfig,
    pub training: TrainingConfig,
    pub auto_tune: AutoTuneConfig,
}

impl NodeRegressionPipeline {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            task: TaskType::NodeRegression,
            feature_stages: Vec::new(),
            candidates: Vec::new(),
            split: SplitConfig::standard(),
            training: TrainingConfig::default(),
            auto_tune: AutoTuneConfig::default(),
        }
    }

    pub fn add_feature_stage(mut self, stage: FeatureStage) -> Self {
        self.feature_stages.push(stage);
        self
    }

    pub fn add_model_candidate(mut self, candidate: ModelChoice) -> Self {
        self.candidates.push(candidate);
        self
    }

    pub fn with_split(mut self, split: SplitConfig) -> Self {
        self.split = split;
        self
    }

    pub fn with_training(mut self, training: TrainingConfig) -> Self {
        self.training = training;
        self
    }

    pub fn with_auto_tune(mut self, auto_tune: AutoTuneConfig) -> Self {
        self.auto_tune = auto_tune;
        self
    }
}

/// Per-candidate regression result: RMSE instead of accuracy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionCandidateOutcome {
    pub model_kind: String,
    pub hidden: usize,
    pub learning_rate: f64,
    pub val_rmse: f32,
    pub test_rmse: f32,
    pub final_loss: f32,
    pub epochs_completed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionTrainResult {
    pub model_name: String,
    pub candidates: Vec<RegressionCandidateOutcome>,
    pub best_index: usize,
}

impl RegressionTrainResult {
    pub fn best(&self) -> &RegressionCandidateOutcome {
        &self.candidates[self.best_index]
    }
}

/// Stage features for a [`RegressionDataset`]: same logic as the classification version, typed for
/// the regression dataset rather than generic over a dataset trait.
pub fn realize_features_regression(
    pipeline: &NodeRegressionPipeline,
    ds: &RegressionDataset,
) -> (Vec<f32>, usize) {
    let n = ds.num_nodes();
    let raw_dim = ds.feat_dim;
    let stage_dims: Vec<usize> = pipeline
        .feature_stages
        .iter()
        .map(|s| s.out_dim())
        .collect();
    let total_dim = raw_dim + stage_dims.iter().sum::<usize>();
    let mut out = vec![0.0f32; n * total_dim];
    for i in 0..n {
        let dst_row = &mut out[i * total_dim..(i + 1) * total_dim];
        let src_row = &ds.features[i * raw_dim..(i + 1) * raw_dim];
        dst_row[..raw_dim].copy_from_slice(src_row);
    }
    let mut col = raw_dim;
    for (stage, &d) in pipeline.feature_stages.iter().zip(stage_dims.iter()) {
        if d == 0 {
            continue;
        }
        let realized = stage.realize(&ds.graph);
        for i in 0..n {
            out[i * total_dim + col..i * total_dim + col + d]
                .copy_from_slice(&realized[i * d..(i + 1) * d]);
        }
        col += d;
    }
    (out, total_dim)
}

fn build_split_masks_regression(
    pipeline: &NodeRegressionPipeline,
    ds: &RegressionDataset,
) -> (Vec<bool>, Vec<bool>, Vec<bool>) {
    let n = ds.num_nodes();
    let mut labelled: Vec<u32> = (0..n as u32)
        .filter(|&i| !ds.targets[i as usize].is_nan())
        .collect();
    let mut rng = StdRng::seed_from_u64(pipeline.split.seed);
    labelled.shuffle(&mut rng);
    let n_lab = labelled.len();
    let n_train = (n_lab as f32 * pipeline.split.train_fraction) as usize;
    let n_val = (n_lab as f32 * pipeline.split.validation_fraction) as usize;
    let mut tr = vec![false; n];
    let mut va = vec![false; n];
    let mut te = vec![false; n];
    for &idx in &labelled[..n_train] {
        tr[idx as usize] = true;
    }
    for &idx in &labelled[n_train..n_train + n_val] {
        va[idx as usize] = true;
    }
    for &idx in &labelled[n_train + n_val..] {
        te[idx as usize] = true;
    }
    (tr, va, te)
}

/// Trains a node-regression pipeline. Output dim is forced to 1; the winner has the *lowest* val
/// RMSE.
pub fn train_node_regression<BA: AutodiffBackend>(
    pipeline: &NodeRegressionPipeline,
    ds: &RegressionDataset,
    device: &BA::Device,
    catalog: Option<&mut ModelCatalog>,
    cancel: &CancellationToken,
) -> Result<RegressionTrainResult> {
    let masks = build_split_masks_regression(pipeline, ds);
    train_nr_on_masks::<BA>(pipeline, ds, &masks, device, catalog, cancel)
}

fn train_nr_on_masks<BA: AutodiffBackend>(
    pipeline: &NodeRegressionPipeline,
    ds: &RegressionDataset,
    (train_mask, val_mask, test_mask): &(Vec<bool>, Vec<bool>, Vec<bool>),
    device: &BA::Device,
    catalog: Option<&mut ModelCatalog>,
    cancel: &CancellationToken,
) -> Result<RegressionTrainResult> {
    if pipeline.candidates.is_empty() {
        return Err(no_candidates());
    }
    let (feat_buf, feat_dim) = realize_features_regression(pipeline, ds);
    let n = ds.num_nodes();
    let inputs = GraphInputs::new(ds.graph.clone(), &[]);
    let x = Tensor::<BA, 1>::from_floats(feat_buf.as_slice(), device).reshape([n, feat_dim]);
    let labelled = |i: usize| !ds.targets[i].is_nan();
    let (train, val, test) = (
        mask_indices(train_mask, labelled),
        mask_indices(val_mask, labelled),
        mask_indices(test_mask, labelled),
    );

    let mut outcomes: Vec<(RegressionCandidateOutcome, Vec<u8>, ModelChoice, String)> = Vec::new();
    for (candidate, lr, tag) in expanded_candidates(
        &pipeline.candidates,
        &pipeline.auto_tune,
        pipeline.training.learning_rate,
    ) {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let params = FitParams {
            choice: candidate.clone(),
            learning_rate: lr,
            epochs: pipeline.training.epochs,
            patience: pipeline.training.patience,
            weight_decay: 0.0,
            seed: pipeline.split.seed,
            batching: Batching::Auto,
        };
        let obj = Objective::Regress {
            targets: &ds.targets,
            train: &train,
            val: &val,
        };
        let fitted = fit::<BA>(&params, feat_dim, 1, &inputs, &x, &obj, device, cancel)?;
        let out = tensor_to_vec(fitted.encoder.forward(x.clone().inner(), &inputs));
        let rmse = |idx: &[u32]| {
            regression_metrics(&ds.targets, &out, idx)[&Metric::RootMeanSquaredError] as f32
        };
        outcomes.push((
            RegressionCandidateOutcome {
                model_kind: candidate.kind().to_string(),
                hidden: candidate.hidden(),
                learning_rate: lr,
                val_rmse: rmse(&val),
                test_rmse: rmse(&test),
                final_loss: fitted.final_loss,
                epochs_completed: fitted.epochs_run,
            },
            fitted.encoder.to_bytes()?,
            candidate,
            tag,
        ));
    }

    // lowest val RMSE wins, ties broken by test RMSE
    let best_index = best_by(&outcomes, |o| -o.0.val_rmse, |o| -o.0.test_rmse);
    let (winner, weights, choice, tag) = outcomes[best_index].clone();
    let model_name = format!("{}-{}", pipeline.name, tag);
    if let Some(catalog) = catalog {
        let mut meta = ModelMetadata::now();
        meta.feature_dim = feat_dim;
        meta.num_classes = 1;
        meta.train_graph_nodes = ds.num_nodes();
        meta.train_graph_edges = ds.graph.num_edges();
        meta.training_metrics
            .insert("val_rmse".into(), winner.val_rmse);
        meta.training_metrics
            .insert("test_rmse".into(), winner.test_rmse);
        meta.training_metrics
            .insert("final_loss".into(), winner.final_loss);
        meta.extra.insert("pipeline".into(), pipeline.name.clone());
        meta.extra.insert("task".into(), "node_regression".into());
        meta.extra.insert("architecture".into(), tag);
        meta.extra.insert(
            "candidate".into(),
            serde_json::to_string(&choice).unwrap_or_default(),
        );
        meta.set_schema_fingerprint();
        catalog.insert(
            model_name.clone(),
            Model {
                kind: winner.model_kind.clone(),
                metadata: meta,
                weights,
            },
        );
    }
    Ok(RegressionTrainResult {
        model_name,
        candidates: outcomes.into_iter().map(|(o, ..)| o).collect(),
        best_index,
    })
}

/// One predicted value per node.
pub fn predict_node_regression<BI: Backend>(
    pipeline: &NodeRegressionPipeline,
    model: &Model,
    ds: &RegressionDataset,
    device: &BI::Device,
) -> Result<Vec<f32>> {
    let (feat_buf, feat_dim) = realize_features_regression(pipeline, ds);
    let inputs = GraphInputs::new(ds.graph.clone(), &[]);
    crate::selection::predict_values::<BI>(model, &inputs, &feat_buf, feat_dim, device)
}

// --- link prediction ---------------------------------------------

/// Edge-list dataset for link prediction. The encoder learns embeddings on `train_graph`
/// (training-positive edges only, no leakage from held-out edges); the decoder scores node pairs by
/// dot product of embeddings.
#[derive(Debug)]
pub struct LinkPredictionDataset {
    /// Message-passing topology, typically the training positives only.
    pub train_graph: CsrGraph,
    pub features: Vec<f32>,
    pub feat_dim: usize,
    pub num_nodes: usize,
    /// Positive edges for training the decoder.
    pub train_pos: Vec<(u32, u32)>,
    /// Held-out positives for picking the winner.
    pub val_pos: Vec<(u32, u32)>,
    /// Held-out positives for final reporting.
    pub test_pos: Vec<(u32, u32)>,
}

impl LinkPredictionDataset {
    pub fn num_nodes(&self) -> usize {
        self.num_nodes
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkPredictionPipeline {
    pub name: String,
    pub task: TaskType,
    pub feature_stages: Vec<FeatureStage>,
    /// Encoder candidates (the `ModelChoice` of NC/NR); the trainer sets `out_dim = embedding_dim`.
    pub candidates: Vec<ModelChoice>,
    /// Embedding width, the encoder's output dim.
    pub embedding_dim: usize,
    /// Negative samples per positive pair per epoch.
    pub neg_samples_per_pos: usize,
    pub split: SplitConfig,
    pub training: TrainingConfig,
    pub auto_tune: AutoTuneConfig,
}

impl LinkPredictionPipeline {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            task: TaskType::LinkPrediction,
            feature_stages: Vec::new(),
            candidates: Vec::new(),
            embedding_dim: 32,
            neg_samples_per_pos: 1,
            split: SplitConfig::standard(),
            training: TrainingConfig::default(),
            auto_tune: AutoTuneConfig::default(),
        }
    }

    pub fn add_feature_stage(mut self, stage: FeatureStage) -> Self {
        self.feature_stages.push(stage);
        self
    }

    pub fn add_model_candidate(mut self, candidate: ModelChoice) -> Self {
        self.candidates.push(candidate);
        self
    }

    pub fn with_embedding_dim(mut self, dim: usize) -> Self {
        self.embedding_dim = dim;
        self
    }

    pub fn with_split(mut self, split: SplitConfig) -> Self {
        self.split = split;
        self
    }

    pub fn with_training(mut self, training: TrainingConfig) -> Self {
        self.training = training;
        self
    }

    pub fn with_auto_tune(mut self, auto_tune: AutoTuneConfig) -> Self {
        self.auto_tune = auto_tune;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkPredictionCandidateOutcome {
    pub model_kind: String,
    pub hidden: usize,
    pub learning_rate: f64,
    pub val_auc: f32,
    pub test_auc: f32,
    pub final_loss: f32,
    pub epochs_completed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkPredictionTrainResult {
    pub model_name: String,
    pub candidates: Vec<LinkPredictionCandidateOutcome>,
    pub best_index: usize,
}

impl LinkPredictionTrainResult {
    pub fn best(&self) -> &LinkPredictionCandidateOutcome {
        &self.candidates[self.best_index]
    }
}

fn realize_features_lp(
    pipeline: &LinkPredictionPipeline,
    ds: &LinkPredictionDataset,
) -> (Vec<f32>, usize) {
    let n = ds.num_nodes();
    let raw_dim = ds.feat_dim;
    let stage_dims: Vec<usize> = pipeline
        .feature_stages
        .iter()
        .map(|s| s.out_dim())
        .collect();
    let total_dim = raw_dim + stage_dims.iter().sum::<usize>();
    let mut out = vec![0.0f32; n * total_dim];
    for i in 0..n {
        out[i * total_dim..i * total_dim + raw_dim]
            .copy_from_slice(&ds.features[i * raw_dim..(i + 1) * raw_dim]);
    }
    let mut col = raw_dim;
    for (stage, &d) in pipeline.feature_stages.iter().zip(stage_dims.iter()) {
        if d == 0 {
            continue;
        }
        let realized = stage.realize(&ds.train_graph);
        for i in 0..n {
            out[i * total_dim + col..i * total_dim + col + d]
                .copy_from_slice(&realized[i * d..(i + 1) * d]);
        }
        col += d;
    }
    (out, total_dim)
}

/// Message-passing inputs over an LP dataset's training topology. GraphSAGE mean-aggregates the
/// unweighted `A + I`, as LP models always have.
fn lp_inputs(train_graph: &CsrGraph) -> GraphInputs {
    let mut g = GraphInputs::new(
        train_graph.clone().add_self_loops().symmetric_normalize(),
        &[],
    );
    g.row = train_graph.clone().add_self_loops().row_normalize();
    g
}

/// Trains an LP pipeline: a GNN encoder mapping each node to `pipeline.embedding_dim`, a fixed
/// dot-product decoder, BCE-with-logits loss against uniform random negatives. The winner has the
/// best validation ROC-AUC.
pub fn train_link_prediction<BA: AutodiffBackend>(
    pipeline: &LinkPredictionPipeline,
    ds: &LinkPredictionDataset,
    device: &BA::Device,
    catalog: Option<&mut ModelCatalog>,
    cancel: &CancellationToken,
) -> Result<LinkPredictionTrainResult> {
    if pipeline.candidates.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "LP pipeline has no candidates",
        ));
    }
    let (feat_buf, feat_dim) = realize_features_lp(pipeline, ds);
    let n = ds.num_nodes();
    let x = Tensor::<BA, 1>::from_floats(feat_buf.as_slice(), device).reshape([n, feat_dim]);
    let inputs = lp_inputs(&ds.train_graph);
    let sampler = NegativeSampler {
        num_nodes: n as u32,
        sources: None,
        targets: None,
        known: ds
            .train_pos
            .iter()
            .chain(&ds.val_pos)
            .chain(&ds.test_pos)
            .map(|&(u, v)| (u.min(v), u.max(v)))
            .collect(),
    };
    let mut rng = StdRng::seed_from_u64(pipeline.split.seed.wrapping_add(0xA17C));
    let eval_neg = sampler.sample(ds.val_pos.len().max(ds.test_pos.len()).max(1) * 3, &mut rng);
    let emb_dim = pipeline.embedding_dim;

    let mut outcomes: Vec<(LinkPredictionCandidateOutcome, Vec<u8>, ModelChoice, String)> =
        Vec::new();
    for (candidate, lr, _) in expanded_candidates(
        &pipeline.candidates,
        &pipeline.auto_tune,
        pipeline.training.learning_rate,
    ) {
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorKind::Interrupted, "cancelled"));
        }
        let params = FitParams {
            choice: candidate.clone(),
            learning_rate: lr,
            epochs: pipeline.training.epochs,
            patience: pipeline.training.patience,
            weight_decay: 0.0,
            seed: pipeline.split.seed,
            batching: Batching::Auto,
        };
        let obj = Objective::Link {
            train_pos: &ds.train_pos,
            val_pos: &ds.val_pos,
            val_neg: &eval_neg,
            negative_ratio: pipeline.neg_samples_per_pos as f64,
            sampler: &sampler,
        };
        let fitted = fit::<BA>(
            &params, feat_dim, emb_dim, &inputs, &x, &obj, device, cancel,
        )?;
        let z = tensor_to_vec(fitted.encoder.forward(x.clone().inner(), &inputs));
        let auc = |pos: &[(u32, u32)]| {
            let p = crate::selection::link_probabilities(&z, emb_dim, pos);
            let q = crate::selection::link_probabilities(&z, emb_dim, &eval_neg);
            link_metrics(&p, &q)[&Metric::RocAuc] as f32
        };
        let tag = format!(
            "{}-h{}-e{}-lr{:.0e}",
            candidate.kind(),
            candidate.hidden(),
            emb_dim,
            lr
        );
        outcomes.push((
            LinkPredictionCandidateOutcome {
                model_kind: candidate.kind().to_string(),
                hidden: candidate.hidden(),
                learning_rate: lr,
                val_auc: auc(&ds.val_pos),
                test_auc: auc(&ds.test_pos),
                final_loss: fitted.final_loss,
                epochs_completed: fitted.epochs_run,
            },
            fitted.encoder.to_bytes()?,
            candidate,
            tag,
        ));
    }

    let best_index = best_by(&outcomes, |o| o.0.val_auc, |o| o.0.test_auc);
    let (winner, weights, choice, tag) = outcomes[best_index].clone();
    let model_name = format!("{}-{}", pipeline.name, tag);
    if let Some(catalog) = catalog {
        let mut meta = ModelMetadata::now();
        meta.feature_dim = feat_dim;
        meta.num_classes = emb_dim;
        meta.train_graph_nodes = ds.num_nodes();
        meta.train_graph_edges = ds.train_graph.num_edges();
        meta.training_metrics
            .insert("val_auc".into(), winner.val_auc);
        meta.training_metrics
            .insert("test_auc".into(), winner.test_auc);
        meta.training_metrics
            .insert("final_loss".into(), winner.final_loss);
        meta.extra.insert("pipeline".into(), pipeline.name.clone());
        meta.extra.insert("task".into(), "link_prediction".into());
        meta.extra.insert("architecture".into(), tag);
        meta.extra.insert(
            "candidate".into(),
            serde_json::to_string(&choice).unwrap_or_default(),
        );
        meta.extra
            .insert("embedding_dim".into(), emb_dim.to_string());
        meta.set_schema_fingerprint();
        catalog.insert(
            model_name.clone(),
            Model {
                kind: winner.model_kind.clone(),
                metadata: meta,
                weights,
            },
        );
    }
    Ok(LinkPredictionTrainResult {
        model_name,
        candidates: outcomes.into_iter().map(|(o, ..)| o).collect(),
        best_index,
    })
}

/// Scores node-pair candidates with a trained LP model. Returns dot-product scores (logits; apply
/// sigmoid for a probability).
pub fn predict_link_scores<BI: Backend>(
    pipeline: &LinkPredictionPipeline,
    model: &Model,
    ds: &LinkPredictionDataset,
    edges: &[(u32, u32)],
    device: &BI::Device,
) -> Result<Vec<f32>> {
    let (feat_buf, feat_dim) = realize_features_lp(pipeline, ds);
    let inputs = lp_inputs(&ds.train_graph);
    let z = crate::selection::encode::<BI>(model, &inputs, &feat_buf, feat_dim, device)?;
    let dim = model.metadata.num_classes.max(1);
    Ok(edges
        .iter()
        .map(|&(u, v)| {
            let a = &z[u as usize * dim..(u as usize + 1) * dim];
            let b = &z[v as usize * dim..(v as usize + 1) * dim];
            a.iter().zip(b).map(|(x, y)| x * y).sum()
        })
        .collect())
}

// --- tests -------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CsrGraph;
    use rand::RngExt;

    type InnerB = burn::backend::NdArray;
    type B = burn::backend::Autodiff<InnerB>;

    fn device() -> burn::backend::ndarray::NdArrayDevice {
        burn::backend::ndarray::NdArrayDevice::default()
    }

    /// Two-block SBM with `block_size` nodes each, intra-block edge density `p_in`, inter-block
    /// `p_out`; a full `ClassificationDataset` for end-to-end tests.
    fn synthetic_sbm_dataset(seed: u64, block_size: usize) -> ClassificationDataset {
        let mut rng = StdRng::seed_from_u64(seed);
        let n = block_size * 2;
        let mut edges: Vec<(u32, u32, f32)> = Vec::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let same_block = (i / block_size) == (j / block_size);
                let p = if same_block { 0.10 } else { 0.005 };
                if rng.random::<f32>() < p {
                    edges.push((i as u32, j as u32, 1.0));
                }
            }
        }
        let graph = CsrGraph::from_edges(n, &edges)
            .add_self_loops()
            .symmetric_normalize();

        // features: a small noisy signal correlated with the block
        let feat_dim = 4;
        let mut features = vec![0.0f32; n * feat_dim];
        for i in 0..n {
            let block = (i / block_size) as f32;
            for f in 0..feat_dim {
                let signal = if f == 0 { block } else { 0.0 };
                let noise = rng.random::<f32>() * 0.1;
                features[i * feat_dim + f] = signal + noise;
            }
        }

        let labels: Vec<i32> = (0..n).map(|i| (i / block_size) as i32).collect();
        let train_mask = vec![true; n];
        let val_mask = vec![true; n];
        let test_mask = vec![true; n];

        ClassificationDataset {
            graph,
            features,
            feat_dim,
            labels,
            train_mask,
            val_mask,
            test_mask,
            num_classes: 2,
        }
    }

    #[test]
    fn feature_stage_realizes_degree() {
        // 4-node line graph 0-1-2-3
        let edges = vec![(0u32, 1u32, 1.0f32), (1, 2, 1.0), (2, 3, 1.0)];
        let graph = CsrGraph::from_edges(4, &edges);
        let stage = FeatureStage::Degree;
        let realized = stage.realize(&graph);
        assert_eq!(realized.len(), 4);
        // degrees 1, 2, 2, 1 -> max 2 -> normalized 0.5, 1.0, 1.0, 0.5
        assert!((realized[0] - 0.5).abs() < 1e-6);
        assert!((realized[1] - 1.0).abs() < 1e-6);
        assert!((realized[2] - 1.0).abs() < 1e-6);
        assert!((realized[3] - 0.5).abs() < 1e-6);

        let log_stage = FeatureStage::LogDegree;
        let log_realized = log_stage.realize(&graph);
        assert_eq!(log_realized.len(), 4);
        // log(1+1) ~ 0.6931, log(1+2) ~ 1.0986
        assert!((log_realized[0] - 2f32.ln()).abs() < 1e-5);
        assert!((log_realized[1] - 3f32.ln()).abs() < 1e-5);
    }

    #[test]
    fn realize_features_concatenates_columns() {
        let ds = synthetic_sbm_dataset(7, 8);
        let pipe = NodeClassificationPipeline::new("test")
            .add_feature_stage(FeatureStage::Degree)
            .add_feature_stage(FeatureStage::LogDegree);
        let (buf, dim) = realize_features(&pipe, &ds);
        assert_eq!(dim, ds.feat_dim + 2);
        assert_eq!(buf.len(), ds.num_nodes() * dim);

        // row 0's first raw-dim columns equal ds.features row 0
        for f in 0..ds.feat_dim {
            assert!(
                (buf[f] - ds.features[f]).abs() < 1e-6,
                "raw column {f} not copied"
            );
        }
    }

    #[test]
    fn build_split_masks_partitions_labelled_nodes() {
        let ds = synthetic_sbm_dataset(11, 50);
        let pipe = NodeClassificationPipeline::new("split-test").with_split(SplitConfig {
            train_fraction: 0.6,
            validation_fraction: 0.2,
            seed: 99,
        });
        let (tr, va, te) = build_split_masks(&pipe, &ds);
        let in_train = tr.iter().filter(|&&b| b).count();
        let in_val = va.iter().filter(|&&b| b).count();
        let in_test = te.iter().filter(|&&b| b).count();
        let labelled = ds.labels.iter().filter(|&&l| l >= 0).count();
        // every labeled node exactly once
        assert_eq!(in_train + in_val + in_test, labelled);
        // no node in two splits
        for i in 0..ds.num_nodes() {
            let bits = tr[i] as u8 + va[i] as u8 + te[i] as u8;
            assert!(bits <= 1, "node {i} appears in {bits} splits");
        }
        // roughly the configured proportions (generous slack for rounding)
        let frac = |c| c as f32 / labelled as f32;
        assert!((frac(in_train) - 0.6).abs() < 0.05);
        assert!((frac(in_val) - 0.2).abs() < 0.05);
        assert!((frac(in_test) - 0.2).abs() < 0.05);
    }

    #[test]
    fn auto_tune_expands_cartesian_product() {
        let cfg = AutoTuneConfig {
            learning_rates: vec![1e-2, 1e-3],
            hidden_dims: vec![16, 32, 64],
        };
        let expanded = cfg.expand(&ModelChoice::Gcn { hidden: 8 }, 5e-2);
        assert_eq!(expanded.len(), 6);
        // an empty grid keeps the base values
        let empty = AutoTuneConfig::default();
        let one = empty.expand(&ModelChoice::Gcn { hidden: 8 }, 5e-2);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].0.hidden(), 8);
        assert!((one[0].1 - 5e-2).abs() < 1e-9);
    }

    #[test]
    fn train_single_gcn_candidate_on_sbm() {
        let ds = synthetic_sbm_dataset(13, 50);
        let pipe = NodeClassificationPipeline::new("sbm-gcn")
            .add_model_candidate(ModelChoice::Gcn { hidden: 16 })
            .with_training(TrainingConfig {
                epochs: 80,
                learning_rate: 1e-2,
                patience: None,
            });
        let cancel = CancellationToken::none();
        let result =
            train_node_classification::<B>(&pipe, &ds, &device(), None, &cancel).expect("train");
        assert_eq!(result.candidates.len(), 1);
        // SBM with a strong block signal: >85% val acc is easy
        assert!(
            result.best().val_acc > 0.85,
            "GCN val_acc on easy SBM = {} (expected > 0.85)",
            result.best().val_acc
        );
    }

    #[test]
    fn train_with_auto_tune_picks_best() {
        let ds = synthetic_sbm_dataset(17, 40);
        let pipe = NodeClassificationPipeline::new("sweep")
            .add_model_candidate(ModelChoice::Gcn { hidden: 16 })
            .with_training(TrainingConfig {
                epochs: 50,
                learning_rate: 1e-2,
                patience: None,
            })
            .with_auto_tune(AutoTuneConfig {
                learning_rates: vec![1e-2, 1e-1],
                hidden_dims: vec![8, 32],
            });
        let cancel = CancellationToken::none();
        let result =
            train_node_classification::<B>(&pipe, &ds, &device(), None, &cancel).expect("train");
        assert_eq!(result.candidates.len(), 4);
        let best_val = result.best().val_acc;
        for c in &result.candidates {
            assert!(
                c.val_acc <= best_val + 1e-6,
                "winner doesn't have max val_acc"
            );
        }
    }

    #[test]
    fn train_with_no_candidates_errors() {
        let ds = synthetic_sbm_dataset(2, 16);
        let pipe = NodeClassificationPipeline::new("empty");
        let cancel = CancellationToken::none();
        let err = train_node_classification::<B>(&pipe, &ds, &device(), None, &cancel)
            .expect_err("should error on empty candidates");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn train_then_predict_round_trip() {
        let ds = synthetic_sbm_dataset(23, 50);
        let pipe = NodeClassificationPipeline::new("rt")
            .add_model_candidate(ModelChoice::Gcn { hidden: 16 })
            .with_training(TrainingConfig {
                epochs: 60,
                learning_rate: 1e-2,
                patience: None,
            });
        let dir = tempfile::tempdir().expect("tempdir");
        let mut catalog = ModelCatalog::new(dir.path().to_path_buf());
        let cancel = CancellationToken::none();
        let result =
            train_node_classification::<B>(&pipe, &ds, &device(), Some(&mut catalog), &cancel)
                .expect("train");
        assert!(catalog.exists(&result.model_name), "model saved");
        catalog.save_to_disk().expect("save");

        // reload + predict on the same dataset, compare argmax counts
        let reloaded = ModelCatalog::load_from_disk(dir.path().to_path_buf()).expect("load");
        let model = reloaded.get(&result.model_name).expect("present");
        let preds =
            predict_node_classification::<InnerB>(&pipe, model, &ds, &device()).expect("predict");
        assert_eq!(preds.len(), ds.num_nodes());
        // on an easy SBM predictions mostly match labels
        let correct = preds
            .iter()
            .zip(ds.labels.iter())
            .filter(|(p, l)| **p == **l)
            .count();
        let acc = correct as f32 / ds.num_nodes() as f32;
        assert!(acc > 0.85, "predict acc on SBM = {acc}");
    }

    // --- auto-tuning tests ---------------------------------------

    // --- cross-validation tests ----------------------------------

    // --- node regression tests -----------------------------------

    /// SBM whose target is `block_id + 0.5 * mean_neighbour_block + eps`, so message passing beats a
    /// per-node-feature baseline: something real to learn.
    fn synthetic_regression_dataset(seed: u64, block_size: usize) -> RegressionDataset {
        let mut rng = StdRng::seed_from_u64(seed);
        let n = block_size * 2;
        let mut edges: Vec<(u32, u32, f32)> = Vec::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let same_block = (i / block_size) == (j / block_size);
                let p = if same_block { 0.12 } else { 0.005 };
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
            for f in 0..feat_dim {
                let signal = if f == 0 { block * 0.1 } else { 0.0 };
                let noise = rng.random::<f32>() * 0.05;
                features[i * feat_dim + f] = signal + noise;
            }
        }
        // target: block_id + noise
        let targets: Vec<f32> = (0..n)
            .map(|i| (i / block_size) as f32 + rng.random::<f32>() * 0.1)
            .collect();

        let train_mask = vec![true; n];
        let val_mask = vec![true; n];
        let test_mask = vec![true; n];
        RegressionDataset {
            graph,
            features,
            feat_dim,
            targets,
            train_mask,
            val_mask,
            test_mask,
        }
    }

    #[test]
    fn train_regression_reduces_rmse() {
        let ds = synthetic_regression_dataset(31, 40);
        // "predict the global mean" scores RMSE ~ 0.5 on a 0/1 target; the trained model must do
        // much better
        let pipe = NodeRegressionPipeline::new("nr-sbm")
            .add_model_candidate(ModelChoice::Gcn { hidden: 16 })
            .with_training(TrainingConfig {
                epochs: 120,
                learning_rate: 5e-2,
                patience: None,
            });
        let cancel = CancellationToken::none();
        let result =
            train_node_regression::<B>(&pipe, &ds, &device(), None, &cancel).expect("train");
        assert_eq!(result.candidates.len(), 1);
        let rmse = result.best().val_rmse;
        assert!(
            rmse < 0.25,
            "regression val_rmse = {rmse} (expected < 0.25 - well below the 0.5 mean-baseline)"
        );
    }

    #[test]
    fn regression_train_predict_round_trip() {
        let ds = synthetic_regression_dataset(37, 30);
        let pipe = NodeRegressionPipeline::new("nr-rt")
            .add_model_candidate(ModelChoice::Gcn { hidden: 16 })
            .with_training(TrainingConfig {
                epochs: 80,
                learning_rate: 5e-2,
                patience: None,
            });
        let dir = tempfile::tempdir().expect("tmp");
        let mut catalog = ModelCatalog::new(dir.path().to_path_buf());
        let cancel = CancellationToken::none();
        let result = train_node_regression::<B>(&pipe, &ds, &device(), Some(&mut catalog), &cancel)
            .expect("train");
        catalog.save_to_disk().expect("save");
        let reloaded = ModelCatalog::load_from_disk(dir.path().to_path_buf()).expect("load");
        let model = reloaded.get(&result.model_name).expect("present");

        let preds =
            predict_node_regression::<InnerB>(&pipe, model, &ds, &device()).expect("predict");
        assert_eq!(preds.len(), ds.num_nodes());
        // predicted block averages land much closer to the {0,1} block centers than random
        let mut sum_block_0 = 0.0;
        let mut count_block_0 = 0;
        let mut sum_block_1 = 0.0;
        let mut count_block_1 = 0;
        let block_size = ds.num_nodes() / 2;
        for (i, &p) in preds.iter().enumerate() {
            if i < block_size {
                sum_block_0 += p;
                count_block_0 += 1;
            } else {
                sum_block_1 += p;
                count_block_1 += 1;
            }
        }
        let avg_b0 = sum_block_0 / count_block_0 as f32;
        let avg_b1 = sum_block_1 / count_block_1 as f32;
        assert!(
            avg_b1 - avg_b0 > 0.5,
            "block-1 avg ({avg_b1}) should be much higher than block-0 ({avg_b0})"
        );
    }

    // --- link prediction tests -----------------------------------

    /// Two-block SBM with 20% of each block's edges held out as test positives and 10% as val;
    /// `train_graph` is built from training positives only.
    fn synthetic_lp_dataset(seed: u64, block_size: usize) -> LinkPredictionDataset {
        let mut rng = StdRng::seed_from_u64(seed);
        let n = block_size * 2;
        let mut all_edges: Vec<(u32, u32)> = Vec::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let same_block = (i / block_size) == (j / block_size);
                let p = if same_block { 0.18 } else { 0.01 };
                if rng.random::<f32>() < p {
                    all_edges.push((i as u32, j as u32));
                }
            }
        }
        // shuffle, then 70% train, 10% val, 20% test
        all_edges.shuffle(&mut rng);
        let n_total = all_edges.len();
        let n_train = (n_total as f32 * 0.7) as usize;
        let n_val = (n_total as f32 * 0.10) as usize;
        let train_pos: Vec<(u32, u32)> = all_edges[..n_train].to_vec();
        let val_pos: Vec<(u32, u32)> = all_edges[n_train..n_train + n_val].to_vec();
        let test_pos: Vec<(u32, u32)> = all_edges[n_train + n_val..].to_vec();

        let train_edges_w: Vec<(u32, u32, f32)> =
            train_pos.iter().map(|(u, v)| (*u, *v, 1.0)).collect();
        let train_graph = CsrGraph::from_edges(n, &train_edges_w);

        // block-correlated features, so the encoder has signal
        let feat_dim = 4;
        let mut features = vec![0.0f32; n * feat_dim];
        for i in 0..n {
            let block = (i / block_size) as f32;
            for f in 0..feat_dim {
                let signal = if f == 0 { block } else { 0.0 };
                let noise = rng.random::<f32>() * 0.05;
                features[i * feat_dim + f] = signal + noise;
            }
        }
        LinkPredictionDataset {
            train_graph,
            features,
            feat_dim,
            num_nodes: n,
            train_pos,
            val_pos,
            test_pos,
        }
    }

    #[test]
    fn train_link_prediction_beats_random() {
        let ds = synthetic_lp_dataset(43, 40);
        let pipe = LinkPredictionPipeline::new("lp-sbm")
            .add_model_candidate(ModelChoice::Gcn { hidden: 32 })
            .with_embedding_dim(32)
            .with_training(TrainingConfig {
                epochs: 200,
                learning_rate: 1e-2,
                patience: None,
            });
        let cancel = CancellationToken::none();
        let result =
            train_link_prediction::<B>(&pipe, &ds, &device(), None, &cancel).expect("train");
        let auc = result.best().val_auc;
        // random is 0.5; require above-random with a generous margin, since val_pos is small (10%
        // holdout, ~14 edges) so the AUC vs random negatives is noisy, and Burn's parameter-init
        // RNG is process-wide so parallel tests perturb it
        assert!(
            auc > 0.60,
            "LP val_auc = {auc} (expected > 0.60 - above 0.5 random)"
        );
    }

    #[test]
    fn link_prediction_train_predict_round_trip() {
        let ds = synthetic_lp_dataset(47, 30);
        let pipe = LinkPredictionPipeline::new("lp-rt")
            .add_model_candidate(ModelChoice::Gcn { hidden: 16 })
            .with_embedding_dim(16)
            .with_training(TrainingConfig {
                epochs: 60,
                learning_rate: 1e-2,
                patience: None,
            });
        let dir = tempfile::tempdir().expect("tmp");
        let mut catalog = ModelCatalog::new(dir.path().to_path_buf());
        let cancel = CancellationToken::none();
        let result = train_link_prediction::<B>(&pipe, &ds, &device(), Some(&mut catalog), &cancel)
            .expect("train");
        catalog.save_to_disk().expect("save");
        let reloaded = ModelCatalog::load_from_disk(dir.path().to_path_buf()).expect("load");
        let model = reloaded.get(&result.model_name).expect("present");

        // test positives + as many random negatives
        let sampler = NegativeSampler {
            num_nodes: ds.num_nodes() as u32,
            sources: None,
            targets: None,
            known: Default::default(),
        };
        let negs = sampler.sample(ds.test_pos.len(), &mut StdRng::seed_from_u64(99));
        let pos_scores = predict_link_scores::<InnerB>(&pipe, model, &ds, &ds.test_pos, &device())
            .expect("predict pos");
        let neg_scores = predict_link_scores::<InnerB>(&pipe, model, &ds, &negs, &device())
            .expect("predict neg");
        let pos_mean: f32 = pos_scores.iter().sum::<f32>() / pos_scores.len() as f32;
        let neg_mean: f32 = neg_scores.iter().sum::<f32>() / neg_scores.len() as f32;
        assert!(
            pos_mean > neg_mean,
            "reloaded model should still rank positives above negatives (pos_mean={pos_mean}, neg_mean={neg_mean})"
        );
    }
}
