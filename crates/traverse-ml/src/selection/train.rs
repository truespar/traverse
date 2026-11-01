//! Training with model selection, per task: node classification, node regression and link
//! prediction.

use std::io::{Error, ErrorKind, Result};

use burn::tensor::Tensor;
use burn::tensor::backend::AutodiffBackend;
use rand::SeedableRng;
use rand::rngs::StdRng;

use crate::cancel::CancellationToken;
use crate::encoder::{AnyEncoder, GraphInputs};
use crate::model::{Model, ModelMetadata};
use crate::pipeline::TaskType;

use super::data::{
    LinkData, NodeData, feature_stats, join_floats, plain_folds, plain_split, standardize,
    stratified_folds, stratified_holdout, use_folds, without,
};
use super::fit::{
    Batching, FitParams, NegativeSampler, Objective, argmax_rows, fit, infer, pair_scores,
    tensor_to_vec,
};
use super::metrics::{classification_metrics, link_metrics, regression_metrics};
use super::search::{
    CandidateSpec, SelectionConfig, SelectionResult, TrialPlan, TrialResult, fold_stats, pick,
    plan_trials, search,
};

// --- node classification -----------------------------------------

/// Node classification with model selection. `labels[i]` is node `i`'s class (`-1` = unlabelled,
/// excluded from training and evaluation but still part of message passing).
pub fn train_node_classification_selected<BA: AutodiffBackend>(
    data: &NodeData,
    labels: &[i32],
    num_classes: usize,
    candidates: &[CandidateSpec],
    cfg: &SelectionConfig,
    device: &BA::Device,
    cancel: &CancellationToken,
    progress: &dyn Fn(usize, usize),
) -> Result<SelectionResult> {
    let n = data.inputs.num_nodes();
    let (means, stds) = feature_stats(&data.features, n, data.feat_dim);
    let feats = standardize(&data.features, data.feat_dim, &means, &stds);
    let x = Tensor::<BA, 1>::from_floats(feats.as_slice(), device).reshape([n, data.feat_dim]);
    let x_inner = x.clone().inner();

    let (train, test) = stratified_holdout(labels, cfg.test_fraction, cfg.seed);
    let plans = plan_trials(candidates, cfg)?;
    // one empty fold = train once on everything, no validation
    let folds = if use_folds(train.len(), cfg.validation_folds, plans.len(), "nodes")? {
        stratified_folds(&train, labels, cfg.validation_folds, cfg.seed ^ 0xF01D)
    } else {
        vec![Vec::new()]
    };

    // one exact inference per fitted model, scored on as many index sets as needed
    let predictions = |enc: &AnyEncoder<BA::InnerBackend>| {
        argmax_rows(
            &tensor_to_vec(infer(enc, x_inner.clone(), &data.inputs)),
            num_classes,
        )
    };
    let metrics_on =
        |preds: &[i32], idx: &[u32]| classification_metrics(labels, preds, idx, num_classes);
    let fit_params = |plan: &TrialPlan, epochs: usize| FitParams {
        choice: plan.choice.clone(),
        learning_rate: plan.learning_rate,
        epochs,
        patience: cfg.patience,
        weight_decay: cfg.weight_decay,
        seed: cfg.seed,
        batching: cfg.batching,
    };

    let (trials, best) = search(plans, cfg, cancel, progress, |plan, epochs| {
        let (mut val, mut tr, mut best_epochs) = (Vec::new(), Vec::new(), Vec::new());
        for fold in &folds {
            let fold_train = without(&train, fold);
            let obj = Objective::Classify {
                labels,
                train: &fold_train,
                val: fold,
            };
            let fitted = fit::<BA>(
                &fit_params(plan, epochs),
                data.feat_dim,
                num_classes,
                &data.inputs,
                &x,
                &obj,
                device,
                cancel,
            )?;
            let preds = predictions(&fitted.encoder);
            if !fold.is_empty() {
                val.push(metrics_on(&preds, fold).0);
            }
            tr.push(metrics_on(&preds, &fold_train).0);
            best_epochs.push(fitted.best_epoch as f64);
        }
        Ok(TrialResult {
            candidate: plan.choice.clone(),
            learning_rate: plan.learning_rate,
            epochs,
            validation: fold_stats(&val, &cfg.metrics),
            train: fold_stats(&tr, &cfg.metrics),
            best_epoch: best_epochs.iter().sum::<f64>() / best_epochs.len().max(1) as f64,
        })
    })?;

    // retrain the winner on the whole training set for the epochs early stopping found
    let winner = trials[best].clone();
    let final_params = FitParams {
        choice: winner.candidate.clone(),
        learning_rate: winner.learning_rate,
        epochs: winner.best_epoch.round().max(1.0) as usize,
        patience: None,
        weight_decay: cfg.weight_decay,
        seed: cfg.seed,
        batching: cfg.batching,
    };
    let obj = Objective::Classify {
        labels,
        train: &train,
        val: &[],
    };
    let fitted = fit::<BA>(
        &final_params,
        data.feat_dim,
        num_classes,
        &data.inputs,
        &x,
        &obj,
        device,
        cancel,
    )?;
    let preds = predictions(&fitted.encoder);
    let (test_all, class_report) = metrics_on(&preds, &test);
    let (train_all, _) = metrics_on(&preds, &train);
    let test_metrics = pick(&test_all, &cfg.metrics);

    let mut meta = base_metadata(
        TaskType::NodeClassification,
        &winner,
        fitted.batching,
        data.feat_dim,
        num_classes,
        data.inputs.num_relations(),
        &means,
        &stds,
    );
    meta.train_graph_nodes = n;
    meta.train_graph_edges = data.inputs.sym.num_edges();
    for (m, v) in &test_metrics {
        meta.training_metrics
            .insert(format!("test_{}", m.name()), *v as f32);
    }
    let model = Model {
        kind: winner.candidate.kind().to_string(),
        metadata: meta,
        weights: fitted.encoder.to_bytes()?,
    };
    Ok(SelectionResult {
        trials,
        best_trial: best,
        test: test_metrics,
        outer_train: pick(&train_all, &cfg.metrics),
        class_report,
        train_size: train.len(),
        test_size: test.len(),
        batching: fitted.batching,
        model,
    })
}

fn base_metadata(
    task: TaskType,
    winner: &TrialResult,
    batching: Batching,
    feat_dim: usize,
    out_dim: usize,
    num_relations: usize,
    means: &[f32],
    stds: &[f32],
) -> ModelMetadata {
    let mut meta = ModelMetadata::now();
    meta.feature_dim = feat_dim;
    meta.num_classes = out_dim;
    let task_name = match task {
        TaskType::NodeClassification => "node_classification",
        TaskType::NodeRegression => "node_regression",
        TaskType::LinkPrediction => "link_prediction",
    };
    meta.extra.insert("task".into(), task_name.into());
    meta.extra.insert(
        "candidate".into(),
        serde_json::to_string(&winner.candidate).unwrap_or_default(),
    );
    meta.extra.insert(
        "architecture".into(),
        format!(
            "{}-h{}-lr{:.0e}",
            winner.candidate.kind(),
            winner.candidate.hidden(),
            winner.learning_rate
        ),
    );
    meta.extra
        .insert("learningRate".into(), winner.learning_rate.to_string());
    meta.extra.insert(
        "batching".into(),
        serde_json::to_string(&batching).unwrap_or_default(),
    );
    meta.extra
        .insert("numRelations".into(), num_relations.to_string());
    meta.extra.insert("featureMeans".into(), join_floats(means));
    meta.extra.insert("featureStds".into(), join_floats(stds));
    meta.set_schema_fingerprint();
    meta
}

// --- node regression ---------------------------------------------

/// Node regression with model selection. `targets[i]` is `NaN` for nodes without a value.
pub fn train_node_regression_selected<BA: AutodiffBackend>(
    data: &NodeData,
    targets: &[f32],
    candidates: &[CandidateSpec],
    cfg: &SelectionConfig,
    device: &BA::Device,
    cancel: &CancellationToken,
    progress: &dyn Fn(usize, usize),
) -> Result<SelectionResult> {
    let n = data.inputs.num_nodes();
    let (means, stds) = feature_stats(&data.features, n, data.feat_dim);
    let feats = standardize(&data.features, data.feat_dim, &means, &stds);
    let x = Tensor::<BA, 1>::from_floats(feats.as_slice(), device).reshape([n, data.feat_dim]);
    let x_inner = x.clone().inner();

    let labelled: Vec<u32> = (0..n as u32)
        .filter(|&i| !targets[i as usize].is_nan())
        .collect();
    let (train, test) = plain_split(&labelled, cfg.test_fraction, cfg.seed);
    let plans = plan_trials(candidates, cfg)?;
    let folds = if use_folds(train.len(), cfg.validation_folds, plans.len(), "nodes")? {
        plain_folds(&train, cfg.validation_folds, cfg.seed ^ 0xF01D)
    } else {
        vec![Vec::new()]
    };

    // targets standardized with training-set stats; predictions mapped back for metrics
    let train_vals: Vec<f32> = train.iter().map(|&i| targets[i as usize]).collect();
    let (t_mean, t_std) = feature_stats(&train_vals, train_vals.len(), 1);
    let (t_mean, t_std) = (t_mean[0], t_std[0]);
    let scaled: Vec<f32> = targets
        .iter()
        .map(|&t| {
            if t.is_nan() {
                0.0
            } else {
                (t - t_mean) / t_std
            }
        })
        .collect();

    // one exact inference per fitted model, in target units
    let predictions = |enc: &AnyEncoder<BA::InnerBackend>| -> Vec<f32> {
        tensor_to_vec(infer(enc, x_inner.clone(), &data.inputs))
            .into_iter()
            .map(|p| p * t_std + t_mean)
            .collect()
    };
    let (trials, best) = search(plans, cfg, cancel, progress, |plan, epochs| {
        let (mut val, mut tr, mut best_epochs) = (Vec::new(), Vec::new(), Vec::new());
        for fold in &folds {
            let fold_train = without(&train, fold);
            let obj = Objective::Regress {
                targets: &scaled,
                train: &fold_train,
                val: fold,
            };
            let params = FitParams {
                choice: plan.choice.clone(),
                learning_rate: plan.learning_rate,
                epochs,
                patience: cfg.patience,
                weight_decay: cfg.weight_decay,
                seed: cfg.seed,
                batching: cfg.batching,
            };
            let fitted = fit::<BA>(
                &params,
                data.feat_dim,
                1,
                &data.inputs,
                &x,
                &obj,
                device,
                cancel,
            )?;
            let preds = predictions(&fitted.encoder);
            if !fold.is_empty() {
                val.push(regression_metrics(targets, &preds, fold));
            }
            tr.push(regression_metrics(targets, &preds, &fold_train));
            best_epochs.push(fitted.best_epoch as f64);
        }
        Ok(TrialResult {
            candidate: plan.choice.clone(),
            learning_rate: plan.learning_rate,
            epochs,
            validation: fold_stats(&val, &cfg.metrics),
            train: fold_stats(&tr, &cfg.metrics),
            best_epoch: best_epochs.iter().sum::<f64>() / best_epochs.len().max(1) as f64,
        })
    })?;

    let winner = trials[best].clone();
    let params = FitParams {
        choice: winner.candidate.clone(),
        learning_rate: winner.learning_rate,
        epochs: winner.best_epoch.round().max(1.0) as usize,
        patience: None,
        weight_decay: cfg.weight_decay,
        seed: cfg.seed,
        batching: cfg.batching,
    };
    let obj = Objective::Regress {
        targets: &scaled,
        train: &train,
        val: &[],
    };
    let fitted = fit::<BA>(
        &params,
        data.feat_dim,
        1,
        &data.inputs,
        &x,
        &obj,
        device,
        cancel,
    )?;
    let preds = predictions(&fitted.encoder);
    let test_metrics = pick(&regression_metrics(targets, &preds, &test), &cfg.metrics);
    let train_metrics = pick(&regression_metrics(targets, &preds, &train), &cfg.metrics);

    let mut meta = base_metadata(
        TaskType::NodeRegression,
        &winner,
        fitted.batching,
        data.feat_dim,
        1,
        data.inputs.num_relations(),
        &means,
        &stds,
    );
    meta.train_graph_nodes = n;
    meta.train_graph_edges = data.inputs.sym.num_edges();
    meta.extra.insert("targetMean".into(), t_mean.to_string());
    meta.extra.insert("targetStd".into(), t_std.to_string());
    for (m, v) in &test_metrics {
        meta.training_metrics
            .insert(format!("test_{}", m.name()), *v as f32);
    }
    let model = Model {
        kind: winner.candidate.kind().to_string(),
        metadata: meta,
        weights: fitted.encoder.to_bytes()?,
    };
    Ok(SelectionResult {
        trials,
        best_trial: best,
        test: test_metrics,
        outer_train: train_metrics,
        class_report: Vec::new(),
        train_size: train.len(),
        test_size: test.len(),
        batching: fitted.batching,
        model,
    })
}

// --- link prediction ---------------------------------------------

/// Link prediction with model selection: the encoder embeds nodes, a dot product scores a pair.
/// Held-out edges never take part in message passing while they're evaluated.
pub fn train_link_prediction_selected<BA: AutodiffBackend>(
    data: &LinkData,
    candidates: &[CandidateSpec],
    cfg: &SelectionConfig,
    device: &BA::Device,
    cancel: &CancellationToken,
    progress: &dyn Fn(usize, usize),
) -> Result<SelectionResult> {
    let n = data.num_nodes;
    if data.positives.len() < 4 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "only {} target relationships; link prediction needs more examples",
                data.positives.len()
            ),
        ));
    }
    let (means, stds) = feature_stats(&data.features, n, data.feat_dim);
    let feats = standardize(&data.features, data.feat_dim, &means, &stds);
    let x = Tensor::<BA, 1>::from_floats(feats.as_slice(), device).reshape([n, data.feat_dim]);
    let x_inner = x.clone().inner();

    let sampler = NegativeSampler {
        num_nodes: n as u32,
        sources: data.source_nodes.clone(),
        targets: data.target_nodes.clone(),
        known: data
            .positives
            .iter()
            .map(|&(u, v)| (u.min(v), u.max(v)))
            .collect(),
    };
    let mut rng = StdRng::seed_from_u64(cfg.seed ^ 0x11E6);
    let neg_for = |pos: usize, rng: &mut StdRng| {
        let count = ((pos as f64) * cfg.negative_sampling_ratio).ceil().max(1.0) as usize;
        sampler.sample(count, rng)
    };

    let (train, test) = plain_split(&data.positives, cfg.test_fraction, cfg.seed);
    let plans = plan_trials(candidates, cfg)?;
    let test_neg = neg_for(test.len(), &mut rng);
    let folds = if use_folds(
        train.len(),
        cfg.validation_folds,
        plans.len(),
        "relationships",
    )? {
        plain_folds(&train, cfg.validation_folds, cfg.seed ^ 0xF01D)
    } else {
        vec![Vec::new()]
    };
    let fold_inputs: Vec<(Vec<(u32, u32)>, GraphInputs, Vec<(u32, u32)>)> = folds
        .iter()
        .map(|fold| {
            let rest = without(&train, fold);
            let g = data.inputs(&rest);
            let neg = neg_for(fold.len(), &mut rng);
            (rest, g, neg)
        })
        .collect();

    // embeddings from one exact inference per fitted model, scored on several pair sets
    let embed =
        |enc: &AnyEncoder<BA::InnerBackend>, g: &GraphInputs| infer(enc, x_inner.clone(), g);
    let score_pairs = |z: &Tensor<BA::InnerBackend, 2>, pos: &[(u32, u32)], neg: &[(u32, u32)]| {
        let p = tensor_to_vec(pair_scores(z.clone(), pos, device));
        let q = tensor_to_vec(pair_scores(z.clone(), neg, device));
        link_metrics(&p, &q)
    };
    let emb = cfg.embedding_dim.max(1);
    let (trials, best) = search(plans, cfg, cancel, progress, |plan, epochs| {
        let (mut val, mut tr, mut best_epochs) = (Vec::new(), Vec::new(), Vec::new());
        for (fold, (rest, g, fold_neg)) in folds.iter().zip(&fold_inputs) {
            let obj = Objective::Link {
                train_pos: rest,
                val_pos: fold,
                val_neg: fold_neg,
                negative_ratio: cfg.negative_sampling_ratio,
                sampler: &sampler,
            };
            let params = FitParams {
                choice: plan.choice.clone(),
                learning_rate: plan.learning_rate,
                epochs,
                patience: cfg.patience,
                weight_decay: cfg.weight_decay,
                seed: cfg.seed,
                batching: cfg.batching,
            };
            let fitted = fit::<BA>(&params, data.feat_dim, emb, g, &x, &obj, device, cancel)?;
            let z = embed(&fitted.encoder, g);
            if !fold.is_empty() {
                val.push(score_pairs(&z, fold, fold_neg));
            }
            let train_neg =
                sampler.sample(fold_neg.len().max(1), &mut StdRng::seed_from_u64(cfg.seed));
            tr.push(score_pairs(&z, rest, &train_neg));
            best_epochs.push(fitted.best_epoch as f64);
        }
        Ok(TrialResult {
            candidate: plan.choice.clone(),
            learning_rate: plan.learning_rate,
            epochs,
            validation: fold_stats(&val, &cfg.metrics),
            train: fold_stats(&tr, &cfg.metrics),
            best_epoch: best_epochs.iter().sum::<f64>() / best_epochs.len().max(1) as f64,
        })
    })?;

    let winner = trials[best].clone();
    let g_train = data.inputs(&train);
    let params = FitParams {
        choice: winner.candidate.clone(),
        learning_rate: winner.learning_rate,
        epochs: winner.best_epoch.round().max(1.0) as usize,
        patience: None,
        weight_decay: cfg.weight_decay,
        seed: cfg.seed,
        batching: cfg.batching,
    };
    let obj = Objective::Link {
        train_pos: &train,
        val_pos: &[],
        val_neg: &[],
        negative_ratio: cfg.negative_sampling_ratio,
        sampler: &sampler,
    };
    let fitted = fit::<BA>(
        &params,
        data.feat_dim,
        emb,
        &g_train,
        &x,
        &obj,
        device,
        cancel,
    )?;
    let z = embed(&fitted.encoder, &g_train);
    let test_metrics = pick(&score_pairs(&z, &test, &test_neg), &cfg.metrics);
    let train_neg = neg_for(train.len(), &mut rng);
    let train_metrics = pick(&score_pairs(&z, &train, &train_neg), &cfg.metrics);

    let mut meta = base_metadata(
        TaskType::LinkPrediction,
        &winner,
        fitted.batching,
        data.feat_dim,
        emb,
        data.num_relations.max(1),
        &means,
        &stds,
    );
    meta.train_graph_nodes = n;
    meta.train_graph_edges = g_train.sym.num_edges();
    meta.extra.insert("embeddingDim".into(), emb.to_string());
    meta.extra
        .insert("targetRelation".into(), data.target_relation.to_string());
    for (m, v) in &test_metrics {
        meta.training_metrics
            .insert(format!("test_{}", m.name()), *v as f32);
    }
    let model = Model {
        kind: winner.candidate.kind().to_string(),
        metadata: meta,
        weights: fitted.encoder.to_bytes()?,
    };
    Ok(SelectionResult {
        trials,
        best_trial: best,
        test: test_metrics,
        outer_train: train_metrics,
        class_report: Vec::new(),
        train_size: train.len(),
        test_size: test.len(),
        batching: fitted.batching,
        model,
    })
}
