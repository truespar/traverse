// Studio data contract: every store and panel reaches the backend through the
// `ApiClient` interface below. `HttpApiClient` (the default) talks to
// `traverse-server` over the JSON HTTP API on port 7691; `WasmApiClient`
// satisfies the same interface against an in-process engine in a Web Worker.
// Studio doesn't know which backend it's talking to.
//
// The singleton `api` is the live binding the stores use; `setApiClient` at
// boot swaps in another backend.

// -- Types -----------------------------------------------------------

export interface HealthResponse {
  status: string
  version: string
  uptime_secs: number
  memory_bytes: number
  databases: DatabaseInfo[]
  allow_drop_database?: boolean
  auth_required?: boolean
  /** Compile-time feature flags - used by Studio to hide UI for
   *  things this server can't do (e.g. `gql: false` removes the
   *  GQL option from the dialect picker). */
  features?: Record<string, boolean>
}

export interface DatabaseInfo {
  name: string
  default: boolean
  nodes: number
  edges: number
  memory_bytes: number
  readOnly?: boolean
  aliases?: string[]
}

export interface PropertyKeyInfo {
  name: string
  type: string
}

export interface LabelDetail {
  name: string
  properties: PropertyKeyInfo[]
}

export interface RelationshipTypeDetail {
  name: string
  properties: PropertyKeyInfo[]
}

export interface SchemaResponse {
  labels: string[]
  relationship_types: string[]
  property_keys: PropertyKeyInfo[]
  indexes: IndexInfo[]
  constraints: ConstraintInfo[]
  labels_detail?: LabelDetail[]
  relationship_types_detail?: RelationshipTypeDetail[]
}

export interface IndexInfo {
  name: string
  label: string
  property: string
  entries: number
}

export interface ConstraintInfo {
  name: string
  type: string
  label: string
  property: string
}

// -- Graph Data Science (GDS) ----------------------------------------

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

export type GdsConfigParamType = 'bool' | 'integer' | 'float' | 'string' | 'enum' | 'stringList'

export interface GdsConfigParam {
  key: string
  type: GdsConfigParamType
  default: unknown
  description: string
  advanced: boolean
  /**
   * `true`: the algorithm fails without this value, and the GDS drawer disables Run until every
   * required field is filled. Absent means `false`.
   */
  required?: boolean
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

export type GdsMode = 'stream' | 'stats' | 'write'

/** Query dialect - openCypher is the default. `gql` routes through
 *  ISO/IEC 39075 parsing on the server (requires the `gql` feature). */
export type QueryDialect = 'cypher' | 'gql'

export interface QueryResponse {
  columns: string[]
  rows: unknown[][]
  total_rows: number
  truncated: boolean
  nodes?: unknown[]
  edges?: unknown[]
  entities_truncated?: boolean
  query_type: string
  stats: QueryStats
  /**
   * Logical plan tree for `EXPLAIN` / `PROFILE`. The HTTP server doesn't send it, WasmApiClient
   * does; Studio's JSON tab renders it when present.
   */
  plan?: unknown
  time_ms: number
  parsing_time_ms?: number
  planning_time_ms?: number
  execution_time_ms?: number
}

export interface QueryStats {
  nodes_created: number
  nodes_deleted: number
  relationships_created: number
  relationships_deleted: number
  properties_set: number
  labels_added: number
  labels_removed: number
}

export interface TaskInfo {
  id: string
  operation: string
  status: 'pending' | 'completed' | 'failed'
  result: unknown | null
  error: string | null
  createdAt: string
  finishedAt: string | null
}

export interface UserInfo {
  username: string
  role: string
}

export type StyleRuleOp = '=' | '!=' | '>' | '<' | '>=' | '<=' | 'contains' | 'starts_with' | 'exists' | 'not_exists'

export interface StyleRule {
  property: string
  op: StyleRuleOp
  value?: string | number | boolean
  style: Partial<NodeStyle> | Partial<EdgeStyle>
}

export interface NodeStyle {
  color?: string
  size?: number
  program?: 'circle' | 'hexagon' | 'pulse'
  rules?: StyleRule[]
}

export interface EdgeStyle {
  color?: string
  thickness?: number
  rules?: StyleRule[]
}

export interface StylesData {
  nodes: Record<string, NodeStyle>
  edges: Record<string, EdgeStyle>
}

export interface ImportResult {
  total_statements: number
  executed: number
  errors: { statement: string; error: string }[]
  stats: QueryStats
  time_ms: number
}

export interface SavedQueryEntry {
  name: string
  query: string
  createdAt: string
}

export interface CsvColumnMapping {
  name: string
  property: string
  type: 'string' | 'integer' | 'float' | 'boolean'
}

export interface CsvImportResult {
  nodes_created: number
  properties_set: number
  labels_added: number
  errors: { row?: number; error: string }[]
  time_ms: number
}

export interface CsvEdgeImportSource {
  label: string
  column: string
  property: string
}

export interface CsvEdgeImportResult {
  edges_created: number
  properties_set: number
  errors: { row?: number; error: string }[]
  time_ms: number
}

export interface UploadResult {
  ok: boolean
  name: string
  file?: string
  bytes: number
  loaded?: boolean
}

export interface AiChatMessage {
  role: 'user' | 'assistant'
  content: string
}

export interface AiChatMetadata {
  graphSchema: string
  /** The editor's query language, so the assistant answers in it. */
  queryDialect?: QueryDialect
  currentQuery?: string
  lastQueryTimeMs?: number
  lastQueryPlanningMs?: number
  lastQueryExecutionMs?: number
  /** Live GDS algorithm catalog (compact text form). Sent so the AI
   * can recommend exact algorithm names + supported modes without
   * hallucinating. Mirrors how `graphSchema` is passed. */
  gdsAlgorithms?: string
}

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message)
    this.name = 'ApiError'
  }
}

// -- Auth helpers ----------------------------------------------------
//
// API-key handling is HTTP-specific but lives at module scope because
// `AuthGate`, `AppHeader`, and the connection store reach for it
// directly (without going through `api`). The WASM build is single-
// user and ignores these entirely.

const API_KEY_STORAGE_KEY = 'tv-studio-api-key'

export function getApiKey(): string | null {
  return localStorage.getItem(API_KEY_STORAGE_KEY)
}

export function setApiKey(key: string): void {
  localStorage.setItem(API_KEY_STORAGE_KEY, key)
}

export function clearApiKey(): void {
  localStorage.removeItem(API_KEY_STORAGE_KEY)
}

type AuthFailedHandler = () => void
let onAuthFailed: AuthFailedHandler | null = null

export function setAuthFailedHandler(handler: AuthFailedHandler): void {
  onAuthFailed = handler
}

// -- ApiClient interface ---------------------------------------------
//
// Every Studio store / panel calls into this. The HTTP implementation is
// below, the WASM one in its own file. Capability flags let panels render
// features only one backend supports (e.g. user management is HTTP-only; the
// WASM build is single-user).

export interface ApiCapabilities {
  /** Server supports the user-management endpoints (auth-enabled HTTP). */
  users: boolean
  /** Server can stream AI-chat responses. */
  aiChat: boolean
  /** Persistence is server-managed (.tvdb files on the server's
   *  filesystem, listable / uploadable / downloadable over HTTP).
   *  When false, persistence is local (OPFS in the WASM build). */
  serverFiles: boolean
  /** Server emits Prometheus-style metrics on `/metrics`. */
  metrics: boolean
  /** A running query stops when its request is aborted (the server cancels it). */
  cancelQuery: boolean
}

export interface ApiClient {
  readonly capabilities: ApiCapabilities

  // Health / catalog
  health(): Promise<HealthResponse>
  schema(database?: string): Promise<SchemaResponse>
  databases(): Promise<{ name: string; default: boolean }[]>
  algorithms(): Promise<AlgorithmsResponse>

  // Query execution. `dialect` defaults to `'cypher'`; pass `'gql'`
  // to route through the ISO/IEC 39075 parser on the server (requires
  // the server to be built with `--features gql`).
  query(
    cypher: string,
    parameters?: Record<string, unknown>,
    database?: string,
    dialect?: QueryDialect,
    signal?: AbortSignal,
  ): Promise<QueryResponse>
  runAlgorithm(
    algorithm: GdsAlgorithm,
    mode: GdsMode,
    config: Record<string, unknown>,
    database?: string,
  ): Promise<QueryResponse>

  // Database management
  createDatabase(name: string): Promise<{ ok: boolean; name: string }>
  dropDatabase(name: string): Promise<unknown>
  renameDatabase(oldName: string, newName: string): Promise<{ ok: boolean; name: string }>
  save(): Promise<{ status: string }>
  loadDatabase(name: string, file: string): Promise<{ ok: boolean; name: string; readOnly: boolean }>
  unloadDatabase(name: string): Promise<{ ok: boolean }>

  // Aliases
  aliases(): Promise<{ alias: string; database: string }[]>
  createAlias(alias: string, database: string): Promise<{ ok: boolean }>
  alterAlias(alias: string, database: string): Promise<{ ok: boolean }>
  dropAlias(alias: string): Promise<{ ok: boolean }>

  // Import
  importCypher(cypher: string, database?: string): Promise<ImportResult>
  importCsv(
    csv: string,
    label: string,
    columns: CsvColumnMapping[],
    delimiter?: string,
    database?: string,
  ): Promise<CsvImportResult>
  importCsvEdges(
    csv: string,
    edgeType: string,
    source: CsvEdgeImportSource,
    target: CsvEdgeImportSource,
    columns: CsvColumnMapping[],
    delimiter?: string,
    database?: string,
  ): Promise<CsvEdgeImportResult>

  // Studio metadata
  savedQueries(): Promise<SavedQueryEntry[]>
  saveQuery(name: string, query: string): Promise<unknown>
  deleteSavedQuery(name: string): Promise<unknown>
  getStyles(database?: string): Promise<StylesData>
  putStyles(styles: StylesData, database?: string): Promise<unknown>

  // User management (HTTP-only; WASM build throws NotSupportedError)
  users(): Promise<UserInfo[]>
  createUser(username: string, password: string, role: string): Promise<{ ok: boolean }>
  updateUser(username: string, updates: { password?: string; role?: string }): Promise<{ ok: boolean }>
  deleteUser(username: string): Promise<{ ok: boolean }>

  // File management
  listFiles(): Promise<
    { name: string; file: string; bytes: number; loaded: boolean; kind?: 'database' | 'model' }[]
  >
  /** Delete a `.tvdb` (`kind: 'database'`, default) or a `.tvmodel`
   *  (`kind: 'model'`). The server routes by extension internally. */
  deleteFile(name: string, kind?: 'database' | 'model'): Promise<{ ok: boolean }>
  downloadDatabase(name: string): Promise<void>
  /** Download a `.tvmodel` file. Same endpoint as
   *  `downloadDatabase`, gated by `?kind=model`. */
  downloadModel(name: string): Promise<void>
  uploadDatabase(
    name: string,
    file: Blob,
    onProgress?: (loaded: number, total: number) => void,
  ): Promise<UploadResult>
  /** Upload a `.tvmodel` file. Routes through the same Files
   *  endpoint as `uploadDatabase` (PUT /api/files/{name}) but with
   *  `?kind=model`. On success the server has both written the file
   *  to disk and registered it in the live ML catalog. */
  uploadModel(
    name: string,
    file: Blob,
    onProgress?: (loaded: number, total: number) => void,
  ): Promise<UploadResult>

  // Misc
  metrics(): Promise<string>
}

/** Thrown by `ApiClient` implementations when a feature is not
 *  supported on the active backend. Panels should consult
 *  `api.capabilities` to gate UI; this is the runtime safety net. */
export class NotSupportedError extends Error {
  constructor(feature: string) {
    super(`${feature} is not supported by the active backend`)
    this.name = 'NotSupportedError'
  }
}

// -- Cypher helpers --------------------------------------------------
//
// Shared by every ApiClient implementation - both HTTP and WASM use
// the same CALL syntax for GDS, so the call-builder lives here.

/** Serialize a JS value into a Cypher literal. Used by
 * `buildAlgorithmCall` to inline the config map into the CALL
 * statement. Mirrors the limited value shapes the algorithm config
 * supports - primitives + string lists. `null` / `undefined` are
 * skipped at the caller; everything else falls back to a JSON string. */
function cypherLiteral(value: unknown): string {
  if (value === null || value === undefined) return 'null'
  if (typeof value === 'boolean') return value ? 'true' : 'false'
  if (typeof value === 'number') return Number.isFinite(value) ? String(value) : 'null'
  if (typeof value === 'string') return JSON.stringify(value)
  if (Array.isArray(value)) return `[${value.map(cypherLiteral).join(', ')}]`
  if (typeof value === 'object') {
    const entries = Object.entries(value as Record<string, unknown>)
      .filter(([, v]) => v !== undefined)
      .map(([k, v]) => `${k}: ${cypherLiteral(v)}`)
    return `{${entries.join(', ')}}`
  }
  return JSON.stringify(String(value))
}

function columnsForMode(algorithm: GdsAlgorithm, mode: GdsMode): string[] {
  switch (mode) {
    case 'stream':
      return algorithm.outputColumns
    case 'stats':
      return algorithm.statsColumns
    case 'write':
      return algorithm.writeColumns
  }
}

/** Build a Cypher CALL statement for a GDS algorithm. Both backends
 *  reuse this so the same Cypher hits the same procedure on either
 *  the server or the in-process WASM engine. */
export function buildAlgorithmCall(
  algorithm: GdsAlgorithm,
  mode: GdsMode,
  config: Record<string, unknown>,
): string {
  const configMap = cypherLiteral(config ?? {})
  const procName = `traverse.${algorithm.name}.${mode}`
  const cols = columnsForMode(algorithm, mode)
  if (cols.length > 0) {
    const cs = cols.join(', ')
    return `CALL ${procName}(${configMap}) YIELD ${cs} RETURN ${cs}`
  }
  return `CALL ${procName}(${configMap})`
}

// -- HTTP request plumbing -------------------------------------------

async function request<T>(url: string, options?: RequestInit): Promise<T> {
  const apiKey = getApiKey()
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...(options?.headers as Record<string, string> | undefined),
  }
  if (apiKey) headers['X-API-Key'] = apiKey

  const res = await fetch(url, {
    ...options,
    headers,
  })

  if (!res.ok) {
    if (res.status === 401) {
      clearApiKey()
      onAuthFailed?.()
    }
    let message = res.statusText
    try {
      const body = await res.json()
      if (body.error) message = body.error
    } catch { /* ignore */ }
    throw new ApiError(res.status, message)
  }

  return res.json()
}

async function pollTask(taskId: string): Promise<TaskInfo> {
  let delay = 200
  const maxDelay = 2000
  while (true) {
    const task = await request<TaskInfo>(`/api/tasks/${encodeURIComponent(taskId)}`)
    if (task.status === 'completed' || task.status === 'failed') {
      return task
    }
    await new Promise((r) => setTimeout(r, delay))
    delay = Math.min(delay * 2, maxDelay)
  }
}

/** Shared GET-and-save helper used by `downloadDatabase` and
 *  `downloadModel`. Routes by `kind` query param so the same
 *  endpoint serves both file types. */
async function downloadFile(name: string, kind: 'database' | 'model'): Promise<void> {
  const apiKey = getApiKey()
  const headers: Record<string, string> = {}
  if (apiKey) headers['X-API-Key'] = apiKey

  const qs = kind === 'model' ? '?kind=model' : ''
  const res = await fetch(`/api/files/${encodeURIComponent(name)}${qs}`, { headers })
  if (!res.ok) {
    if (res.status === 401) {
      clearApiKey()
      onAuthFailed?.()
    }
    let message = res.statusText
    try {
      const body = await res.json()
      if (body.error) message = body.error
    } catch { /* ignore */ }
    throw new ApiError(res.status, message)
  }
  const ext = kind === 'model' ? 'tvmodel' : 'tvdb'
  const blob = await res.blob()
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = `${name}.${ext}`
  document.body.appendChild(a)
  a.click()
  document.body.removeChild(a)
  URL.revokeObjectURL(url)
}

/** Shared PUT-with-progress helper for the `.tvdb` and `.tvmodel`
 *  upload paths. The same `/api/files/{name}` endpoint serves both
 * - `kind` is routed via query param so we don't fragment the
 *  Studio API. */
function uploadFile(
  name: string,
  file: Blob,
  kind: 'database' | 'model',
  onProgress?: (loaded: number, total: number) => void,
): Promise<UploadResult> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest()
    const qs = kind === 'model' ? '?kind=model' : ''
    xhr.open('PUT', `/api/files/${encodeURIComponent(name)}${qs}`)
    xhr.setRequestHeader('Content-Type', 'application/octet-stream')
    const apiKey = getApiKey()
    if (apiKey) xhr.setRequestHeader('X-API-Key', apiKey)

    if (onProgress) {
      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable) onProgress(e.loaded, e.total)
      }
    }

    let settled = false
    xhr.onreadystatechange = () => {
      if (settled) return
      if (xhr.readyState >= 2 && xhr.status >= 400) {
        settled = true
        xhr.abort()
        let message = xhr.statusText
        try {
          const body = JSON.parse(xhr.responseText)
          if (body.error) message = body.error
        } catch { /* not yet fully received */ }
        reject(new ApiError(xhr.status, message))
      }
    }

    const ext = kind === 'model' ? 'tvmodel' : 'tvdb'
    xhr.onload = () => {
      if (settled) return
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          resolve(JSON.parse(xhr.responseText))
        } catch {
          resolve({ ok: true, name, file: `${name}.${ext}`, bytes: file.size })
        }
      } else {
        if (xhr.status === 401) {
          clearApiKey()
          onAuthFailed?.()
        }
        let message = xhr.statusText
        try {
          const body = JSON.parse(xhr.responseText)
          if (body.error) message = body.error
        } catch { /* ignore */ }
        reject(new ApiError(xhr.status, message))
      }
    }

    xhr.onerror = () => {
      if (settled) return
      settled = true
      let message = 'Upload failed - connection error'
      try {
        if (xhr.responseText) {
          const body = JSON.parse(xhr.responseText)
          if (body.error) message = body.error
        }
      } catch { /* ignore */ }
      reject(new ApiError(xhr.status || 0, message))
    }
    xhr.send(file)
  })
}

async function requestAsync<T>(url: string, options?: RequestInit): Promise<T> {
  const apiKey = getApiKey()
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...(options?.headers as Record<string, string> | undefined),
  }
  if (apiKey) headers['X-API-Key'] = apiKey

  const res = await fetch(url, { ...options, headers })

  if (res.status === 202) {
    const { taskId } = (await res.json()) as { taskId: string }
    const task = await pollTask(taskId)
    if (task.status === 'failed') {
      throw new ApiError(500, task.error ?? 'async task failed')
    }
    return task.result as T
  }

  if (!res.ok) {
    if (res.status === 401) {
      clearApiKey()
      onAuthFailed?.()
    }
    let message = res.statusText
    try {
      const body = await res.json()
      if (body.error) message = body.error
    } catch { /* ignore */ }
    throw new ApiError(res.status, message)
  }

  return res.json()
}

// -- HttpApiClient ---------------------------------------------------

/** ApiClient backed by `traverse-server` over JSON HTTP. This is the
 *  default backend for the bundled Studio served on port 7691. */
export class HttpApiClient implements ApiClient {
  readonly capabilities: ApiCapabilities = {
    users: true,
    aiChat: true,
    serverFiles: true,
    metrics: true,
    cancelQuery: true,
  }

  health(): Promise<HealthResponse> {
    return request('/api/health')
  }

  schema(database?: string): Promise<SchemaResponse> {
    const params = database ? `?database=${encodeURIComponent(database)}` : ''
    return request(`/api/schema${params}`)
  }

  query(
    cypher: string,
    parameters?: Record<string, unknown>,
    database?: string,
    dialect?: QueryDialect,
    signal?: AbortSignal,
  ): Promise<QueryResponse> {
    // Only attach the header when GQL is selected - keeps default
    // requests byte-identical to pre-dialect builds, so a downgrade
    // of the server doesn't surprise.
    const headers = dialect === 'gql' ? { 'X-Query-Dialect': 'gql' } : undefined
    // aborting the request stops the query: the server cancels a statement whose client left
    return request('/api/query', {
      method: 'POST',
      headers,
      signal,
      body: JSON.stringify({
        query: cypher,
        database,
        parameters,
      }),
    })
  }

  metrics(): Promise<string> {
    return fetch('/metrics').then((r) => r.text())
  }

  algorithms(): Promise<AlgorithmsResponse> {
    return request('/api/algorithms')
  }

  runAlgorithm(
    algorithm: GdsAlgorithm,
    mode: GdsMode,
    config: Record<string, unknown>,
    database?: string,
  ): Promise<QueryResponse> {
    const cypher = buildAlgorithmCall(algorithm, mode, config)
    return this.query(cypher, undefined, database)
  }

  databases(): Promise<{ name: string; default: boolean }[]> {
    return request('/api/databases')
  }

  createDatabase(name: string): Promise<{ ok: boolean; name: string }> {
    return request('/api/databases', {
      method: 'POST',
      body: JSON.stringify({ name }),
    })
  }

  dropDatabase(name: string): Promise<unknown> {
    return requestAsync(`/api/databases/${encodeURIComponent(name)}?async=true`, {
      method: 'DELETE',
    })
  }

  renameDatabase(oldName: string, newName: string): Promise<{ ok: boolean; name: string }> {
    return request(`/api/databases/${encodeURIComponent(oldName)}/rename`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: newName }),
    })
  }

  save(): Promise<{ status: string }> {
    return requestAsync('/api/save?async=true', { method: 'POST' })
  }

  loadDatabase(name: string, file: string): Promise<{ ok: boolean; name: string; readOnly: boolean }> {
    return requestAsync('/api/databases/load?async=true', {
      method: 'POST',
      body: JSON.stringify({ name, file }),
    })
  }

  unloadDatabase(name: string): Promise<{ ok: boolean }> {
    return requestAsync(`/api/databases/${encodeURIComponent(name)}/unload?async=true`, {
      method: 'POST',
    })
  }

  aliases(): Promise<{ alias: string; database: string }[]> {
    return request('/api/aliases')
  }

  createAlias(alias: string, database: string): Promise<{ ok: boolean }> {
    return request('/api/aliases', {
      method: 'POST',
      body: JSON.stringify({ alias, database }),
    })
  }

  alterAlias(alias: string, database: string): Promise<{ ok: boolean }> {
    return request(`/api/aliases/${encodeURIComponent(alias)}`, {
      method: 'PUT',
      body: JSON.stringify({ database }),
    })
  }

  dropAlias(alias: string): Promise<{ ok: boolean }> {
    return request(`/api/aliases/${encodeURIComponent(alias)}`, {
      method: 'DELETE',
    })
  }

  importCypher(cypher: string, database?: string): Promise<ImportResult> {
    return request('/api/import', {
      method: 'POST',
      body: JSON.stringify({ cypher, database }),
    })
  }

  importCsv(
    csv: string,
    label: string,
    columns: CsvColumnMapping[],
    delimiter?: string,
    database?: string,
  ): Promise<CsvImportResult> {
    return request('/api/import/csv', {
      method: 'POST',
      body: JSON.stringify({ csv, label, columns, delimiter, skip_header: true, database }),
    })
  }

  importCsvEdges(
    csv: string,
    edgeType: string,
    source: CsvEdgeImportSource,
    target: CsvEdgeImportSource,
    columns: CsvColumnMapping[],
    delimiter?: string,
    database?: string,
  ): Promise<CsvEdgeImportResult> {
    return request('/api/import/csv/edges', {
      method: 'POST',
      body: JSON.stringify({
        csv,
        edge_type: edgeType,
        source,
        target,
        columns,
        delimiter,
        skip_header: true,
        database,
      }),
    })
  }

  savedQueries(): Promise<SavedQueryEntry[]> {
    return request('/api/studio/queries')
  }

  saveQuery(name: string, query: string): Promise<unknown> {
    return request('/api/studio/queries', {
      method: 'POST',
      body: JSON.stringify({ name, query }),
    })
  }

  deleteSavedQuery(name: string): Promise<unknown> {
    return request(`/api/studio/queries/${encodeURIComponent(name)}`, {
      method: 'DELETE',
    })
  }

  getStyles(database?: string): Promise<StylesData> {
    const params = database ? `?database=${encodeURIComponent(database)}` : ''
    return request(`/api/studio/styles${params}`)
  }

  putStyles(styles: StylesData, database?: string): Promise<unknown> {
    const params = database ? `?database=${encodeURIComponent(database)}` : ''
    return request(`/api/studio/styles${params}`, {
      method: 'PUT',
      body: JSON.stringify(styles),
    })
  }

  users(): Promise<UserInfo[]> {
    return request('/api/users')
  }

  createUser(username: string, password: string, role: string): Promise<{ ok: boolean }> {
    return request('/api/users', {
      method: 'POST',
      body: JSON.stringify({ username, password, role }),
    })
  }

  updateUser(username: string, updates: { password?: string; role?: string }): Promise<{ ok: boolean }> {
    return request(`/api/users/${encodeURIComponent(username)}`, {
      method: 'PUT',
      body: JSON.stringify(updates),
    })
  }

  deleteUser(username: string): Promise<{ ok: boolean }> {
    return request(`/api/users/${encodeURIComponent(username)}`, {
      method: 'DELETE',
    })
  }

  listFiles(): Promise<{ name: string; file: string; bytes: number; loaded: boolean }[]> {
    return request('/api/files')
  }

  deleteFile(name: string, kind: 'database' | 'model' = 'database'): Promise<{ ok: boolean }> {
    const qs = kind === 'model' ? '?kind=model' : ''
    return request(`/api/files/${encodeURIComponent(name)}${qs}`, {
      method: 'DELETE',
    })
  }

  async downloadDatabase(name: string): Promise<void> {
    return downloadFile(name, 'database')
  }

  async downloadModel(name: string): Promise<void> {
    return downloadFile(name, 'model')
  }

  uploadDatabase(
    name: string,
    file: Blob,
    onProgress?: (loaded: number, total: number) => void,
  ): Promise<UploadResult> {
    return uploadFile(name, file, 'database', onProgress)
  }

  uploadModel(
    name: string,
    file: Blob,
    onProgress?: (loaded: number, total: number) => void,
  ): Promise<UploadResult> {
    return uploadFile(name, file, 'model', onProgress)
  }
}

// -- Singleton -------------------------------------------------------
//
// `api` is a live binding - ES modules expose `let` exports such that
// consumers see the current value at every property access. The boot
// path can call `setApiClient(new WasmApiClient(...))` before any
// stores are constructed to swap backends without code churn at the
// call sites.

export let api: ApiClient = new HttpApiClient()

export function setApiClient(client: ApiClient): void {
  api = client
}

// -- AI chat (kept as free function) ---------------------------------
//
// SSE streaming has a different shape than the request/response
// methods on `ApiClient`. It stays a free export because there's only
// ever one consumer (`stores/chat.ts`). The server answers with the model
// it's configured with (`ai_provider`; health reports `features.ai`). The
// browser build has no server to hold a model's key: it has a chat only when
// its build names a hosted endpoint. Chat UI is gated on `conn.aiChatAvailable`.

/** A hosted assistant endpoint for the browser build, set at build time
 *  (`VITE_TRAVERSE_PUBLIC_AI_URL`). A plain build has none, and the chat stays off. */
const PUBLIC_AI_CHAT_URL = import.meta.env.VITE_TRAVERSE_PUBLIC_AI_URL as string | undefined

export async function streamAiChat(
  messages: AiChatMessage[],
  metadata: AiChatMetadata,
  onDelta: (text: string) => void,
  signal?: AbortSignal,
): Promise<void> {
  const apiKey = getApiKey()
  // A server build posts to its own endpoint, which answers with the model the
  // operator configured. The browser build has no server, so it posts to the
  // hosted endpoint its build was given; `capabilities.aiChat` has already
  // gated the UI off everywhere else.
  const wasmMode = !api.capabilities.serverFiles
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    Accept: 'text/event-stream',
  }
  // Never to the hosted endpoint: it is anonymous, and the header fails its CORS
  // preflight rather than being ignored.
  if (apiKey && !wasmMode) headers['X-API-Key'] = apiKey
  const url = wasmMode ? PUBLIC_AI_CHAT_URL : '/api/ai/chat'
  if (!url) throw new Error('this build has no assistant endpoint')

  const res = await fetch(url, {
    method: 'POST',
    headers,
    body: JSON.stringify({ messages, metadata }),
    signal,
  })

  if (!res.ok) {
    if (res.status === 401) {
      clearApiKey()
      onAuthFailed?.()
    }
    let message = res.statusText
    try {
      const body = await res.json()
      if (body.error) message = body.error
    } catch { /* ignore */ }
    throw new ApiError(res.status, message)
  }

  const reader = res.body?.getReader()
  if (!reader) return

  const decoder = new TextDecoder()
  let buffer = ''

  while (true) {
    const { done, value } = await reader.read()
    if (done) break

    buffer += decoder.decode(value, { stream: true })
    const lines = buffer.split('\n')
    buffer = lines.pop() ?? ''

    for (const line of lines) {
      if (!line.startsWith('data: ')) continue
      const data = line.slice(6)
      if (data === '[DONE]') return

      let event
      try {
        event = JSON.parse(data)
      } catch {
        continue // not JSON: skip the line
      }
      if (event.type === 'content_block_delta' && event.delta?.text) {
        onDelta(event.delta.text)
      } else if (event.type === 'text_delta' && event.text) {
        onDelta(event.text)
      } else if (event.type === 'text') {
        onDelta(event.text)
      } else if (event.type === 'error') {
        // why the answer stopped (provider error, refusal, cut-off)
        throw new Error(event.error || 'Stream error')
      }
    }
  }
}
