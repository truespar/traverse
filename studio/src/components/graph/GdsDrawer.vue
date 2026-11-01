<script setup lang="ts">
import { ref, computed, onMounted, watch } from 'vue'
import { useGdsStore, valueKind, type GdsRun, type GdsStylingMode } from '../../stores/gds'
import { useSchemaStore } from '../../stores/schema'
import { useToast } from '../../composables/useToast'
import type { GdsAlgorithm, GdsConfigParam, GdsMode } from '../../utils/api'
import FgSelect from '../common/FgSelect.vue'
import DisclosureIcon from '../common/DisclosureIcon.vue'

const props = defineProps<{
  open: boolean
}>()

const emit = defineEmits<{
  close: []
  run: [runId: string]
}>()

const gds = useGdsStore()
const schema = useSchemaStore()
const toast = useToast()

const selectedAlgorithm = ref<GdsAlgorithm | null>(null)
const mode = ref<GdsMode>('stream')
const orientation = ref<'NATURAL' | 'REVERSE' | 'UNDIRECTED'>('NATURAL')
const nodeLabels = ref<string[]>([])
const edgeTypes = ref<string[]>([])
const writeProperty = ref('')
const showAdvanced = ref(false)
// Per-algorithm config values (keyed by config param key)
const algoConfig = ref<Record<string, unknown>>({})
const collapsedCategories = ref(new Set<string>())

onMounted(() => {
  gds.ensureAlgorithmsLoaded()
  schema.fetchSchema()
})

// Reset config when selecting a different algorithm.
watch(selectedAlgorithm, (a) => {
  if (!a) {
    algoConfig.value = {}
    return
  }
  const next: Record<string, unknown> = {}
  for (const p of a.configSchema) next[p.key] = p.default
  algoConfig.value = next
  // Sane mode default: write requires user opt-in.
  if (!a.modes.includes(mode.value)) mode.value = a.modes[0] ?? 'stream'
  if (mode.value === 'write' && !writeProperty.value) {
    writeProperty.value = a.name + 'Score'
  }
  showAdvanced.value = false
})

const categoryLabel: Record<string, string> = {
  centrality: 'Centrality',
  community: 'Community Detection',
  paths: 'Path Finding',
  dag: 'DAG',
  similarity: 'Node Similarity',
  linkPrediction: 'Link Prediction',
  embeddings: 'Embeddings',
}

function toggleCategory(cat: string) {
  if (collapsedCategories.value.has(cat)) collapsedCategories.value.delete(cat)
  else collapsedCategories.value.add(cat)
}

const orientationOptions = [
  { value: 'NATURAL', label: 'NATURAL - follow outgoing edges' },
  { value: 'REVERSE', label: 'REVERSE - follow incoming edges' },
  { value: 'UNDIRECTED', label: 'UNDIRECTED - both directions' },
]

const labelOptions = computed(() =>
  schema.labels.map((l) => ({ value: l, label: l })),
)
const edgeTypeOptions = computed(() =>
  schema.relationshipTypes.map((t) => ({ value: t, label: t })),
)

const canRun = computed(() => {
  if (!selectedAlgorithm.value || gds.isRunning) return false
  if (mode.value === 'write' && !writeProperty.value.trim()) return false
  // Block Run if the algorithm has required params the user hasn't
  // filled in. The error otherwise comes back from the executor as a
  // confusing "X is required" - the form should catch it.
  for (const p of selectedAlgorithm.value.configSchema) {
    if (!p.required) continue
    const v = algoConfig.value[p.key]
    if (v === undefined || v === null) return false
    if (typeof v === 'string' && v.trim() === '') return false
    if (Array.isArray(v) && v.length === 0) return false
  }
  return true
})

const configWithBase = computed<Record<string, unknown>>(() => {
  const cfg: Record<string, unknown> = { ...algoConfig.value }
  if (nodeLabels.value.length > 0) cfg.nodeLabels = [...nodeLabels.value]
  if (edgeTypes.value.length > 0) cfg.relationshipTypes = [...edgeTypes.value]
  if (orientation.value !== 'NATURAL') cfg.orientation = orientation.value
  if (mode.value === 'write') cfg.writeProperty = writeProperty.value.trim()
  return cfg
})

function paramVisible(p: GdsConfigParam): boolean {
  return !p.advanced || showAdvanced.value
}

const advancedParams = computed(() =>
  selectedAlgorithm.value?.configSchema.filter((p) => p.advanced) ?? [],
)
const basicParams = computed(() =>
  selectedAlgorithm.value?.configSchema.filter((p) => !p.advanced) ?? [],
)

async function run() {
  if (!selectedAlgorithm.value) return
  try {
    const r = await gds.runAlgorithm(
      selectedAlgorithm.value,
      mode.value,
      configWithBase.value,
    )
    toast.showToast(formatRunSummary(r), 'success')
    emit('run', r.id)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    toast.showToast(`Algorithm failed: ${msg}`, 'error')
  }
}

function selectAlgorithm(a: GdsAlgorithm) {
  selectedAlgorithm.value = a
}

function modeOptions(a: GdsAlgorithm): { value: GdsMode; label: string; desc: string }[] {
  const opts: { value: GdsMode; label: string; desc: string }[] = []
  if (a.modes.includes('stream')) opts.push({ value: 'stream', label: 'stream', desc: 'Return per-node values without writing' })
  if (a.modes.includes('stats')) opts.push({ value: 'stats', label: 'stats', desc: 'Return aggregate statistics only' })
  if (a.modes.includes('write')) opts.push({ value: 'write', label: 'write', desc: 'Persist values as a node property' })
  return opts
}

function formatRunSummary(run: GdsRun): string {
  const count = (
    run.algorithm.outputKind === 'path' ? run.paths.length :
    run.algorithm.outputKind === 'nodePair' ? run.nodePairs.length :
    run.algorithm.outputKind === 'entitySet' ? (run.entitySet?.nodeIds.length ?? 0) :
    run.algorithm.outputKind === 'nodeVector' ? run.nodeVectors.size :
    run.values.size
  )
  const label = run.algorithm.outputKind === 'path' ? 'paths'
    : run.algorithm.outputKind === 'nodePair' ? 'pairs'
    : run.algorithm.outputKind === 'entitySet' ? 'entities'
    : 'nodes'
  return `${run.algorithm.displayName}, ${run.mode}, ${count.toLocaleString()} ${label}, ${Math.round(run.durationMs).toLocaleString()}ms`
}

function selectRun(id: string) {
  gds.selectRun(id)
}

// -- Per-OutputKind renderer state ------------------------------------
const PATH_DISPLAY_LIMIT = 20
const PAIR_DISPLAY_LIMIT = 50
const VECTOR_DISPLAY_LIMIT = 50
const showAllPaths = ref(false)

const pathsToShow = computed(() => {
  const r = gds.currentRun
  if (!r) return []
  if (showAllPaths.value || r.paths.length <= PATH_DISPLAY_LIMIT) return r.paths
  // Sorted by totalCost ascending - the cheapest paths are usually
  // what the user came for.
  return [...r.paths].sort((a, b) => a.totalCost - b.totalCost).slice(0, PATH_DISPLAY_LIMIT)
})

const pairsToShow = computed(() => {
  const r = gds.currentRun
  if (!r) return []
  return [...r.nodePairs].sort((a, b) => b.score - a.score).slice(0, PAIR_DISPLAY_LIMIT)
})

const vectorsToShow = computed(() => {
  const r = gds.currentRun
  if (!r) return []
  const entries = Array.from(r.nodeVectors.entries())
  return entries.slice(0, VECTOR_DISPLAY_LIMIT)
})

/** Hover-highlight: paint the path under the mouse without committing
 * to a selection. On leave we restore whatever was clicked (or clear). */
let hoverRestoreId: number | null = null
function hoverPath(pathId: number | null) {
  if (pathId === null) {
    if (hoverRestoreId === null) {
      // No clicked path -> clear; otherwise restore the click.
      if (gds.highlightedPathId !== null && gds.highlightedPathId !== hoverRestoreId) {
        gds.highlightPath(null)
      }
    } else {
      gds.highlightPath(hoverRestoreId)
    }
    hoverRestoreId = null
    return
  }
  if (hoverRestoreId === null) {
    hoverRestoreId = gds.highlightedPathId
  }
  gds.highlightPath(pathId)
}

// Mode options for the current run - continuous vs discrete drives
// which style modes are meaningful.
const currentRunStyleModes = computed<{ value: GdsStylingMode; label: string; desc: string }[]>(() => {
  const r = gds.currentRun
  if (!r) return []
  const kind = valueKind(r.algorithm.outputKind)
  const opts: { value: GdsStylingMode; label: string; desc: string }[] = [
    { value: 'none', label: 'None', desc: 'No styling override' },
  ]
  if (kind === 'continuous') {
    opts.push(
      { value: 'size', label: 'Size scale', desc: 'Node radius scaled by score' },
      { value: 'gradient', label: 'Color gradient', desc: 'Node color interpolated by score' },
    )
  } else if (kind === 'discrete') {
    opts.push({ value: 'community', label: 'Community color', desc: 'Distinct palette per community' })
  }
  return opts
})

function setStylingMode(m: GdsStylingMode) {
  gds.setStylingMode(m)
}

function onColorStartChange(ev: Event) {
  const v = (ev.target as HTMLInputElement).value
  gds.setColorStops([v, gds.styling.colorStops[1]])
}
function onColorEndChange(ev: Event) {
  const v = (ev.target as HTMLInputElement).value
  gds.setColorStops([gds.styling.colorStops[0], v])
}
function onSizeMinChange(ev: Event) {
  const v = Number((ev.target as HTMLInputElement).value)
  if (Number.isFinite(v)) gds.setSizeRange([v, gds.styling.sizeRange[1]])
}
function onSizeMaxChange(ev: Event) {
  const v = Number((ev.target as HTMLInputElement).value)
  if (Number.isFinite(v)) gds.setSizeRange([gds.styling.sizeRange[0], v])
}

function dropRun(id: string, ev: MouseEvent) {
  ev.stopPropagation()
  gds.dropRun(id)
}
</script>

<template>
  <Transition name="gds-drawer">
    <aside v-if="open" class="gds-drawer">
      <header class="gds-drawer__header">
        <span class="gds-drawer__title">Graph Data Science</span>
        <button class="gds-drawer__close" type="button" title="Close" @click="emit('close')">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M18 6 6 18" /><path d="m6 6 12 12" />
          </svg>
        </button>
      </header>

      <!-- Catalog: categories with algorithms. Fills the drawer until
           the user picks an algorithm, then collapses to the top third
           so the config form below has room. -->
      <section
        class="gds-drawer__catalog"
        :class="{ 'gds-drawer__catalog--split': selectedAlgorithm }"
      >
        <div v-if="gds.isLoadingAlgorithms" class="gds-drawer__loading">Loading algorithms...</div>
        <div v-else-if="gds.availableAlgorithms.length === 0" class="gds-drawer__empty">
          No algorithms available. <button class="gds-drawer__link" @click="gds.loadAlgorithms()">Retry</button>
        </div>
        <template v-else>
          <div v-for="group in gds.algorithmsByCategory" :key="group.category" class="gds-cat">
            <button class="gds-cat__head" type="button" @click="toggleCategory(group.category)">
              <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"
                :class="{ 'gds-cat__chev--collapsed': collapsedCategories.has(group.category) }">
                <path d="m6 9 6 6 6-6" />
              </svg>
              <span>{{ categoryLabel[group.category] || group.category }}</span>
              <span class="gds-cat__count">{{ group.algorithms.length }}</span>
            </button>
            <ul v-if="!collapsedCategories.has(group.category)" class="gds-cat__list">
              <li
                v-for="a in group.algorithms"
                :key="a.name"
                class="gds-alg"
                :class="{ 'gds-alg--selected': selectedAlgorithm?.name === a.name }"
                @click="selectAlgorithm(a)"
              >
                <span class="gds-alg__name">{{ a.displayName }}</span>
              </li>
            </ul>
          </div>
        </template>
      </section>

      <!-- Selected algorithm form -->
      <section v-if="selectedAlgorithm" class="gds-drawer__form">
        <h3 class="gds-form__name">{{ selectedAlgorithm.displayName }}</h3>
        <p class="gds-form__desc">{{ selectedAlgorithm.description }}</p>

        <!-- Mode picker -->
        <div class="gds-field">
          <label class="gds-field__label">Mode</label>
          <div class="gds-mode-group">
            <button
              v-for="opt in modeOptions(selectedAlgorithm)"
              :key="opt.value"
              type="button"
              class="gds-mode"
              :class="{ 'gds-mode--active': mode === opt.value }"
              :title="opt.desc"
              @click="mode = opt.value"
            >{{ opt.label }}</button>
          </div>
        </div>

        <!-- Write property -->
        <div v-if="mode === 'write'" class="gds-field">
          <label class="gds-field__label">Write property
            <span class="gds-field__hint">Persisted as a node property on each affected node.</span>
          </label>
          <input v-model="writeProperty" class="gds-input" placeholder="propertyName" />
        </div>

        <!-- Node labels filter -->
        <div class="gds-field">
          <label class="gds-field__label">Node labels
            <span class="gds-field__hint">Empty = all labels</span>
          </label>
          <div class="gds-chips">
            <span v-for="l in nodeLabels" :key="l" class="gds-chip">
              {{ l }}
              <button type="button" class="gds-chip__rm" aria-label="Remove" @click="nodeLabels = nodeLabels.filter(x => x !== l)"><svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" aria-hidden="true"><path d="M18 6 6 18" /><path d="m6 6 12 12" /></svg></button>
            </span>
            <FgSelect
              v-if="labelOptions.length > 0"
              :model-value="''"
              :options="labelOptions.filter(o => !nodeLabels.includes(o.value))"
              placeholder="+ Add label"
              small
              @update:model-value="(v) => { if (v) nodeLabels = [...nodeLabels, v] }"
            />
          </div>
        </div>

        <!-- Edge types filter -->
        <div class="gds-field">
          <label class="gds-field__label">Edge types
            <span class="gds-field__hint">Empty = all types</span>
          </label>
          <div class="gds-chips">
            <span v-for="t in edgeTypes" :key="t" class="gds-chip">
              {{ t }}
              <button type="button" class="gds-chip__rm" aria-label="Remove" @click="edgeTypes = edgeTypes.filter(x => x !== t)"><svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" aria-hidden="true"><path d="M18 6 6 18" /><path d="m6 6 12 12" /></svg></button>
            </span>
            <FgSelect
              v-if="edgeTypeOptions.length > 0"
              :model-value="''"
              :options="edgeTypeOptions.filter(o => !edgeTypes.includes(o.value))"
              placeholder="+ Add type"
              small
              @update:model-value="(v) => { if (v) edgeTypes = [...edgeTypes, v] }"
            />
          </div>
        </div>

        <!-- Orientation -->
        <div class="gds-field">
          <label class="gds-field__label">Orientation</label>
          <FgSelect
            v-model="orientation as string"
            :options="orientationOptions"
            small
          />
        </div>

        <!-- Basic algorithm params -->
        <div v-for="p in basicParams" :key="p.key" class="gds-field">
          <label class="gds-field__label">
            {{ p.key }}
            <span v-if="p.required" class="gds-field__required">required</span>
            <span class="gds-field__hint">{{ p.description }}</span>
          </label>
          <input
            v-if="p.type === 'integer' || p.type === 'float'"
            v-model.number="algoConfig[p.key]"
            type="number"
            :step="p.type === 'float' ? '0.0001' : '1'"
            class="gds-input"
          />
          <label v-else-if="p.type === 'bool'" class="gds-bool">
            <input
              type="checkbox"
              :checked="!!algoConfig[p.key]"
              @change="(e) => algoConfig[p.key] = (e.target as HTMLInputElement).checked"
            />
            <span>{{ algoConfig[p.key] ? 'on' : 'off' }}</span>
          </label>
          <input v-else v-model="algoConfig[p.key] as string" class="gds-input" />
        </div>

        <!-- Advanced toggle + params -->
        <div v-if="advancedParams.length > 0" class="gds-advanced">
          <button type="button" class="gds-advanced__toggle" @click="showAdvanced = !showAdvanced">
            <DisclosureIcon :open="showAdvanced" /> Advanced ({{ advancedParams.length }})
          </button>
          <template v-if="showAdvanced">
            <div v-for="p in advancedParams" :key="p.key" class="gds-field">
              <label class="gds-field__label">
                {{ p.key }}
                <span class="gds-field__hint">{{ p.description }}</span>
              </label>
              <input
                v-if="p.type === 'integer' || p.type === 'float'"
                v-model.number="algoConfig[p.key]"
                type="number"
                :step="p.type === 'float' ? '0.0001' : '1'"
                class="gds-input"
              />
              <label v-else-if="p.type === 'bool'" class="gds-bool">
                <input
                  type="checkbox"
                  :checked="!!algoConfig[p.key]"
                  @change="(e) => algoConfig[p.key] = (e.target as HTMLInputElement).checked"
                />
                <span>{{ algoConfig[p.key] ? 'on' : 'off' }}</span>
              </label>
              <input v-else v-model="algoConfig[p.key] as string" class="gds-input" />
            </div>
          </template>
        </div>

        <!-- Run -->
        <div class="gds-actions">
          <button
            class="gds-run"
            type="button"
            :disabled="!canRun"
            @click="run"
          >
            <span v-if="gds.isRunning" class="gds-run__spinner" />
            <svg v-else width="10" height="10" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M7 4.5v15l12.5-7.5z" /></svg>
            Run {{ mode }}
          </button>
        </div>
      </section>

      <!-- Per-OutputKind renderers (Path / NodePair / EntitySet / NodeVector) -->
      <section
        v-if="gds.currentRun && gds.currentRun.mode === 'stream' && gds.currentRun.algorithm.outputKind === 'path' && gds.currentRun.paths.length > 0"
        class="gds-drawer__paths"
      >
        <h4 class="gds-runs__title">
          Paths
          <span class="gds-runs__count">{{ gds.currentRun.paths.length.toLocaleString() }}</span>
          <button
            v-if="gds.highlightedPathId !== null"
            type="button"
            class="gds-clear-link"
            @click="gds.clearHighlight()"
          >clear</button>
        </h4>
        <ul class="gds-paths">
          <li
            v-for="p in pathsToShow"
            :key="p.id"
            class="gds-path"
            :class="{ 'gds-path--active': gds.highlightedPathId === p.id }"
            @click="gds.highlightPath(gds.highlightedPathId === p.id ? null : p.id)"
            @mouseenter="hoverPath(p.id)"
            @mouseleave="hoverPath(null)"
          >
            <span class="gds-path__route">
              <span class="gds-path__endpoint">#{{ p.source }}</span>
              <span class="gds-path__arrow">&rarr;</span>
              <span class="gds-path__endpoint">#{{ p.target }}</span>
            </span>
            <span class="gds-path__stats">
              {{ (p.nodeIds.length - 1).toLocaleString() }} hops, cost {{ p.totalCost.toLocaleString(undefined, { maximumFractionDigits: 4 }) }}
            </span>
          </li>
        </ul>
        <button
          v-if="gds.currentRun.paths.length > PATH_DISPLAY_LIMIT"
          type="button"
          class="gds-drawer__link"
          @click="showAllPaths = !showAllPaths"
        >
          {{ showAllPaths ? 'show top ' + PATH_DISPLAY_LIMIT.toLocaleString() : 'show all ' + gds.currentRun.paths.length.toLocaleString() }}
        </button>
      </section>

      <section
        v-if="gds.currentRun && gds.currentRun.mode === 'stream' && gds.currentRun.algorithm.outputKind === 'nodePair' && gds.currentRun.nodePairs.length > 0"
        class="gds-drawer__pairs"
      >
        <h4 class="gds-runs__title">
          Pairs
          <span class="gds-runs__count">{{ gds.currentRun.nodePairs.length.toLocaleString() }}</span>
        </h4>
        <table class="gds-pair-table">
          <thead>
            <tr><th>source</th><th>target</th><th>score</th></tr>
          </thead>
          <tbody>
            <tr v-for="(p, i) in pairsToShow" :key="i">
              <td>#{{ p.source }}</td>
              <td>#{{ p.target }}</td>
              <td class="gds-pair-table__score">{{ p.score.toFixed(4) }}</td>
            </tr>
          </tbody>
        </table>
      </section>

      <section
        v-if="gds.currentRun && gds.currentRun.mode === 'stream' && gds.currentRun.algorithm.outputKind === 'entitySet' && gds.currentRun.entitySet"
        class="gds-drawer__entityset"
      >
        <h4 class="gds-runs__title">
          Entities
          <span class="gds-runs__count">
            {{ gds.currentRun.entitySet.nodeIds.length.toLocaleString() }}n, {{ gds.currentRun.entitySet.edgeIds.length.toLocaleString() }}e
          </span>
          <button type="button" class="gds-clear-link" @click="gds.highlightEntitySet()">highlight</button>
        </h4>
        <div class="gds-chips">
          <span
            v-for="n in (gds.currentRun.entitySet.nodeIds.slice(0, 40))"
            :key="n"
            class="gds-chip"
          >#{{ n }}</span>
          <span
            v-if="gds.currentRun.entitySet.nodeIds.length > 40"
            class="gds-field__hint"
          >+{{ (gds.currentRun.entitySet.nodeIds.length - 40).toLocaleString() }} more</span>
        </div>
      </section>

      <section
        v-if="gds.currentRun && gds.currentRun.mode === 'stream' && gds.currentRun.algorithm.outputKind === 'nodeVector' && gds.currentRun.nodeVectors.size > 0"
        class="gds-drawer__vectors"
      >
        <h4 class="gds-runs__title">
          Embeddings
          <span class="gds-runs__count">{{ gds.currentRun.nodeVectors.size.toLocaleString() }}</span>
        </h4>
        <table class="gds-pair-table">
          <thead>
            <tr><th>node</th><th>dim 0</th><th>dim 1</th><th>dim 2</th><th>...</th></tr>
          </thead>
          <tbody>
            <tr v-for="[nid, vec] in vectorsToShow" :key="nid">
              <td>#{{ nid }}</td>
              <td>{{ vec[0]?.toFixed(3) ?? '-' }}</td>
              <td>{{ vec[1]?.toFixed(3) ?? '-' }}</td>
              <td>{{ vec[2]?.toFixed(3) ?? '-' }}</td>
              <td class="gds-pair-table__score">[{{ vec.length }}]</td>
            </tr>
          </tbody>
        </table>
      </section>

      <!-- Result styling (size / color / community) -->
      <section v-if="gds.currentRun && gds.currentRun.mode === 'stream' && (gds.currentRun.algorithm.outputKind === 'nodeScalar' || gds.currentRun.algorithm.outputKind === 'nodeCommunity' || gds.currentRun.algorithm.outputKind === 'nodeMulti')" class="gds-drawer__styling">
        <h4 class="gds-runs__title">Apply to graph</h4>
        <div class="gds-style-modes">
          <button
            v-for="opt in currentRunStyleModes"
            :key="opt.value"
            type="button"
            class="gds-style-mode"
            :class="{ 'gds-style-mode--active': gds.styling.mode === opt.value }"
            :title="opt.desc"
            @click="setStylingMode(opt.value)"
          >{{ opt.label }}</button>
        </div>

        <div v-if="gds.styling.mode === 'size'" class="gds-style-row">
          <span class="gds-style-row__label">Range</span>
          <input
            type="number"
            class="gds-input gds-input--xs"
            :value="gds.styling.sizeRange[0]"
            min="1"
            step="1"
            @input="onSizeMinChange"
          />
          <span class="gds-style-row__dash">&rarr;</span>
          <input
            type="number"
            class="gds-input gds-input--xs"
            :value="gds.styling.sizeRange[1]"
            min="1"
            step="1"
            @input="onSizeMaxChange"
          />
        </div>

        <div v-if="gds.styling.mode === 'gradient'" class="gds-style-row">
          <span class="gds-style-row__label">Gradient</span>
          <input
            type="color"
            class="gds-color"
            :value="gds.styling.colorStops[0]"
            @input="onColorStartChange"
          />
          <span
            class="gds-gradient-preview"
            :style="`background: linear-gradient(to right, ${gds.styling.colorStops[0]}, ${gds.styling.colorStops[1]});`"
          />
          <input
            type="color"
            class="gds-color"
            :value="gds.styling.colorStops[1]"
            @input="onColorEndChange"
          />
        </div>

        <div v-if="gds.currentRunRange && gds.styling.mode !== 'community'" class="gds-style-stats">
          min: {{ gds.currentRunRange.min.toLocaleString(undefined, { maximumFractionDigits: 4 }) }},
          max: {{ gds.currentRunRange.max.toLocaleString(undefined, { maximumFractionDigits: 4 }) }}
        </div>
        <div v-else-if="gds.styling.mode === 'community'" class="gds-style-stats">
          {{ gds.currentRunCommunities.length.toLocaleString() }} communities
        </div>
      </section>

      <!-- Run history -->
      <section v-if="gds.runs.length > 0" class="gds-drawer__runs">
        <h4 class="gds-runs__title">Recent runs</h4>
        <ul class="gds-runs__list">
          <li
            v-for="r in [...gds.runs].reverse()"
            :key="r.id"
            class="gds-run-item"
            :class="{ 'gds-run-item--active': r.id === gds.currentRunId }"
            @click="selectRun(r.id)"
          >
            <span class="gds-run-item__name">{{ formatRunSummary(r) }}</span>
            <button class="gds-run-item__rm" type="button" title="Discard" aria-label="Discard" @click="dropRun(r.id, $event)"><svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" aria-hidden="true"><path d="M18 6 6 18" /><path d="m6 6 12 12" /></svg></button>
          </li>
        </ul>
      </section>
    </aside>
  </Transition>
</template>

<style scoped>
.gds-drawer {
  position: absolute;
  top: 0;
  right: 0;
  bottom: 0;
  width: 360px;
  max-width: 100%;
  background: var(--tv-bg-surface);
  border-left: 1px solid var(--tv-border-default);
  display: flex;
  flex-direction: column;
  overflow: hidden;
  box-shadow: var(--tv-shadow-lg);
  z-index: 50;
}

/* On phones the 360px drawer covers nearly the whole canvas - go
   full-width so the inner content has room to breathe. */
@media (max-width: 768px) {
  .gds-drawer {
    width: 100%;
  }
}

.gds-drawer__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 12px;
  border-bottom: 1px solid var(--tv-border-default);
  background: var(--tv-bg-base);
  flex-shrink: 0;
}

.gds-drawer__title {
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}

.gds-drawer__close {
  background: none;
  border: none;
  color: var(--tv-text-muted);
  cursor: pointer;
  padding: 4px;
  border-radius: var(--tv-radius-sm);
  display: flex;
  align-items: center;
  justify-content: center;
}
.gds-drawer__close:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}

.gds-drawer__catalog {
  border-bottom: 1px solid var(--tv-border-default);
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 4px 0;
}
/* When an algorithm is selected the form below appears - shrink the
   catalog so they share the drawer roughly 35/65. */
.gds-drawer__catalog--split {
  flex: none;
  max-height: 35%;
}

.gds-drawer__loading,
.gds-drawer__empty {
  padding: 14px 12px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
}

.gds-drawer__link {
  background: none;
  border: none;
  color: var(--tv-accent);
  cursor: pointer;
  font-size: var(--tv-font-size-xs);
  padding: 0;
  text-decoration: underline;
}

.gds-cat__head {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  padding: 6px 10px;
  background: none;
  border: none;
  cursor: pointer;
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-xs);
  font-weight: 600;
  color: var(--tv-text-secondary);
  text-transform: uppercase;
  letter-spacing: 0.02em;
}
.gds-cat__head:hover {
  background: var(--tv-bg-hover);
}

.gds-cat__chev--collapsed {
  transform: rotate(-90deg);
}

.gds-cat__count {
  margin-left: auto;
  font-weight: 500;
  color: var(--tv-text-muted);
  font-size: 10px;
}

.gds-cat__list {
  list-style: none;
  margin: 0;
  padding: 0;
}

.gds-alg {
  display: flex;
  align-items: center;
  padding: 5px 10px 5px 24px;
  cursor: pointer;
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-primary);
}
.gds-alg:hover {
  background: var(--tv-bg-hover);
}
.gds-alg--selected {
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
}

.gds-alg__name {
  flex: 1;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.gds-alg__badge {
  font-size: 9px;
  font-weight: 600;
  text-transform: uppercase;
  padding: 1px 5px;
  border-radius: 3px;
  margin-left: 6px;
  letter-spacing: 0.04em;
}
.gds-alg__badge--beta {
  background: rgba(255, 165, 0, 0.15);
  color: #c87000;
}
.gds-alg__badge--experimental {
  background: rgba(220, 53, 69, 0.15);
  color: #b02a3a;
}

.gds-drawer__form {
  padding: 10px 12px;
  overflow-y: auto;
  flex: 1;
  border-bottom: 1px solid var(--tv-border-default);
}

.gds-form__name {
  margin: 0 0 4px 0;
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}

.gds-form__desc {
  margin: 0 0 12px 0;
  font-size: 11px;
  line-height: 1.45;
  color: var(--tv-text-muted);
}

.gds-field {
  margin-bottom: 10px;
}

.gds-field__label {
  display: block;
  font-family: var(--tv-font-sans);
  font-size: 11px;
  font-weight: 600;
  color: var(--tv-text-secondary);
  margin-bottom: 4px;
}

.gds-field__hint {
  display: block;
  font-weight: 400;
  font-size: 10px;
  color: var(--tv-text-muted);
  margin-top: 1px;
}

/* "required" pill next to the field name for algorithms whose config
   has params without a sensible default (community property, source
   node id, etc.). Paired with the canRun gate on the Run button. */
.gds-field__required {
  display: inline-block;
  margin-left: 6px;
  padding: 0 5px;
  font-size: 9px;
  font-weight: 600;
  letter-spacing: 0.02em;
  text-transform: uppercase;
  color: var(--tv-text-danger, #b91c1c);
  background: var(--tv-bg-danger-subtle, #fef2f2);
  border-radius: 4px;
  vertical-align: middle;
}

.gds-mode-group {
  display: flex;
  gap: 1px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  overflow: hidden;
  width: fit-content;
}

.gds-mode {
  padding: 4px 12px;
  background: var(--tv-bg-base);
  border: none;
  cursor: pointer;
  font-family: var(--tv-font-sans);
  font-size: 11px;
  color: var(--tv-text-secondary);
}
.gds-mode:hover {
  background: var(--tv-bg-hover);
}
.gds-mode--active {
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
}
.gds-mode--active:hover {
  background: var(--tv-accent-hover);
}

.gds-input {
  width: 100%;
  height: 28px;
  padding: 0 8px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  background: var(--tv-bg-base);
  color: var(--tv-text-primary);
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-xs);
  box-sizing: border-box;
}
.gds-input:focus {
  outline: none;
  border-color: var(--tv-border-focus);
  box-shadow: 0 0 0 2px var(--tv-accent-subtle);
}

.gds-bool {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 11px;
  color: var(--tv-text-secondary);
  cursor: pointer;
}

.gds-chips {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  align-items: center;
}

.gds-chip {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  padding: 2px 4px 2px 7px;
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
  border-radius: var(--tv-radius-sm);
  font-family: var(--tv-font-sans);
  font-size: 11px;
}

.gds-chip__rm {
  background: none;
  border: none;
  color: inherit;
  cursor: pointer;
  font-size: 13px;
  line-height: 1;
  padding: 0 3px;
  opacity: 0.7;
}
.gds-chip__rm:hover {
  opacity: 1;
}

.gds-advanced {
  margin-top: 6px;
  padding-top: 6px;
  border-top: 1px dashed var(--tv-border-default);
}

.gds-advanced__toggle {
  background: none;
  border: none;
  color: var(--tv-text-muted);
  font-family: var(--tv-font-sans);
  font-size: 11px;
  font-weight: 600;
  cursor: pointer;
  padding: 2px 0;
  margin-bottom: 6px;
}
.gds-advanced__toggle:hover {
  color: var(--tv-text-primary);
}

.gds-actions {
  margin-top: 14px;
}

.gds-run {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  width: 100%;
  height: 32px;
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
  border: none;
  border-radius: var(--tv-radius-md);
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  cursor: pointer;
}
.gds-run:hover:not(:disabled) {
  background: var(--tv-accent-hover);
}
.gds-run:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.gds-run__spinner {
  width: 12px;
  height: 12px;
  border: 2px solid currentColor;
  border-top-color: transparent;
  border-radius: 50%;
  animation: gds-spin 0.8s linear infinite;
}

@keyframes gds-spin {
  to { transform: rotate(360deg); }
}

.gds-drawer__runs {
  padding: 8px 12px;
  max-height: 25%;
  overflow-y: auto;
  background: var(--tv-bg-base);
  flex-shrink: 0;
}

.gds-runs__title {
  font-family: var(--tv-font-sans);
  font-size: 10px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  color: var(--tv-text-muted);
  margin: 0 0 6px 0;
}

.gds-runs__list {
  list-style: none;
  margin: 0;
  padding: 0;
}

.gds-run-item {
  display: flex;
  align-items: center;
  padding: 4px 6px;
  border-radius: var(--tv-radius-sm);
  cursor: pointer;
  font-family: var(--tv-font-sans);
  font-size: 11px;
  color: var(--tv-text-secondary);
}
.gds-run-item:hover {
  background: var(--tv-bg-hover);
}
.gds-run-item--active {
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
}

.gds-run-item__name {
  flex: 1;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.gds-run-item__rm {
  background: none;
  border: none;
  color: inherit;
  opacity: 0.5;
  cursor: pointer;
  padding: 0 4px;
  font-size: 13px;
}
.gds-run-item__rm:hover {
  opacity: 1;
}

.gds-drawer__styling {
  padding: 8px 12px;
  border-top: 1px solid var(--tv-border-default);
  background: var(--tv-bg-base);
  flex-shrink: 0;
}

.gds-drawer__paths,
.gds-drawer__pairs,
.gds-drawer__entityset,
.gds-drawer__vectors {
  padding: 8px 12px;
  border-top: 1px solid var(--tv-border-default);
  background: var(--tv-bg-base);
  max-height: 35%;
  overflow-y: auto;
  flex-shrink: 0;
}

.gds-runs__count {
  margin-left: 6px;
  font-weight: 500;
  color: var(--tv-text-muted);
  font-size: 10px;
}

.gds-clear-link {
  margin-left: auto;
  background: none;
  border: none;
  color: var(--tv-accent);
  cursor: pointer;
  font-family: var(--tv-font-sans);
  font-size: 10px;
  font-weight: 500;
  text-transform: lowercase;
  padding: 0;
}
.gds-clear-link:hover {
  text-decoration: underline;
}

.gds-paths {
  list-style: none;
  margin: 0;
  padding: 0;
}

.gds-path {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 4px 6px;
  cursor: pointer;
  border-radius: var(--tv-radius-sm);
  font-family: var(--tv-font-sans);
  font-size: 11px;
  color: var(--tv-text-secondary);
}
.gds-path:hover {
  background: var(--tv-bg-hover);
}
.gds-path--active {
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
}

.gds-path__route {
  display: inline-flex;
  align-items: center;
  gap: 5px;
}
.gds-path__endpoint {
  font-family: var(--tv-font-mono, monospace);
}
.gds-path__arrow {
  color: var(--tv-text-muted);
}
.gds-path__stats {
  font-family: var(--tv-font-mono, monospace);
  font-size: 10px;
  color: var(--tv-text-muted);
}

.gds-pair-table {
  width: 100%;
  border-collapse: collapse;
  font-family: var(--tv-font-sans);
  font-size: 11px;
}
.gds-pair-table th {
  text-align: left;
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  font-size: 10px;
  padding: 3px 4px;
  border-bottom: 1px solid var(--tv-border-default);
}
.gds-pair-table td {
  padding: 3px 4px;
  border-bottom: 1px solid var(--tv-border-subtle);
}
.gds-pair-table__score {
  font-family: var(--tv-font-mono, monospace);
  color: var(--tv-text-muted);
}

.gds-style-modes {
  display: flex;
  gap: 1px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  overflow: hidden;
  width: fit-content;
  margin-bottom: 6px;
}

.gds-style-mode {
  padding: 3px 9px;
  background: var(--tv-bg-base);
  border: none;
  cursor: pointer;
  font-family: var(--tv-font-sans);
  font-size: 10px;
  color: var(--tv-text-secondary);
  white-space: nowrap;
}
.gds-style-mode:hover {
  background: var(--tv-bg-hover);
}
.gds-style-mode--active {
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
}

.gds-style-row {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 4px;
}

.gds-style-row__label {
  font-family: var(--tv-font-sans);
  font-size: 10px;
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.04em;
  width: 48px;
}

.gds-style-row__dash {
  font-size: 10px;
  color: var(--tv-text-muted);
}

.gds-input--xs {
  width: 56px;
  height: 24px;
  font-size: 11px;
}

.gds-color {
  width: 28px;
  height: 24px;
  padding: 0;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-base);
  cursor: pointer;
}

.gds-gradient-preview {
  flex: 1;
  height: 14px;
  border-radius: var(--tv-radius-sm);
  border: 1px solid var(--tv-border-default);
}

.gds-style-stats {
  margin-top: 4px;
  font-family: var(--tv-font-mono, monospace);
  font-size: 10px;
  color: var(--tv-text-muted);
}

.gds-drawer-enter-active,
.gds-drawer-leave-active {
  transition: transform 0.18s ease;
}
.gds-drawer-enter-from,
.gds-drawer-leave-to {
  transform: translateX(100%);
}
</style>
