//! Generate the WASM smoke-test fixture (`tests/fixtures/tiny_gcn.tvmodel`).
//!
//! Trains a 2-layer GCN on a deterministic 4-node line graph with
//! 2-class block labels, then serialises the trained model via
//! `ModelCatalog::save_to_disk`. The result is committed to the
//! repo; tests `include_bytes!` it.
//!
//! Run from the workspace root:
//!
//! ```sh
//! cargo run --example gen_fixture -p traverse-ml-wasm
//! ```
//!
//! The fixture must be regenerated whenever the `Model` /
//! `ModelMetadata` schema changes (the encoding is positional, so a
//! changed field set no longer decodes). The checked-in fixtures were
//! written by 0.8.2 in the headerless bincode layout and so exercise
//! the legacy decode path; regenerating produces the framed
//! `traverse-codec` layout - both load. Detect schema drift
//! via the `wasm_smoke_predict_round_trips` test in
//! `tests/wasm_predict.rs` panicking on `ModelHandle::new`.

use burn::module::Module;
use burn::optim::{AdamConfig, GradientsParams, Optimizer};
use burn::prelude::ElementConversion;
use burn::record::{BinBytesRecorder, FullPrecisionSettings, Recorder};
use burn::tensor::{Int, Tensor, activation::log_softmax, backend::AutodiffBackend};
use std::path::PathBuf;
use traverse_ml::gnn::{Gcn, GcnConfig};
use traverse_ml::graph::CsrGraph;
use traverse_ml::model::{Model, ModelCatalog, ModelMetadata};

type B = burn::backend::Autodiff<burn::backend::NdArray>;

fn nll<BA: AutodiffBackend>(logits: Tensor<BA, 2>, labels: Tensor<BA, 1, Int>) -> Tensor<BA, 1> {
    let log_probs = log_softmax(logits, 1);
    let target_lp = log_probs.gather(1, labels.unsqueeze_dim(1));
    target_lp.neg().mean()
}

fn main() -> std::io::Result<()> {
    let device = burn::backend::ndarray::NdArrayDevice::default();

    // 4-node line graph 0-1-2-3 with self-loops + symmetric norm.
    // Hand-picked edge list is the simplest deterministic shape
    // we can reproduce on the WASM side without any RNG.
    let edges = vec![(0u32, 1u32, 1.0f32), (1, 2, 1.0), (2, 3, 1.0)];
    let graph = CsrGraph::from_edges(4, &edges)
        .add_self_loops()
        .symmetric_normalize();

    // 2 features per node, hand-picked to be linearly separable by
    // block (nodes 0, 1 vs nodes 2, 3) once message-passed.
    let features: Vec<f32> = vec![
        0.1, 0.2, // node 0  (block 0)
        0.2, 0.1, // node 1  (block 0)
        0.8, 0.9, // node 2  (block 1)
        0.9, 0.8, // node 3  (block 1)
    ];
    let labels: Vec<i32> = vec![0, 0, 1, 1];

    let x: Tensor<B, 2> = Tensor::<B, 1>::from_floats(features.as_slice(), &device).reshape([4, 2]);
    let y: Tensor<B, 1, Int> = Tensor::<B, 1, Int>::from_ints(labels.as_slice(), &device);

    let mut model: Gcn<B> = GcnConfig::new(2, 8, 2).init(&device);
    let mut optim = AdamConfig::new().init();

    let mut last_loss = f32::NAN;
    for _ in 1..=100 {
        let logits = model.forward(x.clone(), &graph);
        let loss = nll::<B>(logits, y.clone());
        last_loss = loss.clone().into_scalar().elem();
        let grads = loss.backward();
        let grads = GradientsParams::from_grads(grads, &model);
        model = optim.step(1e-2, model, grads);
    }
    eprintln!("final training loss: {last_loss:.4}");

    let recorder = BinBytesRecorder::<FullPrecisionSettings>::new();
    let weights = recorder.record(model.into_record(), ()).expect("recorder");

    let mut meta = ModelMetadata::now();
    meta.feature_dim = 2;
    meta.num_classes = 2;
    meta.train_graph_nodes = 4;
    meta.train_graph_edges = graph.num_edges();
    meta.training_metrics.insert("final_loss".into(), last_loss);
    meta.extra
        .insert("architecture".into(), "gcn-2layer-h8".into());
    meta.extra
        .insert("fixture".into(), "wasm_predict_smoke".into());
    meta.set_schema_fingerprint();

    let saved = Model {
        kind: "gcn-2layer".into(),
        metadata: meta,
        weights,
    };
    let fixture_dir: PathBuf = PathBuf::from("crates/traverse-ml-wasm/tests/fixtures");
    let mut catalog = ModelCatalog::new(fixture_dir.clone());
    catalog.insert("tiny_gcn", saved);
    catalog.save_to_disk()?;
    eprintln!("wrote {}", fixture_dir.join("tiny_gcn.tvmodel").display());
    eprintln!(
        "train_graph_nodes = 4, train_graph_edges = {}",
        graph.num_edges()
    );
    Ok(())
}
