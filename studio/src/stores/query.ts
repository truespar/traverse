import { defineStore } from 'pinia'
import { ref, computed, watch } from 'vue'
import {
  api,
  type QueryDialect,
  type QueryResponse,
  type QueryStats,
  type SavedQueryEntry,
} from '../utils/api'
import { useConnectionStore } from './connection'
import { useSchemaStore } from './schema'
import { useToast } from '../composables/useToast'

export interface HistoryEntry {
  query: string
  timestamp: number
  duration_ms: number
  rowCount: number
  error?: string
}

const HISTORY_KEY = 'fg_query_history'
const DRAFT_KEY_PREFIX = 'tv_query_draft_'
const DIALECT_KEY_PREFIX = 'tv_query_dialect_'
const DEFAULT_QUERY = 'MATCH (a)-[r]->(b) RETURN a, r, b LIMIT 250'
const MAX_HISTORY = 100
/** Default dialect when no per-database choice has been made. */
const DEFAULT_DIALECT: QueryDialect = 'cypher'

function loadHistory(): HistoryEntry[] {
  try {
    const raw = localStorage.getItem(HISTORY_KEY)
    return raw ? JSON.parse(raw) : []
  } catch {
    return []
  }
}

function saveHistory(h: HistoryEntry[]) {
  try {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(h.slice(0, MAX_HISTORY)))
  } catch {
    // localStorage quota exceeded - ignore
  }
}

function loadDraft(db: string): string {
  try {
    return localStorage.getItem(DRAFT_KEY_PREFIX + db) ?? DEFAULT_QUERY
  } catch {
    return DEFAULT_QUERY
  }
}

function saveDraft(db: string, query: string) {
  try {
    localStorage.setItem(DRAFT_KEY_PREFIX + db, query)
  } catch {
    // localStorage quota exceeded - ignore
  }
}

function loadDialect(db: string): QueryDialect {
  try {
    const raw = localStorage.getItem(DIALECT_KEY_PREFIX + db)
    return raw === 'gql' ? 'gql' : DEFAULT_DIALECT
  } catch {
    return DEFAULT_DIALECT
  }
}

function saveDialect(db: string, dialect: QueryDialect) {
  try {
    localStorage.setItem(DIALECT_KEY_PREFIX + db, dialect)
  } catch {
    // localStorage quota exceeded - ignore
  }
}

/** Consume the share-link payload main.ts decoded from `#q=...` into
 *  localStorage. Returns the query string (and optionally switches
 *  to the embedded database name) only the first time it's called
 *  for a given share link. */
function consumePendingShare(): { q: string; db?: string } | null {
  try {
    const raw = localStorage.getItem('tv-pending-share')
    if (!raw) return null
    localStorage.removeItem('tv-pending-share')
    const payload = JSON.parse(raw) as { q?: string; db?: string }
    if (typeof payload.q !== 'string' || !payload.q) return null
    return { q: payload.q, db: payload.db }
  } catch {
    return null
  }
}

export const useQueryStore = defineStore('query', () => {
  const conn = useConnectionStore()
  const pendingShare = consumePendingShare()
  // If a share link primed `tv-pending-share`, take its query as the
  // initial editor content; otherwise fall back to the per-database
  // draft saved last time the user was on this DB.
  const currentQuery = ref(pendingShare?.q ?? loadDraft(conn.currentDatabase))
  /** Active dialect for the current database. Defaults to Cypher;
   *  user picks GQL via the editor toolbar dropdown and the choice
   *  persists per-database through localStorage. */
  const currentDialect = ref<QueryDialect>(loadDialect(conn.currentDatabase))
  if (pendingShare?.db) {
    // Best-effort: switch to the database the sharer was on. Falls
    // back silently if it doesn't exist on this side (e.g. they had
    // a custom OPFS-only database).
    conn.currentDatabase = pendingShare.db
  }
  const columns = ref<string[]>([])
  const rows = ref<unknown[][]>([])
  const queryType = ref('')
  const stats = ref<QueryStats | null>(null)
  const timeMs = ref(0)
  const roundtripMs = ref(0)
  const parsingTimeMs = ref(0)
  const planningTimeMs = ref(0)
  const executionTimeMs = ref(0)
  const isExecuting = ref(false)
  const error = ref<string | null>(null)
  const truncated = ref(false)
  const totalRows = ref(0)
  /** Distinct nodes collected by the server over the FULL result set (not
   * just the row sample). Falls back to client-side extraction from `rows`
   * when absent (older server). */
  const distinctNodes = ref<unknown[] | null>(null)
  const distinctEdges = ref<unknown[] | null>(null)
  const entitiesTruncated = ref(false)
  const resultViewMode = ref<'table' | 'graph' | 'json'>('table')
  const queryHistory = ref<HistoryEntry[]>(loadHistory())
  const savedQueries = ref<SavedQueryEntry[]>([])

  const hasResults = computed(() => {
    if (columns.value.length > 0) return true
    // Write/Schema queries always show feedback, even if 0 rows affected
    if (queryType.value && queryType.value !== 'Read') return true
    return false
  })

  function isGraphValue(v: any): boolean {
    if (!v || typeof v !== 'object') return false
    if (Array.isArray(v)) return v.some(isGraphValue)
    return v._type === 'node' || v._type === 'edge' || v._type === 'path'
  }

  function collectGraphEntities(v: any, nodes: Set<number>, edges: Set<number>) {
    if (!v || typeof v !== 'object') return
    if (Array.isArray(v)) { v.forEach((item) => collectGraphEntities(item, nodes, edges)); return }
    if (v._type === 'node') nodes.add(v.id)
    else if (v._type === 'edge') edges.add(v.id)
    else if (v._type === 'path') {
      if (Array.isArray(v.nodes)) for (const n of v.nodes) nodes.add(n.id)
      if (Array.isArray(v.edges)) for (const e of v.edges) edges.add(e.id)
    }
  }

  const hasGraphData = computed(() => {
    if (distinctNodes.value && distinctNodes.value.length > 0) return true
    if (distinctEdges.value && distinctEdges.value.length > 0) return true
    return rows.value.some((row) => row.some(isGraphValue))
  })

  // Prefer server-side deduped counts (computed over the FULL result set, not
  // just the row sample). Fall back to scanning visible rows for older
  // servers / non-graph queries.
  const returnedNodes = computed(() => {
    if (distinctNodes.value) return distinctNodes.value.length
    const nodes = new Set<number>()
    const edges = new Set<number>()
    for (const row of rows.value) for (const cell of row) collectGraphEntities(cell, nodes, edges)
    return nodes.size
  })

  const returnedEdges = computed(() => {
    if (distinctEdges.value) return distinctEdges.value.length
    const nodes = new Set<number>()
    const edges = new Set<number>()
    for (const row of rows.value) for (const cell of row) collectGraphEntities(cell, nodes, edges)
    return edges.size
  })

  // Fetch saved queries from server
  async function fetchSavedQueries() {
    try {
      savedQueries.value = await api.savedQueries()
    } catch {
      // server may not support this yet
    }
  }

  /** The running query's request, so Stop can abort it (the server then cancels the query). */
  let running: AbortController | null = null

  /** Stops the running query, if any. */
  function cancelQuery() {
    running?.abort()
  }

  async function executeQuery(query?: string) {
    const q = query ?? currentQuery.value
    if (!q.trim()) return

    const conn = useConnectionStore()
    const targetDb = conn.currentDatabase
    isExecuting.value = true
    error.value = null

    const controller = new AbortController()
    running = controller
    const start = performance.now()
    try {
      const data: QueryResponse = await api.query(
        q,
        undefined,
        targetDb,
        currentDialect.value,
        controller.signal,
      )
      // Discard results if user switched databases during execution
      if (conn.currentDatabase !== targetDb) return
      columns.value = data.columns
      rows.value = data.rows
      totalRows.value = data.total_rows ?? data.rows.length
      truncated.value = data.truncated ?? false
      distinctNodes.value = data.nodes ?? null
      distinctEdges.value = data.edges ?? null
      entitiesTruncated.value = data.entities_truncated ?? false
      queryType.value = data.query_type
      stats.value = data.stats
      timeMs.value = data.time_ms
      roundtripMs.value = Math.round((performance.now() - start) * 100) / 100
      parsingTimeMs.value = data.parsing_time_ms ?? 0
      planningTimeMs.value = data.planning_time_ms ?? 0
      executionTimeMs.value = data.execution_time_ms ?? 0
      error.value = null

      // Auto-switch to graph view if results contain graph data
      if (rows.value.some((row) => row.some(isGraphValue))) {
        resultViewMode.value = 'graph'
      } else {
        resultViewMode.value = 'table'
      }

      // Add to history
      const entry: HistoryEntry = {
        query: q,
        timestamp: Date.now(),
        duration_ms: Math.round(data.time_ms * 100) / 100,
        rowCount: data.rows.length,
      }
      queryHistory.value = [entry, ...queryHistory.value.slice(0, MAX_HISTORY - 1)]
      saveHistory(queryHistory.value)

      // Refresh health and schema after write queries
      if (data.query_type !== 'ReadOnly') {
        conn.pollHealth()
        const schema = useSchemaStore()
        schema.fetchSchema()
      }
    } catch (e: any) {
      const cancelled = controller.signal.aborted
      error.value = cancelled ? 'Query stopped' : e.message || 'Query failed'
      columns.value = []
      rows.value = []
      stats.value = null
      roundtripMs.value = 0
      parsingTimeMs.value = 0
      planningTimeMs.value = 0
      executionTimeMs.value = 0
      // Surface the error as a toast in addition to the inline
      // ResultPanel display - the user may be on a panel that
      // replaces the result area (Examples, Dashboard, Schema), or
      // on mobile where the result pane is collapsed behind the
      // graph. The inline error stays for the full diagnostic, the
      // toast guarantees attention.
      const { showToast } = useToast()
      showToast(error.value ?? 'Query failed', cancelled ? 'success' : 'error')

      const entry: HistoryEntry = {
        query: q,
        timestamp: Date.now(),
        duration_ms: Math.round((performance.now() - start) * 100) / 100,
        rowCount: 0,
        error: error.value ?? undefined,
      }
      queryHistory.value = [entry, ...queryHistory.value.slice(0, MAX_HISTORY - 1)]
      saveHistory(queryHistory.value)
    } finally {
      if (running === controller) running = null
      isExecuting.value = false
    }
  }

  function clearResults() {
    columns.value = []
    rows.value = []
    queryType.value = ''
    stats.value = null
    error.value = null
  }

  async function saveQuery(name: string, query?: string) {
    const q = query ?? currentQuery.value
    if (!q.trim()) return
    const { showToast } = useToast()
    try {
      await api.saveQuery(name, q)
      await fetchSavedQueries()
    } catch (e: any) {
      showToast(e.message || 'Failed to save query', 'error')
    }
  }

  async function removeSavedQuery(name: string) {
    const { showToast } = useToast()
    try {
      await api.deleteSavedQuery(name)
      await fetchSavedQueries()
    } catch (e: any) {
      showToast(e.message || 'Failed to delete query', 'error')
    }
  }

  // Persist draft per database
  watch(currentQuery, (q) => {
    saveDraft(conn.currentDatabase, q)
  })

  // Persist dialect per database
  watch(currentDialect, (d) => {
    saveDialect(conn.currentDatabase, d)
  })

  // Restore draft + dialect when switching databases
  watch(() => conn.currentDatabase, (db) => {
    currentQuery.value = loadDraft(db)
    currentDialect.value = loadDialect(db)
  })

  return {
    currentQuery,
    currentDialect,
    columns,
    rows,
    totalRows,
    truncated,
    distinctNodes,
    distinctEdges,
    entitiesTruncated,
    queryType,
    stats,
    timeMs,
    roundtripMs,
    parsingTimeMs,
    planningTimeMs,
    executionTimeMs,
    isExecuting,
    error,
    resultViewMode,
    queryHistory,
    hasResults,
    hasGraphData,
    returnedNodes,
    returnedEdges,
    executeQuery,
    cancelQuery,
    clearResults,
    savedQueries,
    saveQuery,
    removeSavedQuery,
    fetchSavedQueries,
  }
})
