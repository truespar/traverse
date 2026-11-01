# traverse-ml-wasm

WebAssembly bindings for [`traverse-ml`](../traverse-ml) - graph-ML
**inference** in a browser tab.

## Why a separate crate from `traverse-wasm`?

`traverse-wasm` ships the Cypher engine + 32 GDS algorithms in a
**~4 MB** WASM binary (release, post-`wasm-opt`). Adding ML pushes
the artifact to **~10 MB (CPU)** or **~30 MB (WGPU)** because Burn +
CubeCL drag in tensor primitives, autograd machinery, and kernel
codegen.

Users who only run Cypher queries shouldn't pay that download cost.
A page loads `traverse-wasm` eagerly and `traverse-ml-wasm` only when
it needs inference (Studio doesn't use it yet, see below).

This mirrors the native split - `traverse-server` / `traverse-cli`
consume `traverse-ml` as a separate crate, not as part of the core.

## What it does

**Inference only.** The trainer needs filesystem I/O and runs
multi-hour epochs; both are awkward in a browser tab. The intended
flow is "train on the server, predict in the browser":

1. Train server-side with the native `traverse-ml` crate.
2. Save via `ModelCatalog::save_to_disk()` -> produces `.tvmodel`
   bytes (a framed, CRC-32-checked container; files from 0.8.x load too).
3. Ship those bytes to the browser via HTTP fetch, OPFS, or
   wasm-bindgen pass-through.
4. Construct `new ModelHandle(bytes)` in the browser.
5. Call `predict_node_classification_js(model, input_json)`.

Node classification, node regression and link prediction models load,
whatever their architecture (GCN, GraphSAGE, GAT; R-GCN when trained on a
single relationship type, since the input carries no relationship types).

## OPFS persistence

`js/index.js` ships helpers parallel to `traverse-wasm`'s `.tvdb`
helpers, but for `.tvmodel` files under `/traverse-ml/` in OPFS:

```js
import {
  loadTraverseMl,
  opfsWriteModel, opfsReadModel,
  opfsListModels, opfsDeleteModel,
} from '@truespar/traverse-ml-wasm'

const { ModelHandle, predict_node_classification_js } = await loadTraverseMl()

// Persist a model to OPFS (e.g. just downloaded from the server):
await opfsWriteModel('pokec_gcn', modelBytes)

// Later - read back without a fresh HTTP fetch:
const bytes = await opfsReadModel('pokec_gcn')
const model = new ModelHandle(bytes)
console.log(model.metadata())
//  { kind: 'gcn-2layer', feature_dim: 2, num_classes: 2, ... }
```

## Build

```sh
# CPU (NdArray) - smaller artifact, runs anywhere.
wasm-pack build crates/traverse-ml-wasm --target web --release \
    --no-default-features --features cpu

# WGPU - larger artifact, runs on GPU via WebGPU. Requires:
#   - Chrome 113+ / Firefox Nightly with WebGPU enabled
#   - Hosting page with COOP/COEP headers (same as traverse-wasm)
wasm-pack build crates/traverse-ml-wasm --target web --release \
    --no-default-features --features wgpu
```

## Status

* Compiles for `wasm32-unknown-unknown` (CPU and WGPU); on other targets the crate is empty.
* `tests/wasm_predict.rs` runs node classification, regression and link prediction on a trained
  `.tvmodel` fixture in a WASM runtime: `./crates/traverse-ml-wasm/run_wasm_smoke.sh`.
* Not published to npm yet, and Studio doesn't use it yet (its ML panel predicts on the server).

See `crates/traverse-ml/README.md` for the ML stack itself.
