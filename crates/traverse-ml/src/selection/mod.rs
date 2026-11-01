//! Training with model selection, the way Neo4j GDS trains its ML pipelines:
//!
//! 1. Hold out `test_fraction` of the labelled examples (stratified for classification).
//! 2. Plan trials: every concrete candidate, then random samples from the candidates that
//!    declare ranges until `max_trials`.
//! 3. Score each trial with k-fold cross-validation on the remaining training examples
//!    (stratified for classification), or prune them by successive halving (the core of
//!    Hyperband, Li et al. 2017) when [`SearchStrategy::SuccessiveHalving`] is set.
//! 4. Retrain the winner on the whole training set and report its train ("outer train") and
//!    test metrics.
//!
//! One fit loop serves node classification, node regression and link prediction over any
//! [`AnyEncoder`] kind: AdamW, early stopping on the validation fold with best-weights restore,
//! gradient-norm clipping. Each step sees the whole graph, or a mini-batch of examples with their
//! sampled neighborhoods ([`Batching`]; automatic for graphs too big for full-graph steps).
//! Evaluation and prediction run exact inference a block of rows at a time ([`infer`]), so their
//! memory stays bounded too. Inputs are standardized per feature (stats over all nodes, so no
//! label leakage) and regression targets per training set; both transforms are stored with the
//! model and re-applied at prediction time.

mod data;
mod fit;
mod metrics;
mod predict;
mod search;
#[cfg(test)]
mod tests;
mod train;

pub use data::{LinkData, NodeData, feature_stats};
pub use fit::{Batching, DEFAULT_BATCH_SIZE, DEFAULT_FANOUTS, FULL_GRAPH_VALUES, infer};
pub(crate) use fit::{FitParams, NegativeSampler, Objective, argmax_rows, fit, tensor_to_vec};
pub use metrics::{ClassReport, Metric, classification_metrics, link_metrics, regression_metrics};
pub use predict::{
    encode, link_probabilities, load_encoder, model_relations, predict_classes, predict_values,
    prepare_features,
};
pub use search::{
    CandidateSpec, MetricStats, ParamSpace, SearchStrategy, SelectionConfig, SelectionResult,
    TrialResult,
};
pub use train::{
    train_link_prediction_selected, train_node_classification_selected,
    train_node_regression_selected,
};
