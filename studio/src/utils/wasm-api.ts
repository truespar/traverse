// WasmApiClient - Studio backend that runs the Traverse engine
// in-process via WebAssembly (no server). Spawned by `main.ts` when
// `?wasm=1` (or matching env flag) is set; satisfies the same
// `ApiClient` interface as `HttpApiClient` so stores/panels are
// unaware which backend is active.
//
// Backing model:
// - One `TraverseDb` (Web Worker + WASM module) holds the active graph.
// - Databases are persisted as `.tvdb` binary blobs in OPFS; switching
//   databases means `commit(current)` followed by `load(target)`.
// - Per-database studio metadata (saved queries + styles) lives in an
//   OPFS sidecar `<name>.studio.json` accessed from the main thread
//   via the async OPFS API. Sync access handles are worker-only and
//   not needed for this small JSON.
// - Server-only features (users, AI chat, file upload over
//   HTTP, metrics, aliases) raise
//   `NotSupportedError`. Capability flags let panels gate UI off
//   before reaching that fallback.

import { TraverseDb } from '@truespar/traverse-wasm'
import type {
  AlgorithmsResponse,
  ApiCapabilities,
  ApiClient,
  CsvColumnMapping,
  CsvEdgeImportResult,
  CsvEdgeImportSource,
  CsvImportResult,
  DatabaseInfo,
  GdsAlgorithm,
  GdsMode,
  HealthResponse,
  ImportResult,
  QueryDialect,
  QueryResponse,
  SavedQueryEntry,
  SchemaResponse,
  StylesData,
  UploadResult,
  UserInfo,
} from './api'
import { buildAlgorithmCall, NotSupportedError } from './api'
import { EXAMPLE_GRAPHS } from '../data/example-graphs'

const DEFAULT_DB_NAME = 'traverse'

/** Minimal RFC-4180-ish CSV parser. Handles quoted fields, escaped
 *  double-quotes (`""`), and \r\n / \n line endings. Returns rows as
 *  string[][]. Does NOT attempt to detect the delimiter - the caller
 *  passes it explicitly (defaults to `,` in the WasmApiClient calls).
 *
 *  Why hand-roll instead of pulling in a dep: keeps the WASM bundle
 *  free of `papaparse` (~30 KB gzipped) and the parser only needs to
 *  handle well-formed input the Studio import panel produces. */
function parseCsv(text: string, delimiter: string): string[][] {
  const rows: string[][] = []
  let cell = ''
  let row: string[] = []
  let inQuotes = false
  for (let i = 0; i < text.length; i++) {
    const ch = text[i]
    if (inQuotes) {
      if (ch === '"') {
        // Doubled quote within a quoted field is a literal quote.
        if (text[i + 1] === '"') {
          cell += '"'
          i++
        } else {
          inQuotes = false
        }
      } else {
        cell += ch
      }
    } else {
      if (ch === '"' && cell === '') {
        inQuotes = true
      } else if (ch === delimiter) {
        row.push(cell)
        cell = ''
      } else if (ch === '\n') {
        row.push(cell)
        rows.push(row)
        cell = ''
        row = []
      } else if (ch === '\r') {
        // Skip; \n that follows finishes the row.
      } else {
        cell += ch
      }
    }
  }
  // Final cell - only push if the last line had content (no trailing
  // newline) so we don't append a spurious empty row.
  if (cell !== '' || row.length > 0) {
    row.push(cell)
    rows.push(row)
  }
  return rows
}

/** Escape a raw CSV cell into a Cypher literal of the requested type.
 *  Throws on unparseable numeric / boolean values so the caller can
 *  surface a per-row error rather than producing invalid Cypher. */
function csvCellToCypherLiteral(
  raw: string,
  type: 'string' | 'integer' | 'float' | 'boolean',
): string {
  switch (type) {
    case 'integer': {
      const n = Number(raw)
      if (!Number.isFinite(n) || Math.floor(n) !== n) {
        throw new Error(`"${raw}" is not an integer`)
      }
      return String(n)
    }
    case 'float': {
      const n = Number(raw)
      if (!Number.isFinite(n)) throw new Error(`"${raw}" is not a number`)
      return String(n)
    }
    case 'boolean': {
      const v = raw.trim().toLowerCase()
      if (v === 'true' || v === '1' || v === 'yes') return 'true'
      if (v === 'false' || v === '0' || v === 'no') return 'false'
      throw new Error(`"${raw}" is not a boolean`)
    }
    case 'string':
    default:
      // JSON-stringify escapes quotes/backslashes/newlines properly
      // for Cypher's string literal - same syntax (`"..."` with `\n`
      // escapes etc.).
      return JSON.stringify(raw)
  }
}

/** Backtick-quote a Cypher identifier (label / type / property key)
 *  when it contains non-ident characters. The Studio's column picker
 *  doesn't currently validate identifier syntax, so backtick-wrap
 *  anything that isn't a plain ident as a safety net. */
function cypherIdent(name: string): string {
  if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) return name
  // Escape any backticks in the name (rare).
  return '`' + name.replace(/`/g, '``') + '`'
}

/** Name of the seed database installed on first WASM boot when the
 *  OPFS subtree is empty. Keeping it stable (rather than e.g. 'sample')
 *  means a returning user with a still-populated OPFS lands on the
 *  same content they had last time. */
const SEED_DB_NAME = 'movies'
const SEED_GRAPH_ID = 'example-movies'

/** Per-database sidecar - Studio metadata that lives next to the
 *  binary `.tvdb` in OPFS. Small JSON, async OPFS API is fine. */
interface DatabaseSidecar {
  styles?: StylesData
  savedQueries?: SavedQueryEntry[]
}

const EMPTY_STYLES: StylesData = { nodes: {}, edges: {} }

/** The browser build has a Graph Assistant only when its build names a hosted
 *  endpoint (`VITE_TRAVERSE_PUBLIC_AI_URL`) and the page runs on one of the
 *  origins that endpoint accepts (`VITE_TRAVERSE_PUBLIC_AI_ORIGINS`: hostnames,
 *  `*.example.com` for a domain's subdomains). Anywhere else the chat button
 *  stays hidden rather than failing every click, so the same bundle served
 *  from another domain shows none. A plain build names neither. */
function hostedAssistantAvailable(): boolean {
  const url = import.meta.env.VITE_TRAVERSE_PUBLIC_AI_URL as string | undefined
  const origins = (import.meta.env.VITE_TRAVERSE_PUBLIC_AI_ORIGINS as string | undefined) ?? ''
  if (!url) return false
  const h = location.hostname
  return origins
    .split(',')
    .map((o) => o.trim())
    .filter(Boolean)
    .some((o) => (o.startsWith('*.') ? h.endsWith(o.slice(1)) : h === o))
}

export class WasmApiClient implements ApiClient {
  readonly capabilities: ApiCapabilities = {
    users: false,
    // The Graph Assistant needs a server that holds a model's key. The
    // browser build has none, so it is on only where its build names a
    // hosted endpoint for the page's origin; anywhere else it stays off.
    aiChat: hostedAssistantAvailable(),
    serverFiles: false,
    metrics: false,
    // the engine runs in a worker busy with the query; its time limit is what stops it
    cancelQuery: false,
  }

  /** Engine wrapper - single TraverseDb / Web Worker for the tab. */
  private db: TraverseDb | null = null
  /** Currently-loaded database name. Empty string => nothing loaded. */
  private activeDb = ''
  /** Set of database names known to exist (in OPFS or as the active
   *  in-memory graph). Kept in sync with OPFS via `listDatabases`. */
  private known = new Set<string>()
  /** Startup timestamp - used to synthesize `uptime_secs` on health. */
  private startedAt = Date.now()
  /** Cached library version once `open()` resolves. */
  private version = ''

  /** Async constructor. Spawns the worker, initializes the thread pool,
   *  restores the user's last-active database (if any) from OPFS, or
   *  on a fresh OPFS subtree seeds the bundled "Movies" sample so the
   *  first-time visitor lands on something to query.
   *
   *  Pass `{ seedSample: false }` to skip seeding (integrators wrapping
   *  WasmApiClient programmatically usually want a blank slate). */
  static async open(options?: { numThreads?: number; seedSample?: boolean }): Promise<WasmApiClient> {
    const client = new WasmApiClient()
    const threads = Math.min(
      options?.numThreads ?? navigator.hardwareConcurrency ?? 4,
      16,
    )
    const db = await TraverseDb.open({ numThreads: threads })
    client.db = db
    client.version = db.version ?? ''
    const existing = await db.listDatabases()
    for (const e of existing) client.known.add(e.name)
    if (existing.length === 0 && options?.seedSample !== false) {
      await client.seedSample()
      // The Graph Assistant panel is deliberately NOT pre-opened here. It
      // used to be, to make the chat discoverable on a true first visit, and
      // that left the panel and the Graph Assistant button out of step: the
      // panel was already open, so a visitor's first click on the button
      // closed it, and a second reopened it. That reads as a panel flickering
      // and ignoring everything you type. The button does the opening.
    }
    // Restore the database the user was last viewing - connection store
    // mirrors this in localStorage under `tv-studio-database`. If that
    // selection no longer exists fall back to whatever's first in
    // `known` (which, on a fresh tab, is the seed sample).
    const lastDb = localStorage.getItem('tv-studio-database')
    if (lastDb && client.known.has(lastDb) && lastDb !== client.activeDb) {
      const r = await db.load(lastDb)
      if (r.ok) client.activeDb = lastDb
    } else if (!client.activeDb && client.known.size > 0) {
      const first = client.known.values().next().value as string
      const r = await db.load(first)
      if (r.ok) client.activeDb = first
    }

    // Auto-commit when the tab is hidden (user switching tabs, closing,
    // minimizing on mobile). Browsers don't fire 'beforeunload' reliably
    // on mobile or for back-forward navigations, but they do fire
    // visibilitychange. Best-effort: if the commit can't finish before
    // the tab really closes, OPFS rolls back to the last successful
    // commit and the user loses only post-last-commit work.
    document.addEventListener('visibilitychange', () => {
      if (document.visibilityState === 'hidden') {
        client.flushIfActive()
      }
    })
    return client
  }

  /** Best-effort commit of the currently-active database. Called from
   *  the `visibilitychange` handler when the tab is being backgrounded
   *  or closed. Errors are swallowed because there's nothing we can
   *  show the user at that point. */
  private async flushIfActive(): Promise<void> {
    if (!this.db || !this.activeDb) return
    try {
      await this.db.commit(this.activeDb)
    } catch (err) {
      console.warn('Traverse WASM: visibility-flush failed', err)
    }
  }

  /** Install the bundled "Movies" sample as a fresh OPFS database.
   *  Called from `open()` when the OPFS subtree is empty so the
   *  first-time visitor lands on a populated graph. Idempotent: if the
   *  seed database already exists it's left alone.
   *
   *  The starter queries shipped on the example graph are persisted as
   *  saved queries on the seed db so the user immediately has working
   *  examples in the saved-query panel. */
  private async seedSample(): Promise<void> {
    const sample = EXAMPLE_GRAPHS.find((g) => g.id === SEED_GRAPH_ID)
    if (!sample) return
    const db = this.requireDb()
    await db.clear()
    try {
      await db.importCypher(sample.cypher)
    } catch (err) {
      // The seed mustn't fatally fail the WASM boot - Studio still
      // works with an empty graph, the user can re-import via the
      // Examples panel.
      console.warn(`Traverse WASM: failed to seed ${SEED_DB_NAME} sample:`, err)
      return
    }
    await db.commit(SEED_DB_NAME)
    this.activeDb = SEED_DB_NAME
    this.known.add(SEED_DB_NAME)
    // Persist the example's starterQueries as saved queries on the
    // seed database. Side door: `writeSidecar` keys off `this.activeDb`
    // - which we just set - so the JSON lands next to the new tvdb.
    const saved: SavedQueryEntry[] = sample.starterQueries.map((q) => ({
      name: q.label,
      query: q.query,
      createdAt: new Date().toISOString(),
    }))
    try {
      await this.writeSidecar(SEED_DB_NAME, { savedQueries: saved })
    } catch (err) {
      console.warn('Traverse WASM: failed to seed saved queries:', err)
    }
  }

  private requireDb(): TraverseDb {
    if (!this.db) throw new Error('WasmApiClient: worker not initialized - call WasmApiClient.open() first')
    return this.db
  }

  /** Switch the active database. Commits the current one first (lossless
   * - same byte format) then loads the target from OPFS. No-op when
   *  already on the target. Creates an empty graph if the name is new. */
  private async useDatabase(name: string): Promise<void> {
    if (!name || name === this.activeDb) return
    const db = this.requireDb()
    if (this.activeDb) {
      await db.commit(this.activeDb)
    }
    if (this.known.has(name)) {
      const r = await db.load(name)
      if (!r.ok) {
        // OPFS entry vanished between listing and load - fall back to empty.
        await db.clear()
      }
    } else {
      await db.clear()
      this.known.add(name)
    }
    this.activeDb = name
  }

  // -- Catalog / health -----------------------------------------------

  async health(): Promise<HealthResponse> {
    const db = this.requireDb()
    const databases = await this.databasesDetailed()
    const memory = databases.reduce((sum, d) => sum + d.memory_bytes, 0)
    return {
      status: 'ok',
      version: this.version || 'wasm',
      uptime_secs: Math.floor((Date.now() - this.startedAt) / 1000),
      memory_bytes: memory,
      databases,
      allow_drop_database: true,
      auth_required: false,
      // Compile-time feature flags, mirroring the server's health
      // payload. `gql` gates the dialect picker in the query editor -
      // the WASM bundle links traverse-gql, so ISO/IEC 39075 works
      // in-tab exactly like the server's `X-Query-Dialect: gql` route.
      features: { gql: true },
    }
  }

  async schema(database?: string): Promise<SchemaResponse> {
    if (database) await this.useDatabase(database)
    const db = this.requireDb()
    return (await db.schema()) as SchemaResponse
  }

  algorithms(): Promise<AlgorithmsResponse> {
    return this.requireDb().algorithms() as Promise<AlgorithmsResponse>
  }

  // -- Query execution ------------------------------------------------

  async query(
    cypher: string,
    parameters?: Record<string, unknown>,
    database?: string,
    dialect?: QueryDialect,
  ): Promise<QueryResponse> {
    if (database) await this.useDatabase(database)
    const db = this.requireDb()
    // 60 s default: enough for an algorithm on the seed movies graph with 10x slack, short enough
    // that a runaway loop (a typo'd pattern without LIMIT) doesn't pin the worker until reload.
    // `dialect: 'gql'` uses the bundled ISO/IEC 39075 parser (traverse-gql) on the same engine.
    const resp = (await db.query(cypher, parameters ?? null, {
      timeoutMs: 60_000,
      dialect,
    })) as Partial<QueryResponse>
    // Fill in the fields the WASM crate doesn't yet emit so Studio's
    // stores can keep their existing assumptions. nodes/edges are the
    // hydrated entity arrays the graph view consumes - without them
    // the Graph tab renders empty even when rows have node references.
    return {
      columns: resp.columns ?? [],
      rows: resp.rows ?? [],
      total_rows: resp.total_rows ?? resp.rows?.length ?? 0,
      truncated: resp.truncated ?? false,
      nodes: resp.nodes ?? [],
      edges: resp.edges ?? [],
      entities_truncated: resp.entities_truncated ?? false,
      query_type: resp.query_type ?? 'Read',
      plan: resp.plan,
      stats: resp.stats ?? {
        nodes_created: 0,
        nodes_deleted: 0,
        relationships_created: 0,
        relationships_deleted: 0,
        properties_set: 0,
        labels_added: 0,
        labels_removed: 0,
      },
      time_ms: resp.time_ms ?? 0,
      parsing_time_ms: resp.parsing_time_ms,
      planning_time_ms: resp.planning_time_ms,
      execution_time_ms: resp.execution_time_ms,
    }
  }

  runAlgorithm(
    algorithm: GdsAlgorithm,
    mode: GdsMode,
    config: Record<string, unknown>,
    database?: string,
  ): Promise<QueryResponse> {
    return this.query(buildAlgorithmCall(algorithm, mode, config), undefined, database)
  }

  // -- Database management --------------------------------------------

  async databases(): Promise<{ name: string; default: boolean }[]> {
    const detailed = await this.databasesDetailed()
    return detailed.map((d) => ({ name: d.name, default: d.default }))
  }

  /** Detailed view used by health() - exposes node/edge/memory totals
   *  for the active database. Other databases report as 0 / 0 because
   *  they aren't loaded into memory (single-graph WASM model); their
   *  `memory_bytes` is the on-disk OPFS .tvdb size, which is a useful
   *  signal in the file manager even though it's not "RAM". The
   *  active database reports the real engine estimate. */
  private async databasesDetailed(): Promise<DatabaseInfo[]> {
    const db = this.requireDb()
    const entries = await db.listDatabases()
    for (const e of entries) this.known.add(e.name)
    // Look up the active database's in-memory footprint once - every
    // subsequent comparison just checks `e.name === this.activeDb`.
    const activeMemory = this.activeDb ? await db.estimatedMemory() : 0
    const activeNodes = this.activeDb ? await db.nodeCount() : 0
    const activeEdges = this.activeDb ? await db.edgeCount() : 0
    const result: DatabaseInfo[] = []
    let sawActive = false
    for (const e of entries) {
      const isActive = e.name === this.activeDb
      if (isActive) sawActive = true
      result.push({
        name: e.name,
        default: isActive || (!this.activeDb && e === entries[0]),
        nodes: isActive ? activeNodes : 0,
        edges: isActive ? activeEdges : 0,
        memory_bytes: isActive ? activeMemory : e.size,
      })
    }
    // Active DB hasn't been committed yet - surface it so Studio shows
    // unsaved progress.
    if (this.activeDb && !sawActive) {
      result.unshift({
        name: this.activeDb,
        default: true,
        nodes: activeNodes,
        edges: activeEdges,
        memory_bytes: activeMemory,
      })
    }
    if (result.length === 0) {
      // Empty WASM tab - synthesize the default name so the connection
      // store has something to bind to. The actual database is created
      // on first write.
      result.push({
        name: DEFAULT_DB_NAME,
        default: true,
        nodes: 0,
        edges: 0,
        memory_bytes: 0,
      })
    }
    return result
  }

  async createDatabase(name: string): Promise<{ ok: boolean; name: string }> {
    const db = this.requireDb()
    if (this.activeDb) {
      await db.commit(this.activeDb)
    }
    await db.clear()
    this.activeDb = name
    this.known.add(name)
    await db.commit(name)
    return { ok: true, name }
  }

  async dropDatabase(name: string): Promise<unknown> {
    const db = this.requireDb()
    if (name === this.activeDb) {
      await db.clear()
      this.activeDb = ''
    }
    await db.deleteDatabase(name)
    this.known.delete(name)
    await this.deleteSidecar(name)
    return { ok: true }
  }

  async renameDatabase(oldName: string, newName: string): Promise<{ ok: boolean; name: string }> {
    const db = this.requireDb()
    if (oldName === this.activeDb) {
      // Active - write under the new name, drop the old.
      await db.commit(newName)
      await db.deleteDatabase(oldName)
      this.activeDb = newName
    } else {
      // Inactive - load -> rename -> restore previous active.
      const prevActive = this.activeDb
      if (prevActive) await db.commit(prevActive)
      const r = await db.load(oldName)
      if (!r.ok) throw new Error(`database ${oldName} not found`)
      await db.commit(newName)
      await db.deleteDatabase(oldName)
      if (prevActive) {
        const back = await db.load(prevActive)
        this.activeDb = back.ok ? prevActive : ''
      } else {
        this.activeDb = newName
      }
    }
    // Migrate the sidecar JSON.
    const sidecar = await this.readSidecar(oldName)
    await this.writeSidecar(newName, sidecar)
    await this.deleteSidecar(oldName)
    this.known.delete(oldName)
    this.known.add(newName)
    return { ok: true, name: newName }
  }

  async save(): Promise<{ status: string }> {
    const db = this.requireDb()
    if (this.activeDb) await db.commit(this.activeDb)
    return { status: 'ok' }
  }

  async loadDatabase(name: string, _file: string): Promise<{ ok: boolean; name: string; readOnly: boolean }> {
    // HTTP mode loads a `.tvdb` from the server's filesystem under a
    // given name. WASM mode reinterprets that as "make the OPFS-
    // resident `<name>.tvdb` the active in-memory graph". The
    // FileManagerPanel calls this right after `uploadDatabase`
    // returns `loaded: false`, so the user sees the "Loading into
    // memory..." phase covering the slow `loadTvdb` parse instead of
    // a silent freeze after "Uploading 100%".
    await this.useDatabase(name)
    return { ok: true, name, readOnly: false }
  }

  async unloadDatabase(name: string): Promise<{ ok: boolean }> {
    const db = this.requireDb()
    if (name === this.activeDb) {
      await db.commit(this.activeDb)
      await db.clear()
      this.activeDb = ''
    }
    return { ok: true }
  }

  // -- aliases (not supported in WASM) ---------------------------------

  aliases(): Promise<{ alias: string; database: string }[]> {
    return Promise.resolve([])
  }

  createAlias(_alias: string, _database: string): Promise<{ ok: boolean }> {
    return Promise.reject(new NotSupportedError('aliases'))
  }

  alterAlias(_alias: string, _database: string): Promise<{ ok: boolean }> {
    return Promise.reject(new NotSupportedError('aliases'))
  }

  dropAlias(_alias: string): Promise<{ ok: boolean }> {
    return Promise.reject(new NotSupportedError('aliases'))
  }

  // -- Import ---------------------------------------------------------

  async importCypher(cypher: string, database?: string): Promise<ImportResult> {
    if (database) await this.useDatabase(database)
    const db = this.requireDb()
    return db.importCypher(cypher) as Promise<ImportResult>
  }

  /** Parse a CSV blob client-side and synthesize Cypher `CREATE`
   *  statements against the WASM Cypher executor. Rows are batched
   *  into multi-pattern `CREATE` statements so a 10k-row import
   *  becomes ~50 statements rather than 10k worker round-trips.
   *
   *  Same `CsvImportResult` shape the HTTP server returns. */
  async importCsv(
    csv: string,
    label: string,
    columns: CsvColumnMapping[],
    delimiter?: string,
    database?: string,
  ): Promise<CsvImportResult> {
    if (database) await this.useDatabase(database)
    const db = this.requireDb()
    const t0 = performance.now()
    const errors: { row?: number; error: string }[] = []
    const beforeNodes = await db.nodeCount()
    let propertiesSet = 0
    let labelsAdded = 0

    const rows = parseCsv(csv, delimiter ?? ',')
    // First row is the header - column order in the CSV.
    if (rows.length < 2) {
      return {
        nodes_created: 0,
        properties_set: 0,
        labels_added: 0,
        errors: [{ error: 'CSV needs a header row and at least one data row.' }],
        time_ms: performance.now() - t0,
      }
    }
    const header = rows[0]
    const data = rows.slice(1)
    const colIndex = new Map(header.map((h, i) => [h, i]))

    const CHUNK = 200
    for (let i = 0; i < data.length; i += CHUNK) {
      const chunk = data.slice(i, i + CHUNK)
      const patterns: string[] = []
      for (let j = 0; j < chunk.length; j++) {
        const row = chunk[j]
        const props: string[] = []
        let rowOk = true
        for (const c of columns) {
          const idx = colIndex.get(c.name)
          if (idx === undefined) continue
          const raw = row[idx]
          if (raw === undefined || raw === '') continue
          try {
            const literal = csvCellToCypherLiteral(raw, c.type)
            props.push(`${cypherIdent(c.property)}: ${literal}`)
            propertiesSet++
          } catch (e) {
            errors.push({
              row: i + j + 2, // +2 for header + 1-indexed
              error: `column "${c.name}": ${(e as Error).message}`,
            })
            rowOk = false
            break
          }
        }
        if (rowOk) {
          patterns.push(`(:${cypherIdent(label)} {${props.join(', ')}})`)
        }
      }
      if (patterns.length > 0) {
        try {
          await db.importCypher(`CREATE ${patterns.join(', ')}`)
          labelsAdded += patterns.length
        } catch (e) {
          errors.push({
            row: i + 2,
            error: `batch failed: ${(e as Error).message}`,
          })
        }
      }
    }
    const created = (await db.nodeCount()) - beforeNodes
    return {
      nodes_created: created,
      properties_set: propertiesSet,
      labels_added: labelsAdded,
      errors,
      time_ms: performance.now() - t0,
    }
  }

  /** Edge import variant. Each row matches source + target nodes by
   *  the configured key column, then creates a relationship between
   *  them. Batched with `UNWIND` to amortize parse cost - falls back
   *  to per-row MATCH+CREATE if UNWIND surfaces a Cypher error. */
  async importCsvEdges(
    csv: string,
    edgeType: string,
    source: CsvEdgeImportSource,
    target: CsvEdgeImportSource,
    columns: CsvColumnMapping[],
    delimiter?: string,
    database?: string,
  ): Promise<CsvEdgeImportResult> {
    if (database) await this.useDatabase(database)
    const db = this.requireDb()
    const t0 = performance.now()
    const errors: { row?: number; error: string }[] = []
    const beforeEdges = await db.edgeCount()
    let propertiesSet = 0

    const rows = parseCsv(csv, delimiter ?? ',')
    if (rows.length < 2) {
      return {
        edges_created: 0,
        properties_set: 0,
        errors: [{ error: 'CSV needs a header row and at least one data row.' }],
        time_ms: performance.now() - t0,
      }
    }
    const header = rows[0]
    const data = rows.slice(1)
    const colIndex = new Map(header.map((h, i) => [h, i]))
    const srcIdx = colIndex.get(source.column)
    const tgtIdx = colIndex.get(target.column)
    if (srcIdx === undefined || tgtIdx === undefined) {
      return {
        edges_created: 0,
        properties_set: 0,
        errors: [{ error: `CSV is missing source or target column.` }],
        time_ms: performance.now() - t0,
      }
    }

    // Per-row MATCH+CREATE. Lookups should hit the existing index on
    // the source/target property if one exists; otherwise the user
    // pays a scan per row. Worth it for correctness over throughput
    // in the demo build.
    for (let i = 0; i < data.length; i++) {
      const row = data[i]
      const srcKey = row[srcIdx]
      const tgtKey = row[tgtIdx]
      if (srcKey === undefined || tgtKey === undefined || srcKey === '' || tgtKey === '') {
        continue
      }
      const props: string[] = []
      let rowOk = true
      for (const c of columns) {
        if (c.name === source.column || c.name === target.column) continue
        const idx = colIndex.get(c.name)
        if (idx === undefined) continue
        const raw = row[idx]
        if (raw === undefined || raw === '') continue
        try {
          const literal = csvCellToCypherLiteral(raw, c.type)
          props.push(`${cypherIdent(c.property)}: ${literal}`)
          propertiesSet++
        } catch (e) {
          errors.push({
            row: i + 2,
            error: `column "${c.name}": ${(e as Error).message}`,
          })
          rowOk = false
          break
        }
      }
      if (!rowOk) continue
      const propsLit = props.length > 0 ? ` {${props.join(', ')}}` : ''
      const cypher =
        `MATCH (a:${cypherIdent(source.label)} {${cypherIdent(source.property)}: ${csvCellToCypherLiteral(srcKey, 'string')}}), ` +
        `(b:${cypherIdent(target.label)} {${cypherIdent(target.property)}: ${csvCellToCypherLiteral(tgtKey, 'string')}}) ` +
        `CREATE (a)-[:${cypherIdent(edgeType)}${propsLit}]->(b)`
      try {
        await db.importCypher(cypher)
      } catch (e) {
        errors.push({ row: i + 2, error: (e as Error).message })
      }
    }
    return {
      edges_created: (await db.edgeCount()) - beforeEdges,
      properties_set: propertiesSet,
      errors,
      time_ms: performance.now() - t0,
    }
  }

  // -- Studio metadata (OPFS sidecar) ---------------------------------

  async savedQueries(): Promise<SavedQueryEntry[]> {
    if (!this.activeDb) return []
    const sidecar = await this.readSidecar(this.activeDb)
    return sidecar.savedQueries ?? []
  }

  async saveQuery(name: string, query: string): Promise<unknown> {
    if (!this.activeDb) throw new Error('no active database')
    const sidecar = await this.readSidecar(this.activeDb)
    const list = sidecar.savedQueries ?? []
    const existing = list.findIndex((e) => e.name === name)
    const entry: SavedQueryEntry = {
      name,
      query,
      createdAt: new Date().toISOString(),
    }
    if (existing >= 0) list[existing] = entry
    else list.unshift(entry)
    await this.writeSidecar(this.activeDb, { ...sidecar, savedQueries: list })
    return { ok: true }
  }

  async deleteSavedQuery(name: string): Promise<unknown> {
    if (!this.activeDb) return { ok: true }
    const sidecar = await this.readSidecar(this.activeDb)
    const list = (sidecar.savedQueries ?? []).filter((e) => e.name !== name)
    await this.writeSidecar(this.activeDb, { ...sidecar, savedQueries: list })
    return { ok: true }
  }

  async getStyles(database?: string): Promise<StylesData> {
    const target = database || this.activeDb
    if (!target) return { ...EMPTY_STYLES }
    const sidecar = await this.readSidecar(target)
    return sidecar.styles ?? { ...EMPTY_STYLES }
  }

  async putStyles(styles: StylesData, database?: string): Promise<unknown> {
    const target = database || this.activeDb
    if (!target) throw new Error('no active database')
    const sidecar = await this.readSidecar(target)
    await this.writeSidecar(target, { ...sidecar, styles })
    return { ok: true }
  }

  // -- User management / chat - HTTP-only -----------------------------

  users(): Promise<UserInfo[]> {
    return Promise.reject(new NotSupportedError('user management'))
  }

  createUser(_u: string, _p: string, _r: string): Promise<{ ok: boolean }> {
    return Promise.reject(new NotSupportedError('user management'))
  }

  updateUser(_u: string, _up: { password?: string; role?: string }): Promise<{ ok: boolean }> {
    return Promise.reject(new NotSupportedError('user management'))
  }

  deleteUser(_u: string): Promise<{ ok: boolean }> {
    return Promise.reject(new NotSupportedError('user management'))
  }

  // -- File management ------------------------------------------------
  //
  // In WASM mode there is no server-side filesystem; "files" are OPFS-
  // resident `.tvdb` blobs. `listFiles` mirrors `listDatabases` so the
  // existing FileManagerPanel renders the same way.

  async listFiles(): Promise<{ name: string; file: string; bytes: number; loaded: boolean }[]> {
    const entries = await this.requireDb().listDatabases()
    return entries.map((e) => ({
      name: e.name,
      file: `${e.name}.tvdb`,
      bytes: e.size,
      loaded: e.name === this.activeDb,
    }))
  }

  async deleteFile(name: string, kind: 'database' | 'model' = 'database'): Promise<{ ok: boolean }> {
    if (kind === 'model') {
      // Models aren't persisted in WASM mode - accept the call so the
      // shared FileManagerPanel UI doesn't break, but it's a no-op.
      return { ok: true }
    }
    await this.requireDb().deleteDatabase(name)
    this.known.delete(name)
    await this.deleteSidecar(name)
    if (this.activeDb === name) this.activeDb = ''
    return { ok: true }
  }

  /** Export the named database as a `.tvdb` file the user can save
   *  locally. If the requested database isn't the one currently loaded,
   *  we round-trip via OPFS to retrieve its bytes. */
  async downloadDatabase(name: string): Promise<void> {
    const db = this.requireDb()
    let bytes: Uint8Array
    if (name === this.activeDb) {
      bytes = await db.exportTvdb()
    } else {
      const prev = this.activeDb
      if (prev) await db.commit(prev)
      const r = await db.load(name)
      if (!r.ok) throw new Error(`database ${name} not found`)
      bytes = await db.exportTvdb()
      // Restore previously active database.
      if (prev) await db.load(prev)
      this.activeDb = prev
    }
    // Copy into a fresh ArrayBuffer-backed Uint8Array so the Blob
    // constructor accepts it - when threaded WASM is enabled the
    // exported bytes can be a SharedArrayBuffer view, which Blob
    // doesn't accept directly.
    const owned = new Uint8Array(bytes.byteLength)
    owned.set(bytes)
    const blob = new Blob([owned], { type: 'application/octet-stream' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `${name}.tvdb`
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
    URL.revokeObjectURL(url)
  }

  /** Load a `.tvdb` blob from the browser File API into OPFS as
   *  `<name>.tvdb`. Does NOT parse it into the engine - that happens
   *  when the caller follows up with `loadDatabase(name, ...)`, which
   *  matches the HTTP server's two-step shape and lets the file
   *  manager show "uploading" and "loading into memory" as separate
   *  phases (a 500 MB tvdb takes 1-2 min to parse, which would
   *  otherwise be invisible after the upload progress hits 100%).
   *
   *  Same binary format as the native server, so `.tvdb` files
   *  downloaded from a server deployment work unchanged. */
  async uploadDatabase(
    name: string,
    file: Blob,
    onProgress?: (loaded: number, total: number) => void,
  ): Promise<UploadResult> {
    const total = file.size
    onProgress?.(0, total)
    const buf = new Uint8Array(await file.arrayBuffer())
    onProgress?.(total, total)
    const db = this.requireDb()
    await db.writeBytes(name, buf)
    this.known.add(name)
    // `loaded: false` triggers the FileManagerPanel's `loadDatabase`
    // step, which surfaces the "Loading into memory..." UI while the
    // engine parses the (possibly large) `.tvdb`.
    return { ok: true, name, file: `${name}.tvdb`, bytes: buf.byteLength, loaded: false }
  }

  /** ML model persistence is server-only - there's no `traverse-ml`
   *  in the browser bundle. Reject with a clear message so the
   *  shared FileManagerPanel can render a useful error. */
  async uploadModel(_name: string, _file: Blob): Promise<UploadResult> {
    throw new Error('Model upload is only available on the hosted server, not in browser mode.')
  }
  async downloadModel(_name: string): Promise<void> {
    throw new Error('Model download is only available on the hosted server, not in browser mode.')
  }

  // -- Misc -----------------------------------------------------------

  metrics(): Promise<string> {
    return Promise.resolve('')
  }

  // -- OPFS sidecar JSON (styles + saved queries) ---------------------
  //
  // Sidecar = `<dbname>.studio.json` next to the binary `.tvdb` blob.
  // Async OPFS API works on the main thread and is fine for this small
  // payload; sync access handles are only required for big files.

  private async sidecarHandle(name: string, create: boolean): Promise<FileSystemFileHandle | null> {
    if (!('storage' in navigator) || !navigator.storage.getDirectory) return null
    const root = await navigator.storage.getDirectory()
    const dir = await root.getDirectoryHandle('traverse', { create: true })
    try {
      return await dir.getFileHandle(`${this.sanitize(name)}.studio.json`, { create })
    } catch {
      return null
    }
  }

  private sanitize(name: string): string {
    return name.replace(/[/\\.]/g, '_')
  }

  private async readSidecar(name: string): Promise<DatabaseSidecar> {
    const handle = await this.sidecarHandle(name, false)
    if (!handle) return {}
    try {
      const file = await handle.getFile()
      if (file.size === 0) return {}
      return JSON.parse(await file.text()) as DatabaseSidecar
    } catch {
      return {}
    }
  }

  private async writeSidecar(name: string, data: DatabaseSidecar): Promise<void> {
    const handle = await this.sidecarHandle(name, true)
    if (!handle) return
    const w = await handle.createWritable()
    try {
      await w.write(JSON.stringify(data))
    } finally {
      await w.close()
    }
  }

  private async deleteSidecar(name: string): Promise<void> {
    if (!('storage' in navigator) || !navigator.storage.getDirectory) return
    const root = await navigator.storage.getDirectory()
    const dir = await root.getDirectoryHandle('traverse', { create: true })
    try {
      await dir.removeEntry(`${this.sanitize(name)}.studio.json`)
    } catch {
      // Already gone - fine.
    }
  }
}
