<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, nextTick } from 'vue'
import { useQueryStore, type HistoryEntry } from '../../stores/query'

const emit = defineEmits<{
  close: []
  select: [payload: { query: string }]
}>()

const queryStore = useQueryStore()

const searchInput = ref<HTMLInputElement | null>(null)
const filter = ref('')
const highlightIndex = ref(0)

interface PaletteItem {
  type: 'saved' | 'history'
  name?: string
  query: string
  timestamp?: number
  duration_ms?: number
  rowCount?: number
  error?: string
}

const filteredItems = computed<PaletteItem[]>(() => {
  const q = filter.value.toLowerCase()
  const items: PaletteItem[] = []

  for (const s of queryStore.savedQueries) {
    if (!q || s.name.toLowerCase().includes(q) || s.query.toLowerCase().includes(q)) {
      items.push({ type: 'saved', name: s.name, query: s.query })
    }
  }

  for (const h of queryStore.queryHistory.slice(0, 50)) {
    if (!q || h.query.toLowerCase().includes(q)) {
      items.push({
        type: 'history',
        query: h.query,
        timestamp: h.timestamp,
        duration_ms: h.duration_ms,
        rowCount: h.rowCount,
        error: h.error,
      })
    }
  }

  return items
})

const savedItems = computed(() => filteredItems.value.filter((i) => i.type === 'saved'))
const historyItems = computed(() => filteredItems.value.filter((i) => i.type === 'history'))

function selectItem(item: PaletteItem) {
  emit('select', { query: item.query })
  emit('close')
}

function deleteSaved(name: string, event: Event) {
  event.stopPropagation()
  queryStore.removeSavedQuery(name)
}

function onKeydown(e: KeyboardEvent) {
  const total = filteredItems.value.length
  if (e.key === 'ArrowDown') {
    e.preventDefault()
    highlightIndex.value = total > 0 ? (highlightIndex.value + 1) % total : 0
    scrollToHighlighted()
  } else if (e.key === 'ArrowUp') {
    e.preventDefault()
    highlightIndex.value = total > 0 ? (highlightIndex.value - 1 + total) % total : 0
    scrollToHighlighted()
  } else if (e.key === 'Enter') {
    e.preventDefault()
    const item = filteredItems.value[highlightIndex.value]
    if (item) selectItem(item)
  } else if (e.key === 'Escape') {
    emit('close')
  }
}

function scrollToHighlighted() {
  nextTick(() => {
    const el = document.querySelector('.query-palette__item--active')
    el?.scrollIntoView({ block: 'nearest' })
  })
}

function formatTime(ts: number): string {
  const d = new Date(ts)
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
}

function globalIndex(type: 'saved' | 'history', localIdx: number): number {
  if (type === 'saved') return localIdx
  return savedItems.value.length + localIdx
}

onMounted(() => {
  nextTick(() => searchInput.value?.focus())
})
</script>

<template>
  <div class="query-palette__backdrop" @click="$emit('close')">
    <div class="query-palette" role="dialog" aria-modal="true" aria-label="Query palette" @click.stop>
      <input
        ref="searchInput"
        v-model="filter"
        class="query-palette__search fg-input"
        placeholder="Search saved queries and history..."
        @keydown="onKeydown"
        @input="highlightIndex = 0"
      />

      <div class="query-palette__list">
        <template v-if="filteredItems.length === 0">
          <div class="query-palette__empty">No matching queries</div>
        </template>

        <!-- Saved section -->
        <template v-if="savedItems.length > 0">
          <div class="query-palette__section-header">Saved</div>
          <!-- Each saved item is a clickable row that *contains* a
               delete button. The row can't be a `<button>` (HTML
               forbids nested buttons), so it's a div with role/key
               handlers. The inner delete is a real button so it
               still gets keyboard focus and ARIA. -->
          <div
            v-for="(item, i) in savedItems"
            :key="'s-' + i"
            class="query-palette__item"
            role="button"
            tabindex="0"
            :class="{ 'query-palette__item--active': highlightIndex === globalIndex('saved', i) }"
            @click="selectItem(item)"
            @keydown.enter.prevent="selectItem(item)"
            @keydown.space.prevent="selectItem(item)"
            @mouseenter="highlightIndex = globalIndex('saved', i)"
          >
            <svg class="query-palette__icon" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z" />
            </svg>
            <div class="query-palette__item-content">
              <div class="query-palette__item-name">{{ item.name }}</div>
              <div class="query-palette__item-query">{{ item.query }}</div>
            </div>
            <button class="query-palette__delete" @click.stop="deleteSaved(item.name!, $event)" title="Delete saved query" aria-label="Delete saved query">&times;</button>
          </div>
        </template>

        <!-- History section -->
        <template v-if="historyItems.length > 0">
          <div class="query-palette__section-header">History</div>
          <button
            v-for="(item, i) in historyItems"
            :key="'h-' + i"
            class="query-palette__item"
            :class="{
              'query-palette__item--active': highlightIndex === globalIndex('history', i),
              'query-palette__item--error': !!item.error,
            }"
            @click="selectItem(item)"
            @mouseenter="highlightIndex = globalIndex('history', i)"
          >
            <svg class="query-palette__icon" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <circle cx="12" cy="12" r="10" /><path d="M12 6v6l4 2" />
            </svg>
            <div class="query-palette__item-content">
              <div class="query-palette__item-query">{{ item.query }}</div>
              <div class="query-palette__item-meta">
                <span>{{ formatTime(item.timestamp!) }}</span>
                <span>{{ item.duration_ms }} ms</span>
                <span v-if="!item.error">{{ item.rowCount }} rows</span>
                <span v-else class="query-palette__item-meta--error">Error</span>
              </div>
            </div>
          </button>
        </template>
      </div>
    </div>
  </div>
</template>

<style scoped>
.query-palette__backdrop {
  position: fixed;
  inset: 0;
  z-index: 50;
  display: flex;
  justify-content: center;
  background: var(--tv-bg-overlay);
}

.query-palette {
  position: absolute;
  top: 12px;
  width: 480px;
  max-height: 400px;
  display: flex;
  flex-direction: column;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  box-shadow: var(--tv-shadow-xl);
  overflow: hidden;
}

.query-palette__search {
  flex-shrink: 0;
  margin: 8px;
  font-size: var(--tv-font-size-sm);
}

.query-palette__list {
  flex: 1;
  overflow-y: auto;
  padding-bottom: 4px;
}

.query-palette__empty {
  padding: 24px 16px;
  text-align: center;
  color: var(--tv-text-muted);
  font-size: var(--tv-font-size-sm);
}

.query-palette__section-header {
  padding: 6px 12px 4px;
  font-size: 11px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  color: var(--tv-text-muted);
}

.query-palette__item {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  padding: 6px 12px;
  cursor: pointer;
  width: 100%;
  border: none;
  background: none;
  text-align: left;
  font-family: inherit;
}

.query-palette__item:hover,
.query-palette__item--active {
  background: var(--tv-accent-subtle);
}

.query-palette__item--error {
  border-left: 2px solid var(--tv-status-error);
}

.query-palette__icon {
  flex-shrink: 0;
  margin-top: 2px;
  color: var(--tv-text-muted);
}

.query-palette__item-content {
  flex: 1;
  min-width: 0;
}

.query-palette__item-name {
  font-size: var(--tv-font-size-sm);
  font-weight: 500;
  color: var(--tv-text-primary);
  margin-bottom: 1px;
}

.query-palette__item-query {
  font-family: var(--tv-font-mono);
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.query-palette__item-meta {
  display: flex;
  gap: 8px;
  font-size: 10px;
  color: var(--tv-text-muted);
  margin-top: 2px;
}

.query-palette__item-meta--error {
  color: var(--tv-status-error);
}

.query-palette__delete {
  flex-shrink: 0;
  background: none;
  border: none;
  color: var(--tv-text-muted);
  cursor: pointer;
  font-size: 16px;
  line-height: 1;
  padding: 0 2px;
  opacity: 0;
}

.query-palette__item:hover .query-palette__delete {
  opacity: 1;
}

.query-palette__delete:hover {
  color: var(--tv-status-error);
}
</style>
