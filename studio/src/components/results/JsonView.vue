<script setup lang="ts">
import { computed } from 'vue'
import { useQueryStore } from '../../stores/query'

const query = useQueryStore()

// Rendering 10 k+ node/edge rows as a single pretty-printed <pre> can produce
// hundreds of MB of layout work and OOM the browser. Cap the rendered view
// and let users Copy to get the full JSON.
const MAX_RENDERED_ROWS = 500

const renderedRows = computed(() => {
  return query.rows.length > MAX_RENDERED_ROWS
    ? query.rows.slice(0, MAX_RENDERED_ROWS)
    : query.rows
})

const hiddenRowCount = computed(() => query.rows.length - renderedRows.value.length)

const jsonText = computed(() => {
  const data = {
    columns: query.columns,
    rows: renderedRows.value,
    stats: query.stats,
  }
  return JSON.stringify(data, null, 2)
})

function copyJson() {
  // Always copy the FULL result, not the capped view.
  const full = {
    columns: query.columns,
    rows: query.rows,
    stats: query.stats,
  }
  navigator.clipboard.writeText(JSON.stringify(full, null, 2))
}
</script>

<template>
  <div class="json-view">
    <div class="json-view__actions">
      <button class="fg-btn fg-btn--sm" @click="copyJson">Copy JSON</button>
      <span v-if="hiddenRowCount > 0" class="json-view__notice">
        Showing first {{ renderedRows.length.toLocaleString() }} rows
        ({{ hiddenRowCount.toLocaleString() }} more - Copy JSON includes everything).
      </span>
    </div>
    <pre class="json-view__content">{{ jsonText }}</pre>
  </div>
</template>

<style scoped>
.json-view {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.json-view__actions {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 4px 8px;
  flex-shrink: 0;
}

.json-view__notice {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  margin-left: 4px;
}

.json-view__content {
  flex: 1;
  overflow: auto;
  padding: 8px 12px;
  font-family: var(--tv-font-mono);
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-primary);
  line-height: 1.6;
  white-space: pre-wrap;
  word-break: break-all;
  margin: 0;
}
</style>
