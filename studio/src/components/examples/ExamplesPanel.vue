<script setup lang="ts">
import { ref, computed, nextTick, onMounted } from 'vue'
import { useConnectionStore } from '../../stores/connection'
import { useSchemaStore } from '../../stores/schema'
import { useSettingsStore } from '../../stores/settings'
import { useStyleStore } from '../../stores/style'
import { useQueryStore } from '../../stores/query'
import { useMlStore } from '../../stores/ml'
import { api } from '../../utils/api'
import { useToast } from '../../composables/useToast'
import { EXAMPLE_GRAPHS } from '../../data/example-graphs'
import type { ExampleGraph } from '../../data/example-graphs'
import {
  fetchSamplesCatalog,
  downloadSampleBytes,
  formatSampleSize,
  samplesState,
  type SampleEntry,
} from '../../utils/samples-catalog'

const conn = useConnectionStore()
const schemaStore = useSchemaStore()
const settings = useSettingsStore()
const styleStore = useStyleStore()
const queryStore = useQueryStore()
const { showToast } = useToast()

const importing = ref<string | null>(null)
const loadedExample = ref<string | null>(null)
const error = ref<string | null>(null)

// Confirmation modal (shown when database already has data)
const confirmModal = ref<ExampleGraph | null>(null)
const confirmInput = ref('')
const confirmInputEl = ref<HTMLInputElement | null>(null)
const confirmValid = computed(
  () => confirmModal.value && confirmInput.value === confirmModal.value.name,
)

const CATEGORY_COLORS: Record<string, string> = {
  Entertainment: '#e07c3e',
  Business: '#3e8ee0',
  Social: '#9b59b6',
  Science: '#27ae60',
  Technology: '#e74c3c',
}

async function loadExample(example: ExampleGraph) {
  if (importing.value) return
  error.value = null

  // Database has data - confirm before clearing
  if (conn.nodeCount > 0) {
    confirmModal.value = example
    confirmInput.value = ''
    nextTick(() => confirmInputEl.value?.focus())
    return
  }

  await doLoadExample(example)
}

async function onConfirm() {
  if (!confirmModal.value || !confirmValid.value) return
  const example = confirmModal.value
  confirmModal.value = null
  await doLoadExample(example)
}

async function doLoadExample(example: ExampleGraph) {
  importing.value = example.id
  try {
    // Clear all existing data first
    await api.importCypher('MATCH (n) DETACH DELETE n', conn.currentDatabase)

    const result = await api.importCypher(example.cypher, conn.currentDatabase)

    await conn.pollHealth()
    await schemaStore.fetchSchema()

    // Reset styles and auto-assign fresh colors for the new schema
    styleStore.nodeStyles = {}
    styleStore.edgeStyles = {}
    styleStore.autoAssignMissing(schemaStore.labels, settings.theme === 'dark')
    if (schemaStore.labels.length > 0) {
      const pick = schemaStore.labels[Math.floor(Math.random() * schemaStore.labels.length)]
      styleStore.setNodeStyle(pick, { program: 'hexagon' })
    }

    api.save().catch(() => {})
    loadedExample.value = example.id

    const parts: string[] = []
    if (result.stats.nodes_created) parts.push(`${result.stats.nodes_created} nodes`)
    if (result.stats.relationships_created) parts.push(`${result.stats.relationships_created} relationships`)
    const summary = parts.length > 0 ? parts.join(', ') : 'data'
    showToast(`Loaded ${example.name}: ${summary}`, 'success')

    // Special-case: the fraud-detection example is built specifically
    // to demo the ML panel. Navigate there and pre-fill the wizard
    // with `category` instead of running the default sample query.
    if (example.id === 'example-fraud-detection') {
      settings.activePanel = 'ml'
      // The store reads target candidates on mount, but it may have
      // populated before the new graph was loaded - re-trigger.
      const mlStore = useMlStore()
      await mlStore.refreshTargetCandidates()
      ;(window as { __traverseMlPrefill?: string }).__traverseMlPrefill = 'category'
    } else {
      runQuery('MATCH (a)-[r]->(b) RETURN a, r, b LIMIT 250')
    }
  } catch (e: any) {
    error.value = e.message || 'Failed to load example'
    showToast(`Failed to load ${example.name}`, 'error')
  } finally {
    importing.value = null
  }
}

// -- Pre-built sample databases (from releases.truespar.com) --------
//
// AppShell mounts panels with `v-if`, so this component is destroyed
// every time the user switches activity-bar tabs. We hoist the
// catalog state into module-scope refs that survive unmount - the
// section keeps its "Loaded" badge and the cards stay visible when
// the user comes back. Catalog fetch only runs the first time
// anyone opens the panel in this tab.

const samples = samplesState.samples
const samplesError = samplesState.error
const samplesLoading = samplesState.loading
/** Per-sample progress: id -> "Downloading 42%" / "Loading..." / null. */
const sampleStatus = samplesState.status
const loadedSample = samplesState.loadedId

onMounted(() => {
  // Kick off the catalog fetch on first mount; subsequent mounts
  // re-use the cached state instantly.
  if (!samples.value.length && !samplesLoading.value && !samplesError.value) {
    samplesLoading.value = true
    fetchSamplesCatalog()
      .then((catalog) => {
        samples.value = catalog.samples
      })
      .catch((e: unknown) => {
        samplesError.value = e instanceof Error ? e.message : String(e)
      })
      .finally(() => {
        samplesLoading.value = false
      })
  }
})

async function loadSample(sample: SampleEntry) {
  if (importing.value || sampleStatus.value[sample.id]) return
  // Same confirmation guard as `loadExample` - wiping the current
  // graph deserves an explicit OK if it has data.
  if (conn.nodeCount > 0) {
    confirmSample.value = sample
    confirmSampleInput.value = ''
    nextTick(() => confirmSampleInputEl.value?.focus())
    return
  }
  await doLoadSample(sample)
}

async function onConfirmSample() {
  if (!confirmSample.value || !confirmSampleValid.value) return
  const sample = confirmSample.value
  confirmSample.value = null
  await doLoadSample(sample)
}

async function doLoadSample(sample: SampleEntry) {
  sampleStatus.value = { ...sampleStatus.value, [sample.id]: 'Downloading 0%' }
  importing.value = sample.id
  try {
    // 1. Fetch bytes from releases.truespar.com (with progress).
    const bytes = await downloadSampleBytes(sample, (loaded, total) => {
      const pct = total > 0 ? Math.round((loaded / total) * 100) : 0
      sampleStatus.value = {
        ...sampleStatus.value,
        [sample.id]: `Downloading ${pct}%`,
      }
    })
    // 2. Hand the blob to the active backend's upload path. Both
    //    HttpApiClient and WasmApiClient implement uploadDatabase
    //    + loadDatabase; the call site is backend-agnostic.
    sampleStatus.value = { ...sampleStatus.value, [sample.id]: 'Uploading...' }
    // `bytes.buffer` is always ArrayBuffer here (allocated by us in
    // `downloadSampleBytes`), but TS widens to ArrayBufferLike which
    // Blob's typedef rejects. Cast through to keep the constructor
    // happy.
    const blob = new Blob([bytes as unknown as Uint8Array<ArrayBuffer>])
    const uploadResult = await api.uploadDatabase(sample.id, blob)
    if (!uploadResult.loaded) {
      // HTTP mode (server stored the file but didn't auto-load) or
      // WASM mode (writes to OPFS then we explicitly load).
      sampleStatus.value = {
        ...sampleStatus.value,
        [sample.id]: 'Loading into memory...',
      }
      await api.loadDatabase(sample.id, `${sample.id}.tvdb`)
    }
    // 3. Wire up Studio state. Same dance as the Cypher loader.
    await conn.pollHealth()
    conn.currentDatabase = sample.id
    await schemaStore.fetchSchema()
    styleStore.nodeStyles = {}
    styleStore.edgeStyles = {}
    styleStore.autoAssignMissing(schemaStore.labels, settings.theme === 'dark')
    api.save().catch(() => {})

    loadedSample.value = sample.id

    // Persist starter queries as saved queries so they appear in
    // the Query Palette (Ctrl+P) alongside the user's own. Best-
    // effort - if any one fails (e.g. duplicate name), keep going.
    if (sample.starterQueries) {
      for (const sq of sample.starterQueries) {
        try {
          await api.saveQuery(`${sample.name}: ${sq.label}`, sq.query)
        } catch { /* ignore - likely already saved */ }
      }
      await queryStore.fetchSavedQueries()
    }

    showToast(`Loaded ${sample.name} (${sample.nodes.toLocaleString()} nodes, ${sample.edges.toLocaleString()} edges)`, 'success')
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e)
    showToast(`Failed to load ${sample.name}: ${msg}`, 'error')
  } finally {
    sampleStatus.value = { ...sampleStatus.value, [sample.id]: null }
    importing.value = null
  }
}

// Confirmation modal - reuse the same "type the name to confirm"
// pattern as the Cypher loader so the user can't accidentally wipe
// their active graph by clicking the wrong card.
const confirmSample = ref<SampleEntry | null>(null)
const confirmSampleInput = ref('')
const confirmSampleInputEl = ref<HTMLInputElement | null>(null)
const confirmSampleValid = computed(
  () => confirmSample.value && confirmSampleInput.value === confirmSample.value.name,
)

function formatSampleStat(n: number): string {
  if (n < 1000) return String(n)
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}K`
  return `${(n / 1_000_000).toFixed(n < 10_000_000 ? 1 : 0)}M`
}

function runQuery(query: string) {
  queryStore.currentQuery = query
  queryStore.executeQuery()
  settings.activePanel = 'schema'
}

function insertQuery(query: string) {
  runQuery(query)
}
</script>

<template>
  <div class="examples-panel">
    <!-- Confirmation modal when database already has data -->
    <Teleport to="body">
      <div v-if="confirmModal" class="replace-modal-overlay" @click.self="confirmModal = null">
        <div class="replace-modal">
          <div class="replace-modal__title">Replace existing data?</div>
          <p class="replace-modal__desc">
            All existing data in <strong>{{ conn.currentDatabase }}</strong> will be
            cleared and replaced with the <strong>{{ confirmModal.name }}</strong> example.
          </p>
          <label class="replace-modal__label">
            Type <strong>{{ confirmModal.name }}</strong> to confirm:
          </label>
          <input
            ref="confirmInputEl"
            v-model="confirmInput"
            class="replace-modal__input fg-input"
            :placeholder="confirmModal.name"
            @keydown.enter="confirmValid && onConfirm()"
            @keydown.escape="confirmModal = null"
          />
          <div class="replace-modal__actions">
            <button class="fg-btn" @click="confirmModal = null">Cancel</button>
            <button
              class="fg-btn fg-btn--danger"
              :disabled="!confirmValid"
              @click="onConfirm"
            >
              Replace
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <!-- Same modal for pre-built sample loading. Reuses the
         replace-modal styles. -->
    <Teleport to="body">
      <div v-if="confirmSample" class="replace-modal-overlay" @click.self="confirmSample = null">
        <div class="replace-modal">
          <div class="replace-modal__title">Replace existing data?</div>
          <p class="replace-modal__desc">
            Loading <strong>{{ confirmSample.name }}</strong> will replace the
            current graph in <strong>{{ conn.currentDatabase }}</strong>.
          </p>
          <label class="replace-modal__label">
            Type <strong>{{ confirmSample.name }}</strong> to confirm:
          </label>
          <input
            ref="confirmSampleInputEl"
            v-model="confirmSampleInput"
            class="replace-modal__input fg-input"
            :placeholder="confirmSample.name"
            @keydown.enter="confirmSampleValid && onConfirmSample()"
            @keydown.escape="confirmSample = null"
          />
          <div class="replace-modal__actions">
            <button class="fg-btn" @click="confirmSample = null">Cancel</button>
            <button
              class="fg-btn fg-btn--danger"
              :disabled="!confirmSampleValid"
              @click="onConfirmSample"
            >
              Replace
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <div class="examples-panel__header">
      <span class="examples-panel__title">Examples</span>
    </div>

    <div class="examples-panel__body">
      <p class="examples-panel__hint">
        Load a pre-built dataset to start exploring.
      </p>

      <div v-if="error" class="examples-panel__error">
        {{ error }}
      </div>

      <!-- Pre-built samples from releases.truespar.com. Shown above
           the Cypher imports because they load in seconds rather
           than running CREATE statement-by-statement. -->
      <template v-if="samples.length > 0 || samplesLoading">
        <div class="examples-panel__section-label">
          Pre-built databases
          <span class="examples-panel__section-hint">- instant load</span>
        </div>

        <div v-if="samplesLoading" class="examples-panel__hint">
          <span class="fg-spinner" style="width:10px;height:10px;border-width:1.5px" />
          Loading catalog...
        </div>
        <div v-if="samplesError" class="examples-panel__error">
          Sample catalog unavailable: {{ samplesError }}
        </div>

        <div
          v-for="sample in samples"
          :key="sample.id"
          class="examples-panel__card"
        >
          <div class="examples-panel__card-header">
            <span
              class="examples-panel__badge"
              :style="{ background: CATEGORY_COLORS[sample.category] || '#888' }"
            >
              {{ sample.category }}
            </span>
            <span class="examples-panel__size">{{ formatSampleSize(sample.bytes) }}</span>
          </div>

          <div class="examples-panel__card-name">{{ sample.name }}</div>
          <div class="examples-panel__card-desc">{{ sample.description }}</div>
          <div class="examples-panel__card-stats">
            {{ formatSampleStat(sample.nodes) }} nodes,
            {{ formatSampleStat(sample.edges) }} edges
          </div>

          <div class="examples-panel__card-actions">
            <button
              v-if="loadedSample !== sample.id"
              class="fg-btn fg-btn--primary fg-btn--sm"
              :disabled="importing !== null"
              @click="loadSample(sample)"
            >
              <span
                v-if="sampleStatus[sample.id]"
                class="fg-spinner"
                style="width: 12px; height: 12px; border-width: 1.5px"
              />
              {{ sampleStatus[sample.id] || 'Load' }}
            </button>
            <span v-else class="examples-panel__loaded">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <polyline points="20 6 9 17 4 12" />
              </svg>
              Loaded
            </span>
          </div>

          <!-- Starter queries - shown after the sample is loaded,
               mirrors the EXAMPLE_GRAPHS Cypher-import section. -->
          <div
            v-if="loadedSample === sample.id && sample.starterQueries && sample.starterQueries.length"
            class="examples-panel__queries"
          >
            <div class="examples-panel__queries-label">Starter queries</div>
            <button
              v-for="sq in sample.starterQueries"
              :key="sq.label"
              class="examples-panel__query-btn"
              :title="sq.query"
              @click="insertQuery(sq.query)"
            >
              {{ sq.label }}
            </button>
          </div>
        </div>

        <div class="examples-panel__section-label">
          Cypher imports
          <span class="examples-panel__section-hint">- rebuilt from CREATE statements</span>
        </div>
      </template>

      <div
        v-for="example in EXAMPLE_GRAPHS"
        :key="example.id"
        class="examples-panel__card"
      >
        <div class="examples-panel__card-header">
          <span
            class="examples-panel__badge"
            :style="{ background: CATEGORY_COLORS[example.category] || '#888' }"
          >
            {{ example.category }}
          </span>
        </div>

        <div class="examples-panel__card-name">{{ example.name }}</div>
        <div class="examples-panel__card-desc">{{ example.description }}</div>
        <div class="examples-panel__card-stats">
          ~{{ example.nodeCount }} nodes, ~{{ example.edgeCount }} edges
        </div>

        <div class="examples-panel__card-actions">
          <button
            v-if="loadedExample !== example.id"
            class="fg-btn fg-btn--primary fg-btn--sm"
            :disabled="importing !== null"
            @click="loadExample(example)"
          >
            <span
              v-if="importing === example.id"
              class="fg-spinner"
              style="width: 12px; height: 12px; border-width: 1.5px"
            />
            {{ importing === example.id ? 'Loading...' : 'Load' }}
          </button>

          <template v-else>
            <span class="examples-panel__loaded">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <polyline points="20 6 9 17 4 12" />
              </svg>
              Loaded
            </span>
          </template>
        </div>

        <!-- Starter queries (shown after load) -->
        <div v-if="loadedExample === example.id" class="examples-panel__queries">
          <div class="examples-panel__queries-label">Starter queries</div>
          <button
            v-for="sq in example.starterQueries"
            :key="sq.label"
            class="examples-panel__query-btn"
            :title="sq.query"
            @click="insertQuery(sq.query)"
          >
            {{ sq.label }}
          </button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.examples-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.examples-panel__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 12px;
  border-bottom: 1px solid var(--tv-border-default);
  height: 38px;
  flex-shrink: 0;
}

.examples-panel__title {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}

.examples-panel__body {
  flex: 1;
  overflow-y: auto;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.examples-panel__hint {
  margin: 0;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
}

.examples-panel__error {
  padding: 6px 10px;
  border-radius: var(--tv-radius-sm);
  background: color-mix(in srgb, var(--tv-status-error) 12%, var(--tv-bg-elevated));
  border: 1px solid color-mix(in srgb, var(--tv-status-error) 30%, transparent);
  color: var(--tv-status-error);
  font-size: var(--tv-font-size-xs);
}

.examples-panel__card {
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  padding: 10px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  background: var(--tv-bg-elevated);
}

.examples-panel__card-header {
  display: flex;
  align-items: center;
  gap: 6px;
}

.examples-panel__badge {
  display: inline-block;
  padding: 1px 7px;
  border-radius: 9px;
  font-size: 10px;
  font-weight: 600;
  color: var(--tv-text-inverse);
  line-height: 16px;
}

/* Section divider - "Pre-built databases" / "Cypher imports" between
   the two groups of cards. Visually muted; meant as a label, not a
   header. */
.examples-panel__section-label {
  font-size: 11px;
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
  padding: 16px 0 4px;
}
.examples-panel__section-label:first-child {
  padding-top: 4px;
}
.examples-panel__section-hint {
  font-weight: 400;
  text-transform: none;
  letter-spacing: 0;
  margin-left: 6px;
  color: var(--tv-text-muted);
}

/* On-disk size pill on the right edge of each sample card header. */
.examples-panel__size {
  margin-left: auto;
  font-size: 11px;
  color: var(--tv-text-muted);
  font-family: var(--tv-font-mono);
}

.examples-panel__card-name {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}

.examples-panel__card-desc {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  line-height: 1.4;
}

.examples-panel__card-stats {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
}

.examples-panel__card-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 2px;
}

.examples-panel__loaded {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
  color: var(--tv-status-success);
}

.examples-panel__queries {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin-top: 2px;
  padding-top: 6px;
  border-top: 1px solid var(--tv-border-default);
}

.examples-panel__queries-label {
  font-size: 10px;
  font-weight: 500;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.examples-panel__query-btn {
  display: block;
  width: 100%;
  text-align: left;
  padding: 4px 8px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-base);
  color: var(--tv-accent);
  font-size: var(--tv-font-size-xs);
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s;
}

.examples-panel__query-btn:hover {
  background: var(--tv-accent-subtle);
  border-color: var(--tv-accent);
}

/* Replace-database confirmation modal */
.replace-modal-overlay {
  position: fixed;
  inset: 0;
  z-index: 9999;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.5);
}

.replace-modal {
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  padding: 24px;
  max-width: 400px;
  width: 90%;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.replace-modal__title {
  font-size: var(--tv-font-size-md);
  font-weight: 600;
  color: var(--tv-text-primary);
}

.replace-modal__desc {
  font-size: var(--tv-font-size-sm);
  color: var(--tv-text-secondary);
  line-height: 1.5;
  margin: 0;
}

.replace-modal__label {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
}

.replace-modal__input {
  font-size: var(--tv-font-size-sm);
}

.replace-modal__actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 4px;
}
</style>
