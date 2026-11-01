# traverse-ml

Traverse's graph machine learning: graph neural networks trained and run inside the engine, on
the database's own graph. The `traverse.ml.*` procedures in `traverse-cypher` call it (in
servers built with the `ml` feature), and `traverse-ml-wasm` runs its models in the browser.

It's built on [Burn](https://burn.dev), a Rust tensor and autograd framework, so training needs
no Python and no separate service.

## What it does

- **Pipelines** (`traverse.ml.pipeline.nodeClassification.*`, `.nodeRegression.*`,
  `.linkPrediction.*`): configure features and model candidates, `train`, then `predict.stream`
  or `predict.write`. Link prediction scores node pairs that aren't connected yet.
- **Models**: GCN, GraphSAGE, GAT (single- and multi-head) and R-GCN (one weight matrix per
  relationship type).
- **Model selection**: a test holdout, (stratified) k-fold cross-validation, random search over
  hyperparameter ranges and successive halving, scored by a chosen metric.
- **Graphs of any size**: full-graph training when it fits, else mini-batches of sampled
  neighborhoods, weighted so training matches whole-graph prediction. Prediction is exact and
  runs a block of nodes at a time.
- **Node property steps**: any node-property algorithm (PageRank, FastRP, Louvain, ...) or
  GraphMAE can feed features, computed on the training graph without writing to the database.
- **GraphMAE** (`traverse.ml.graphMae.*`): self-supervised node embeddings.
- **Explanations** (`traverse.ml.explain.*`): PGExplainer, counterfactuals and a reproducible
  subgraph audit, for the relationships behind a prediction.
- **Jobs**: training runs in the background, one job at a time by default
  (`traverse.ml.trainJobs`, `traverse.ml.trainCancel`). GraphMAE, predictions and explanations
  stop on the query's time limit or memory budget.

## Backends

| Feature | Backend | Where |
|---|---|---|
| `cpu` (default) | Burn NdArray | everywhere; also the fallback when a GPU can't be used |
| `cuda` | Burn CUDA | NVIDIA GPUs, chosen at run time (`traverse.ml.devices()` lists them); the driver and NVRTC are loaded dynamically, so one binary runs with or without a GPU |
| `wgpu` | Burn WGPU | the browser build (`traverse-ml-wasm`) |

Training runs natively only. Models train on the server and predict anywhere, the browser
included (CPU or WebGPU through `traverse-ml-wasm`).

## Models on disk

A trained model is a `.tvmodel` file next to its database: a framed, CRC-32-checked container
(type tag and format version) around the model's weights and metadata. Prediction refuses a graph
whose shape doesn't match what the model was trained on (a schema fingerprint over node and edge
counts, feature width and classes).

## Modules

```text
device/    runtime CPU / GPU choice, GPU probe
graph/     CsrGraph, edge-list construction, normalization
sparse/    SpMM (dense baseline, gather/scatter-add)
sampling/  neighbor sampling for mini-batches
gnn/       GCN, GraphSAGE, R-GCN, GAT
encoder/   one module enum over the GNNs and their graph inputs
ssl/       GraphMAE self-supervised pretraining
pipeline/  node classification, regression and link prediction pipelines, catalog
selection/ training with test holdout, k-fold CV, random search / successive halving
explain/   PGExplainer, counterfactual, SubgraphX audit
model/     Model, catalog, .tvmodel persistence
loader/    dataset loaders
cancel/    cooperative cancellation
```

## Examples and tests

```bash
cargo test -p traverse-ml

# a GCN on a synthetic graph
cargo run --release --example gcn_smoke -p traverse-ml

# Pokec (10k nodes): train, save a .tvmodel, reload it in a fresh process
cargo run --release -p traverse-bench -- --download small --format csv
cargo run --release --example train_pokec -p traverse-ml
cargo run --release --example predict_pokec -p traverse-ml

# the inference path in a WASM runtime (needs wasm-pack and Node.js)
./crates/traverse-ml/run_wasm_smoke.sh
```

## License

Licensed under MIT or Apache-2.0, at your option.
