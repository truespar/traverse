<script setup lang="ts">
import { ref, computed, nextTick } from 'vue'
import { useQueryStore } from '../../stores/query'
import {
  cypherReference,
  CATEGORY_LABELS,
  type CypherCategory,
  type CypherRefEntry,
} from '../../data/cypher-reference'

const queryStore = useQueryStore()

const search = ref('')
const categoryFilter = ref<CypherCategory | ''>('')
const expandedId = ref<string | null>(null)

const filtered = computed(() => {
  let entries = cypherReference
  if (categoryFilter.value) {
    entries = entries.filter((e) => e.category === categoryFilter.value)
  }
  const q = search.value.trim().toLowerCase()
  if (q) {
    entries = entries.filter(
      (e) =>
        e.name.toLowerCase().includes(q) ||
        e.summary.toLowerCase().includes(q) ||
        (e.aliases && e.aliases.some((a) => a.toLowerCase().includes(q)))
    )
  }
  return entries
})

const grouped = computed(() => {
  const groups: { label: string; entries: CypherRefEntry[] }[] = []
  const map = new Map<string, CypherRefEntry[]>()
  for (const e of filtered.value) {
    const key = e.subcategory || 'General'
    let arr = map.get(key)
    if (!arr) {
      arr = []
      map.set(key, arr)
      groups.push({ label: key, entries: arr })
    }
    arr.push(e)
  }
  return groups
})

function toggle(id: string) {
  expandedId.value = expandedId.value === id ? null : id
}

function useExample(query: string) {
  queryStore.currentQuery = query
}

function goToRelated(id: string) {
  search.value = ''
  categoryFilter.value = ''
  expandedId.value = id
  nextTick(() => {
    const el = document.getElementById(`cypher-ref-${id}`)
    el?.scrollIntoView({ behavior: 'smooth', block: 'center' })
  })
}
</script>

<template>
  <div class="cypher-ref">
    <!-- Toolbar -->
    <div class="cypher-ref__toolbar">
      <span class="cypher-ref__title">Cypher Reference</span>
      <div class="cypher-ref__filters">
        <input
          v-model="search"
          class="cypher-ref__search"
          type="text"
          placeholder="Search commands..."
        />
        <select v-model="categoryFilter" class="cypher-ref__select">
          <option value="">All categories</option>
          <option v-for="(label, key) in CATEGORY_LABELS" :key="key" :value="key">
            {{ label }}
          </option>
        </select>
      </div>
      <span class="cypher-ref__count">{{ filtered.length }} commands</span>
    </div>

    <!-- Body -->
    <div class="cypher-ref__body">
      <div class="cypher-ref__content">
        <div v-if="filtered.length === 0" class="cypher-ref__empty">
          No matching entries.
        </div>

        <div v-for="group in grouped" :key="group.label" class="cypher-ref-group">
          <div class="cypher-ref-group__label">{{ group.label }}</div>

          <div
            v-for="entry in group.entries"
            :key="entry.id"
            :id="'cypher-ref-' + entry.id"
            class="cypher-ref-entry"
            :class="{ 'cypher-ref-entry--expanded': expandedId === entry.id }"
          >
            <!-- Collapsed header -->
            <button class="cypher-ref-entry__header" @click="toggle(entry.id)">
              <span class="cypher-ref-entry__name">{{ entry.name }}</span>
              <span class="cypher-ref-entry__summary">{{ entry.summary }}</span>
              <svg
                class="cypher-ref-entry__chevron"
                width="14" height="14" viewBox="0 0 24 24"
                fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"
              >
                <path d="m6 9 6 6 6-6" />
              </svg>
            </button>

            <!-- Expanded body -->
            <div v-if="expandedId === entry.id" class="cypher-ref-entry__body">
              <div class="cypher-ref-entry__columns">
                <!-- Left: syntax + description -->
                <div class="cypher-ref-entry__main">
                  <div class="cypher-ref-entry__section-label">Syntax</div>
                  <pre class="cypher-ref-entry__syntax">{{ entry.syntax }}</pre>

                  <div class="cypher-ref-entry__description">{{ entry.description }}</div>

                  <!-- Related -->
                  <div v-if="entry.related && entry.related.length" class="cypher-ref-entry__related">
                    <span class="cypher-ref-entry__section-label">Related</span>
                    <span
                      v-for="rid in entry.related"
                      :key="rid"
                      class="cypher-ref-entry__related-link"
                      @click="goToRelated(rid)"
                    >{{ cypherReference.find(e => e.id === rid)?.name || rid }}</span>
                  </div>
                </div>

                <!-- Right: examples -->
                <div v-if="entry.examples.length" class="cypher-ref-entry__examples">
                  <div class="cypher-ref-entry__section-label">Examples</div>
                  <div v-for="(ex, i) in entry.examples" :key="i" class="cypher-ref-example">
                    <div class="cypher-ref-example__header">
                      <span class="cypher-ref-example__label">{{ ex.label }}</span>
                      <button
                        class="cypher-ref-example__use"
                        title="Use in editor"
                        @click="useExample(ex.query)"
                      >
                        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                          <rect width="8" height="4" x="8" y="2" rx="1" ry="1" />
                          <path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" />
                        </svg>
                        Use
                      </button>
                    </div>
                    <pre class="cypher-ref-example__query">{{ ex.query }}</pre>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.cypher-ref {
  display: flex;
  flex-direction: column;
  height: 100%;
}

/* Toolbar */
.cypher-ref__toolbar {
  display: flex;
  align-items: center;
  gap: 12px;
  height: 38px;
  padding: 0 16px;
  border-bottom: 1px solid var(--tv-border-default);
  flex-shrink: 0;
  background: var(--tv-bg-surface);
}

.cypher-ref__title {
  font-size: 13px;
  font-weight: 600;
  color: var(--tv-text-primary);
  flex-shrink: 0;
}

.cypher-ref__filters {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: 1;
  min-width: 0;
}

.cypher-ref__search {
  width: 220px;
  padding: 4px 8px;
  font-size: 12px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-input);
  color: var(--tv-text-primary);
  outline: none;
}
.cypher-ref__search:focus {
  border-color: var(--tv-accent);
}

.cypher-ref__select {
  padding: 4px 6px;
  font-size: 12px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-input);
  color: var(--tv-text-primary);
  outline: none;
}

.cypher-ref__count {
  font-size: 11px;
  color: var(--tv-text-muted);
  flex-shrink: 0;
}

/* Body */
.cypher-ref__body {
  flex: 1;
  overflow-y: auto;
  padding: 8px 0;
}

.cypher-ref__content {
  padding: 0 16px;
}

.cypher-ref__empty {
  padding: 48px 16px;
  text-align: center;
  font-size: 13px;
  color: var(--tv-text-muted);
}

/* Groups */
.cypher-ref-group {
  margin-bottom: 8px;
}

.cypher-ref-group__label {
  padding: 12px 0 4px;
  font-size: 11px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--tv-text-muted);
}

/* Entries */
.cypher-ref-entry {
  border-bottom: 1px solid var(--tv-border-subtle, var(--tv-border-default));
}

.cypher-ref-entry__header {
  display: flex;
  align-items: baseline;
  gap: 12px;
  width: 100%;
  padding: 8px 12px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: none;
  color: var(--tv-text-primary);
  cursor: pointer;
  text-align: left;
  font-size: 13px;
}
.cypher-ref-entry__header:hover {
  background: var(--tv-bg-hover);
}

.cypher-ref-entry__name {
  font-family: var(--tv-font-mono, monospace);
  font-weight: 600;
  font-size: 13px;
  flex-shrink: 0;
  color: var(--tv-accent);
}

.cypher-ref-entry__summary {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--tv-text-secondary);
  font-size: 12px;
}

.cypher-ref-entry__chevron {
  flex-shrink: 0;
  transition: transform 0.15s;
  color: var(--tv-text-muted);
}
.cypher-ref-entry--expanded .cypher-ref-entry__chevron {
  transform: rotate(180deg);
}

/* Expanded body */
.cypher-ref-entry__body {
  padding: 4px 12px 16px;
}

.cypher-ref-entry__columns {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 24px;
}

.cypher-ref-entry__main {
  min-width: 0;
}

.cypher-ref-entry__examples {
  min-width: 0;
}

.cypher-ref-entry__section-label {
  font-size: 10px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--tv-text-muted);
  margin-top: 8px;
  margin-bottom: 4px;
}

.cypher-ref-entry__syntax {
  font-family: var(--tv-font-mono, monospace);
  font-size: 12px;
  line-height: 1.5;
  padding: 10px 12px;
  background: var(--tv-bg-elevated);
  border-radius: var(--tv-radius-sm);
  overflow-x: auto;
  white-space: pre-wrap;
  word-break: break-word;
  margin: 0;
  color: var(--tv-text-primary);
}

.cypher-ref-entry__description {
  font-size: 13px;
  line-height: 1.6;
  color: var(--tv-text-secondary);
  margin-top: 12px;
}

/* Examples */
.cypher-ref-example {
  margin-top: 8px;
}

.cypher-ref-example__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 4px;
}

.cypher-ref-example__label {
  font-size: 12px;
  color: var(--tv-text-secondary);
}

.cypher-ref-example__use {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 3px 8px;
  font-size: 11px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: none;
  color: var(--tv-text-muted);
  cursor: pointer;
}
.cypher-ref-example__use:hover {
  color: var(--tv-accent);
  border-color: var(--tv-accent);
}

.cypher-ref-example__query {
  font-family: var(--tv-font-mono, monospace);
  font-size: 12px;
  line-height: 1.5;
  padding: 10px 12px;
  background: var(--tv-bg-elevated);
  border-radius: var(--tv-radius-sm);
  overflow-x: auto;
  white-space: pre-wrap;
  word-break: break-word;
  margin: 0;
  color: var(--tv-text-primary);
}

/* Related links */
.cypher-ref-entry__related {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
  margin-top: 12px;
}

.cypher-ref-entry__related-link {
  font-family: var(--tv-font-mono, monospace);
  font-size: 12px;
  padding: 2px 8px;
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-elevated);
  color: var(--tv-accent);
  cursor: pointer;
}
.cypher-ref-entry__related-link:hover {
  text-decoration: underline;
}
</style>
