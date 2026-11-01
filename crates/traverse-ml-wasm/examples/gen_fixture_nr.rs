//! Generate the WASM smoke-test fixture for node regression
//! (`tests/fixtures/tiny_nr.tvmodel`).
//!
//! Trains a 2-layer GCN with `out_dim = 1` on the same 4-node line
//! graph the NC fixture uses, but with continuous targets
//! `[0.0, 0.0, 1.0, 1.0]` instead of class labels. The trained
//! model should produce per-node predictions close to those
//! targets after ~100 epochs.
//!
//! Run from the workspace root:
//!
//! ```sh
//! cargo run --example gen_fixture_nr -p traverse-ml-wasm
//! ```

use burn::module::Module;
use burn::optim::{AdamConfig, GradientsParams, Optimizer};
use burn::prelude::ElementConversion;
use burn::record::{BinBytesRecorder, FullPrecisionSettings, Recorder};
use burn::tensor::{Tensor, backend::AutodiffBackend};
use std::path::PathBuf;
use traverse_ml::gnn::{Gcn, GcnConfig};
use traverse_ml::graph::CsrGraph;
use traverse_ml::model::{Model, ModelCatalog, ModelMetadata};

type B = burn::backend::Autodiff<burn::backend::NdArray>;

fn mse<BA: AutodiffBackend>(
    preds: Tensor<BA, 2>, // [N, 1]
    targets: Tensor<BA, 1>,
) -> Tensor<BA, 1> {
    let preds_1d = preds.squeeze::<1>();
    let diff = preds_1d - targets;
    let sq = diff.clone() * diff;
    sq.mean()
}

fn main() -> std::io::Result<()> {
    let device = burn::backend::ndarray::NdArrayDevice::default();
    let edges = vec![(0u32, 1u32, 1.0f32), (1, 2, 1.0), (2, 3, 1.0)];
    let graph = CsrGraph::from_edges(4, &edges)
        .add_self_loops()
        .symmetric_normalize();

    let features: Vec<f32> = vec![0.1, 0.2, 0.2, 0.1, 0.8, 0.9, 0.9, 0.8];
    let targets: Vec<f32> = vec![0.0, 0.0, 1.0, 1.0];

    let x: Tensor<B, 2> = Tensor::<B, 1>::from_floats(features.as_slice(), &device).reshape([4, 2]);
    let y: Tensor<B, 1> = Tensor::<B, 1>::from_floats(targets.as_slice(), &device);

    // Regression head: out_dim = 1.
    let mut model: Gcn<B> = GcnConfig::new(2, 8, 1).init(&device);
    let mut optim = AdamConfig::new().init();

    let mut last_loss = f32::NAN;
    for _ in 1..=150 {
        let preds = model.forward(x.clone(), &graph);
        let loss = mse::<B>(preds, y.clone());
        last_loss = loss.clone().into_scalar().elem();
        let grads = loss.backward();
        let grads = GradientsParams::from_grads(grads, &model);
        model = optim.step(2e-2, model, grads);
    }
    eprintln!("final NR training loss: {last_loss:.4}");

    let recorder = BinBytesRecorder::<FullPrecisionSettings>::new();
    let weights = recorder.record(model.into_record(), ()).expect("recorder");

    let mut meta = ModelMetadata::now();
    meta.feature_dim = 2;
    meta.num_classes = 1; // out_dim
    meta.train_graph_nodes = 4;
    meta.train_graph_edges = graph.num_edges();
    meta.training_metrics.insert("final_loss".into(), last_loss);
    meta.extra
        .insert("architecture".into(), "gcn-2layer-h8".into());
    meta.extra
        .insert("fixture".into(), "wasm_predict_nr_smoke".into());
    meta.extra.insert("task".into(), "node_regression".into());
    meta.set_schema_fingerprint();

    let saved = Model {
        kind: "gcn-2layer".into(),
        metadata: meta,
        weights,
    };
    let fixture_dir: PathBuf = PathBuf::from("crates/traverse-ml-wasm/tests/fixtures");
    let mut catalog = ModelCatalog::new(fixture_dir.clone());
    catalog.insert("tiny_nr", saved);
    catalog.save_to_disk()?;
    eprintln!("wrote {}", fixture_dir.join("tiny_nr.tvmodel").display());
    Ok(())
}
