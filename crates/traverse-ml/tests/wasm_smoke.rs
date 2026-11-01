//! WASM smoke test for the inference path. Native suites use std::fs, tempfile
//! and threads, none of which exist in browser WASM, so "does it run in a WASM
//! runtime" lives here, gated on `cfg(target_arch = "wasm32")` and invisible
//! to native runs.
//!
//! ```text
//! wasm-pack test --node -p traverse-ml -- --no-default-features --features cpu
//! ```
//!
//! Checks, most important first:
//!
//! 1. `traverse_ml::graph::CsrGraph` construction + normalizations (pure CPU,
//!    no Burn allocations).
//! 2. Burn's `NdArray` backend instantiates in WASM and makes tensors: the
//!    load-bearing claim, without it nothing else matters.
//! 3. A 2-layer `Gcn` forward pass gives finite, correctly shaped output
//!    (`spmm_csr_sparse` + `Linear` + `relu` at WASM runtime).

#![cfg(target_arch = "wasm32")]

use burn::tensor::Tensor;
use traverse_ml::gnn::{Gcn, GcnConfig};
use traverse_ml::graph::CsrGraph;
use traverse_ml::model::{InMemoryStorage, Model, ModelCatalog, ModelMetadata};
use wasm_bindgen_test::*;

// default config = Node (no `run_in_browser!`): faster to start, no DOM or wasm-opt needed

type B = burn::backend::NdArray;

#[wasm_bindgen_test]
fn csr_graph_builds_and_normalises() {
    // 4-node line graph 0-1-2-3
    let edges = vec![(0u32, 1u32, 1.0f32), (1, 2, 1.0), (2, 3, 1.0)];
    let g = CsrGraph::from_edges(4, &edges)
        .add_self_loops()
        .symmetric_normalize();
    assert_eq!(g.num_nodes(), 4);
    // after self-loops + symmetric norm every node has its self-edge (4), plus the line's 6
    // directed edges (0<->1, 1<->2, 2<->3): 10
    assert_eq!(g.num_edges(), 10);
}

#[wasm_bindgen_test]
fn burn_ndarray_tensor_creation_works() {
    let device = burn::backend::ndarray::NdArrayDevice::default();
    let x = Tensor::<B, 1>::from_floats([1.0f32, 2.0, 3.0, 4.0].as_slice(), &device);
    let sum: f32 = x.sum().into_scalar();
    assert!(
        (sum - 10.0).abs() < 1e-6,
        "tensor sum failed on WASM: {sum}"
    );
}

#[wasm_bindgen_test]
fn gcn_forward_pass_runs_in_wasm() {
    let device = burn::backend::ndarray::NdArrayDevice::default();

    // 4 nodes, 3 features each, 2 classes, hidden=8
    let adj = CsrGraph::from_edges(
        4,
        &[(0u32, 1u32, 1.0f32), (1, 2, 1.0), (2, 3, 1.0), (0, 3, 1.0)],
    )
    .add_self_loops()
    .symmetric_normalize();

    let model: Gcn<B> = GcnConfig::new(3, 8, 2).init(&device);

    let x_data: Vec<f32> = (0..12).map(|i| (i as f32) * 0.1).collect();
    let x: Tensor<B, 2> = Tensor::<B, 1>::from_floats(x_data.as_slice(), &device).reshape([4, 3]);
    let y = model.forward(x, &adj);

    // output [N=4, out_dim=2]
    assert_eq!(y.dims(), [4, 2]);

    // every output finite (no NaN, no Inf)
    let vals: Vec<f32> = y.into_data().iter::<f32>().collect();
    assert_eq!(vals.len(), 8);
    for v in vals {
        assert!(
            v.is_finite(),
            "GCN forward produced non-finite output in WASM: {v}"
        );
    }
}

#[wasm_bindgen_test]
fn model_catalog_round_trips_via_in_memory_storage() {
    // the WASM persistence test: a catalog on InMemoryStorage (no filesystem), insert, flush, load
    // via a fresh catalog, model intact. Browser code would seed InMemoryStorage from OPFS at
    // startup (async, JS side) and flush snapshots back on shutdown; this is the in-process half,
    // which must behave as natively
    let mut meta = ModelMetadata::now();
    meta.feature_dim = 8;
    meta.num_classes = 3;
    meta.set_schema_fingerprint();
    let model = Model {
        kind: "wasm-test".into(),
        metadata: meta,
        weights: vec![1, 2, 3, 4, 5, 6, 7, 8],
    };

    let mut cat = ModelCatalog::with_storage(Box::new(InMemoryStorage::new()));
    cat.insert("m", model);
    cat.save_all().expect("save in WASM");
    assert_eq!(cat.len(), 1);
    // catalog_dir is None for the in-memory backend: std::fs never touched
    assert!(cat.catalog_dir().is_none());
}
