// Public types for @truespar/traverse-wasm. `index.js` is JSDoc-typed; this `.d.ts` restates the
// surface in plain TypeScript for consumers that don't checkJs third-party code.

export interface QueryStats {
  nodes_created: number
  nodes_deleted: number
  relationships_created: number
  relationships_deleted: number
  properties_set: number
  labels_added: number
  labels_removed: number
  indexes_added: number
  indexes_removed: number
  constraints_added: number
  constraints_removed: number
}

export interface QueryResponse {
  columns: string[]
  rows: unknown[][]
  total_rows: number
  truncated: boolean
  nodes: unknown[]
  edges: unknown[]
  entities_truncated: boolean
  /** Entities the query's patterns BOUND, returned or not - the provenance
   *  behind a projection like `RETURN t.amount`, hydrated to the same shape
   *  as `nodes`/`edges`. A graph view highlights these instead of guessing
   *  from the rows. Superset of the returned entities; dedupe by id. */
  bound_nodes: unknown[]
  bound_edges: unknown[]
  /** Read, Write, ReadWrite or Schema, from the executor's classification. */
  query_type: 'Read' | 'Write' | 'ReadWrite' | 'Schema'
  stats: QueryStats
  time_ms: number
}

export type GdsCategory =
  | 'centrality'
  | 'community'
  | 'paths'
  | 'dag'
  | 'similarity'
  | 'linkPrediction'
  | 'embeddings'

export type GdsOutputKind =
  | 'nodeScalar'
  | 'nodeCommunity'
  | 'nodeMulti'
  | 'nodeVector'
  | 'path'
  | 'nodePair'
  | 'entitySet'
  | 'statsOnly'

export type GdsStability = 'stable' | 'beta' | 'experimental'

export type GdsConfigParamType =
  | 'bool'
  | 'integer'
  | 'float'
  | 'string'
  | 'enum'
  | 'stringList'

export interface GdsConfigParam {
  key: string
  type: GdsConfigParamType
  default: unknown
  description: string
  advanced: boolean
}

export interface GdsAlgorithm {
  name: string
  displayName: string
  category: GdsCategory
  outputKind: GdsOutputKind
  stability: GdsStability
  description: string
  writable: boolean
  modes: ('stream' | 'stats' | 'write')[]
  configSchema: GdsConfigParam[]
  outputColumns: string[]
  statsColumns: string[]
  writeColumns: string[]
}

export interface AlgorithmsResponse {
  algorithms: GdsAlgorithm[]
}

export interface PropertyKeyInfo {
  name: string
  type: string
}

export interface OwnerProperties {
  name: string
  properties: PropertyKeyInfo[]
}

export interface SchemaResponse {
  labels: string[]
  relationship_types: string[]
  property_keys: PropertyKeyInfo[]
  indexes: unknown[]
  constraints: unknown[]
  labels_detail: OwnerProperties[]
  relationship_types_detail: OwnerProperties[]
}

export interface ImportResult {
  total_statements: number
  executed: number
  errors: { statement: string; error: string }[]
  stats: QueryStats
  time_ms: number
}

export interface TraverseDbOpenOptions {
  /** wasm-bindgen-rayon pool size. Default `Math.min(navigator.hardwareConcurrency, 16)`. */
  numThreads?: number
  /** Worker script URL override; rarely needed. */
  workerUrl?: URL | string
  /**
   * OPFS database to restore on open; absent -> empty. If it doesn't exist yet, opens empty;
   * populate, then `commit(name)`.
   */
  name?: string
}

export interface DatabaseListEntry {
  name: string
  size: number
  lastModified: number
}

export interface CallOptions {
  /** Cancels queued work before dispatch. For active synchronous WASM, only
   * the caller is cancelled; AbortError.executionMayHaveCompleted is true.
   * This is not a rollback or a physical interruption guarantee. */
  signal?: AbortSignal
}

export interface QueryOptions extends CallOptions {
  /** Cooperative execution deadline, checked at executor boundaries.
   * Defaults to 10,000 ms; capped at 30,000 ms. Not a hard wall-clock limit. */
  timeoutMs?: number
  /** Query dialect: `'gql'` uses the ISO/IEC 39075 parser (same engine); default openCypher. */
  dialect?: 'cypher' | 'gql'
}

/** In-tab Traverse engine in a dedicated Web Worker, so the main thread never blocks on Cypher. */
export class TraverseDb {
  /** Bounded admission: 32 requests / 256 MiB estimated payloads, one active.
   * Includes a caller-cancelled active request until its worker reply. */
  readonly queueStats: { active: number; queued: number; bytes: number; closed: boolean }
  /** Library version reported by the worker after init. */
  readonly version: string | null

  /**
   * Opens a database: spawns a worker, initializes the WASM module + thread pool, and restores the
   * OPFS database `options.name` if set.
   */
  static open(options?: TraverseDbOpenOptions): Promise<TraverseDb>

  /** Lists OPFS databases without opening a TraverseDb (a short-lived worker). */
  static listDatabases(options?: { workerUrl?: URL | string }): Promise<DatabaseListEntry[]>

  /** Deletes an OPFS database without opening it; no-op if absent. */
  static deleteDatabase(name: string, options?: { workerUrl?: URL | string }): Promise<void>

  /** Runs a Cypher query; returns the standard QueryResponse. */
  query(
    cypher: string,
    params?: Record<string, unknown> | null,
    options?: QueryOptions,
  ): Promise<QueryResponse>

  /** Algorithm catalog metadata, the HTTP `/api/algorithms` shape. */
  algorithms(): Promise<AlgorithmsResponse>

  /**
   * Schema summary: labels, relationship types, property keys, per-label / per-edge-type
   * properties, indexes, constraints; the HTTP `/api/schema` shape.
   */
  schema(): Promise<SchemaResponse>

  /**
   * Runs a Cypher import (multi-statement OK); returns the HTTP `/api/import` shape (total /
   * executed / errors / stats / time_ms). Prefer it over `query()` for import panels.
   */
  importCypher(cypher: string): Promise<ImportResult>

  /**
   * Splits a script into statements like the server's import endpoint and the
   * CLI: semicolons inside string literals and line comments aren't separators,
   * so `{name: 'Smith; Jones'}` survives. `query()` takes exactly one
   * statement; use this to cut up a pasted script.
   */
  static splitStatements(script: string): string[]

  /** Resets to an empty graph, dropping indexes and properties. */
  clear(): Promise<void>

  /** Current node count. */
  nodeCount(): Promise<number>

  /** Current edge count. */
  edgeCount(): Promise<number>

  /** Node + edge counts in one round trip. */
  stats(): Promise<{ nodes: number; edges: number }>

  /**
   * Approximate in-memory size of the graph in bytes (node + edge property + index storage), like
   * the server's per-DB `memory_bytes`; excludes WASM linear memory and the JS heap.
   */
  estimatedMemory(): Promise<number>

  /** Persists the current graph to OPFS under `name`. */
  commit(name: string): Promise<{ bytes: number }>

  /**
   * Writes a `.tvdb` buffer to OPFS without loading it; `load(name)` parses it. Lets large uploads
   * show separate "uploading" / "loading" phases.
   */
  writeBytes(name: string, bytes: Uint8Array): Promise<{ ok: true; bytes: number }>

  /** Restores the graph from OPFS under `name`; `{ ok: false, missing: true }` if absent. */
  load(name: string): Promise<{ ok: boolean; missing?: boolean; nodes?: number; edges?: number }>

  /** OPFS databases for this origin. */
  listDatabases(): Promise<DatabaseListEntry[]>

  /** Deletes a database from OPFS. */
  deleteDatabase(name: string): Promise<void>

  /**
   * Exports the graph as a `.tvdb` buffer in the native server's on-disk format (round-trips with
   * native `Database::open`), for downloads or cross-origin transfer; same-origin persistence is
   * `commit()` (OPFS).
   */
  exportTvdb(): Promise<Uint8Array>

  /** Replaces the graph with one from a `.tvdb` buffer. */
  loadTvdb(bytes: Uint8Array): Promise<{ ok: boolean; nodes?: number; edges?: number }>

  /** Tears down the worker; pending requests reject with AbortError. */
  close(): void
}

/** Cheap accessor for the library version. Returns null until at least
 *  one TraverseDb has been opened in this realm. */
export function version(): string | null
