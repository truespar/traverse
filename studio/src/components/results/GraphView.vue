<script setup lang="ts">
import { ref, onMounted, onUnmounted, watch, nextTick } from 'vue'
import { useQueryStore } from '../../stores/query'
import { useSettingsStore } from '../../stores/settings'
import { useGraphExplorer } from '../../composables/useGraphExplorer'
import { useConnectionStore } from '../../stores/connection'
import { useStyleStore } from '../../stores/style'
import { useGdsStore } from '../../stores/gds'
import { api } from '../../utils/api'
import GraphControls from '../graph/GraphControls.vue'
import NodeTooltip from '../graph/NodeTooltip.vue'
import GdsDrawer from '../graph/GdsDrawer.vue'

const queryStore = useQueryStore()
const settings = useSettingsStore()
const conn = useConnectionStore()
const styleStore = useStyleStore()
const gdsStore = useGdsStore()
const containerRef = ref<HTMLElement | null>(null)

const tooltipData = ref<Record<string, unknown> | null>(null)
const tooltipPos = ref({ x: 0, y: 0 })
const limitExceeded = ref(false)
const limitInfo = ref('')
const gdsOpen = ref(false)

const explorer = useGraphExplorer({
  onNodeClick: (data) => {
    tooltipData.value = data
  },
  onEdgeClick: (data) => {
    tooltipData.value = data
  },
  onStageClick: () => {
    tooltipData.value = null
  },
})

function renderGraph() {
  if (!containerRef.value) return
  const isDark = settings.theme === 'dark'
  const container = containerRef.value

  limitExceeded.value = false
  limitInfo.value = ''

  const graph = explorer.buildGraphFromResults(
    queryStore.rows as unknown[][],
    isDark,
    queryStore.distinctNodes,
    queryStore.distinctEdges,
  )
  const nodeCount = graph?.order ?? 0
  const edgeCount = graph?.size ?? 0

  // Check render limits
  if (nodeCount > settings.maxNodes || edgeCount > settings.maxEdges) {
    limitExceeded.value = true
    limitInfo.value = `Graph too large to render (${nodeCount.toLocaleString()} nodes, ${edgeCount.toLocaleString()} edges). Adjust limits in Settings.`
    explorer.destroy()
    return
  }

  if (nodeCount > 500) {
    explorer.applyLayout(100, null, false, () => {
      explorer.initializeSigma(container, isDark)
      // Re-apply any active GDS / ML run colouring on top of the
      // freshly-built default styles. Without this, registering a
      // run *before* rendering the graph (e.g. ML "Show on graph"
      // -> MATCH query) leaves the new nodes uncoloured.
      explorer.applyStyleOverrides()
    })
  } else {
    explorer.initializeSigma(container, isDark)
    explorer.applyLayout(100, null, false)
    explorer.applyStyleOverrides()
  }
}


onMounted(() => {
  renderGraph()
})

// Re-render when results change
watch(() => queryStore.rows, () => {
  renderGraph()
})

// Redraw sigma on theme change (WebGL canvas needs full re-init)
watch(() => settings.theme, (t) => {
  if (!containerRef.value || !explorer.graphRef.value) return
  const isDark = t === 'dark'
  explorer.updateColors(isDark)
  explorer.initializeSigma(containerRef.value, isDark)
  nextTick(() => explorer.zoomToFit())
})

// Live-update graph when styles change
watch([() => styleStore.nodeStyles, () => styleStore.edgeStyles], () => {
  explorer.applyStyleOverrides()
}, { deep: true })

// Live-update when GDS run / styling / highlight state changes
watch(
  [
    () => gdsStore.currentRunId,
    () => gdsStore.styling,
    () => gdsStore.highlightedPathId,
    () => gdsStore.highlightedNodeIds,
    () => gdsStore.highlightedEdgeIds,
    () => gdsStore.highlightColor,
  ],
  () => {
    explorer.applyStyleOverrides()
  },
  { deep: true },
)

onUnmounted(() => {
  explorer.destroy()
})

// Double-click to expand
async function handleDblClickExpand() {
  if (!explorer.selectedNode.value || !explorer.graphRef.value) return
  const nodeAttrs = explorer.graphRef.value.getNodeAttributes(explorer.selectedNode.value)
  const fgId = nodeAttrs.fgId
  if (fgId === undefined) return

  try {
    const result = await api.query(
      `MATCH (n)-[r]-(m) WHERE id(n) = $id RETURN n,r,m LIMIT 50`,
      { id: fgId },
      conn.currentDatabase,
    )
    explorer.mergeResults(result.rows as unknown[][], settings.theme === 'dark')
    explorer.applyLayout(50, null, false)
    explorer.sigmaRef.value?.refresh()
  } catch {
    // ignore expansion errors
  }
}
</script>

<template>
  <div class="graph-view" @dblclick="handleDblClickExpand">
    <!-- Limit exceeded message -->
    <div v-if="limitExceeded" class="graph-view__limit">
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="12" cy="12" r="10" />
        <path d="M12 16v-4" />
        <path d="M12 8h.01" />
      </svg>
      <span>{{ limitInfo }}</span>
    </div>

    <!-- Normal graph canvas -->
    <template v-else>
      <div ref="containerRef" class="graph-view__canvas" />
      <div v-if="explorer.error.value" role="alert" class="graph-view__error">
        {{ explorer.error.value }}
      </div>
      <GraphControls
        :layout="explorer.currentLayout.value"
        :is-animating="explorer.isAnimating.value"
        :gds-open="gdsOpen"
        @zoom-in="explorer.zoomIn()"
        @zoom-out="explorer.zoomOut()"
        @zoom-fit="explorer.zoomToFit()"
        @relayout="explorer.applyLayout(100, null, true)"
        @switch-layout="explorer.switchLayout($event)"
        @export="explorer.exportAsImage()"
        @toggle-gds="gdsOpen = !gdsOpen"
      />
      <GdsDrawer :open="gdsOpen" @close="gdsOpen = false" />
      <NodeTooltip
        v-if="tooltipData"
        :data="tooltipData"
        @close="tooltipData = null"
      />
      <div class="graph-view__hint" v-if="explorer.nodeCount.value > 0">
        {{ explorer.nodeCount.value.toLocaleString() }} nodes, {{ explorer.edgeCount.value.toLocaleString() }} edges
        <template v-if="explorer.simplifiedEdges.value">, simplified</template>
        <template v-if="explorer.selectedNode.value">. Double-click to expand</template>
      </div>
    </template>
  </div>
</template>

<style scoped>
.graph-view {
  position: relative;
  width: 100%;
  height: 100%;
  background: var(--tv-graph-bg);
}

.graph-view__canvas {
  width: 100%;
  height: 100%;
}

.graph-view__hint {
  position: absolute;
  bottom: 8px;
  left: 8px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  pointer-events: none;
}

.graph-view__error {
  position: absolute;
  top: 12px;
  left: 12px;
  max-width: min(420px, 80%);
  padding: 10px 14px;
  background: var(--tv-bg-surface);
  color: var(--tv-text-primary);
  border: 1px solid var(--tv-border-default);
  border-radius: 6px;
  font-size: var(--tv-font-size-sm);
}

.graph-view__limit {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  height: 100%;
  color: var(--tv-text-muted);
  font-size: var(--tv-font-size-sm);
  padding: 24px;
  text-align: center;
}

</style>
