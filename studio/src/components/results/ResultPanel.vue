<script setup lang="ts">
import { ref, onMounted, onBeforeUnmount, watch, nextTick } from 'vue'
import { useQueryStore } from '../../stores/query'
import GraphView from './GraphView.vue'
import TableView from './TableView.vue'
import JsonView from './JsonView.vue'

const query = useQueryStore()
const showTimingDetail = ref(false)
const fastAnimKey = ref(0)
const timingToggleRef = ref<HTMLElement | null>(null)
const timingPopoverPos = ref({ top: 0, right: 0 })

function toggleTimingDetail(e: MouseEvent) {
  showTimingDetail.value = !showTimingDetail.value
  if (showTimingDetail.value) {
    const el = (e.currentTarget as HTMLElement) ?? timingToggleRef.value
    if (el) {
      const r = el.getBoundingClientRect()
      timingPopoverPos.value = {
        top: r.bottom + 6,
        right: Math.max(8, window.innerWidth - r.right),
      }
    }
  }
}

watch(() => query.isExecuting, (executing, was) => {
  if (was && !executing && query.timeMs > 0 && query.timeMs < 100) {
    // bump key to re-trigger the animation on each fast query
    fastAnimKey.value++
  }
})

function formatTime(ms: number): string {
  if (ms < 1) return `${(ms * 1000).toFixed(0)} us`
  return `${ms.toFixed(1)} ms`
}

function onDocClick() {
  showTimingDetail.value = false
}

onMounted(() => document.addEventListener('click', onDocClick))
onBeforeUnmount(() => document.removeEventListener('click', onDocClick))

watch(() => query.isExecuting, (v) => { if (v) showTimingDetail.value = false })
</script>

<template>
  <div class="result-panel">
    <!-- Error display -->
    <div v-if="query.error" class="result-panel__error">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="12" cy="12" r="10" /><path d="m15 9-6 6" /><path d="m9 9 6 6" />
      </svg>
      {{ query.error }}
    </div>

    <!-- Tabs + result area -->
    <template v-if="query.hasResults">
      <div class="fg-tabs">
        <template v-if="query.columns.length > 0">
          <button
            v-if="query.hasGraphData"
            class="fg-tab"
            :class="{ 'fg-tab--active': query.resultViewMode === 'graph' }"
            @click="query.resultViewMode = 'graph'"
          >
            Graph
          </button>
          <button
            class="fg-tab"
            :class="{ 'fg-tab--active': query.resultViewMode === 'table' }"
            @click="query.resultViewMode = 'table'"
          >
            Table
          </button>
          <button
            class="fg-tab"
            :class="{ 'fg-tab--active': query.resultViewMode === 'json' }"
            @click="query.resultViewMode = 'json'"
          >
            JSON
          </button>
        </template>

        <div style="flex:1" />

        <!-- Stats -->
        <div class="result-panel__stats-wrap">
          <span class="result-panel__stats">
            <span v-if="query.columns.length > 0" class="result-panel__stat">{{ query.rows.length.toLocaleString() }}<span v-if="query.truncated" class="result-panel__truncated"> / {{ query.totalRows.toLocaleString() }}</span>{{ query.rows.length === 1 ? ' row' : ' rows' }}</span>
            <span v-if="query.returnedNodes > 0" class="result-panel__stat">{{ query.returnedNodes.toLocaleString() }} {{ query.returnedNodes === 1 ? 'node' : 'nodes' }}</span>
            <span v-if="query.returnedEdges > 0" class="result-panel__stat">{{ query.returnedEdges.toLocaleString() }} {{ query.returnedEdges === 1 ? 'edge' : 'edges' }}</span>
            <template v-if="query.stats">
              <span v-if="query.stats.nodes_created" class="result-panel__stat">{{ query.stats.nodes_created }} created</span>
              <span v-if="query.stats.nodes_deleted" class="result-panel__stat">{{ query.stats.nodes_deleted }} deleted</span>
              <span v-if="query.stats.relationships_created" class="result-panel__stat">{{ query.stats.relationships_created }} rels created</span>
              <span v-if="query.stats.relationships_deleted" class="result-panel__stat">{{ query.stats.relationships_deleted }} rels deleted</span>
              <span v-if="query.stats.properties_set" class="result-panel__stat">{{ query.stats.properties_set }} props set</span>
            </template>
            <span v-if="query.timeMs" class="result-panel__stat"><span
                ref="timingToggleRef"
                :key="fastAnimKey"
                class="result-panel__timing-toggle"
                :class="{ 'result-panel__timing-toggle--fast': query.timeMs < 100 }"
                @click.stop="toggleTimingDetail"
              >{{ formatTime(query.timeMs) }} <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6" /></svg></span></span>
          </span>
          <!-- Timing breakdown popover. Teleported to body so the
               .fg-tabs `overflow: hidden` (needed for horizontal
               scroll) doesn't clip it. -->
          <Teleport to="body">
          <div
            v-if="showTimingDetail"
            class="result-panel__timing-popover"
            :style="{ top: timingPopoverPos.top + 'px', right: timingPopoverPos.right + 'px' }"
            @click.stop>
            <div class="result-panel__timing-row">
              <span class="result-panel__timing-value">{{ formatTime(query.parsingTimeMs) }}</span>
              <span class="result-panel__timing-label">parse</span>
            </div>
            <div class="result-panel__timing-row">
              <span class="result-panel__timing-value">{{ formatTime(query.planningTimeMs) }}</span>
              <span class="result-panel__timing-label">plan</span>
            </div>
            <div class="result-panel__timing-row">
              <span class="result-panel__timing-value">{{ formatTime(query.executionTimeMs) }}</span>
              <span class="result-panel__timing-label">execute</span>
            </div>
            <div class="result-panel__timing-divider" />
            <div class="result-panel__timing-row">
              <span class="result-panel__timing-value">{{ formatTime(query.timeMs) }}</span>
              <span class="result-panel__timing-label">server total</span>
            </div>
            <div class="result-panel__timing-row">
              <span class="result-panel__timing-value">{{ formatTime(query.roundtripMs) }}</span>
              <span class="result-panel__timing-label">roundtrip</span>
            </div>
          </div>
          </Teleport>
        </div>
      </div>

      <div class="result-panel__content">
        <template v-if="query.columns.length > 0">
          <GraphView v-if="query.resultViewMode === 'graph' && query.hasGraphData" />
          <TableView v-else-if="query.resultViewMode === 'table'" />
          <JsonView v-else-if="query.resultViewMode === 'json'" />
        </template>
        <div v-else class="result-panel__write-summary">
          {{ query.stats && (query.stats.nodes_created || query.stats.nodes_deleted || query.stats.relationships_created || query.stats.relationships_deleted || query.stats.properties_set) ? 'Query completed successfully' : 'Query completed - no changes' }}
        </div>
      </div>
    </template>

    <!-- Empty state -->
    <div v-if="!query.hasResults && !query.error && !query.isExecuting" class="result-panel__empty">
      <span>Run a query to see results</span>
    </div>
  </div>
</template>

<style scoped>
.result-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--tv-bg-surface);
}

.result-panel__error {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  margin: 8px;
  background: color-mix(in srgb, var(--tv-status-error) 10%, transparent);
  border: 1px solid color-mix(in srgb, var(--tv-status-error) 30%, transparent);
  border-radius: var(--tv-radius-md);
  color: var(--tv-status-error);
  font-size: var(--tv-font-size-sm);
}

.result-panel__content {
  flex: 1;
  min-height: 0;
  overflow: hidden;
}

.result-panel__stats-wrap {
  position: relative;
  display: flex;
  align-items: center;
  padding-right: 8px;
}

.result-panel__stats {
  /* Plain inline text - `display: flex` would treat each text run as a separate
   * flex item and collapse the whitespace at item boundaries, producing
   * `34,496rows` because the leading space in ` rows` sits at the start of a
   * flex item box. */
  display: inline;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  white-space: nowrap;
}

.result-panel__stat + .result-panel__stat::before {
  content: ', ';
}

.result-panel__timing-toggle {
  cursor: pointer;
  user-select: none;
  position: relative;
  padding: 2px 5px;
  border-radius: var(--tv-radius-md);
  z-index: 0;
}

.result-panel__timing-toggle:hover {
  color: var(--tv-text-default);
}

.result-panel__timing-toggle--fast {
  color: var(--tv-status-success);
}

.result-panel__timing-toggle--fast::before {
  content: '';
  position: absolute;
  inset: -1px;
  border-radius: var(--tv-radius-md);
  background: conic-gradient(
    from var(--_border-angle),
    transparent 0%,
    transparent 65%,
    color-mix(in srgb, var(--tv-status-success) 40%, transparent) 74%,
    var(--tv-status-success) 80%,
    color-mix(in srgb, var(--tv-status-success) 40%, transparent) 86%,
    transparent 95%,
    transparent 100%
  );
  z-index: -1;
  animation: border-spin 2.8s linear forwards;
}

.result-panel__timing-toggle--fast::after {
  content: '';
  position: absolute;
  inset: 1px;
  border-radius: calc(var(--tv-radius-md) - 1px);
  background: var(--tv-bg-surface);
  z-index: -1;
}

@property --_border-angle {
  syntax: '<angle>';
  initial-value: 0deg;
  inherits: false;
}

@keyframes border-spin {
  0%   { --_border-angle: 0deg;    opacity: 0; }
  3%   {                           opacity: 1; }
  75%  { --_border-angle: 720deg;  opacity: 1; }
  100% { --_border-angle: 720deg;  opacity: 0; }
}

.result-panel__timing-popover {
  position: fixed;
  z-index: 200;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  padding: 8px 12px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.15);
  min-width: 160px;
}

.result-panel__timing-row {
  display: flex;
  justify-content: space-between;
  gap: 16px;
  padding: 2px 0;
}

.result-panel__timing-value {
  font-variant-numeric: tabular-nums;
  color: var(--tv-text-default);
}

.result-panel__timing-label {
  color: var(--tv-text-muted);
}

.result-panel__timing-divider {
  border-top: 1px solid var(--tv-border-default);
  margin: 4px 0;
}

.result-panel__write-summary {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  color: var(--tv-status-success);
  font-size: var(--tv-font-size-sm);
  font-weight: 500;
}

.result-panel__truncated {
  color: var(--tv-status-warning);
  font-weight: 500;
}

.result-panel__empty {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  color: var(--tv-text-muted);
  font-size: var(--tv-font-size-sm);
}
</style>
