// High-level JS wrapper around the traverse-ml-wasm bindings.
//
// Mirrors traverse-wasm/js/index.js for the ML side:
//   * Hide the wasm-pack init dance behind a single `loadTraverseMl()`.
//   * Wrap the ModelHandle class so callers see plain JS shapes.
//   * Provide OPFS helpers for `.tvmodel` files under `/traverse-ml/`,
//     parallel to traverse-wasm's `/traverse/` directory for `.tvdb`.
//
// Usage:
//
//   import { loadTraverseMl, opfsReadModel, opfsListModels } from
//     '@truespar/traverse-ml-wasm'
//
//   const { ModelHandle, predict_node_classification_js } =
//     await loadTraverseMl()
//
//   // Option A - fetch model from server:
//   const bytes = new Uint8Array(
//     await (await fetch('/api/models/pokec_gcn.tvmodel')).arrayBuffer()
//   )
//
//   // Option B - read from OPFS (previously saved by the user):
//   const bytes = await opfsReadModel('pokec_gcn')
//
//   const model = new ModelHandle(bytes)
//   console.log(model.metadata())
//
//   const input = JSON.stringify({
//     edges: [[0, 1], [1, 2], [2, 3]],
//     features: [0.5, 0.2,  0.3, 0.1,  0.7, 0.4,  0.6, 0.8],
//     feat_dim: 2,
//     num_nodes: 4,
//   })
//   const preds = predict_node_classification_js(model, input)
//   //  -> [0, 0, 1, 1]   (one class per node)

import init, * as wasm from './pkg/traverse_ml_wasm.js'

/** Single-shot wasm-pack initialiser. Subsequent calls are no-ops. */
let _initPromise = null
export function loadTraverseMl() {
  if (_initPromise == null) {
    _initPromise = init().then(() => wasm)
  }
  return _initPromise
}

// -- OPFS helpers ----------------------------------------------------
//
// Layout: `/traverse-ml/{name}.tvmodel` in the OPFS root, next to
// traverse-wasm's `/traverse/{name}.tvdb`.
//
// Usable from the main thread *or* a Web Worker. The main thread uses
// `createWritable()` (slower, available everywhere); in a Worker callers can
// swap in `createSyncAccessHandle()` for throughput, as traverse-wasm's
// worker.js does for .tvdb.

/** Get-or-create the traverse-ml subdirectory in OPFS root. */
async function opfsModelDir() {
  const root = await navigator.storage.getDirectory()
  return root.getDirectoryHandle('traverse-ml', { create: true })
}

/** Write model bytes to OPFS under `name` (extension is implicit). */
export async function opfsWriteModel(name, bytes) {
  const dir = await opfsModelDir()
  const file = await dir.getFileHandle(`${name}.tvmodel`, { create: true })
  const writable = await file.createWritable()
  try {
    await writable.truncate(0)
    await writable.write(bytes)
  } finally {
    await writable.close()
  }
}

/** Read model bytes from OPFS. Returns null if absent. */
export async function opfsReadModel(name) {
  const dir = await opfsModelDir()
  let file
  try {
    file = await dir.getFileHandle(`${name}.tvmodel`)
  } catch {
    return null
  }
  const blob = await file.getFile()
  return new Uint8Array(await blob.arrayBuffer())
}

/** List models currently in OPFS. Returns `[{name, size, lastModified}]`. */
export async function opfsListModels() {
  const dir = await opfsModelDir()
  const items = []
  for await (const [filename, entry] of dir.entries()) {
    if (entry.kind !== 'file') continue
    if (!filename.endsWith('.tvmodel')) continue
    const f = await entry.getFile()
    items.push({
      name: filename.slice(0, -'.tvmodel'.length),
      size: f.size,
      lastModified: f.lastModified,
    })
  }
  items.sort((a, b) => b.lastModified - a.lastModified)
  return items
}

/** Delete a model from OPFS. No-op if it doesn't exist. */
export async function opfsDeleteModel(name) {
  const dir = await opfsModelDir()
  try {
    await dir.removeEntry(`${name}.tvmodel`)
  } catch {
    /* not present - ignore */
  }
}
