//! WebAssembly bindings for `traverse-ml`.
//!
//! Companion to `traverse-wasm` (Cypher + GDS), shipped as a separate crate /
//! npm package because the ML artifact (~10 MB CPU, ~30 MB WGPU after release +
//! wasm-opt) is 3-7x the Cypher binary; Cypher-only users shouldn't pay that
//! download. See `crates/traverse-ml/README.md`.
//!
//! ## Surface
//!
//! Inference only, no training in WASM: **train on the server, predict in the
//! browser**.
//!
//! 1. Train server-side with `traverse-ml`; `ModelCatalog::save_to_disk()`
//!    produces `.tvmodel` bytes.
//! 2. Ship the bytes to the browser (HTTP fetch, OPFS pre-seed, or a
//!    wasm-bindgen byte buffer).
//! 3. `ModelHandle::new(bytes)` in the browser.
//! 4. `predict_node_classification(model, graph, features)`.
//!
//! The OPFS helpers in `js/index.js` round-trip `.tvmodel` files the way
//! `traverse-wasm` does `.tvdb` files.
//!
//! ## Build
//!
//! ```sh
//! # CPU (NdArray) - smaller artifact, runs anywhere:
//! wasm-pack build crates/traverse-ml-wasm --target web --release \
//!     --no-default-features --features cpu
//!
//! # WGPU - larger artifact, runs on GPU via WebGPU. Requires
//! # Chrome 113+ or Firefox Nightly with WebGPU + COOP/COEP.
//! wasm-pack build crates/traverse-ml-wasm --target web --release \
//!     --no-default-features --features wgpu
//! ```

#[cfg(target_arch = "wasm32")]
mod wasm {
    use serde::{Deserialize, Serialize};
    use traverse_ml::graph::CsrGraph;
    use traverse_ml::loader::ClassificationDataset;
    use traverse_ml::model::{InMemoryStorage, Model, ModelCatalog};
    use traverse_ml::pipeline::{
        FeatureStage, LinkPredictionDataset, LinkPredictionPipeline, ModelChoice,
        NodeClassificationPipeline, NodeRegressionPipeline, RegressionDataset, predict_link_scores,
        predict_node_classification, predict_node_regression,
    };
    use wasm_bindgen::prelude::*;

    /// Panic hook installed on first load, so panics show in the browser console as readable stack
    /// traces instead of `unreachable`.
    fn install_panic_hook() {
        use std::sync::Once;
        static HOOK: Once = Once::new();
        HOOK.call_once(|| {
            console_error_panic_hook::set_once();
        });
    }

    /// JS-side model: the raw `.tvmodel` bytes plus the decoded [`Model`] for the metadata
    /// accessors.
    #[wasm_bindgen]
    pub struct ModelHandle {
        bytes: Vec<u8>,
        model: Model,
    }

    #[wasm_bindgen]
    impl ModelHandle {
        /// Handle from raw `.tvmodel` bytes (usually from OPFS or HTTP).
        #[wasm_bindgen(constructor)]
        pub fn new(bytes: &[u8]) -> Result<ModelHandle, JsValue> {
            install_panic_hook();
            // ModelCatalog::load_all wants a Box<dyn ModelStorage>: seed an in-memory storage with
            // one entry, load, take the model out; keeps `.tvmodel` decoding in one place
            let mut entries = std::collections::HashMap::new();
            entries.insert("the-model".to_string(), bytes.to_vec());
            let storage = InMemoryStorage::from_entries(entries);
            let cat = ModelCatalog::load_all(Box::new(storage))
                .map_err(|e| JsValue::from_str(&format!("decode model: {e}")))?;
            let model = cat
                .get("the-model")
                .ok_or_else(|| JsValue::from_str("model not present after decode"))?
                .clone();
            Ok(ModelHandle {
                bytes: bytes.to_vec(),
                model,
            })
        }

        /// Model metadata as a plain JS object: `{ kind, feature_dim, num_classes,
        /// train_graph_nodes, train_graph_edges, training_metrics, extra }`.
        pub fn metadata(&self) -> Result<JsValue, JsValue> {
            #[derive(Serialize)]
            struct Meta<'a> {
                kind: &'a str,
                feature_dim: usize,
                num_classes: usize,
                train_graph_nodes: usize,
                train_graph_edges: usize,
                training_metrics: &'a std::collections::HashMap<String, f32>,
                extra: &'a std::collections::HashMap<String, String>,
            }
            let m = Meta {
                kind: &self.model.kind,
                feature_dim: self.model.metadata.feature_dim,
                num_classes: self.model.metadata.num_classes,
                train_graph_nodes: self.model.metadata.train_graph_nodes,
                train_graph_edges: self.model.metadata.train_graph_edges,
                training_metrics: &self.model.metadata.training_metrics,
                extra: &self.model.metadata.extra,
            };
            serde_wasm_bindgen::to_value(&m)
                .map_err(|e| JsValue::from_str(&format!("metadata: {e}")))
        }

        /// The underlying bytes (e.g. to flush back to OPFS without a separate JS copy).
        pub fn bytes(&self) -> Vec<u8> {
            self.bytes.clone()
        }
    }

    /// Predict input JSON: `ClassificationDataset` with only the fields predict reads (no labels /
    /// masks).
    #[derive(Deserialize)]
    struct PredictInput {
        /// Edge list `[(src, dst, weight?)]`, weight defaults to 1.0.
        edges: Vec<EdgeJson>,
        /// Flat row-major features, `[num_nodes x feat_dim]`.
        features: Vec<f32>,
        feat_dim: usize,
        num_nodes: usize,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum EdgeJson {
        Weighted([f32; 3]),
        Unweighted([u32; 2]),
    }

    /// Node-classification inference: the caller passes JSON describing graph + features, gets one
    /// predicted class index per node (`Vec<i32>`). The pipeline is minimal (no feature stages, one
    /// candidate matching the model's architecture), which works because
    /// `predict_node_classification` only uses the pipeline for feature realization, a no-op
    /// without stages.
    #[wasm_bindgen]
    pub fn predict_node_classification_js(
        model: &ModelHandle,
        input_json: &str,
    ) -> Result<JsValue, JsValue> {
        install_panic_hook();
        let input: PredictInput = serde_json::from_str(input_json)
            .map_err(|e| JsValue::from_str(&format!("input JSON: {e}")))?;

        // the graph, with self-loops + symmetric_normalize like the trainer's GCN preprocessing
        // (`predict_node_classification` hands ds.graph to the GCN as-is)
        let edges: Vec<(u32, u32, f32)> = input
            .edges
            .into_iter()
            .map(|e| match e {
                EdgeJson::Weighted([s, d, w]) => (s as u32, d as u32, w),
                EdgeJson::Unweighted([s, d]) => (s, d, 1.0),
            })
            .collect();
        let graph = CsrGraph::from_edges(input.num_nodes, &edges)
            .add_self_loops()
            .symmetric_normalize();

        if input.features.len() != input.num_nodes * input.feat_dim {
            return Err(JsValue::from_str(&format!(
                "features length {} doesn't match num_nodes ({}) x feat_dim ({})",
                input.features.len(),
                input.num_nodes,
                input.feat_dim
            )));
        }

        let labels = vec![-1i32; input.num_nodes];
        let train_mask = vec![false; input.num_nodes];
        let val_mask = vec![false; input.num_nodes];
        let test_mask = vec![false; input.num_nodes];
        let ds = ClassificationDataset {
            graph,
            features: input.features,
            feat_dim: input.feat_dim,
            labels,
            train_mask,
            val_mask,
            test_mask,
            num_classes: model.model.metadata.num_classes,
        };

        // one-candidate pipeline matching the saved model's kind; `predict_node_classification`
        // reads the hidden dim off the metadata itself
        // the stored candidate (any kind: GCN, GraphSAGE, R-GCN, GAT); R-GCN models need the
        // single-relation graphs they were trained on, since the input carries no edge types
        let candidate = ModelChoice::of_model(&model.model)
            .map_err(|e| JsValue::from_str(&format!("model: {e}")))?;
        let pipeline = NodeClassificationPipeline::new("wasm-predict")
            .add_model_candidate(candidate)
            // no feature stages: predict uses the raw features (training-time stages are the
            // caller's job before serializing features)
            .add_feature_stage(FeatureStage::Property { name: "raw".into() });

        // backend chosen at build time by the `cpu` / `wgpu` feature
        #[cfg(feature = "wgpu")]
        let preds = {
            let device = burn::backend::wgpu::WgpuDevice::default();
            predict_node_classification::<burn::backend::Wgpu>(
                &pipeline,
                &model.model,
                &ds,
                &device,
            )
            .map_err(|e| JsValue::from_str(&format!("predict (wgpu): {e}")))?
        };
        #[cfg(not(feature = "wgpu"))]
        let preds = {
            let device = burn::backend::ndarray::NdArrayDevice::default();
            predict_node_classification::<burn::backend::NdArray>(
                &pipeline,
                &model.model,
                &ds,
                &device,
            )
            .map_err(|e| JsValue::from_str(&format!("predict (cpu): {e}")))?
        };

        serde_wasm_bindgen::to_value(&preds)
            .map_err(|e| JsValue::from_str(&format!("serialize preds: {e}")))
    }

    // --- node regression ----------------------------------------

    /// NR predict input JSON: like NC without `num_classes` (the model declares its own output dim,
    /// 1 for scalar regression).
    #[derive(Deserialize)]
    struct RegressionPredictInput {
        edges: Vec<EdgeJson>,
        features: Vec<f32>,
        feat_dim: usize,
        num_nodes: usize,
    }

    /// Node-regression inference; one `f32` per node.
    #[wasm_bindgen]
    pub fn predict_node_regression_js(
        model: &ModelHandle,
        input_json: &str,
    ) -> Result<JsValue, JsValue> {
        install_panic_hook();
        let input: RegressionPredictInput = serde_json::from_str(input_json)
            .map_err(|e| JsValue::from_str(&format!("input JSON: {e}")))?;

        let edges: Vec<(u32, u32, f32)> = input
            .edges
            .into_iter()
            .map(|e| match e {
                EdgeJson::Weighted([s, d, w]) => (s as u32, d as u32, w),
                EdgeJson::Unweighted([s, d]) => (s, d, 1.0),
            })
            .collect();
        let graph = CsrGraph::from_edges(input.num_nodes, &edges)
            .add_self_loops()
            .symmetric_normalize();

        if input.features.len() != input.num_nodes * input.feat_dim {
            return Err(JsValue::from_str(&format!(
                "features length {} doesn't match num_nodes ({}) x feat_dim ({})",
                input.features.len(),
                input.num_nodes,
                input.feat_dim
            )));
        }

        let targets = vec![f32::NAN; input.num_nodes];
        let train_mask = vec![false; input.num_nodes];
        let val_mask = vec![false; input.num_nodes];
        let test_mask = vec![false; input.num_nodes];
        let ds = RegressionDataset {
            graph,
            features: input.features,
            feat_dim: input.feat_dim,
            targets,
            train_mask,
            val_mask,
            test_mask,
        };

        // the stored candidate (any kind: GCN, GraphSAGE, R-GCN, GAT); R-GCN models need the
        // single-relation graphs they were trained on, since the input carries no edge types
        let candidate = ModelChoice::of_model(&model.model)
            .map_err(|e| JsValue::from_str(&format!("model: {e}")))?;
        let pipeline = NodeRegressionPipeline::new("wasm-predict")
            .add_model_candidate(candidate)
            .add_feature_stage(FeatureStage::Property { name: "raw".into() });

        #[cfg(feature = "wgpu")]
        let preds = {
            let device = burn::backend::wgpu::WgpuDevice::default();
            predict_node_regression::<burn::backend::Wgpu>(&pipeline, &model.model, &ds, &device)
                .map_err(|e| JsValue::from_str(&format!("predict NR (wgpu): {e}")))?
        };
        #[cfg(not(feature = "wgpu"))]
        let preds = {
            let device = burn::backend::ndarray::NdArrayDevice::default();
            predict_node_regression::<burn::backend::NdArray>(&pipeline, &model.model, &ds, &device)
                .map_err(|e| JsValue::from_str(&format!("predict NR (cpu): {e}")))?
        };

        serde_wasm_bindgen::to_value(&preds)
            .map_err(|e| JsValue::from_str(&format!("serialize preds: {e}")))
    }

    // --- link prediction ----------------------------------------

    /// LP predict input JSON. `edges` is the *training-positive* edge set (the encoder's
    /// message-passing topology, also used for the schema-fingerprint check); `pairs` are the node
    /// pairs to score.
    #[derive(Deserialize)]
    struct LinkPredictInput {
        edges: Vec<[u32; 2]>,
        pairs: Vec<[u32; 2]>,
        features: Vec<f32>,
        feat_dim: usize,
        num_nodes: usize,
    }

    /// Scores node-pair candidates with a trained LP model: one `f32` per `pair`, in order (raw
    /// logit; sigmoid for a probability).
    #[wasm_bindgen]
    pub fn predict_link_scores_js(
        model: &ModelHandle,
        input_json: &str,
    ) -> Result<JsValue, JsValue> {
        install_panic_hook();
        let input: LinkPredictInput = serde_json::from_str(input_json)
            .map_err(|e| JsValue::from_str(&format!("input JSON: {e}")))?;

        // LP's `train_graph` holds raw (unnormalized) edges and the trainer applies self-loops +
        // norm each forward, so no pre-normalizing here
        let weighted_edges: Vec<(u32, u32, f32)> =
            input.edges.iter().map(|[s, d]| (*s, *d, 1.0f32)).collect();
        let train_graph = CsrGraph::from_edges(input.num_nodes, &weighted_edges);

        if input.features.len() != input.num_nodes * input.feat_dim {
            return Err(JsValue::from_str(&format!(
                "features length {} doesn't match num_nodes ({}) x feat_dim ({})",
                input.features.len(),
                input.num_nodes,
                input.feat_dim
            )));
        }

        let pairs: Vec<(u32, u32)> = input.pairs.iter().map(|[s, d]| (*s, *d)).collect();

        let ds = LinkPredictionDataset {
            train_graph,
            features: input.features,
            feat_dim: input.feat_dim,
            num_nodes: input.num_nodes,
            train_pos: Vec::new(),
            val_pos: Vec::new(),
            test_pos: Vec::new(),
        };

        // the stored candidate (any kind: GCN, GraphSAGE, R-GCN, GAT); R-GCN models need the
        // single-relation graphs they were trained on, since the input carries no edge types
        let candidate = ModelChoice::of_model(&model.model)
            .map_err(|e| JsValue::from_str(&format!("model: {e}")))?;
        let pipeline = LinkPredictionPipeline::new("wasm-predict")
            .add_model_candidate(candidate)
            .add_feature_stage(FeatureStage::Property { name: "raw".into() });

        #[cfg(feature = "wgpu")]
        let scores = {
            let device = burn::backend::wgpu::WgpuDevice::default();
            predict_link_scores::<burn::backend::Wgpu>(
                &pipeline,
                &model.model,
                &ds,
                &pairs,
                &device,
            )
            .map_err(|e| JsValue::from_str(&format!("predict LP (wgpu): {e}")))?
        };
        #[cfg(not(feature = "wgpu"))]
        let scores = {
            let device = burn::backend::ndarray::NdArrayDevice::default();
            predict_link_scores::<burn::backend::NdArray>(
                &pipeline,
                &model.model,
                &ds,
                &pairs,
                &device,
            )
            .map_err(|e| JsValue::from_str(&format!("predict LP (cpu): {e}")))?
        };

        serde_wasm_bindgen::to_value(&scores)
            .map_err(|e| JsValue::from_str(&format!("serialize scores: {e}")))
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm::*;
