//! Trains a 2-layer GCN on Pokec-small and saves it to a `.tvmodel` catalog;
//! `predict_pokec` then loads it and checks it reproduces the training-time test
//! accuracy (to float epsilon).
//!
//! Pokec is the SNAP Slovak social network with demographic node attributes.
//! The bench tool downloads the small variant (10k nodes, 122k edges):
//!
//! ```text
//! cargo run --release -p traverse-bench -- --download small --format csv
//! ```
//!
//! which writes `pokec_small_nodes.csv` and `pokec_small_edges.csv` to
//! `benchmarks/data/dataset_cache/`; pass another directory as the first argument.
//!
//! Task: binary gender classification (man/woman). Nodes with unknown gender
//! are excluded from the masks but stay in the graph (their features still
//! propagate). Splits 60/20/20, seed=42.
//!
//! Features are the raw demographic columns (completion%, age), not random, so
//! this and `predict_pokec` build identical inputs and the reload check means
//! something.
//!
//! ```text
//! cargo run --release --example train_pokec -p traverse-ml [-- <data dir>]
//! ```

use burn::module::Module;
use burn::optim::{AdamConfig, GradientsParams, Optimizer};
use burn::prelude::ElementConversion;
use burn::record::{BinBytesRecorder, FullPrecisionSettings, Recorder};
use burn::tensor::{
    Bool, Int, Tensor,
    activation::log_softmax,
    backend::{AutodiffBackend, Backend},
};
use std::path::PathBuf;
use traverse_ml::cancel::CancellationToken;
use traverse_ml::gnn::{Gcn, GcnConfig};
use traverse_ml::loader::ClassificationDataset;

mod pokec_data;
use pokec_data::load_pokec_small;

/// Where `traverse-bench --download small --format csv` puts the CSVs.
const DEFAULT_DATA_DIR: &str = "benchmarks/data/dataset_cache";
use traverse_ml::model::{Model, ModelCatalog, ModelMetadata};

// --- backend -----------------------------------------------------
#[cfg(all(feature = "cuda", not(feature = "wgpu")))]
type InnerBackend = burn::backend::Cuda;
#[cfg(all(feature = "wgpu", not(feature = "cuda")))]
type InnerBackend = burn::backend::Wgpu;
#[cfg(not(any(feature = "cuda", feature = "wgpu")))]
type InnerBackend = burn::backend::NdArray;

type B = burn::backend::Autodiff<InnerBackend>;
// inner non-autodiff backend for predict-time eval (no gradients); the inner-backend Gcn is rebuilt
// for predict
type BI = InnerBackend;

#[cfg(all(feature = "cuda", not(feature = "wgpu")))]
fn default_device() -> burn::backend::cuda::CudaDevice {
    burn::backend::cuda::CudaDevice::default()
}
#[cfg(all(feature = "wgpu", not(feature = "cuda")))]
fn default_device() -> burn::backend::wgpu::WgpuDevice {
    burn::backend::wgpu::WgpuDevice::default()
}
#[cfg(not(any(feature = "cuda", feature = "wgpu")))]
fn default_device() -> burn::backend::ndarray::NdArrayDevice {
    burn::backend::ndarray::NdArrayDevice::default()
}

fn backend_name() -> &'static str {
    if cfg!(feature = "cuda") {
        "CUDA"
    } else if cfg!(feature = "wgpu") {
        "WGPU"
    } else {
        "NdArray (CPU)"
    }
}

// --- loss + accuracy helpers -------------------------------------

fn masked_nll<BA: AutodiffBackend>(
    logits: Tensor<BA, 2>,
    mask: Tensor<BA, 1, Bool>,
    safe_labels: Tensor<BA, 1, Int>,
) -> Tensor<BA, 1> {
    let log_probs = log_softmax(logits, 1);
    let target_lp = log_probs
        .gather(1, safe_labels.unsqueeze_dim(1))
        .squeeze::<1>();
    let mask_f = mask.clone().float();
    let n_active = mask.int().sum().float();
    (target_lp.neg() * mask_f).sum() / n_active
}

fn accuracy_on_mask<BB: Backend>(
    logits: Tensor<BB, 2>,
    labels: Tensor<BB, 1, Int>,
    mask: Tensor<BB, 1, Bool>,
) -> f32 {
    let preds = logits.argmax(1).squeeze::<1>();
    let correct = preds
        .equal(labels)
        .int()
        .mul(mask.clone().int())
        .sum()
        .into_scalar()
        .elem::<i32>();
    let total = mask.int().sum().into_scalar().elem::<i32>();
    if total == 0 {
        0.0
    } else {
        correct as f32 / total as f32
    }
}

// --- data -> tensors ----------------------------------------------

struct TensorBundle<BB: Backend> {
    features: Tensor<BB, 2>,
    labels: Tensor<BB, 1, Int>,
    safe_labels: Tensor<BB, 1, Int>,
    train_mask: Tensor<BB, 1, Bool>,
    val_mask: Tensor<BB, 1, Bool>,
    test_mask: Tensor<BB, 1, Bool>,
}

fn dataset_to_tensors<BB: Backend>(
    ds: &ClassificationDataset,
    device: &BB::Device,
) -> TensorBundle<BB> {
    let n = ds.num_nodes();
    let features =
        Tensor::<BB, 1>::from_floats(ds.features.as_slice(), device).reshape([n, ds.feat_dim]);
    let labels = Tensor::<BB, 1, Int>::from_ints(ds.labels.as_slice(), device);
    let safe: Vec<i32> = ds
        .labels
        .iter()
        .map(|&l| if l < 0 { 0 } else { l })
        .collect();
    let safe_labels = Tensor::<BB, 1, Int>::from_ints(safe.as_slice(), device);
    let train_mask = bool_tensor(&ds.train_mask, device);
    let val_mask = bool_tensor(&ds.val_mask, device);
    let test_mask = bool_tensor(&ds.test_mask, device);
    TensorBundle {
        features,
        labels,
        safe_labels,
        train_mask,
        val_mask,
        test_mask,
    }
}

fn bool_tensor<BB: Backend>(mask: &[bool], device: &BB::Device) -> Tensor<BB, 1, Bool> {
    let ints: Vec<i32> = mask.iter().map(|&b| if b { 1 } else { 0 }).collect();
    Tensor::<BB, 1, Int>::from_ints(ints.as_slice(), device).bool()
}

// --- main --------------------------------------------------------

fn main() -> std::io::Result<()> {
    let data_dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_DATA_DIR), PathBuf::from);
    let nodes_csv = data_dir.join("pokec_small_nodes.csv");
    let edges_csv = data_dir.join("pokec_small_edges.csv");
    if !nodes_csv.exists() || !edges_csv.exists() {
        eprintln!(
            "pokec CSVs not found at\n  {}\n  {}\n\nDownload them from the repo root with\n  \
             cargo run --release -p traverse-bench -- --download small --format csv",
            nodes_csv.display(),
            edges_csv.display()
        );
        std::process::exit(1);
    }

    println!("--- Pokec-small GCN training ---------------------------");
    println!("Backend: {}", backend_name());
    println!("loading dataset ...");
    let ds = load_pokec_small(&nodes_csv, &edges_csv, 42)?;
    println!(
        "  nodes: {}   labelled: {}   train: {}   val: {}   test: {}",
        ds.num_nodes(),
        ds.labels.iter().filter(|&&l| l >= 0).count(),
        ds.train_mask.iter().filter(|&&b| b).count(),
        ds.val_mask.iter().filter(|&&b| b).count(),
        ds.test_mask.iter().filter(|&&b| b).count(),
    );

    let device = default_device();
    let tb = dataset_to_tensors::<B>(&ds, &device);

    let in_dim = ds.feat_dim;
    let hidden = 32;
    let epochs = 200;
    let lr = 1e-2;
    let mut model: Gcn<B> = GcnConfig::new(in_dim, hidden, ds.num_classes).init(&device);
    let mut optim = AdamConfig::new().init();

    // cooperative cancellation: the example owns the token and never cancels, so training runs to
    // completion; a server would clone it before training and `.cancel()` from its Ctrl-C /
    // deadline / HTTP-cancelled handler
    let cancel = CancellationToken::none();

    let mut completed_epochs = 0usize;
    println!("training {epochs} epochs ...");
    for epoch in 1..=epochs {
        if cancel.is_cancelled() {
            eprintln!("cancellation requested at start of epoch {epoch}; bailing");
            break;
        }
        let logits = model.forward(tb.features.clone(), &ds.graph);
        let loss = masked_nll(
            logits.clone(),
            tb.train_mask.clone(),
            tb.safe_labels.clone(),
        );
        let loss_val: f32 = loss.clone().into_scalar().elem();
        let grads = loss.backward();
        let grads = GradientsParams::from_grads(grads, &model);
        model = optim.step(lr, model, grads);
        completed_epochs = epoch;

        if epoch == 1 || epoch % 20 == 0 {
            let val_acc = accuracy_on_mask(
                logits.clone().detach(),
                tb.labels.clone(),
                tb.val_mask.clone(),
            );
            println!("epoch {epoch:>3}  loss = {loss_val:.4}  val_acc = {val_acc:.3}");
        }
    }
    if completed_epochs < epochs {
        eprintln!("trained for {completed_epochs} of {epochs} epochs before cancellation");
    }

    // -- final test pass (training-time number) --
    let logits_train_time = model.forward(tb.features.clone(), &ds.graph);
    let test_acc_train_time = accuracy_on_mask(
        logits_train_time.detach(),
        tb.labels.clone(),
        tb.test_mask.clone(),
    );
    println!("training-time test_acc = {test_acc_train_time:.4}");

    // -- persist to the catalog --
    let catalog_dir = PathBuf::from("pokec_catalog");
    let mut catalog = ModelCatalog::new(catalog_dir.clone());
    let recorder = BinBytesRecorder::<FullPrecisionSettings>::new();
    let weights = recorder
        .record(model.clone().into_record(), ())
        .expect("record");

    let mut meta = ModelMetadata::now();
    meta.feature_dim = in_dim;
    meta.num_classes = ds.num_classes;
    meta.train_graph_nodes = ds.num_nodes();
    meta.train_graph_edges = ds.graph.num_edges();
    meta.training_metrics
        .insert("test_acc".into(), test_acc_train_time);
    meta.extra.insert("dataset".into(), "pokec_small".into());
    meta.extra
        .insert("architecture".into(), format!("gcn-2layer-h{hidden}"));
    meta.extra
        .insert("epochs".into(), completed_epochs.to_string());
    if completed_epochs < epochs {
        meta.extra
            .insert("training_status".into(), "cancelled".into());
    }
    meta.set_schema_fingerprint();
    catalog.insert(
        "pokec_gcn",
        Model {
            kind: "gcn-2layer".into(),
            metadata: meta,
            weights,
        },
    );
    catalog.save_to_disk()?;
    println!(
        "saved model to {} (catalog has {} entries)",
        catalog_dir.join("pokec_gcn.tvmodel").display(),
        catalog.len()
    );

    // -- reload + predict == train check -- train uses the autodiff-wrapped backend, but the saved
    // record is precision-agnostic and loads into either; predict uses the plain inner backend (no
    // gradients needed)
    let reloaded_catalog = ModelCatalog::load_from_disk(catalog_dir)?;
    let reloaded = reloaded_catalog.get("pokec_gcn").expect("model just saved");
    let record = recorder
        .load(reloaded.weights.clone(), &device)
        .expect("recorder load");
    let predict_model: Gcn<BI> = GcnConfig::new(in_dim, hidden, ds.num_classes)
        .init(&device)
        .load_record(record);

    let tb_predict = dataset_to_tensors::<BI>(&ds, &device);
    let logits_reload = predict_model.forward(tb_predict.features, &ds.graph);
    let test_acc_reload = accuracy_on_mask(logits_reload, tb_predict.labels, tb_predict.test_mask);

    println!("reload-time test_acc   = {test_acc_reload:.4}");
    let drift = (test_acc_train_time - test_acc_reload).abs();
    if drift < 1e-4 {
        println!("--- train -> save -> load -> predict cycle OK ----------");
    } else {
        eprintln!(
            "--- DRIFT: train_acc={test_acc_train_time:.6}, reload_acc={test_acc_reload:.6}, diff={drift:.6} ---"
        );
        std::process::exit(1);
    }
    Ok(())
}
