//! Companion to `train_pokec`: loads the saved model from the catalog, runs
//! inference on the same dataset and checks the test accuracy matches the
//! training-time number. The full lifecycle:
//!
//! ```text
//! train_pokec  --->  pokec_catalog/pokec_gcn.tvmodel  --->  predict_pokec
//! ```
//!
//! Both use the raw demographic features (`completion%`, `age`) from CSV,
//! deterministic across processes, so a match proves the catalog round-trips a
//! trained GCN without drift.
//!
//! ```text
//! cargo run --release --example train_pokec   -p traverse-ml [-- <data dir>]
//! cargo run --release --example predict_pokec -p traverse-ml [-- <data dir>]
//! ```
//!
//! The data comes from `traverse-bench`; see `train_pokec`.

use burn::module::Module;
use burn::prelude::ElementConversion;
use burn::record::{BinBytesRecorder, FullPrecisionSettings, Recorder};
use burn::tensor::{Bool, Int, Tensor};
use std::path::PathBuf;
use traverse_ml::gnn::{Gcn, GcnConfig};
use traverse_ml::loader::ClassificationDataset;

mod pokec_data;
use pokec_data::load_pokec_small;
use traverse_ml::model::ModelCatalog;

/// Where `traverse-bench --download small --format csv` puts the CSVs.
const DEFAULT_DATA_DIR: &str = "benchmarks/data/dataset_cache";

// --- backend (non-autodiff, predict only needs forward) ----------
#[cfg(all(feature = "cuda", not(feature = "wgpu")))]
type B = burn::backend::Cuda;
#[cfg(all(feature = "wgpu", not(feature = "cuda")))]
type B = burn::backend::Wgpu;
#[cfg(not(any(feature = "cuda", feature = "wgpu")))]
type B = burn::backend::NdArray;

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

fn accuracy_on_mask(
    logits: Tensor<B, 2>,
    labels: Tensor<B, 1, Int>,
    mask: Tensor<B, 1, Bool>,
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

// concrete device type per backend feature (Burn 0.21's `Backend::Device` lives on a parent trait,
// as in gcn_smoke.rs)
#[cfg(all(feature = "cuda", not(feature = "wgpu")))]
type Dev = burn::backend::cuda::CudaDevice;
#[cfg(all(feature = "wgpu", not(feature = "cuda")))]
type Dev = burn::backend::wgpu::WgpuDevice;
#[cfg(not(any(feature = "cuda", feature = "wgpu")))]
type Dev = burn::backend::ndarray::NdArrayDevice;

fn bool_tensor(mask: &[bool], device: &Dev) -> Tensor<B, 1, Bool> {
    let ints: Vec<i32> = mask.iter().map(|&b| if b { 1 } else { 0 }).collect();
    Tensor::<B, 1, Int>::from_ints(ints.as_slice(), device).bool()
}

fn dataset_to_tensors(
    ds: &ClassificationDataset,
    device: &Dev,
) -> (Tensor<B, 2>, Tensor<B, 1, Int>, Tensor<B, 1, Bool>) {
    let n = ds.num_nodes();
    let features =
        Tensor::<B, 1>::from_floats(ds.features.as_slice(), device).reshape([n, ds.feat_dim]);
    let labels = Tensor::<B, 1, Int>::from_ints(ds.labels.as_slice(), device);
    let test_mask = bool_tensor(&ds.test_mask, device);
    (features, labels, test_mask)
}

fn main() -> std::io::Result<()> {
    let data_dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_DATA_DIR), PathBuf::from);
    let nodes_csv = data_dir.join("pokec_small_nodes.csv");
    let edges_csv = data_dir.join("pokec_small_edges.csv");
    let catalog_dir = PathBuf::from("pokec_catalog");

    if !catalog_dir.exists() {
        eprintln!(
            "catalog dir {} not found - run `cargo run --release --example train_pokec -p traverse-ml` first",
            catalog_dir.display()
        );
        std::process::exit(1);
    }

    println!("--- Pokec-small predict from catalog ------------------");
    println!("Backend: {}", backend_name());

    // -- load catalog --
    let catalog = ModelCatalog::load_from_disk(catalog_dir.clone())?;
    println!(
        "loaded catalog from {}: {} model(s) registered",
        catalog_dir.display(),
        catalog.len()
    );
    for name in catalog.list() {
        let m = catalog.get(name).unwrap();
        println!(
            "  - {name:<16} kind={:<14} feat_dim={} num_classes={}",
            m.kind, m.metadata.feature_dim, m.metadata.num_classes
        );
        if let Some(acc) = m.metadata.training_metrics.get("test_acc") {
            println!("    training_metrics.test_acc = {acc:.4}");
        }
        if let Some(arch) = m.metadata.extra.get("architecture") {
            println!("    architecture              = {arch}");
        }
    }

    let stored = catalog
        .get("pokec_gcn")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no `pokec_gcn` entry in catalog",
            )
        })?
        .clone();

    // -- load dataset (same CSVs + seed -> same masks/features) --
    let ds = load_pokec_small(&nodes_csv, &edges_csv, 42)?;

    // schema fingerprint guard: no predicting on a dataset whose shape differs from training
    if !stored.check_schema(ds.num_nodes(), ds.graph.num_edges(), ds.feat_dim) {
        eprintln!(
            "schema fingerprint mismatch - saved model was trained on a graph with \
             different shape than the loaded dataset. Refusing to predict."
        );
        std::process::exit(1);
    }

    let device = default_device();
    let (features, labels, test_mask) = dataset_to_tensors(&ds, &device);

    // -- rebuild the Gcn from metadata, load weights --
    let hidden: usize = stored
        .metadata
        .extra
        .get("architecture")
        .and_then(|s| s.strip_prefix("gcn-2layer-h"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);

    let recorder = BinBytesRecorder::<FullPrecisionSettings>::new();
    let record = recorder
        .load(stored.weights, &device)
        .expect("recorder load");
    let model: Gcn<B> = GcnConfig::new(
        stored.metadata.feature_dim,
        hidden,
        stored.metadata.num_classes,
    )
    .init(&device)
    .load_record(record);

    // -- forward on the same dataset, masked accuracy --
    let logits = model.forward(features, &ds.graph);
    let test_acc = accuracy_on_mask(logits, labels, test_mask);

    println!("predict test_acc = {test_acc:.4}");

    if let Some(training_acc) = stored.metadata.training_metrics.get("test_acc") {
        let drift = (training_acc - test_acc).abs();
        println!("training_acc     = {training_acc:.4}   (drift {drift:.6})");
        if drift < 1e-4 {
            println!("--- train -> save -> load -> predict cycle OK ----------");
        } else {
            eprintln!("--- DRIFT exceeded 1e-4 - investigate ---");
            std::process::exit(1);
        }
    }
    Ok(())
}
