/** Graph Explorer composable for Traverse Studio: generic Cypher-result graph visualization. */
import { ref, shallowRef, computed, onUnmounted } from 'vue'
import Graph from 'graphology'
import Sigma from 'sigma'
import { downloadAsPNG } from '@sigma/export-image'
import { LayoutController } from '../../../packages/graph-layout/index.js'
import NodePulseProgram from '../programs/NodePulseProgram'
import NodeHexagonProgram from '../programs/NodeHexagonProgram'
import {
  DEFAULT_EDGE_CURVATURE,
  EdgeCurvedArrowProgram,
  indexParallelEdgesIndex,
} from '@sigma/edge-curve'
import { EdgeArrowProgram, drawDiscNodeHover } from 'sigma/rendering'
import { useStyleStore } from '../stores/style'
import { useGdsStore, colorForCommunity, interpolateHex } from '../stores/gds'

/* ---------- Parallel edge curvature ---------- */
function getCurvature(index: number, maxIndex: number): number {
  if (maxIndex <= 0) throw new Error('Invalid maxIndex')
  if (index < 0) return -getCurvature(-index, maxIndex)
  const amplitude = 12
  const maxCurvature = amplitude * (1 - Math.exp(-maxIndex / amplitude)) * DEFAULT_EDGE_CURVATURE
  return (maxCurvature * index) / maxIndex
}

/* ---------- Layout types ---------- */
export const LAYOUT_TYPES = {
  FORCE_ATLAS_2: 'forceAtlas2',
  D3_FORCE: 'd3Force',
} as const

/* ---------- Theme colours ---------- */
/* Slate-forward palette with muted, content-friendly node colors */
const NODE_PALETTE_LIGHT = [
  '#0369A1', '#2A6BB5', '#C53030', '#B8860B', '#7C3AED',
  '#0284C7', '#3B82C4', '#D44040', '#4A90D0', '#6366f1',
]
const NODE_PALETTE_DARK = [
  '#38BDF8', '#5B9BD5', '#E8716E', '#D4A030', '#a78bfa',
  '#7DD3FC', '#7BB4E0', '#f87171', '#60a5fa', '#818cf8',
]

const COLORS_LIGHT = {
  edge: '#D0D5DA', label: '#1A1D20', edgeLabel: '#5A6370',
  defaultNode: '#9AA1AB', defaultEdge: '#D0D5DA', fadedFallback: '#D0D5DA',
}
const COLORS_DARK = {
  edge: '#2A3A48', label: '#C9CDCF', edgeLabel: '#7E8B97',
  defaultNode: '#3A4A58', defaultEdge: '#2A3A48', fadedFallback: '#2A3A48',
}

function getColors(isDark: boolean) { return isDark ? COLORS_DARK : COLORS_LIGHT }
export function getNodePalette(isDark: boolean) { return isDark ? NODE_PALETTE_DARK : NODE_PALETTE_LIGHT }

/* ---------- Label -> colour mapping ---------- */
// Per-instance map, reset on each new graph build so colors are
// consistent regardless of which database was queried previously.
let labelColorMap = new Map<string, number>()
let nextColorIdx = 0

function resetLabelColors() {
  labelColorMap = new Map()
  nextColorIdx = 0
}

export function colorForLabel(label: string, isDark: boolean): string {
  if (!labelColorMap.has(label)) {
    labelColorMap.set(label, nextColorIdx++)
  }
  const palette = getNodePalette(isDark)
  return palette[labelColorMap.get(label)! % palette.length]
}

/* ---------- Fade colour helper ---------- */
function fadeColor(hex: string | undefined, factor: number, isDark: boolean): string {
  if (!hex) return isDark ? COLORS_DARK.fadedFallback : COLORS_LIGHT.fadedFallback
  const parseHex = (h: string) => {
    const r = /^#?([a-f\d]{2})([a-f\d]{2})([a-f\d]{2})$/i.exec(h)
    return r ? { r: parseInt(r[1], 16), g: parseInt(r[2], 16), b: parseInt(r[3], 16) } : null
  }
  let r: number, g: number, b: number
  if (hex.startsWith('rgb')) {
    const m = hex.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)/)
    if (!m) return hex
    r = +m[1]; g = +m[2]; b = +m[3]
  } else {
    const p = parseHex(hex)
    if (!p) return hex
    r = p.r; g = p.g; b = p.b
  }
  const [bgR, bgG, bgB] = isDark ? [21, 29, 36] : [243, 244, 245]
  return `rgb(${Math.round(r * factor + bgR * (1 - factor))}, ${Math.round(g * factor + bgG * (1 - factor))}, ${Math.round(b * factor + bgB * (1 - factor))})`
}

/* ---------- Large graph threshold ---------- */
const CURVED_EDGE_THRESHOLD = 2000

/* ========================================================
   Main composable
   ======================================================== */

export interface GraphExplorerOptions {
  onNodeClick?: (data: Record<string, unknown>) => void
  onEdgeClick?: (data: Record<string, unknown>) => void
  onStageClick?: () => void
}

export function useGraphExplorer(options: GraphExplorerOptions = {}) {
  const graphRef = shallowRef<Graph | null>(null)
  const sigmaRef = shallowRef<Sigma | null>(null)
  const layoutSupervisorRef = shallowRef<LayoutController | null>(null)
  const isLoading = ref(false)
  const error = ref<string | null>(null)
  const isAnimating = ref(false)

  const hoveredNode = ref<string | null>(null)
  const selectedNode = ref<string | null>(null)
  const highlightedNodes = ref(new Set<string>())
  const highlightedEdges = ref(new Set<string>())
  const currentIsDarkTheme = ref(false)

  const pulseAnimationRef = shallowRef<number | null>(null)

  const nodeCountRef = ref(0)
  const edgeCountRef = ref(0)
  const nodeCount = computed(() => nodeCountRef.value)
  const edgeCount = computed(() => edgeCountRef.value)

  function updateCounts() {
    nodeCountRef.value = graphRef.value?.order || 0
    edgeCountRef.value = graphRef.value?.size || 0
  }
  const currentLayout = ref<string>(LAYOUT_TYPES.FORCE_ATLAS_2)
  const simplifiedEdges = ref(false)

  /* ---- Build graph from Traverse query result ---- */

  interface TvNode { _type: 'node'; id: number; labels: string[]; properties: Record<string, unknown> }
  interface TvEdge { _type: 'edge'; id: number; type: string; source: number; target: number; properties: Record<string, unknown> }
  interface TvPath { _type: 'path'; nodes: TvNode[]; edges: TvEdge[] }

  function buildGraphFromResults(
    rows: unknown[][],
    isDark = false,
    distinctNodes?: unknown[] | null,
    distinctEdges?: unknown[] | null,
  ) {
    resetLabelColors()
    const styleStore = useStyleStore()
    const nodes = new Map<number, TvNode>()
    const edges = new Map<number, TvEdge>()

    function extract(cell: unknown) {
      if (!cell || typeof cell !== 'object') return
      if (Array.isArray(cell)) { cell.forEach(extract); return }
      const c = cell as Record<string, unknown>
      if (c._type === 'node') {
        const n = c as unknown as TvNode
        nodes.set(n.id, n)
      } else if (c._type === 'edge') {
        const e = c as unknown as TvEdge
        edges.set(e.id, e)
      } else if (c._type === 'path') {
        const p = c as unknown as TvPath
        p.nodes?.forEach((n) => nodes.set(n.id, n))
        p.edges?.forEach((e) => edges.set(e.id, e))
      }
    }

    // Prefer the server-deduped entity lists when present: they're computed
    // over the FULL result set (not just the row sample), so the graph view
    // stays complete even when rows are truncated for the table.
    if (distinctNodes && distinctNodes.length > 0) {
      for (const n of distinctNodes) extract(n)
    }
    if (distinctEdges && distinctEdges.length > 0) {
      for (const e of distinctEdges) extract(e)
    }
    // Fall back to scanning rows for older servers / for path/list cells the
    // dedup pass may not surface separately.
    if (nodes.size === 0 && edges.size === 0) {
      rows.forEach((row) => row.forEach(extract))
    }

    const graph = new Graph({ multi: true })
    graphRef.value = graph

    const spread = Math.max(nodes.size * 5, 100)

    nodes.forEach((node) => {
      const label = node.labels[0] || 'Node'
      const customStyle = styleStore.nodeStyles[label]
      const ruleOverrides = styleStore.resolveNode(label, node.properties || {})
      const color = ruleOverrides.color || customStyle?.color || colorForLabel(label, isDark)
      const size = ruleOverrides.size ?? customStyle?.size ?? 6
      const program = ruleOverrides.program || customStyle?.program
      const displayLabel = node.properties.name
        ? String(node.properties.name)
        : `${label} #${node.id}`

      const attrs: Record<string, unknown> = {
        label: displayLabel,
        x: (Math.random() - 0.5) * spread,
        y: (Math.random() - 0.5) * spread,
        size,
        color,
        originalColor: color,
        labelColor: isDark ? COLORS_DARK.label : COLORS_LIGHT.label,
        nodeType: label,
        properties: node.properties,
        nodeLabels: node.labels,
        fgId: node.id,
      }
      if (program && program !== 'circle') {
        attrs.type = program
      }
      graph.addNode(String(node.id), attrs)
    })

    const defaultEdgeColor = isDark ? COLORS_DARK.edge : COLORS_LIGHT.edge

    edges.forEach((edge) => {
      const src = String(edge.source)
      const tgt = String(edge.target)
      if (!graph.hasNode(src) || !graph.hasNode(tgt)) return

      const customEdge = styleStore.edgeStyles[edge.type]
      const edgeRuleOverrides = styleStore.resolveEdge(edge.type, edge.properties || {})
      const edgeColor = edgeRuleOverrides.color || customEdge?.color || defaultEdgeColor
      const edgeSize = edgeRuleOverrides.thickness ?? customEdge?.thickness ?? 2

      graph.addEdge(src, tgt, {
        label: edge.type,
        size: edgeSize,
        color: edgeColor,
        originalColor: edgeColor,
        edgeType: edge.type,
        properties: edge.properties,
        fgId: edge.id,
        forceLabel: true,
      })
    })

    // Parallel edge curvature (skip for large graphs - curved edges are slow)
    if (graph.size <= CURVED_EDGE_THRESHOLD) {
      simplifiedEdges.value = false
      indexParallelEdgesIndex(graph, {
        edgeIndexAttribute: 'parallelIndex',
        edgeMinIndexAttribute: 'parallelMinIndex',
        edgeMaxIndexAttribute: 'parallelMaxIndex',
      })

      graph.forEachEdge((edge, attrs) => {
        const { parallelIndex, parallelMinIndex, parallelMaxIndex } = attrs as Record<string, number | undefined>
        if (typeof parallelMinIndex === 'number') {
          graph.mergeEdgeAttributes(edge, {
            type: parallelIndex ? 'curved' : 'straight',
            curvature: getCurvature(parallelIndex ?? 0, parallelMaxIndex ?? 1),
          })
        } else if (typeof parallelIndex === 'number') {
          graph.mergeEdgeAttributes(edge, {
            type: 'curved',
            curvature: getCurvature(parallelIndex, parallelMaxIndex ?? 1),
          })
        } else {
          graph.setEdgeAttribute(edge, 'type', 'straight')
        }
      })
    } else {
      simplifiedEdges.value = true
      graph.forEachEdge((edge) => {
        graph.setEdgeAttribute(edge, 'type', 'straight')
      })
    }

    updateCounts()
    return graph
  }

  /* ---- Merge additional results into existing graph ---- */

  function mergeResults(rows: unknown[][], isDark = false) {
    if (!graphRef.value) {
      buildGraphFromResults(rows, isDark)
      return
    }
    const styleStore = useStyleStore()
    const graph = graphRef.value
    const nodes = new Map<number, TvNode>()
    const edges = new Map<number, TvEdge>()

    function extract(cell: unknown) {
      if (!cell || typeof cell !== 'object') return
      if (Array.isArray(cell)) { cell.forEach(extract); return }
      const c = cell as Record<string, unknown>
      if (c._type === 'node') { const n = c as unknown as TvNode; nodes.set(n.id, n) }
      else if (c._type === 'edge') { const e = c as unknown as TvEdge; edges.set(e.id, e) }
      else if (c._type === 'path') {
        const p = c as unknown as TvPath
        p.nodes?.forEach((n) => nodes.set(n.id, n))
        p.edges?.forEach((e) => edges.set(e.id, e))
      }
    }

    rows.forEach((row) => row.forEach(extract))

    nodes.forEach((node) => {
      const key = String(node.id)
      if (graph.hasNode(key)) return
      const label = node.labels[0] || 'Node'
      const customStyle = styleStore.nodeStyles[label]
      const ruleOverrides = styleStore.resolveNode(label, node.properties || {})
      const color = ruleOverrides.color || customStyle?.color || colorForLabel(label, isDark)
      const size = ruleOverrides.size ?? customStyle?.size ?? 6
      const program = ruleOverrides.program || customStyle?.program
      const displayLabel = node.properties.name ? String(node.properties.name) : `${label} #${node.id}`
      const attrs: Record<string, unknown> = {
        label: displayLabel,
        x: (Math.random() - 0.5) * 200,
        y: (Math.random() - 0.5) * 200,
        size,
        color,
        originalColor: color,
        labelColor: isDark ? COLORS_DARK.label : COLORS_LIGHT.label,
        nodeType: label,
        properties: node.properties,
        nodeLabels: node.labels,
        fgId: node.id,
      }
      if (program && program !== 'circle') {
        attrs.type = program
      }
      graph.addNode(key, attrs)
    })

    const defaultEdgeColor = isDark ? COLORS_DARK.edge : COLORS_LIGHT.edge
    edges.forEach((edge) => {
      const src = String(edge.source)
      const tgt = String(edge.target)
      if (!graph.hasNode(src) || !graph.hasNode(tgt)) return
      // Check for duplicate edge
      let exists = false
      graph.forEachEdge(src, tgt, (_e, attrs) => {
        if ((attrs as Record<string, unknown>).fgId === edge.id) exists = true
      })
      if (exists) return
      const customEdge = styleStore.edgeStyles[edge.type]
      const edgeRuleOverrides = styleStore.resolveEdge(edge.type, edge.properties || {})
      const edgeColor = edgeRuleOverrides.color || customEdge?.color || defaultEdgeColor
      const edgeSize = edgeRuleOverrides.thickness ?? customEdge?.thickness ?? 2
      graph.addEdge(src, tgt, {
        label: edge.type,
        size: edgeSize,
        color: edgeColor,
        originalColor: edgeColor,
        edgeType: edge.type,
        properties: edge.properties,
        fgId: edge.id,
        forceLabel: true,
      })
    })

    // Re-index parallel edges (skip for large graphs)
    if (graph.size <= CURVED_EDGE_THRESHOLD) {
      simplifiedEdges.value = false
      indexParallelEdgesIndex(graph, {
        edgeIndexAttribute: 'parallelIndex',
        edgeMinIndexAttribute: 'parallelMinIndex',
        edgeMaxIndexAttribute: 'parallelMaxIndex',
      })
      graph.forEachEdge((edge, attrs) => {
        const { parallelIndex, parallelMinIndex, parallelMaxIndex } = attrs as Record<string, number | undefined>
        if (typeof parallelMinIndex === 'number') {
          graph.mergeEdgeAttributes(edge, {
            type: parallelIndex ? 'curved' : 'straight',
            curvature: getCurvature(parallelIndex ?? 0, parallelMaxIndex ?? 1),
          })
        } else if (typeof parallelIndex === 'number') {
          graph.mergeEdgeAttributes(edge, {
            type: 'curved',
            curvature: getCurvature(parallelIndex, parallelMaxIndex ?? 1),
          })
        } else {
          graph.setEdgeAttribute(edge, 'type', 'straight')
        }
      })
    } else {
      simplifiedEdges.value = true
      graph.forEachEdge((edge) => {
        graph.setEdgeAttribute(edge, 'type', 'straight')
      })
    }

    updateCounts()
  }

  /* ---- Layout ---- */

  function stopAnimatedLayout() {
    layoutSupervisorRef.value?.kill()
    layoutSupervisorRef.value = null
    isAnimating.value = false
    sigmaRef.value?.setSetting('renderLabels', true)
  }

  function startAnimatedLayout(onReady?: () => void) {
    if (!graphRef.value) return
    stopAnimatedLayout()
    error.value = null
    const graph = graphRef.value
    isAnimating.value = true
    sigmaRef.value?.setSetting('renderLabels', false)
    let ready = false
    const signalReady = () => { if (!ready) { ready = true; onReady?.() } }
    const controller = new LayoutController(graph, {
      kind: currentLayout.value === LAYOUT_TYPES.D3_FORCE ? 'd3' : 'fa2',
      onCancel: () => {
        if (layoutSupervisorRef.value !== controller) return
        isAnimating.value = false
        sigmaRef.value?.setSetting('renderLabels', true)
      },
      onProgress: () => { if (layoutSupervisorRef.value === controller) signalReady() },
      onComplete: () => {
        if (layoutSupervisorRef.value !== controller || graphRef.value !== graph) return
        isAnimating.value = false
        signalReady()
        sigmaRef.value?.setSetting('renderLabels', true)
        sigmaRef.value?.getCamera().animatedReset({ duration: 200 })
      },
      onError: cause => {
        if (layoutSupervisorRef.value !== controller) return
        isAnimating.value = false
        sigmaRef.value?.setSetting('renderLabels', true)
        error.value = `Graph layout failed: ${cause.message}`
        signalReady()
      },
    })
    layoutSupervisorRef.value = controller
    try { controller.start() } catch (cause) {
      stopAnimatedLayout()
      error.value = `Graph layout failed: ${cause instanceof Error ? cause.message : String(cause)}`
      signalReady()
    }
  }

  function applyLayout(iterations = 100, layoutType: string | null = null, randomize = true, onReady?: () => void) {
    if (!graphRef.value) return
    stopAnimatedLayout()

    const layout = layoutType || currentLayout.value
    currentLayout.value = layout

    if (randomize) {
      graphRef.value.forEachNode((n, a) => {
        graphRef.value!.setNodeAttribute(n, 'x', (a.x as number) + (Math.random() - 0.5) * 50)
        graphRef.value!.setNodeAttribute(n, 'y', (a.y as number) + (Math.random() - 0.5) * 50)
      })
    }

    // Both layout choices use the same bounded worker lifecycle.
    startAnimatedLayout(onReady)
  }

  function switchLayout(layoutType: string, iterations = 100) {
    applyLayout(iterations, layoutType, true)
    sigmaRef.value?.refresh()
  }

  /* ---- Sigma init ---- */

  function initializeSigma(container: HTMLElement, isDark = false) {
    if (!container || !graphRef.value) return null
    if (sigmaRef.value) { sigmaRef.value.kill(); sigmaRef.value = null }

    const colors = getColors(isDark)
    currentIsDarkTheme.value = isDark

    const sigma = new Sigma(graphRef.value, container, {
      renderLabels: !isAnimating.value,
      renderEdgeLabels: false,
      labelRenderedSizeThreshold: 8,
      // Edge labels always shown when renderEdgeLabels is true
      labelFont: 'Inter, system-ui, sans-serif',
      labelSize: 10,
      labelWeight: '500',
      labelColor: { attribute: 'labelColor', color: colors.label },
      edgeLabelFont: 'Inter, system-ui, sans-serif',
      edgeLabelSize: 10,
      edgeLabelColor: { attribute: 'labelColor', color: colors.edgeLabel },
      defaultNodeColor: colors.defaultNode,
      defaultEdgeColor: colors.defaultEdge,
      defaultEdgeType: 'straight',
      defaultDrawNodeHover: drawDiscNodeHover,
      nodeProgramClasses: {
        pulse: NodePulseProgram,
        hexagon: NodeHexagonProgram,
      },
      edgeProgramClasses: simplifiedEdges.value
        ? { straight: EdgeArrowProgram }
        : { straight: EdgeArrowProgram, curved: EdgeCurvedArrowProgram },
      enableEdgeEvents: true,
      allowInvalidContainer: true,
      minCameraRatio: 0.1,
      maxCameraRatio: 10,
      nodeReducer: (node, data) => {
        const res = { ...data }
        if (selectedNode.value === node) {
          res.forceLabel = true
        }
        if (hoveredNode.value) {
          const isDk = currentIsDarkTheme.value
          const c = getColors(isDk)
          if (hoveredNode.value === node) { res.labelColor = COLORS_LIGHT.label; res.forceLabel = true }
          else if (highlightedNodes.value.has(node)) { res.labelColor = c.label; res.forceLabel = true }
          else {
            res.color = fadeColor(data.originalColor as string, isDk ? 0.08 : 0.15, isDk)
            res.labelColor = fadeColor(c.label, isDk ? 0.1 : 0.25, isDk)
          }
        }
        return res
      },
      edgeReducer: (edge, data) => {
        const res = { ...data }
        if (hoveredNode.value) {
          if (!highlightedEdges.value.has(edge)) {
            const isDk = currentIsDarkTheme.value
            res.color = fadeColor(data.originalColor as string, isDk ? 0.05 : 0.1, isDk)
          } else {
            res.size = 3
          }
        }
        // Ensure minimum size for clickability
        if (res.size < 2) res.size = 2
        return res
      },
    })

    sigmaRef.value = sigma
    setupEventHandlers(sigma)
    syncPulseAnimation()
    return sigma
  }

  /* ---- Event handlers ---- */

  function setupEventHandlers(sigma: Sigma) {
    const graph = graphRef.value!
    let draggedNode: string | null = null
    let isDragging = false
    let hasDragged = false

    sigma.on('downNode', (e) => {
      if (isAnimating.value) return
      isDragging = true; hasDragged = false; draggedNode = e.node
      graph.setNodeAttribute(draggedNode, 'highlighted', true)
      if (!sigma.getCustomBBox()) sigma.setCustomBBox(sigma.getBBox())
    })

    sigma.on('moveBody', ({ event }) => {
      if (!isDragging || !draggedNode) return
      hasDragged = true
      const pos = sigma.viewportToGraph(event)
      graph.setNodeAttribute(draggedNode, 'x', pos.x)
      graph.setNodeAttribute(draggedNode, 'y', pos.y)
      event.preventSigmaDefault()
      event.original.preventDefault()
      event.original.stopPropagation()
    })

    const handleUp = () => {
      if (draggedNode) graph.removeNodeAttribute(draggedNode, 'highlighted')
      isDragging = false; draggedNode = null
    }
    sigma.on('upNode', handleUp)
    sigma.on('upStage', handleUp)

    sigma.on('enterNode', ({ node }) => {
      hoveredNode.value = node
      const neighbors = new Set<string>([node])
      const edges = new Set<string>()
      graph.forEachNeighbor(node, (n) => neighbors.add(n))
      graph.forEachEdge(node, (e) => edges.add(e))
      highlightedNodes.value = neighbors
      highlightedEdges.value = edges
      sigma.getContainer().style.cursor = 'pointer'
      sigma.refresh()
    })

    sigma.on('leaveNode', () => {
      hoveredNode.value = null
      highlightedNodes.value = new Set()
      highlightedEdges.value = new Set()
      sigma.getContainer().style.cursor = 'default'
      sigma.refresh()
    })

    sigma.on('enterEdge', () => { sigma.getContainer().style.cursor = 'pointer' })
    sigma.on('leaveEdge', () => { sigma.getContainer().style.cursor = 'default' })

    sigma.on('clickNode', ({ node, event }) => {
      if (isAnimating.value || hasDragged) { hasDragged = false; return }
      event.original.preventDefault()
      const wasSelected = selectedNode.value === node
      selectedNode.value = wasSelected ? null : node
      if (options.onNodeClick) {
        options.onNodeClick({ id: node, ...graph.getNodeAttributes(node) })
      }
      sigma.refresh()
    })

    sigma.on('clickEdge', ({ edge, event }) => {
      event.original.preventDefault()
      selectedNode.value = null; sigma.refresh()
      if (options.onEdgeClick) {
        const src = graph.source(edge)
        const tgt = graph.target(edge)
        const srcAttrs = graph.getNodeAttributes(src)
        const tgtAttrs = graph.getNodeAttributes(tgt)
        options.onEdgeClick({
          id: edge, source: src, target: tgt,
          sourceFgId: srcAttrs.fgId, targetFgId: tgtAttrs.fgId,
          ...graph.getEdgeAttributes(edge),
        })
      }
    })

    sigma.on('clickStage', () => {
      selectedNode.value = null; sigma.refresh()
      if (options.onStageClick) options.onStageClick()
    })

    // Edge labels on zoom
    const camera = sigma.getCamera()
    const isVisible = (r: number) => r < 0.4 && r > 0
    sigma.setSetting('renderEdgeLabels', isVisible(camera.ratio))
    camera.on('updated', () => {
      const v = isVisible(camera.ratio)
      if (sigma.getSetting('renderEdgeLabels') !== v) sigma.setSetting('renderEdgeLabels', v)
    })
  }

  /* ---- Pulse animation ---- */

  function startPulseAnimation() {
    if (pulseAnimationRef.value) return
    const start = performance.now() / 1000
    let odd = false
    function animate() {
      pulseAnimationRef.value = requestAnimationFrame(animate)
      // half-rate, repaint only: a bare refresh() reruns the whole data pipeline (indexation, label
      // grid), and this loop runs as long as any node pulses, so a full reprocess 60x/s would pin a
      // core; the ripple only needs the shader clock and a redraw
      odd = !odd
      if (odd) return
      NodePulseProgram.currentTime = performance.now() / 1000 - start
      sigmaRef.value?.refresh({ skipIndexation: true })
    }
    pulseAnimationRef.value = requestAnimationFrame(animate)
  }

  function stopPulseAnimation() {
    if (pulseAnimationRef.value) {
      cancelAnimationFrame(pulseAnimationRef.value)
      pulseAnimationRef.value = null
      NodePulseProgram.currentTime = 0
    }
  }

  /** Start or stop the pulse animation based on whether any node uses the pulse program. */
  function syncPulseAnimation() {
    if (!graphRef.value) { stopPulseAnimation(); return }
    let hasPulse = false
    graphRef.value.forEachNode((_id, attrs) => {
      if (attrs.type === 'pulse') hasPulse = true
    })
    if (hasPulse) startPulseAnimation()
    else stopPulseAnimation()
  }

  /* ---- Theme update ---- */

  function updateColors(isDark: boolean) {
    if (!graphRef.value) return
    currentIsDarkTheme.value = isDark
    const styleStore = useStyleStore()
    const graph = graphRef.value
    const defaultEdgeColor = isDark ? COLORS_DARK.edge : COLORS_LIGHT.edge
    const labelColor = isDark ? COLORS_DARK.label : COLORS_LIGHT.label

    graph.forEachNode((id, attrs) => {
      const nodeType = attrs.nodeType as string
      const properties = (attrs.properties || {}) as Record<string, unknown>
      const customStyle = styleStore.nodeStyles[nodeType]
      const ruleOverrides = styleStore.resolveNode(nodeType, properties)
      const newColor = ruleOverrides.color || customStyle?.color || colorForLabel(nodeType, isDark)
      graph.setNodeAttribute(id, 'color', newColor)
      graph.setNodeAttribute(id, 'originalColor', newColor)
      graph.setNodeAttribute(id, 'labelColor', labelColor)
    })
    graph.forEachEdge((id, attrs) => {
      const edgeType = attrs.edgeType as string
      const properties = (attrs.properties || {}) as Record<string, unknown>
      const customEdge = edgeType ? styleStore.edgeStyles[edgeType] : undefined
      const ruleOverrides = edgeType ? styleStore.resolveEdge(edgeType, properties) : {}
      const edgeColor = ruleOverrides.color || customEdge?.color || defaultEdgeColor
      graph.setEdgeAttribute(id, 'color', edgeColor)
      graph.setEdgeAttribute(id, 'originalColor', edgeColor)
    })
    if (sigmaRef.value) {
      const c = getColors(isDark)
      sigmaRef.value.setSetting('defaultNodeColor', c.defaultNode)
      sigmaRef.value.setSetting('defaultEdgeColor', c.defaultEdge)
      sigmaRef.value.setSetting('labelColor', { attribute: 'labelColor', color: c.label })
      sigmaRef.value.setSetting('edgeLabelColor', { attribute: 'labelColor', color: c.edgeLabel })
      // Update label font reference for theme
      sigmaRef.value.setSetting('labelFont', "'Inter Variable', Inter, system-ui, sans-serif")
      requestAnimationFrame(() => { (sigmaRef.value as any)?.process(); sigmaRef.value?.refresh() })
    }
  }

  /* ---- Style overrides (called when style store changes) ---- */

  function applyStyleOverrides() {
    if (!graphRef.value) return
    const styleStore = useStyleStore()
    const gds = useGdsStore()
    const graph = graphRef.value
    const isDark = currentIsDarkTheme.value
    const defaultEdgeColor = isDark ? COLORS_DARK.edge : COLORS_LIGHT.edge

    // GDS run state - read once per refresh. The styling layer applies
    // size scaling / color gradient / community palette ON TOP of the
    // base label styles when a run is active and a mode is selected.
    const run = gds.currentRun
    const styling = gds.styling
    const range = gds.currentRunRange
    const communityRank = gds.currentRunCommunityRank
    const stylingActive = run && styling.mode !== 'none' && (
      styling.mode === 'community' ? communityRank.size > 0 : !!range && range.max > range.min
    )

    // Highlight overlay (Path / EntitySet). When a path is highlighted
    // the styling layer paints its nodes + edges in `highlightColor`
    // and dims everything else by `highlightDim` (multiplicative alpha
    // via grayscale blend in hex space).
    const hlNodes = gds.highlightedNodeIds
    const hlEdges = gds.highlightedEdgeIds
    const hlActive = hlNodes.size > 0 || hlEdges.size > 0
    const hlColor = gds.highlightColor
    const hlDim = gds.highlightDim

    graph.forEachNode((id, attrs) => {
      const nodeType = attrs.nodeType as string
      const properties = (attrs.properties || {}) as Record<string, unknown>
      const customStyle = styleStore.nodeStyles[nodeType]
      const ruleOverrides = styleStore.resolveNode(nodeType, properties)
      let color = ruleOverrides.color || customStyle?.color || colorForLabel(nodeType, isDark)
      let size = ruleOverrides.size ?? customStyle?.size ?? 6
      const program = ruleOverrides.program || customStyle?.program

      // GDS override pass. `fgId` is the integer node id used as the key
      // into the run's per-node `values` map.
      if (stylingActive) {
        const fgId = attrs.fgId as number | undefined
        const value = fgId === undefined ? undefined : run!.values.get(fgId)
        if (value !== undefined) {
          if (styling.mode === 'size' && range) {
            const t = (value - range.min) / (range.max - range.min)
            const [sMin, sMax] = styling.sizeRange
            size = sMin + t * (sMax - sMin)
          } else if (styling.mode === 'gradient' && range) {
            const t = (value - range.min) / (range.max - range.min)
            color = interpolateHex(styling.colorStops[0], styling.colorStops[1], t)
          } else if (styling.mode === 'community') {
            color = colorForCommunity(value, communityRank)
          }
        }
      }

      // Highlight overlay: members of the highlighted set keep their
      // color overridden to highlightColor and grow slightly; non-
      // members dim toward white.
      if (hlActive) {
        const fgId = attrs.fgId as number | undefined
        if (fgId !== undefined && hlNodes.has(fgId)) {
          color = hlColor
          size = Math.max(size * 1.25, size + 2)
        } else {
          color = fadeColor(color, hlDim, isDark)
        }
      }

      graph.setNodeAttribute(id, 'color', color)
      graph.setNodeAttribute(id, 'originalColor', color)
      graph.setNodeAttribute(id, 'size', size)
      if (program && program !== 'circle') {
        graph.setNodeAttribute(id, 'type', program)
      } else {
        graph.removeNodeAttribute(id, 'type')
      }
    })

    graph.forEachEdge((id, attrs) => {
      const edgeType = attrs.edgeType as string
      const properties = (attrs.properties || {}) as Record<string, unknown>
      const customEdge = edgeType ? styleStore.edgeStyles[edgeType] : undefined
      const ruleOverrides = edgeType ? styleStore.resolveEdge(edgeType, properties) : {}
      let color = ruleOverrides.color || customEdge?.color || defaultEdgeColor
      let size = ruleOverrides.thickness ?? customEdge?.thickness ?? 1.5
      if (hlActive) {
        const fgId = attrs.fgId as number | undefined
        if (fgId !== undefined && hlEdges.has(fgId)) {
          color = hlColor
          size = Math.max(size * 1.8, size + 1)
        } else {
          color = fadeColor(color, hlDim, isDark)
        }
      }
      graph.setEdgeAttribute(id, 'color', color)
      graph.setEdgeAttribute(id, 'originalColor', color)
      graph.setEdgeAttribute(id, 'size', size)
    })

    sigmaRef.value?.refresh()
    syncPulseAnimation()
  }

  /* ---- Camera controls ---- */

  function zoomToFit() { sigmaRef.value?.getCamera().animatedReset() }
  function zoomIn() { sigmaRef.value?.getCamera().animatedZoom({ duration: 300 }) }
  function zoomOut() { sigmaRef.value?.getCamera().animatedUnzoom({ duration: 300 }) }

  function focusOnNode(nodeId: string) {
    if (!sigmaRef.value || !graphRef.value?.hasNode(nodeId)) return
    const dd = sigmaRef.value.getNodeDisplayData(nodeId)
    if (!dd) return
    sigmaRef.value.setCustomBBox(null)
    const cam = sigmaRef.value.getCamera()
    cam.animate({ x: dd.x, y: dd.y, ratio: cam.getState().ratio }, { duration: 400 })
    selectedNode.value = nodeId
    sigmaRef.value.refresh()
  }

  /* ---- Export ---- */

  async function exportAsImage(filename = 'graph') {
    if (!sigmaRef.value) return
    const container = sigmaRef.value.getContainer()
    const SKIPPED = new Set(['mouse', 'edgeLabels', 'hovers', 'hoverNodes'])
    const layers = [...container.querySelectorAll('canvas')]
      .map((c) => (c.className || '').replace('sigma-', ''))
      .filter((n) => n && !SKIPPED.has(n))

    downloadAsPNG(sigmaRef.value, {
      fileName: filename,
      layers,
      backgroundColor: '#ffffff',
      width: container.offsetWidth * 2,
      height: container.offsetHeight * 2,
    })
  }

  /* ---- Cleanup ---- */

  function destroy() {
    stopAnimatedLayout()
    stopPulseAnimation()
    if (sigmaRef.value) { sigmaRef.value.kill(); sigmaRef.value = null }
    graphRef.value = null
    hoveredNode.value = null
    selectedNode.value = null
    highlightedNodes.value = new Set()
    highlightedEdges.value = new Set()
  }

  onUnmounted(destroy)

  return {
    graphRef, sigmaRef, isLoading, error, currentLayout, isAnimating, simplifiedEdges,
    hoveredNode, selectedNode, nodeCount, edgeCount,
    buildGraphFromResults, mergeResults,
    initializeSigma, applyLayout, switchLayout, updateColors,
    applyStyleOverrides,
    zoomToFit, zoomIn, zoomOut, focusOnNode,
    exportAsImage, destroy,
    LAYOUT_TYPES,
  }
}
