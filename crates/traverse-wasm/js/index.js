// @truespar/traverse-wasm: public entry point.
//
// Runs the Traverse WASM engine in a dedicated Web Worker and exposes a
// Promise-based API to the main thread, shaped like Studio's HttpApiClient so
// a WASM client adapter drops in.
//
// Usage:
//   import { TraverseDb } from '@truespar/traverse-wasm'
//   const db = await TraverseDb.open({ numThreads: navigator.hardwareConcurrency })
//   const r = await db.query('CALL traverse.pageRank.stream({}) YIELD nodeId, score')
//   console.log(r.rows)
//
// Cancellation:
//   const ctl = new AbortController()
//   setTimeout(() => ctl.abort('user cancelled'), 5000)
//   await db.query(cypher, params, { signal: ctl.signal })
// Queued AbortSignals prevent dispatch. An active synchronous query can't see
// an abort message until it returns: its caller rejects, but the slot stays
// occupied until the execution deadline/result. Active AbortErrors carry
// executionMayHaveCompleted=true, never a rollback promise.

import { RequestQueue } from './request-queue.js'

/**
 * In-tab Traverse engine in a dedicated Web Worker, so the main thread never blocks on Cypher.
 * Construct with `TraverseDb.open()` (the constructor does no async work) and await it.
 */
export class TraverseDb {
  /** @type {Worker} */
  #worker
  #queue
  /** @type {string | null} Library version reported by the worker post-init. */
  version = null

  /**
   * @param {Worker} worker
   * @private
   */
  constructor(worker) {
    this.#worker = worker
    this.#queue = new RequestQueue(message => worker.postMessage(message))
    worker.addEventListener('message', ev => this.#queue.reply(ev.data ?? {}))
    const fail = () => {
      this.#queue.close(new Error('Database worker failed; reopen the database'))
      worker.terminate()
    }
    worker.addEventListener('error', fail)
    worker.addEventListener('messageerror', fail)
  }

  /**
   * Spawns a worker, initializes the WASM module + thread pool, and optionally
   * restores a committed database from OPFS.
   *
   * @param {{
   *   numThreads?: number,
   *   workerUrl?: URL | string,
   *   name?: string,
   * }} [options]
   *   `numThreads`: for wasm-bindgen-rayon's initThreadPool; default
   *     `navigator.hardwareConcurrency` clamped to 16.
   *   `workerUrl`: worker script URL override; the default resolves
   *     `./worker.js` via import.meta.
   *   `name`: OPFS database to restore. Absent: empty graph. Present but
   *     missing in OPFS: empty, populate then `commit(name)`.
   * @returns {Promise<TraverseDb>}
   */
  static async open(options = {}) {
    // bundlers (Vite / webpack / Parcel) only recognize the literal `new Worker(new
    // URL('./worker.js', import.meta.url), ...)` as a worker entry; a variable form makes them ship
    // the worker as a raw asset with unresolved imports, so the two paths stay explicit
    const worker = options.workerUrl
      ? new Worker(options.workerUrl, { type: 'module' })
      : new Worker(new URL('./worker.js', import.meta.url), { type: 'module' })
    const db = new TraverseDb(worker)
    const numThreads = Math.min(
      options.numThreads ?? (globalThis.navigator?.hardwareConcurrency ?? 4),
      16,
    )
    let initResult
    try { initResult = await db.#call('init', { numThreads }) }
    catch (error) { db.close(); throw error }
    db.version = initResult?.version ?? null
    if (options.name) {
      await db.load(options.name).catch(() => undefined)
    }
    return db
  }

  /**
   * Lists OPFS databases without opening a `TraverseDb` (a short-lived worker,
   * cheaper than a full open), e.g. for "previously saved" lists.
   * @returns {Promise<Array<{ name: string, size: number, lastModified: number }>>}
   */
  static async listDatabases(options = {}) {
    const worker = options.workerUrl
      ? new Worker(options.workerUrl, { type: 'module' })
      : new Worker(new URL('./worker.js', import.meta.url), { type: 'module' })
    const db = new TraverseDb(worker)
    try {
      await db.#call('init', { numThreads: 1 })
      return await db.listDatabases()
    } finally {
      db.close()
    }
  }

  /** Deletes an OPFS database without opening it (short-lived worker). No-op if it doesn't exist. */
  static async deleteDatabase(name, options = {}) {
    const worker = options.workerUrl
      ? new Worker(options.workerUrl, { type: 'module' })
      : new Worker(new URL('./worker.js', import.meta.url), { type: 'module' })
    const db = new TraverseDb(worker)
    try {
      await db.#call('init', { numThreads: 1 })
      await db.deleteDatabase(name)
    } finally {
      db.close()
    }
  }

  /**
   * Runs a Cypher query. Returns the HTTP `/api/query` `QueryResponse` shape
   * (columns + rows + timing + mutation stats + query type + hydrated entities).
   *
   * @param {string} cypher
   * @param {Record<string, unknown> | null} [params]
   * @param {{ signal?: AbortSignal, timeoutMs?: number, dialect?: 'cypher' | 'gql' }} [options]
   *   `timeoutMs`: deadline the executor checks at iteration boundaries; pair
   *   with `signal` for a responsive cancel (the deadline stops the worker
   *   side, the signal rejects the promise).
   *   `dialect`: `'gql'` uses the ISO/IEC 39075 parser (same engine); default
   *   openCypher.
   * @returns {Promise<QueryResponse>}
   */
  query(cypher, params = null, options = {}) {
    return this.#call(
      'query',
      {
        cypher,
        params,
        timeoutMs: options.timeoutMs ?? null,
        dialect: options.dialect ?? null,
      },
      options,
    )
  }

  /**
   * Algorithm catalog metadata (the `/api/algorithms` shape), for Studio's GDS
   * drawer.
   *
   * @returns {Promise<AlgorithmsResponse>}
   */
  algorithms() {
    return this.#call('algorithms', {})
  }

  /**
   * Schema summary: labels, relationship types, property keys, per-label /
   * per-type property maps; the HTTP `/api/schema` shape.
   * @returns {Promise<SchemaResponse>}
   */
  schema() {
    return this.#call('schema', {})
  }

  /**
   * Runs a Cypher import (multi-statement OK); returns an `ImportResult` like
   * HTTP `/api/import`.
   * @param {string} cypher
   * @returns {Promise<ImportResult>}
   */
  importCypher(cypher) {
    return this.#call('import', { cypher })
  }

  /**
   * Resets to an empty graph, dropping indexes and properties.
   * @returns {Promise<void>}
   */
  async clear() {
    await this.#call('clear', {})
  }

  /** @returns {Promise<number>} */
  nodeCount() {
    return this.#call('nodeCount', {})
  }

  /** @returns {Promise<number>} */
  edgeCount() {
    return this.#call('edgeCount', {})
  }

  /** @returns {Promise<{ nodes: number, edges: number }>} */
  stats() {
    return this.#call('stats', {})
  }

  /**
   * Approximate in-memory size of the graph in bytes (node + edge property +
   * index storage), like the server's per-DB `memory_bytes`; *not* WASM linear
   * memory or the JS heap.
   * @returns {Promise<number>}
   */
  estimatedMemory() {
    return this.#call('estimatedMemory', {})
  }

  // -- OPFS persistence ------------------------------------------

  /**
   * Persists the current graph to OPFS under `name`; later page loads restore it
   * with `TraverseDb.open({ name })`.
   *
   * @param {string} name
   * @returns {Promise<{ bytes: number }>}
   */
  commit(name) {
    return this.#call('commit', { name })
  }

  /**
   * Writes a caller-supplied `.tvdb` buffer to OPFS under `name` WITHOUT loading
   * it; `load(name)` parses it. Lets large uploads show "uploading" and
   * "loading into memory" as separate phases.
   *
   * @param {string} name
   * @param {Uint8Array} bytes
   * @returns {Promise<{ ok: true, bytes: number }>}
   */
  writeBytes(name, bytes) {
    return this.#call('writeBytes', { name, bytes })
  }

  /**
   * Restores the graph from OPFS under `name`, replacing the current one.
   * `{ ok: false, missing: true }` if there's no such database.
   *
   * @param {string} name
   * @returns {Promise<{ ok: boolean, missing?: boolean, nodes?: number, edges?: number }>}
   */
  load(name) {
    return this.#call('open', { name })
  }

  /**
   * OPFS databases for this origin, newest first.
   * @returns {Promise<Array<{ name: string, size: number, lastModified: number }>>}
   */
  listDatabases() {
    return this.#call('listDatabases', {})
  }

  /**
   * Deletes an OPFS database; no-op if absent.
   * @param {string} name
   * @returns {Promise<void>}
   */
  async deleteDatabase(name) {
    await this.#call('deleteDatabase', { name })
  }

  /**
   * Exports the graph as a `.tvdb` buffer in the native server's on-disk format
   * (loads in the native server and vice versa), e.g. for a "Download .tvdb"
   * button. Within the origin use `commit()`, which goes to OPFS without a file
   * dialog.
   * @returns {Promise<Uint8Array>}
   */
  async exportTvdb() {
    const { bytes } = await this.#call('exportTvdb', {})
    return bytes
  }

  /**
   * Replaces the graph with one from a `.tvdb` buffer (from `exportTvdb` or the
   * native server); with `exportTvdb`, moves graphs across tabs / origins via
   * files.
   * @param {Uint8Array} bytes
   */
  async loadTvdb(bytes) {
    return this.#call('loadTvdb', { bytes })
  }

  /** Tears down the worker; pending requests reject with `AbortError`. */
  close() {
    this.#queue.close()
    this.#worker.terminate()
  }

  get queueStats() { return this.#queue.stats }

  #call(kind, payload, options = {}) {
    return this.#queue.call(kind, payload, options)
  }
}

/** Library version (the workspace Cargo version); null until `open()` resolves. */
export function version() {
  return TraverseDb.prototype.version
}

// -- type re-exports for JSDoc consumers ---------------------------

/**
 * @typedef {object} QueryResponse
 * @property {string[]} columns
 * @property {unknown[][]} rows
 * @property {number} total_rows
 * @property {boolean} truncated
 * @property {'Read' | 'Write' | 'ReadWrite' | 'Schema'} query_type
 * @property {number} time_ms
 */

/**
 * @typedef {object} AlgorithmsResponse
 * @property {GdsAlgorithm[]} algorithms
 */

/**
 * @typedef {object} GdsAlgorithm
 * @property {string} name
 * @property {string} displayName
 * @property {string} category
 * @property {string} outputKind
 * @property {string} stability
 * @property {string} description
 * @property {boolean} writable
 * @property {string[]} modes
 * @property {string[]} outputColumns
 * @property {string[]} statsColumns
 * @property {string[]} writeColumns
 */
