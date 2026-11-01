//! Minimal end-to-end check: a 2-layer GCN trained on a 1000-node stochastic
//! block model, confirming that (a) Burn autograd works on graph-shaped
//! tensors, (b) the GCN trains (loss falls monotonically), (c) the selected
//! backend (CPU / CUDA / WGPU) dispatches correctly.
//!
//! ```text
//! cargo run --release --example gcn_smoke -p traverse-ml                    # CPU
//! cargo run --release --example gcn_smoke -p traverse-ml --features cuda    # CUDA
//! cargo run --release --example gcn_smoke -p traverse-ml --features wgpu    # WGPU
//! ```

use burn::module::Module;
use burn::nn::{Linear, LinearConfig};
use burn::optim::{AdamConfig, GradientsParams, Optimizer};
use burn::prelude::ElementConversion;
use burn::tensor::{
    Distribution, Int, Tensor,
    activation::{log_softmax, relu},
    backend::{AutodiffBackend, Backend},
};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use traverse_ml::graph::CsrGraph;
use traverse_ml::sparse::spmm_csr_sparse;

// --- backend selection -------------------------------------------
// Burn separates the *inference* backend (e.g. NdArray) from the *autodiff* wrapper tracking
// gradients; Autodiff<B> is what Module / Optimizer get.

#[cfg(all(feature = "cuda", not(feature = "wgpu")))]
type InnerBackend = burn::backend::Cuda;

#[cfg(all(feature = "wgpu", not(feature = "cuda")))]
type InnerBackend = burn::backend::Wgpu;

#[cfg(not(any(feature = "cuda", feature = "wgpu")))]
type InnerBackend = burn::backend::NdArray;

type B = burn::backend::Autodiff<InnerBackend>;

// --- 2-layer GCN -------------------------------------------------

#[derive(Module, Debug)]
struct Gcn<B: Backend> {
    layer1: Linear<B>,
    layer2: Linear<B>,
}

impl<B: Backend> Gcn<B> {
    fn new(in_dim: usize, hidden: usize, out_dim: usize, device: &B::Device) -> Self {
        Self {
            layer1: LinearConfig::new(in_dim, hidden).init(device),
            layer2: LinearConfig::new(hidden, out_dim).init(device),
        }
    }

    /// Forward: out = A_norm @ relu(A_norm @ X @ W1) @ W2, via `spmm_csr_sparse` (gather +
    /// scatter-add): gradients flow through Burn's autograd on gather/scatter, no custom ops, no
    /// O(N^2) dense matrix.
    fn forward(&self, x: Tensor<B, 2>, adj: &CsrGraph) -> Tensor<B, 2> {
        let h = self.layer1.forward(spmm_csr_sparse(adj, x));
        let h = relu(h);
        self.layer2.forward(spmm_csr_sparse(adj, h))
    }
}

// --- synthetic stochastic block model ----------------------------
// two communities of n/2; intra-community edges with probability p_intra, inter with p_inter,
// emitted as `(src, dst, 1.0)` for `CsrGraph::from_edges`

fn synthesize_sbm(n: usize, p_intra: f32, p_inter: f32, seed: u64) -> (CsrGraph, Vec<i32>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let half = n / 2;
    let labels: Vec<i32> = (0..n).map(|i| if i < half { 0 } else { 1 }).collect();
    let mut edges: Vec<(u32, u32, f32)> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let p = if labels[i] == labels[j] {
                p_intra
            } else {
                p_inter
            };
            if rng.random::<f32>() < p {
                edges.push((i as u32, j as u32, 1.0));
            }
        }
    }
    println!("Graph: {} nodes, {} undirected edges", n, edges.len());
    let adj = CsrGraph::from_edges(n, &edges)
        .add_self_loops()
        .symmetric_normalize();
    (adj, labels)
}

// --- training loop -----------------------------------------------

fn train_step<BA: AutodiffBackend>(
    model: Gcn<BA>,
    optim: &mut impl Optimizer<Gcn<BA>, BA>,
    x: Tensor<BA, 2>,
    adj: &CsrGraph,
    labels: Tensor<BA, 1, Int>,
    lr: f64,
) -> (Gcn<BA>, f32) {
    let logits = model.forward(x, adj);
    let log_probs = log_softmax(logits, 1);
    // NLL: -mean over rows of log_probs[i, labels[i]]
    let nll = log_probs.gather(1, labels.unsqueeze_dim(1)).neg().mean();
    let loss_value: f32 = nll.clone().into_scalar().elem();
    let grads = nll.backward();
    let grads = GradientsParams::from_grads(grads, &model);
    let model = optim.step(lr, model, grads);
    (model, loss_value)
}

// per-backend device: Burn 0.21 moved `Device` from `Backend` to a parent trait (`BackendTypes`),
// so cfg-gating the concrete device type per feature is the cleanest portable path
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

fn main() {
    let device = default_device();
    let n: usize = 1000;
    let in_dim: usize = 16;
    let hidden: usize = 32;
    let classes: usize = 2;
    let epochs: usize = 100;
    let lr: f64 = 1e-2;

    println!("--- Burn GCN smoke test: 1000-node SBM -----------------");
    println!(
        "Backend: {}",
        if cfg!(feature = "cuda") {
            "CUDA"
        } else if cfg!(feature = "wgpu") {
            "WGPU"
        } else {
            "NdArray (CPU)"
        }
    );

    // -- data --
    let (adj, labels_vec) = synthesize_sbm(n, 0.05, 0.005, 42);

    let x: Tensor<B, 2> = Tensor::random([n, in_dim], Distribution::Normal(0.0, 1.0), &device);
    let labels: Tensor<B, 1, Int> = Tensor::from_ints(labels_vec.as_slice(), &device);

    // -- model + optimizer --
    let mut model = Gcn::<B>::new(in_dim, hidden, classes, &device);
    let mut optim = AdamConfig::new().init();

    // -- training --
    let mut losses = Vec::with_capacity(epochs);
    for epoch in 1..=epochs {
        let (m, loss) = train_step(model, &mut optim, x.clone(), &adj, labels.clone(), lr);
        model = m;
        losses.push(loss);
        if epoch == 1 || epoch % 10 == 0 {
            println!("epoch {epoch:>3}  loss = {loss:.4}");
        }
    }

    // -- final accuracy --
    let logits = model.forward(x, &adj);
    let preds = logits.argmax(1).squeeze::<1>();
    let correct: i32 = preds.equal(labels).int().sum().into_scalar().elem::<i32>();
    let acc = correct as f32 / n as f32;
    println!("final accuracy: {:.1}%", acc * 100.0);

    // -- validation gates --
    let first_loss = losses[0];
    let last_loss = *losses.last().unwrap();
    let monotonic = last_loss < first_loss * 0.5;
    println!(
        "loss reduction: {first_loss:.4} -> {last_loss:.4}  ({}x decrease)",
        first_loss / last_loss
    );

    if monotonic && acc > 0.85 {
        println!("--- PASSED ---------------------------------------------");
    } else {
        eprintln!("--- FAILED: loss did not converge or accuracy too low ---");
        std::process::exit(1);
    }
}
