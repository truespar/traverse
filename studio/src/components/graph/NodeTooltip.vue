<script setup lang="ts">
import { computed } from 'vue'
import { formatValue, inferType } from '../../utils/format'
import { useGdsStore } from '../../stores/gds'

const props = defineProps<{
  data: Record<string, unknown>
}>()

const emit = defineEmits<{
  close: []
}>()

const gds = useGdsStore()

const isEdge = computed(() => !!props.data.edgeType)

const properties = computed(() => {
  const p = (props.data.properties || {}) as Record<string, unknown>
  return Object.entries(p).slice(0, 20)
})

const labels = computed(() => {
  return (props.data.nodeLabels as string[]) || []
})

/** GDS value for this node from the active stream run, if any. The
 * styling layer keys on `fgId`, which the explorer puts on every node. */
const gdsBadge = computed<{ label: string; value: string } | null>(() => {
  if (isEdge.value) return null
  const run = gds.currentRun
  if (!run || run.mode !== 'stream') return null
  const fgId = props.data.fgId as number | undefined
  if (fgId === undefined) return null
  const v = run.values.get(fgId)
  if (v === undefined) return null
  // ML runs ("Show on graph" from the ML panel) register with a
  // synthetic algorithm name `ml:<modelName>`. Render them more
  // naturally - "predicted class: 1" - instead of leaking the
  // full model name + the generic "community N" formatting.
  const isMl = run.algorithm.name.startsWith('ml:')
  if (isMl) {
    // Pull the actual property name (e.g. `category`) out of the
    // synthetic run's config so the tooltip reads
    // "predicted category : fraud" instead of the generic
    // "predicted class : 1". User-attached class labels (via the
    // saved-model card's "Name the classes" editor) win when set;
    // otherwise we fall back to the integer.
    const target = typeof run.config.targetProperty === 'string'
      ? run.config.targetProperty
      : 'class'
    const classLabels = (run.config.classLabels as Record<string, string> | undefined) ?? {}
    const named = classLabels[String(v)]
    const display =
      named && named.trim().length > 0
        ? named
        : v.toLocaleString(undefined, {
            maximumFractionDigits: run.algorithm.outputKind === 'nodeScalar' ? 4 : 0,
          })
    return {
      label: `predicted ${target}`,
      value: display,
    }
  }
  const isCommunity = run.algorithm.outputKind === 'nodeCommunity'
  return {
    label: `${run.algorithm.displayName}`,
    value: isCommunity
      ? `community ${v.toLocaleString()}`
      : v.toLocaleString(undefined, { maximumFractionDigits: 4 }),
  }
})
</script>

<template>
  <div class="node-tooltip">
    <div class="node-tooltip__header">
      <!-- Edge header -->
      <template v-if="isEdge">
        <span class="fg-badge fg-badge--edge">{{ data.edgeType }}</span>
        <span class="node-tooltip__id">#{{ data.fgId ?? data.id }}</span>
      </template>
      <!-- Node header -->
      <template v-else>
        <span v-for="l in labels" :key="l" class="fg-badge">{{ l }}</span>
        <span class="node-tooltip__id">#{{ data.fgId ?? data.id }}</span>
      </template>
      <button class="fg-icon-btn" style="margin-left:auto" aria-label="Close" @click="emit('close')">
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M18 6 6 18" /><path d="m6 6 12 12" />
        </svg>
      </button>
    </div>
    <!-- GDS score badge (above properties for prominence) -->
    <div v-if="gdsBadge" class="node-tooltip__gds">
      <span class="node-tooltip__gds-label">{{ gdsBadge.label }}</span>
      <span class="node-tooltip__gds-value">{{ gdsBadge.value }}</span>
    </div>
    <div class="node-tooltip__body">
      <!-- Edge source -> target -->
      <div v-if="isEdge" class="node-tooltip__endpoints">
        <span class="node-tooltip__endpoint">#{{ data.sourceFgId ?? data.source }}</span>
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M5 12h14" /><path d="m12 5 7 7-7 7" />
        </svg>
        <span class="node-tooltip__endpoint">#{{ data.targetFgId ?? data.target }}</span>
      </div>
      <div v-for="[key, val] in properties" :key="key" class="node-tooltip__prop">
        <span class="node-tooltip__key">{{ key }}</span>
        <span class="node-tooltip__val">{{ formatValue(val) }}</span>
        <span class="node-tooltip__type">{{ inferType(val) }}</span>
      </div>
      <div v-if="properties.length === 0" class="node-tooltip__empty">No properties</div>
    </div>
  </div>
</template>

<style scoped>
.node-tooltip {
  position: absolute;
  bottom: 8px;
  right: 8px;
  width: 400px;
  max-height: 300px;
  background: var(--tv-bg-surface);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  box-shadow: var(--tv-shadow-lg);
  overflow: hidden;
  z-index: 10;
}

.node-tooltip__header {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 8px;
  border-bottom: 1px solid var(--tv-border-default);
}

.node-tooltip__id {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  font-family: var(--tv-font-mono);
}

.node-tooltip__body {
  padding: 4px 0;
  max-height: 240px;
  overflow-y: auto;
}

.node-tooltip__prop {
  display: flex;
  padding: 2px 8px;
  font-size: var(--tv-font-size-xs);
  gap: 8px;
}

.node-tooltip__key {
  color: var(--tv-text-secondary);
  min-width: 80px;
  flex-shrink: 0;
}

.node-tooltip__val {
  color: var(--tv-text-primary);
  font-family: var(--tv-font-mono);
  word-break: break-all;
  flex: 1;
  min-width: 0;
}

.node-tooltip__type {
  font-size: 10px;
  color: var(--tv-text-muted);
  font-family: var(--tv-font-mono);
  flex-shrink: 0;
}

.node-tooltip__empty {
  padding: 8px;
  text-align: center;
  color: var(--tv-text-muted);
  font-size: var(--tv-font-size-xs);
}

.fg-badge--edge {
  background: var(--tv-bg-hover);
  border: 1px solid var(--tv-border-default);
}

.node-tooltip__endpoints {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 4px 8px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  border-bottom: 1px solid var(--tv-border-subtle);
}

.node-tooltip__endpoint {
  font-family: var(--tv-font-mono);
}

.node-tooltip__gds {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 4px 8px;
  background: var(--tv-accent-subtle);
  border-bottom: 1px solid var(--tv-border-subtle);
  font-size: var(--tv-font-size-xs);
}
.node-tooltip__gds-label {
  color: var(--tv-accent);
  font-weight: 600;
}
.node-tooltip__gds-value {
  color: var(--tv-accent);
  font-family: var(--tv-font-mono);
}

/* Mobile: bump the tooltip wider (but not full-width - keep a margin
   on each side so the user can still see the graph behind it) and tall
   enough that property lists don't get truncated to a sliver. */
@media (max-width: 768px) {
  .node-tooltip {
    width: calc(100vw - 24px);
    max-width: 360px;
    max-height: 50vh;
    max-height: 50dvh;
  }
  .node-tooltip__body {
    max-height: calc(50vh - 60px);
    max-height: calc(50dvh - 60px);
  }
  .node-tooltip__key {
    min-width: 96px;
  }
}
</style>
