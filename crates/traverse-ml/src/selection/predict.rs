//! Prediction with a trained model.

use std::io::{Error, ErrorKind, Result};

use burn::tensor::Tensor;
use burn::tensor::activation::softmax;
use burn::tensor::backend::Backend;

use crate::encoder::{AnyEncoder, GraphInputs};
use crate::model::Model;
use crate::pipeline::ModelChoice;

use super::data::{parse_floats, standardize};
use super::fit::{infer, tensor_to_vec};

/// Features as the model was trained on them: standardized with the stored stats, or unchanged for
/// models trained without them.
pub fn prepare_features(model: &Model, features: &[f32], feat_dim: usize) -> Result<Vec<f32>> {
    if feat_dim != model.metadata.feature_dim {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "the model expects {} feature columns, the graph gives {feat_dim}",
                model.metadata.feature_dim
            ),
        ));
    }
    let extra = &model.metadata.extra;
    match (extra.get("featureMeans"), extra.get("featureStds")) {
        (Some(m), Some(s)) => {
            let (means, stds) = (parse_floats(m), parse_floats(s));
            if means.len() != feat_dim || stds.len() != feat_dim {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "stored feature statistics don't match the feature count",
                ));
            }
            Ok(standardize(features, feat_dim, &means, &stds))
        }
        _ => Ok(features.to_vec()),
    }
}

/// Relation count a stored model was trained with (1 for models without one).
pub fn model_relations(model: &Model) -> usize {
    model
        .metadata
        .extra
        .get("numRelations")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

/// Loads a stored model's encoder. R-GCN models need `inputs` with the training relation count.
pub fn load_encoder<B: Backend>(
    model: &Model,
    inputs: &GraphInputs,
    device: &B::Device,
) -> Result<AnyEncoder<B>> {
    let choice = ModelChoice::of_model(model)?;
    let relations = model_relations(model);
    if matches!(choice, ModelChoice::RGcn { .. }) && inputs.num_relations() != relations {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "the model was trained on {relations} relationship types, the graph gives {}",
                inputs.num_relations()
            ),
        ));
    }
    AnyEncoder::from_bytes(
        &choice,
        &model.weights,
        model.metadata.feature_dim,
        model.metadata.num_classes,
        relations,
        device,
    )
}

/// Raw encoder output `[N x out_dim]` (row-major) for a stored model.
pub fn encode<B: Backend>(
    model: &Model,
    inputs: &GraphInputs,
    features: &[f32],
    feat_dim: usize,
    device: &B::Device,
) -> Result<Vec<f32>> {
    let feats = prepare_features(model, features, feat_dim)?;
    let n = inputs.num_nodes();
    let x = Tensor::<B, 1>::from_floats(feats.as_slice(), device).reshape([n, feat_dim]);
    let enc = load_encoder::<B>(model, inputs, device)?;
    Ok(tensor_to_vec(infer(&enc, x, inputs)))
}

/// Predicted class and class probabilities per node.
pub fn predict_classes<B: Backend>(
    model: &Model,
    inputs: &GraphInputs,
    features: &[f32],
    feat_dim: usize,
    device: &B::Device,
) -> Result<Vec<(i32, Vec<f32>)>> {
    let feats = prepare_features(model, features, feat_dim)?;
    let n = inputs.num_nodes();
    let x = Tensor::<B, 1>::from_floats(feats.as_slice(), device).reshape([n, feat_dim]);
    let enc = load_encoder::<B>(model, inputs, device)?;
    let c = model.metadata.num_classes.max(1);
    let probs = tensor_to_vec(softmax(infer(&enc, x, inputs), 1));
    Ok(probs
        .chunks(c)
        .map(|row| {
            let class = row
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i as i32)
                .unwrap_or(0);
            (class, row.to_vec())
        })
        .collect())
}

/// Predicted value per node, in target units.
pub fn predict_values<B: Backend>(
    model: &Model,
    inputs: &GraphInputs,
    features: &[f32],
    feat_dim: usize,
    device: &B::Device,
) -> Result<Vec<f32>> {
    let raw = encode::<B>(model, inputs, features, feat_dim, device)?;
    let get = |k: &str, d: f32| {
        model
            .metadata
            .extra
            .get(k)
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(d)
    };
    let (mean, std) = (get("targetMean", 0.0), get("targetStd", 1.0));
    Ok(raw.into_iter().map(|p| p * std + mean).collect())
}

/// `sigmoid(z_u * z_v)` per pair from row-major embeddings of width `dim`.
pub fn link_probabilities(embeddings: &[f32], dim: usize, pairs: &[(u32, u32)]) -> Vec<f32> {
    pairs
        .iter()
        .map(|&(u, v)| {
            let a = &embeddings[u as usize * dim..(u as usize + 1) * dim];
            let b = &embeddings[v as usize * dim..(v as usize + 1) * dim];
            let s: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            1.0 / (1.0 + (-s).exp())
        })
        .collect()
}
