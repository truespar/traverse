import { defineStore } from 'pinia'
import { ref, computed, watch } from 'vue'
import {
  api,
  type GdsAlgorithm,
  type GdsCategory,
  type GdsMode,
  type GdsOutputKind,
  type QueryResponse,
} from '../utils/api'
import { useConnectionStore } from './connection'

/** A reconstructed path returned by BFS/DFS/Dijkstra/A-Star/RandomWalk. */
export interface GdsPath {
  /** Stable id for selection/highlight state (index in the run). */
  id: number
  source: number
  target: number
  totalCost: number
  /** NodeIds along the path, in traversal order. */
  nodeIds: number[]
  /** EdgeIds between consecutive nodes (length = nodeIds.length - 1). */
  edgeIds: number[]
  /** Cumulative weight per step (length = nodeIds.length). */
  costs: number[]
}

/** A pair score from similarity / link-prediction algorithms. */
export interface GdsNodePair {
  source: number
  target: number
  score: number
}

/** A set entity for Bridges / Articulation Points / Cliques. */
export interface GdsEntitySet {
  nodeIds: number[]
  edgeIds: number[]
}

/** A single algorithm run. Stored in `runs` so the drawer can display
 * history, the styling layer can read per-node values, and node tooltips
 * can show the latest score. */
export interface GdsRun {
  id: string
  algorithm: GdsAlgorithm
  mode: GdsMode
  config: Record<string, unknown>
  database: string
  runAt: number
  durationMs: number
  /** Per-node primary value. For `nodeScalar` it's the score (number);
   * for `nodeCommunity` it's the community id (number, but treated as
   * categorical by the styling layer); for `nodeMulti` it's the first
   * non-id column. Missing for non-node output kinds. */
  values: Map<number, number>
  /** All raw rows + columns from the backend. The drawer renders these
   * directly for kinds the styling layer doesn't yet handle (Path,
   * NodePair, EntitySet, NodeVector). */
  columns: string[]
  rows: unknown[][]
  /** Parsed Path results (outputKind === 'path'). */
  paths: GdsPath[]
  /** Parsed NodePair results (outputKind === 'nodePair'). */
  nodePairs: GdsNodePair[]
  /** Parsed EntitySet (outputKind === 'entitySet'). */
  entitySet: GdsEntitySet | null
  /** Parsed embeddings: nodeId -> vector. (outputKind === 'nodeVector'). */
  nodeVectors: Map<number, number[]>
  /** Aggregate stats (only populated when mode === 'stats' / 'write'). */
  summary: Record<string, unknown> | null
}

/** Continuous (numeric) vs discrete (community / category) - drives how
 * the styling layer renders the result. Computed once per run from the
 * algorithm's outputKind. */
export function valueKind(outputKind: GdsOutputKind): 'continuous' | 'discrete' | 'other' {
  if (outputKind === 'nodeScalar' || outputKind === 'nodeMulti') return 'continuous'
  if (outputKind === 'nodeCommunity') return 'discrete'
  return 'other'
}

/** Visual modes the styling layer applies on top of the base label
 * styles when a GDS run is active. */
export type GdsStylingMode = 'none' | 'size' | 'gradient' | 'community'

export interface GdsStyling {
  mode: GdsStylingMode
  /** Linear size scale endpoints. The styling layer maps
   * [run.min, run.max] -> [sizeMin, sizeMax] for the `size` mode. */
  sizeRange: [number, number]
  /** Two-stop color interpolation for the `gradient` mode. */
  colorStops: [string, string]
}

const DEFAULT_STYLING: GdsStyling = {
  mode: 'none',
  sizeRange: [4, 22],
  colorStops: ['#dbe9ff', '#1457d0'],
}

/** Categorical palette for community styling. 16 distinct hues - falls
 * back to hashing the community id for larger sets. */
const COMMUNITY_PALETTE = [
  '#e64980', '#fa5252', '#fd7e14', '#fab005',
  '#82c91e', '#40c057', '#15aabf', '#228be6',
  '#7048e8', '#be4bdb', '#f06595', '#ffa94d',
  '#ffd43b', '#a9e34b', '#69db7c', '#74c0fc',
]

/** Stable color for a community id - top N by size get distinct palette
 * colors; tail falls back to hash-bucketed colors so neighboring ids
 * don't look identical. */
export function colorForCommunity(communityId: number, topRank: Map<number, number>): string {
  const rank = topRank.get(communityId)
  if (rank !== undefined && rank < COMMUNITY_PALETTE.length) {
    return COMMUNITY_PALETTE[rank]
  }
  return COMMUNITY_PALETTE[Math.abs(communityId) % COMMUNITY_PALETTE.length]
}

/** Linear interpolation between two `#rrggbb` hex colors at t in [0,1]. */
export function interpolateHex(a: string, b: string, t: number): string {
  const pa = parseHex(a)
  const pb = parseHex(b)
  const r = Math.round(pa[0] + (pb[0] - pa[0]) * t)
  const g = Math.round(pa[1] + (pb[1] - pa[1]) * t)
  const bl = Math.round(pa[2] + (pb[2] - pa[2]) * t)
  return '#' + [r, g, bl].map((n) => n.toString(16).padStart(2, '0')).join('')
}

function parseHex(c: string): [number, number, number] {
  const s = c.replace(/^#/, '')
  return [parseInt(s.slice(0, 2), 16), parseInt(s.slice(2, 4), 16), parseInt(s.slice(4, 6), 16)]
}

let runCounter = 0
function newRunId() {
  runCounter += 1
  return `run-${Date.now().toString(36)}-${runCounter}`
}

export const useGdsStore = defineStore('gds', () => {
  // -- Available algorithms ------------------------------------------
  const availableAlgorithms = ref<GdsAlgorithm[]>([])
  const algorithmsByName = computed(() => {
    const m = new Map<string, GdsAlgorithm>()
    for (const a of availableAlgorithms.value) m.set(a.name, a)
    return m
  })
  const isLoadingAlgorithms = ref(false)
  const lastAlgorithmsError = ref<string | null>(null)

  async function ensureAlgorithmsLoaded(): Promise<void> {
    if (availableAlgorithms.value.length > 0 || isLoadingAlgorithms.value) return
    await loadAlgorithms()
  }

  async function loadAlgorithms(): Promise<void> {
    isLoadingAlgorithms.value = true
    lastAlgorithmsError.value = null
    try {
      const resp = await api.algorithms()
      availableAlgorithms.value = resp.algorithms
    } catch (e) {
      lastAlgorithmsError.value = e instanceof Error ? e.message : String(e)
      availableAlgorithms.value = []
    } finally {
      isLoadingAlgorithms.value = false
    }
  }

  /** Algorithms grouped by category, in the order the drawer renders them. */
  const algorithmsByCategory = computed(() => {
    const order: GdsCategory[] = [
      'centrality',
      'community',
      'paths',
      'dag',
      'similarity',
      'linkPrediction',
      'embeddings',
    ]
    const groups: Record<GdsCategory, GdsAlgorithm[]> = {
      centrality: [],
      community: [],
      paths: [],
      dag: [],
      similarity: [],
      linkPrediction: [],
      embeddings: [],
    }
    for (const a of availableAlgorithms.value) groups[a.category].push(a)
    return order
      .map((cat) => ({ category: cat, algorithms: groups[cat] }))
      .filter((g) => g.algorithms.length > 0)
  })

  // -- Run history ---------------------------------------------------
  const runs = ref<GdsRun[]>([])
  const currentRunId = ref<string | null>(null)
  const currentRun = computed<GdsRun | null>(
    () => runs.value.find((r) => r.id === currentRunId.value) ?? null,
  )
  const isRunning = ref(false)
  const lastRunError = ref<string | null>(null)

  /** Build the per-node values map from a stream-mode response. The
   * primary column is whatever appears second after `nodeId` - `score`
   * for NodeScalar, `communityId` for NodeCommunity, the first declared
   * multi-field for NodeMulti. */
  function extractNodeValues(
    columns: string[],
    rows: unknown[][],
    outputKind: GdsOutputKind,
  ): Map<number, number> {
    const out = new Map<number, number>()
    const nodeIdIdx = columns.indexOf('nodeId')
    if (nodeIdIdx < 0) return out
    let valueIdx: number
    if (outputKind === 'nodeCommunity') {
      valueIdx = columns.indexOf('communityId')
    } else if (outputKind === 'nodeScalar') {
      valueIdx = columns.indexOf('score')
    } else {
      // nodeMulti / other - first non-id column
      valueIdx = columns.findIndex((c, i) => i !== nodeIdIdx)
    }
    if (valueIdx < 0) return out
    for (const row of rows) {
      const nid = row[nodeIdIdx]
      const val = row[valueIdx]
      if (typeof nid !== 'number' || typeof val !== 'number') continue
      out.set(nid, val)
    }
    return out
  }

  /** Parse a backend Path response row into a GdsPath. The backend
   * declared column order is documented in
   * `OutputKind::default_columns(Path)` and on the PathResult
   * AlgorithmResult impl. */
  function extractPaths(columns: string[], rows: unknown[][]): GdsPath[] {
    const sIdx = columns.indexOf('sourceNodeId')
    const tIdx = columns.indexOf('targetNodeId')
    const cIdx = columns.indexOf('totalCost')
    const nIdx = columns.indexOf('nodeIds')
    const eIdx = columns.indexOf('edgeIds')
    const wIdx = columns.indexOf('costs')
    if (sIdx < 0 || tIdx < 0 || nIdx < 0) return []
    const out: GdsPath[] = []
    for (let i = 0; i < rows.length; i += 1) {
      const row = rows[i]
      const s = row[sIdx], t = row[tIdx]
      const ns = row[nIdx], es = eIdx >= 0 ? row[eIdx] : null, cs = wIdx >= 0 ? row[wIdx] : null
      const tc = cIdx >= 0 ? row[cIdx] : 0
      if (typeof s !== 'number' || typeof t !== 'number' || !Array.isArray(ns)) continue
      out.push({
        id: i,
        source: s,
        target: t,
        totalCost: typeof tc === 'number' ? tc : 0,
        nodeIds: (ns as unknown[]).filter((x): x is number => typeof x === 'number'),
        edgeIds: Array.isArray(es) ? (es as unknown[]).filter((x): x is number => typeof x === 'number') : [],
        costs: Array.isArray(cs) ? (cs as unknown[]).filter((x): x is number => typeof x === 'number') : [],
      })
    }
    return out
  }

  function extractNodePairs(columns: string[], rows: unknown[][]): GdsNodePair[] {
    const sIdx = columns.indexOf('sourceNodeId')
    const tIdx = columns.indexOf('targetNodeId')
    const scIdx = columns.indexOf('score')
    if (sIdx < 0 || tIdx < 0) return []
    const out: GdsNodePair[] = []
    for (const row of rows) {
      const s = row[sIdx], t = row[tIdx]
      const sc = scIdx >= 0 ? row[scIdx] : 0
      if (typeof s !== 'number' || typeof t !== 'number') continue
      out.push({ source: s, target: t, score: typeof sc === 'number' ? sc : 0 })
    }
    return out
  }

  function extractEntitySet(columns: string[], rows: unknown[][]): GdsEntitySet | null {
    const nIdx = columns.indexOf('nodeIds')
    const eIdx = columns.indexOf('relationshipIds')
    if (rows.length === 0) return null
    const row = rows[0]
    const ns = nIdx >= 0 ? row[nIdx] : null
    const es = eIdx >= 0 ? row[eIdx] : null
    return {
      nodeIds: Array.isArray(ns) ? (ns as unknown[]).filter((x): x is number => typeof x === 'number') : [],
      edgeIds: Array.isArray(es) ? (es as unknown[]).filter((x): x is number => typeof x === 'number') : [],
    }
  }

  function extractNodeVectors(columns: string[], rows: unknown[][]): Map<number, number[]> {
    const nIdx = columns.indexOf('nodeId')
    const eIdx = columns.indexOf('embedding')
    const out = new Map<number, number[]>()
    if (nIdx < 0 || eIdx < 0) return out
    for (const row of rows) {
      const nid = row[nIdx]
      const emb = row[eIdx]
      if (typeof nid !== 'number' || !Array.isArray(emb)) continue
      out.set(nid, (emb as unknown[]).filter((x): x is number => typeof x === 'number'))
    }
    return out
  }

  async function runAlgorithm(
    algorithm: GdsAlgorithm,
    mode: GdsMode,
    config: Record<string, unknown>,
  ): Promise<GdsRun> {
    const conn = useConnectionStore()
    const database = conn.currentDatabase
    isRunning.value = true
    lastRunError.value = null
    const started = performance.now()
    try {
      const resp: QueryResponse = await api.runAlgorithm(algorithm, mode, config, database)
      const durationMs = performance.now() - started
      const id = newRunId()
      const isStream = mode === 'stream'
      const kind = algorithm.outputKind
      const values =
        isStream && (kind === 'nodeScalar' || kind === 'nodeCommunity' || kind === 'nodeMulti')
          ? extractNodeValues(resp.columns, resp.rows, kind)
          : new Map<number, number>()
      const paths = isStream && kind === 'path' ? extractPaths(resp.columns, resp.rows) : []
      const nodePairs = isStream && kind === 'nodePair' ? extractNodePairs(resp.columns, resp.rows) : []
      const entitySet = isStream && kind === 'entitySet' ? extractEntitySet(resp.columns, resp.rows) : null
      const nodeVectors =
        isStream && kind === 'nodeVector'
          ? extractNodeVectors(resp.columns, resp.rows)
          : new Map<number, number[]>()
      let summary: Record<string, unknown> | null = null
      if ((mode === 'stats' || mode === 'write') && resp.rows.length > 0) {
        summary = {}
        for (let i = 0; i < resp.columns.length; i += 1) {
          summary[resp.columns[i]] = resp.rows[0][i]
        }
      }
      const run: GdsRun = {
        id,
        algorithm,
        mode,
        config,
        database,
        runAt: Date.now(),
        durationMs,
        values,
        columns: resp.columns,
        rows: resp.rows,
        paths,
        nodePairs,
        entitySet,
        nodeVectors,
        summary,
      }
      runs.value.push(run)
      currentRunId.value = id
      return run
    } catch (e) {
      lastRunError.value = e instanceof Error ? e.message : String(e)
      throw e
    } finally {
      isRunning.value = false
    }
  }

  // -- Highlight (Path / EntitySet) -----------------------------------
  /** Set of node fgIds currently highlighted (e.g. by selecting a path
   * or an entity set). The graph styling layer paints these in
   * `highlightColor` on top of base styles, with other nodes dimmed. */
  const highlightedNodeIds = ref<Set<number>>(new Set())
  const highlightedEdgeIds = ref<Set<number>>(new Set())
  const highlightedPathId = ref<number | null>(null)
  const highlightColor = ref<string>('#ff5b39')
  const highlightDim = ref<number>(0.18)

  /** Highlight one path within the current run by its id. Pass `null`
   * to clear. Hover state is the same as click - the drawer wires both
   * to this setter. */
  function highlightPath(pathId: number | null): void {
    if (pathId === null) {
      highlightedNodeIds.value = new Set()
      highlightedEdgeIds.value = new Set()
      highlightedPathId.value = null
      return
    }
    const run = currentRun.value
    if (!run) return
    const p = run.paths.find((x) => x.id === pathId)
    if (!p) return
    highlightedNodeIds.value = new Set(p.nodeIds)
    highlightedEdgeIds.value = new Set(p.edgeIds)
    highlightedPathId.value = pathId
  }

  function highlightEntitySet(): void {
    const run = currentRun.value
    if (!run || !run.entitySet) return
    highlightedNodeIds.value = new Set(run.entitySet.nodeIds)
    highlightedEdgeIds.value = new Set(run.entitySet.edgeIds)
    highlightedPathId.value = null
  }

  function clearHighlight(): void {
    highlightedNodeIds.value = new Set()
    highlightedEdgeIds.value = new Set()
    highlightedPathId.value = null
  }

  function setHighlightColor(color: string): void {
    highlightColor.value = color
  }

  // -- Styling --------------------------------------------------------
  const styling = ref<GdsStyling>({ ...DEFAULT_STYLING })

  /** Reset to defaults and re-pick the mode based on the run's outputKind:
   * size for continuous, community palette for discrete. */
  function autoStylingForRun(run: GdsRun | null): void {
    if (!run) {
      styling.value = { ...DEFAULT_STYLING }
      return
    }
    const kind = valueKind(run.algorithm.outputKind)
    styling.value = {
      ...DEFAULT_STYLING,
      mode: kind === 'continuous' ? 'size' : kind === 'discrete' ? 'community' : 'none',
    }
  }

  watch(currentRunId, () => {
    autoStylingForRun(currentRun.value)
    // Clear stale highlights when the active run changes - they belong
    // to whichever run produced them.
    clearHighlight()
  })

  function setStylingMode(mode: GdsStylingMode): void {
    styling.value = { ...styling.value, mode }
  }

  function setSizeRange(range: [number, number]): void {
    styling.value = { ...styling.value, sizeRange: range }
  }

  function setColorStops(stops: [string, string]): void {
    styling.value = { ...styling.value, colorStops: stops }
  }

  /** Top-N community ranks for palette assignment. Recomputed on demand
   * so the styling layer always has stable ordering even after runs are
   * dropped / new ones added. */
  const currentRunCommunityRank = computed<Map<number, number>>(() => {
    const m = new Map<number, number>()
    for (let i = 0; i < currentRunCommunities.value.length; i += 1) {
      m.set(currentRunCommunities.value[i].id, i)
    }
    return m
  })

  function selectRun(id: string | null): void {
    if (id === null) {
      currentRunId.value = null
      return
    }
    if (runs.value.some((r) => r.id === id)) currentRunId.value = id
  }

  function dropRun(id: string): void {
    runs.value = runs.value.filter((r) => r.id !== id)
    if (currentRunId.value === id) currentRunId.value = null
  }

  function clearRuns(): void {
    runs.value = []
    currentRunId.value = null
  }

  /** Inject a run produced *outside* the algorithms API - used by the
   *  ML panel's "Show on graph": predicted classes paint with the
   *  community palette, predicted numbers (`continuous`) size and shade
   *  nodes like a score algorithm. The caller supplies the per-node map;
   *  we wrap it in a GdsRun shaped like a `nodeCommunity` or `nodeScalar`
   *  algorithm so all existing styling code (palette assignment,
   *  dim/highlight, tooltips) just works.
   *
   *  If a run with the same id already exists it is replaced - handy for
   *  re-running predictions on the same model without piling up runs. */
  function registerExternalRun(opts: {
    id: string
    displayName: string
    description: string
    database: string
    values: Map<number, number>
    /** Values are measurements (sized and shaded) rather than classes (coloured). */
    continuous?: boolean
    /** Free-form metadata stashed in `run.config` for consumers like
     *  NodeTooltip - used by the ML panel to surface the predicted
     *  property name (`category`, `risk_band`, ...) in tooltips. */
    config?: Record<string, unknown>
  }): GdsRun {
    const valueColumn = opts.continuous ? 'score' : 'communityId'
    const algorithm: GdsAlgorithm = {
      name: opts.id,
      displayName: opts.displayName,
      category: opts.continuous ? 'centrality' : 'community',
      outputKind: opts.continuous ? 'nodeScalar' : 'nodeCommunity',
      stability: 'stable',
      description: opts.description,
      writable: false,
      modes: ['stream'],
      configSchema: [],
      outputColumns: ['nodeId', valueColumn],
      statsColumns: [],
      writeColumns: [],
    }
    const rows: unknown[][] = []
    for (const [nid, value] of opts.values) rows.push([nid, value])
    const run: GdsRun = {
      id: opts.id,
      algorithm,
      mode: 'stream',
      config: opts.config ?? {},
      database: opts.database,
      runAt: Date.now(),
      durationMs: 0,
      values: new Map(opts.values),
      columns: ['nodeId', valueColumn],
      rows,
      paths: [],
      nodePairs: [],
      entitySet: null,
      nodeVectors: new Map(),
      summary: null,
    }
    runs.value = runs.value.filter((r) => r.id !== opts.id)
    runs.value.push(run)
    currentRunId.value = opts.id
    return run
  }

  /** Min/max of the current run's primary values. Useful for the styling
   * layer's size + gradient scales. Returns null when there are no
   * numeric values. */
  const currentRunRange = computed<{ min: number; max: number } | null>(() => {
    const r = currentRun.value
    if (!r) return null
    let min = Infinity
    let max = -Infinity
    for (const v of r.values.values()) {
      if (v < min) min = v
      if (v > max) max = v
    }
    if (!Number.isFinite(min) || !Number.isFinite(max)) return null
    return { min, max }
  })

  /** Distinct community ids in the current run, sorted by size descending.
   * Used by the styling layer's community palette assignment. */
  const currentRunCommunities = computed<{ id: number; size: number }[]>(() => {
    const r = currentRun.value
    if (!r || r.algorithm.outputKind !== 'nodeCommunity') return []
    const sizes = new Map<number, number>()
    for (const v of r.values.values()) sizes.set(v, (sizes.get(v) ?? 0) + 1)
    return Array.from(sizes.entries())
      .map(([id, size]) => ({ id, size }))
      .sort((a, b) => b.size - a.size)
  })

  return {
    availableAlgorithms,
    algorithmsByName,
    algorithmsByCategory,
    isLoadingAlgorithms,
    lastAlgorithmsError,
    loadAlgorithms,
    ensureAlgorithmsLoaded,
    runs,
    currentRunId,
    currentRun,
    currentRunRange,
    currentRunCommunities,
    isRunning,
    lastRunError,
    runAlgorithm,
    selectRun,
    dropRun,
    clearRuns,
    registerExternalRun,
    styling,
    autoStylingForRun,
    setStylingMode,
    setSizeRange,
    setColorStops,
    currentRunCommunityRank,
    highlightedNodeIds,
    highlightedEdgeIds,
    highlightedPathId,
    highlightColor,
    highlightDim,
    highlightPath,
    highlightEntitySet,
    clearHighlight,
    setHighlightColor,
  }
})
