//! Traverse graph ML. See `README.md` for what it does.
//!
//! ```text
//! device/    runtime CPU / GPU choice, GPU probe
//! graph/     CsrGraph, edge-list construction, normalization
//! sparse/    SpMM (dense baseline, gather/scatter-add)
//! sampling/  neighbor sampling for mini-batches
//! gnn/       GCN, GraphSAGE, R-GCN, GAT (single- and multi-head)
//! encoder/   one module enum over the GNNs + their graph inputs
//! ssl/       GraphMAE self-supervised pretraining
//! pipeline/  NC, NR, LP pipelines + catalog, auto-tuning, cross-validation
//! selection/ GDS-style training: test holdout, k-fold CV, random search / successive halving
//! explain/   PGExplainer, counterfactual, SubgraphX audit mode
//! model/     Model + catalog + .tvmodel persistence
//! loader/    dataset loaders
//! cancel/    cooperative cancellation
//! ```
//!
//! ## Backends
//!
//! The CPU backend is always built; the `cuda` feature adds NVIDIA GPUs, picked at
//! runtime through [`device`]. The `wgpu` feature is for the browser build
//! (`traverse-ml-wasm`).

pub mod cancel;
pub mod device;
pub mod encoder;
pub mod explain;
pub mod gnn;
pub mod graph;
pub mod loader;
pub mod model;
pub mod pipeline;
pub mod sampling;
pub mod selection;
pub mod sparse;
pub mod ssl;
