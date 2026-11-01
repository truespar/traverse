//! WGPU-backed WASM smoke test. Unlike `wasm_smoke.rs` (CPU / NdArray under
//! Node.js: fast, no GPU), this is the real-browser gate: Burn's WGPU backend
//! runs a small tensor op + GCN forward and the result must be sane.
//!
//! Requirements (the script enforces them):
//! * a browser with WebGPU (Chrome 113+, or Firefox Nightly with
//!   `dom.webgpu.enabled`) and a visible GPU adapter;
//! * `cargo build --no-default-features --features wgpu`;
//! * `wasm-pack test --chrome --headless` (or `run_wasm_smoke_wgpu.sh`).
//!
//! Gated on `feature = "wgpu"` and `target_arch = "wasm32"`: the NdArray Node.js
//! run mustn't pull in Burn's WGPU types, and native runs would try to init a
//! WebGPU that isn't there.

#![cfg(all(target_arch = "wasm32", feature = "wgpu"))]

use burn::tensor::Tensor;
use traverse_ml::gnn::{Gcn, GcnConfig};
use traverse_ml::graph::CsrGraph;
use wasm_bindgen_test::*;

// WGPU init is async (it talks to the browser's WebGPU API), so every test is async and
// browser-only (Node.js has no WebGPU)
wasm_bindgen_test_configure!(run_in_browser);

type B = burn::backend::Wgpu;

/// Async WGPU init. CubeCL's `init()` must run before any tensor op can `load()` the device; native
/// does it lazily with a blocking init, but on WASM `navigator.gpu.requestAdapter()` is inherently
/// async. Burn 0.21's `init_setup_async` covers this.
async fn init_wgpu() -> burn::backend::wgpu::WgpuDevice {
    let device = burn::backend::wgpu::WgpuDevice::default();
    let _setup = burn::backend::wgpu::init_setup_async::<burn::backend::wgpu::graphics::WebGpu>(
        &device,
        Default::default(),
    )
    .await;
    device
}

/// One consolidated test: CubeCL's compute-server registry is process-global and a second
/// `init_setup_async` on the same `WgpuDevice` panics ("server already registered"), so separate
/// `#[wasm_bindgen_test]`s sharing it can't work.
#[wasm_bindgen_test]
async fn wgpu_backend_runs_in_browser() {
    let device = init_wgpu().await;

    // stage 1: tensor round trip; reads must be async on WASM (GPU -> CPU is async, a sync read
    // panics)
    let probe = Tensor::<B, 1>::from_floats([1.0f32, 2.0, 3.0, 4.0].as_slice(), &device);
    let sum_data = probe.sum().into_data_async().await.expect("read sum");
    let sum: f32 = sum_data.iter::<f32>().next().expect("scalar present");
    assert!(
        (sum - 10.0).abs() < 1e-4,
        "WGPU tensor sum was {sum}, expected 10.0"
    );

    // stage 2: 2-layer GCN forward through CubeCL's WebGPU pipeline: SpMM + Linear + relu compile
    // to WGSL and run on the GPU
    let adj = CsrGraph::from_edges(
        5,
        &[
            (0u32, 1u32, 1.0f32),
            (1, 2, 1.0),
            (2, 3, 1.0),
            (3, 4, 1.0),
            (4, 0, 1.0),
        ],
    )
    .add_self_loops()
    .symmetric_normalize();

    let model: Gcn<B> = GcnConfig::new(3, 8, 2).init(&device);
    let x_data: Vec<f32> = (0..15).map(|i| (i as f32) * 0.1).collect();
    let x: Tensor<B, 2> = Tensor::<B, 1>::from_floats(x_data.as_slice(), &device).reshape([5, 3]);
    let y = model.forward(x, &adj);

    assert_eq!(y.dims(), [5, 2]);
    let vals: Vec<f32> = y
        .into_data_async()
        .await
        .expect("read forward output")
        .iter::<f32>()
        .collect();
    assert_eq!(vals.len(), 10);
    for v in vals {
        assert!(
            v.is_finite(),
            "WGPU GCN forward produced non-finite output: {v}"
        );
    }
}
