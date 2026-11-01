// Web Worker hosting the WASM module. The main thread never calls WASM
// directly; everything goes through postMessage here, so the page stays
// responsive while an algorithm runs for seconds.
//
// Wire protocol (client side in `index.js`):
//   request:  { id: number, kind: string, payload: object, transfer?: Transferable[] }
//   response: { id: number, ok: true, result: unknown }
//           | { id: number, ok: false, error: { code, message, stack? } }
//
// Requests are serialized: one WASM call in flight per worker, responses in
// order.

// namespace import so the single-threaded build (no `initThreadPool` export) still bundles under
// strict resolvers like Vite/rolldown; the threaded build is detected at runtime via
// `wasm.initThreadPool`
import * as wasm from './pkg/traverse_wasm.js'
const init = wasm.default
const TraverseDb = wasm.TraverseDb
const initThreadPool = Reflect.get(wasm, 'initThreadPool')

/** @type {TraverseDb | null} */
let db = null

/**
 * Init promise, resolved once `init` + `initThreadPool` have run in this worker; later requests
 * await it.
 */
let readyPromise = null

async function bootstrap(numThreads) {
  await init()
  if (typeof initThreadPool === 'function') {
    // threaded build: start the rayon pool; JS passes the worker count (the client defaults to
    // navigator.hardwareConcurrency)
    await initThreadPool(numThreads ?? 4)
  }
  db = new TraverseDb()
}

function ensureReady(numThreads) {
  if (!readyPromise) readyPromise = bootstrap(numThreads)
  return readyPromise
}

// serialize async file I/O along with synchronous WASM, so an open/commit can't interleave with
// later queries
let tail = Promise.resolve()
let pending = 0
self.addEventListener('message', (ev) => {
  const { id, kind, payload } = ev.data ?? {}
  const reject = err => self.postMessage({ id, ok: false, error: { code: 'WasmError', message: err instanceof Error ? err.message : String(err) } })
  if (!Number.isSafeInteger(id) || id <= 0 || pending >= 32) { reject(new Error('Invalid or excessive worker request')); return }
  pending++
  tail = tail.then(async () => {
    try {
      const result = await handle(kind, payload)
      const transfer = result?.bytes instanceof Uint8Array ? [result.bytes.buffer] : []
      self.postMessage({ id, ok: true, result }, transfer)
    } catch (error) { reject(error) }
    finally { pending-- }
  })
})

/** Dispatches one request kind; returns a structured-cloneable value for the main thread. */
async function handle(kind, payload) {
  switch (kind) {
    case 'init':
      await ensureReady(payload?.numThreads)
      // `TraverseDb.version` is a static method returning a string, not a getter: call it (posting
      // the function itself fails structured clone with a misleading "could not be cloned")
      return { ready: true, version: TraverseDb.version() }

    case 'query': {
      await ensureReady()
      // `timeoutMs` (optional, > 0) sets the executor's thread-local deadline; pair with a JS
      // AbortController to cancel a runaway algorithm
      const timeoutMs = Math.min(30000, Math.max(1, Number.isFinite(payload.timeoutMs) && payload.timeoutMs > 0 ? payload.timeoutMs : 10000))
      // `dialect` (optional): 'gql' uses the ISO/IEC 39075 parser, anything else openCypher
      const dialect = typeof payload.dialect === 'string' ? payload.dialect : undefined
      return db.query(payload.cypher, payload.params ?? null, timeoutMs, dialect)
    }

    case 'algorithms': {
      await ensureReady()
      return db.algorithms()
    }

    case 'clear': {
      await ensureReady()
      db.clear()
      return { ok: true }
    }

    case 'nodeCount': {
      await ensureReady()
      return db.nodeCount()
    }

    case 'edgeCount': {
      await ensureReady()
      return db.edgeCount()
    }

    case 'stats': {
      await ensureReady()
      return { nodes: db.nodeCount(), edges: db.edgeCount() }
    }

    case 'estimatedMemory': {
      await ensureReady()
      return db.estimatedMemory()
    }

    case 'schema': {
      await ensureReady()
      return db.schema()
    }

    case 'import': {
      // Cypher import: split the script, run each statement, build an ImportResult in the HTTP
      // /api/import shape (same statement loop). `query` takes exactly one statement, like the
      // server, whose import endpoint also splits first; the splitting lives in Rust so there's one
      // implementation.
      await ensureReady()
      const statements = TraverseDb.splitStatements(payload.cypher)
      const before = { n: db.nodeCount(), e: db.edgeCount() }
      const t0 = performance.now()
      let stats = emptyStats()
      let executed = 0
      const errors = []
      for (const statement of statements) {
        try {
          const r = db.query(statement, null)
          executed += 1
          // accumulate against the ORIGINAL counts, so totals cover the whole import, not just the
          // last statement
          stats = stats0FromResponse(r, before, db)
        } catch (err) {
          // keep going, like the server: one bad statement in a long script shouldn't discard the
          // good ones, and the caller learns which failed
          errors.push({ statement, error: String(err.message ?? err) })
        }
      }
      return {
        total_statements: statements.length,
        executed,
        errors,
        stats,
        time_ms: performance.now() - t0,
      }
    }

    // -- OPFS persistence ----------------------------------------

    case 'commit': {
      await ensureReady()
      const bytes = db.exportTvdb()
      await opfsWrite(payload.name, bytes)
      return { ok: true, bytes: bytes.byteLength }
    }

    case 'writeBytes': {
      // write a caller-provided `.tvdb` buffer to OPFS WITHOUT touching the engine: the upload flow
      // shows "uploading" and "loading into memory" as separate phases instead of one long pause
      // while a 500 MB tvdb parses
      await ensureReady()
      const bytes = payload.bytes
      await opfsWrite(payload.name, bytes)
      return { ok: true, bytes: bytes.byteLength }
    }

    case 'open': {
      await ensureReady()
      const bytes = await opfsRead(payload.name)
      if (!bytes) {
        return { ok: false, missing: true }
      }
      db.loadTvdb(bytes)
      return { ok: true, nodes: db.nodeCount(), edges: db.edgeCount() }
    }

    case 'listDatabases':
      return await opfsList()

    case 'deleteDatabase':
      await opfsDelete(payload.name)
      return { ok: true }

    // -- manual byte-level I/O for File-API import / download export -- the bytes are the native
    // server's `.tvdb` format and round-trip between the WASM and native builds

    case 'exportTvdb': {
      await ensureReady()
      const bytes = db.exportTvdb()
      return { bytes }
    }

    case 'loadTvdb': {
      await ensureReady()
      db.loadTvdb(payload.bytes)
      return { ok: true, nodes: db.nodeCount(), edges: db.edgeCount() }
    }

    default:
      throw new Error(`Unknown worker request kind: ${kind}`)
  }
}

// -- OPFS helpers ----------------------------------------------------

/**
 * Get-or-create the Traverse directory in the OPFS root. Databases live there as `<name>.tvdb`
 * files; per-database Studio metadata (styles, saved queries) as `<name>.studio.json` sidecars.
 */
async function opfsDir() {
  const root = await navigator.storage.getDirectory()
  return root.getDirectoryHandle('traverse', { create: true })
}

/** Empty QueryStats; import stats are derived from before/after counts. */
function emptyStats() {
  return {
    nodes_created: 0,
    nodes_deleted: 0,
    relationships_created: 0,
    relationships_deleted: 0,
    properties_set: 0,
    labels_added: 0,
    labels_removed: 0,
  }
}

/** Best-effort stats from entity counts before/after; property/label changes aren't captured. */
function stats0FromResponse(_response, before, db) {
  const after = { n: db.nodeCount(), e: db.edgeCount() }
  const stats = emptyStats()
  const dn = after.n - before.n
  const de = after.e - before.e
  if (dn > 0) stats.nodes_created = dn
  else if (dn < 0) stats.nodes_deleted = -dn
  if (de > 0) stats.relationships_created = de
  else if (de < 0) stats.relationships_deleted = -de
  return stats
}

/** Writes bytes to OPFS via a sync access handle (worker-only API). */
async function opfsWrite(name, bytes) {
  const dir = await opfsDir()
  const file = await dir.getFileHandle(filename(name), { create: true })
  const handle = await file.createSyncAccessHandle()
  try {
    handle.truncate(0)
    handle.write(bytes, { at: 0 })
    handle.flush()
  } finally {
    handle.close()
  }
}

/** Reads bytes from OPFS; null if the file doesn't exist. */
async function opfsRead(name) {
  const dir = await opfsDir()
  let file
  try {
    file = await dir.getFileHandle(filename(name))
  } catch {
    return null
  }
  const handle = await file.createSyncAccessHandle()
  try {
    const size = handle.getSize()
    const buf = new Uint8Array(size)
    handle.read(buf, { at: 0 })
    return buf
  } finally {
    handle.close()
  }
}

/** Databases in the OPFS root; `.studio.json` sidecars are hidden (Studio reads them separately). */
async function opfsList() {
  const dir = await opfsDir()
  const items = []
  // @ts-ignore: OPFS async iteration is well supported in modern browsers
  for await (const [filenameStr, entry] of dir.entries()) {
    if (entry.kind !== 'file') continue
    if (!filenameStr.endsWith('.tvdb')) continue
    const f = await entry.getFile()
    items.push({
      name: filenameStr.slice(0, -'.tvdb'.length),
      size: f.size,
      lastModified: f.lastModified,
    })
  }
  return items.sort((a, b) => b.lastModified - a.lastModified)
}

/** Deletes a database from OPFS; no-op if absent. */
async function opfsDelete(name) {
  const dir = await opfsDir()
  try {
    await dir.removeEntry(filename(name))
  } catch {
    // already gone, fine
  }
}

/**
 * OPFS filename for a database: slashes / dots in the name are stripped to keep the layout flat;
 * the binary tvdb (paged, lz4 per record, the native on-disk format) is `<name>.tvdb`.
 */
function filename(name) {
  const safe = String(name).replace(/[\/\\.]/g, '_')
  return `${safe}.tvdb`
}
