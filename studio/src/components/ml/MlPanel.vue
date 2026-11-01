<script setup lang="ts">
/**
 * ML side panel.
 *
 * The mental model is "answer three questions, get predictions": what to
 * predict (a category, a number, or missing relationships), what the model
 * looks at, and how it learns. Pipelines, candidates and splits live
 * underneath; the Cypher procedures stay there for power users.
 *
 * Layers:
 *   1. Training now - jobs queued or running on the server (they survive a
 *      page reload), with progress and Cancel.
 *   2. Saved models - per task: metrics, predictions, Show on graph, Write,
 *      Explain (classifiers), Suggest/Write links (link prediction), Export,
 *      Delete.
 *   3. Train a new model wizard.
 *   4. Empty state - the fraud-detection example shortcut.
 */
import { ref, computed, onMounted, onUnmounted, watch, nextTick } from 'vue'
import {
  useMlStore,
  type ArchitectureChoice,
  type BatchingMode,
  type ExplainMethod,
  type SavedModel,
  type TaskKind,
} from '../../stores/ml'
import { useSchemaStore } from '../../stores/schema'
import { useConnectionStore } from '../../stores/connection'
import { useSettingsStore } from '../../stores/settings'
import { useStyleStore } from '../../stores/style'
import { useQueryStore } from '../../stores/query'
import { useToast } from '../../composables/useToast'
import { api } from '../../utils/api'
import { EXAMPLE_GRAPHS } from '../../data/example-graphs'
import FgSelect from '../common/FgSelect.vue'
import DisclosureIcon from '../common/DisclosureIcon.vue'
import { useGdsStore, colorForCommunity } from '../../stores/gds'

const ml = useMlStore()
const schema = useSchemaStore()
const conn = useConnectionStore()
const settings = useSettingsStore()
const styleStore = useStyleStore()
const queryStore = useQueryStore()
const gds = useGdsStore()
const { showToast } = useToast()

// -- Wizard state ------------------------------------------------
type ArchitecturePick = ArchitectureChoice | 'Best'

const wizardOpen = ref(false)
const task = ref<TaskKind>('NodeClassification')
const target = ref<string>('')
const sourceLabel = ref('')
const targetLabel = ref('')
const checkedFeatures = ref<Set<string>>(new Set())
const architecture = ref<ArchitecturePick>('Gcn')
const advancedOpen = ref(false)
const hidden = ref(32)
const heads = ref(4)
const epochs = ref(100)
const learningRate = ref(0.01)
const device = ref('')
const batching = ref<BatchingMode>('auto')
const batchSize = ref(512)
const fanout1 = ref(25)
const fanout2 = ref(10)
const modelNameOverride = ref('')

const TASKS: { value: TaskKind; label: string; hint: string }[] = [
  { value: 'NodeClassification', label: 'A category', hint: 'e.g. fraud / not fraud' },
  { value: 'NodeRegression', label: 'A number', hint: 'e.g. risk score, spend' },
  { value: 'LinkPrediction', label: 'Missing links', hint: 'who should connect' },
]

/** Pre-fill the wizard with a classification target (used by the host route and the
 *  Examples panel hand-off). */
function preFillTarget(propertyName: string) {
  task.value = 'NodeClassification'
  target.value = propertyName
  wizardOpen.value = true
}

defineExpose({ preFillTarget })

function isNumericType(type: string): boolean {
  const t = type.toLowerCase()
  return t.includes('int') || t.includes('float') || t === 'double' || t === 'number'
}

const availableFeatureProperties = computed(() => {
  const out: { name: string; type: string }[] = []
  const seen = new Set<string>()
  for (const detail of schema.labelsDetail) {
    for (const p of detail.properties) {
      if (!isNumericType(p.type) || seen.has(p.name)) continue
      if (task.value !== 'LinkPrediction' && p.name === target.value) continue
      seen.add(p.name)
      out.push({ name: p.name, type: p.type })
    }
  }
  out.sort((a, b) => a.name.localeCompare(b.name))
  return out
})

// every numeric property but the target, by default
function defaultFeatures(): string[] {
  if (!target.value) return []
  return availableFeatureProperties.value.map((p) => p.name)
}

watch([task, target], () => {
  checkedFeatures.value = new Set(defaultFeatures())
})
watch(task, () => {
  target.value = ''
  sourceLabel.value = ''
  targetLabel.value = ''
})
watch(
  () => schema.labelsDetail.length,
  () => {
    if (target.value && checkedFeatures.value.size === 0) {
      checkedFeatures.value = new Set(defaultFeatures())
    }
  },
)

function toggleFeature(name: string) {
  const next = new Set(checkedFeatures.value)
  if (next.has(name)) next.delete(name)
  else next.add(name)
  checkedFeatures.value = next
}

// -- Targets -----------------------------------------------------
/** Properties that look like class labels: a few distinct values, each with examples. Others
 *  (`age`, ids) hide behind "show all" so nobody trains a 50-class model on 6 examples. */
const MAX_PLAUSIBLE_CLASSES = 12
const showAllTargets = ref(false)
const plausibleTargets = computed(() =>
  ml.targets.filter(
    (t) =>
      t.labelledNodes >= 2 &&
      t.distinctValues >= 2 &&
      t.distinctValues <= MAX_PLAUSIBLE_CLASSES &&
      t.distinctValues * 2 <= t.labelledNodes,
  ),
)
const visibleTargets = computed(() =>
  showAllTargets.value ? ml.targets : plausibleTargets.value,
)
const hiddenTargetCount = computed(() => ml.targets.length - plausibleTargets.value.length)
function toggleShowAllTargets() {
  showAllTargets.value = !showAllTargets.value
}

const targetOptions = computed(() => {
  if (task.value === 'NodeClassification') {
    return visibleTargets.value
      .filter((t) => t.labelledNodes > 0)
      .map((t) => ({
        value: t.propertyName,
        label: `${t.propertyName} - ${t.distinctValues} ${
          t.distinctValues === 1 ? 'class' : 'classes'
        }, ${t.labelledNodes} / ${t.totalNodes} labelled`,
      }))
  }
  if (task.value === 'NodeRegression') {
    return ml.regressionTargets
      .filter((t) => t.labelledNodes >= 2)
      .map((t) => ({
        value: t.propertyName,
        label: `${t.propertyName} - ${t.labelledNodes} / ${t.totalNodes} with a value`,
      }))
  }
  return schema.relationshipTypes.map((r) => ({ value: r, label: r }))
})

const labelOptions = computed(() => [
  { value: '', label: 'any node' },
  ...schema.labels.map((l) => ({ value: l, label: l })),
])

const selectedTargetStats = computed(() => {
  const list = task.value === 'NodeRegression' ? ml.regressionTargets : ml.targets
  return list.find((t) => t.propertyName === target.value) ?? null
})

const canStartWizard = computed(
  () =>
    ml.targets.length > 0 || ml.regressionTargets.length > 0 || schema.relationshipTypes.length > 0,
)

const canTrain = computed(
  () => !blockReason.value && !ml.isTraining,
)

const blockReason = computed(() => {
  if (!target.value) {
    return task.value === 'LinkPrediction'
      ? 'Pick the relationship type to predict.'
      : 'Pick what you want to predict.'
  }
  if (task.value !== 'LinkPrediction' && (selectedTargetStats.value?.labelledNodes ?? 0) === 0)
    return 'That property has no examples yet. Set it on some nodes first.'
  if (checkedFeatures.value.size === 0) return 'Tick at least one input property.'
  return null
})

// -- Devices and training mode -----------------------------------
const deviceOptions = computed(() => {
  const def = ml.devices.find((d) => d.isDefault)
  const opts = [
    {
      value: '',
      label: def ? `Server default (${def.device}${def.name ? ` - ${def.name}` : ''})` : 'Server default',
    },
  ]
  for (const d of ml.devices) {
    if (!d.available || d.device === 'gpu') continue
    opts.push({ value: d.device, label: `${d.device}${d.name ? ` - ${d.name}` : ''}` })
  }
  return opts
})

/** Why there's no GPU to pick, when there isn't. */
const gpuNote = computed(() => {
  if (ml.devices.some((d) => d.type === 'GPU' && d.available)) return null
  return ml.devices.find((d) => d.type === 'GPU')?.reason ?? null
})

const batchingOptions = [
  { value: 'auto', label: 'Automatic - whole graph if it fits, else mini-batches' },
  { value: 'fullGraph', label: 'Whole graph every step' },
  { value: 'miniBatch', label: 'Mini-batches of sampled neighborhoods' },
]

// -- Train -------------------------------------------------------
function architecturesFor(pick: ArchitecturePick): ArchitectureChoice[] {
  if (pick !== 'Best') return [pick]
  const all: ArchitectureChoice[] = ['Gcn', 'GraphSage', 'Gat']
  // R-GCN only earns its keep with several relationship types
  if (schema.relationshipTypes.length > 1) all.push('RGcn')
  return all
}

async function onTrain() {
  if (!canTrain.value) return
  try {
    const { modelName } = await ml.trainAndPredict({
      task: task.value,
      target: target.value,
      features: Array.from(checkedFeatures.value),
      architectures: architecturesFor(architecture.value),
      hidden: hidden.value,
      heads: heads.value,
      epochs: epochs.value,
      learningRate: learningRate.value,
      device: device.value,
      batching: batching.value,
      batchSize: batchSize.value,
      fanouts: [fanout1.value, fanout2.value],
      sourceLabel: sourceLabel.value || undefined,
      targetLabel: targetLabel.value || undefined,
      modelNameOverride: modelNameOverride.value,
    })
    showToast(`Trained "${modelName}" in ${ml.trainPhase.seconds}s`)
    wizardOpen.value = false
    ml.resetTrainPhase()
  } catch (e) {
    showToast((e as Error).message, 'error')
  }
}

async function onCancelTraining() {
  try {
    await ml.cancelTraining()
    showToast('Stopping the training run...')
  } catch (e) {
    showToast((e as Error).message, 'error')
  }
}

/** Jobs started elsewhere (another tab, before a reload, from Cypher). */
const otherJobs = computed(() =>
  ml.activeJobs.filter((j) => j.jobId !== ml.trainPhase.jobId),
)

async function onCancelJob(jobId: string) {
  try {
    await ml.cancelJob(jobId)
    showToast('Stopping the training run...')
  } catch (e) {
    showToast((e as Error).message, 'error')
  }
}

// while jobs are queued or running, keep their progress fresh
let jobTimer: ReturnType<typeof setInterval> | null = null
function watchJobs() {
  if (jobTimer) return
  jobTimer = setInterval(async () => {
    await ml.refreshJobs()
    if (otherJobs.value.length === 0 && jobTimer) {
      clearInterval(jobTimer)
      jobTimer = null
      await ml.listModels()
    }
  }, 2000)
}
watch(
  () => otherJobs.value.length,
  (n) => {
    if (n > 0) watchJobs()
  },
)
onUnmounted(() => {
  if (jobTimer) clearInterval(jobTimer)
})

function fmtProgress(done: number, planned: number): string {
  return planned > 0 ? `trial ${Math.min(done + 1, planned)} of ${planned}` : ''
}

// -- Saved-model actions -----------------------------------------
const confirmDeleteModel = ref<string | null>(null)
async function onConfirmDelete() {
  if (!confirmDeleteModel.value) return
  const name = confirmDeleteModel.value
  confirmDeleteModel.value = null
  try {
    await ml.dropModel(name)
    // drop the graph overlay this model painted, if any
    gds.dropRun(`ml:${name}`)
    expandedPredictions.value = new Set(
      [...expandedPredictions.value].filter((n) => n !== name),
    )
    showToast(`Model "${name}" deleted`)
  } catch (e) {
    showToast((e as Error).message, 'error')
  }
}

/** Predicts again with the model on the current graph. */
async function onPredict(m: SavedModel) {
  try {
    if (m.taskType === 'NodeClassification') await ml.predictStream(m.modelName)
    else if (m.taskType === 'NodeRegression') await ml.predictValues(m.modelName)
    else await ml.suggestLinks(m.modelName, linkTopN(m))
    expandedPredictions.value = new Set([...expandedPredictions.value, m.modelName])
    showToast(`Predictions refreshed for "${m.modelName}"`)
  } catch (e) {
    showToast((e as Error).message, 'error')
  }
}

async function onExportModel(modelName: string) {
  try {
    await ml.exportModel(modelName)
    showToast(`Downloaded ${modelName}.tvmodel`)
  } catch (e) {
    showToast((e as Error).message, 'error')
  }
}

/** True when the graph is coloured by this model's predictions. */
function isShowingOnGraph(modelName: string): boolean {
  return gds.currentRunId === `ml:${modelName}`
}

async function onShowOnGraph(m: SavedModel) {
  if (isShowingOnGraph(m.modelName)) {
    gds.selectRun(null)
    showToast('Cleared graph colouring.', 'success')
    return
  }
  if (m.taskType === 'LinkPrediction') {
    showLinksOnGraph(m)
    return
  }
  const classes = m.taskType === 'NodeClassification'
  let values = classes ? ml.predictByModel[m.modelName] : ml.valuesByModel[m.modelName]
  if (!values || values.size === 0) {
    try {
      values = classes ? (await ml.predictStream(m.modelName), ml.predictByModel[m.modelName])
        : await ml.predictValues(m.modelName)
    } catch (e) {
      showToast((e as Error).message, 'error')
      return
    }
  }
  if (!values || values.size === 0) {
    showToast(`${m.modelName}: the model predicted no nodes.`, 'error')
    return
  }
  gds.registerExternalRun({
    id: `ml:${m.modelName}`,
    displayName: `ML - ${m.modelName}`,
    description: `Per-node predicted '${m.targetProperty}' from model "${m.modelName}".`,
    database: conn.currentDatabase,
    values,
    continuous: !classes,
    config: {
      targetProperty: m.targetProperty,
      modelName: m.modelName,
      classLabels: m.classLabels,
    },
  })
  showToast(
    classes
      ? `Coloured ${values.size} node(s) by predicted ${m.targetProperty}.`
      : `Sized ${values.size} node(s) by predicted ${m.targetProperty}.`,
    'success',
  )
}

/** Runs a query that brings the given nodes (and the relationships between them) into the
 *  graph view. */
function showNodesOnGraph(ids: number[]) {
  const unique = [...new Set(ids)]
  if (unique.length === 0) return
  const list = unique.join(', ')
  queryStore.currentQuery =
    `MATCH (n) WHERE id(n) IN [${list}]\n` +
    `OPTIONAL MATCH (n)-[r]-(m) WHERE id(m) IN [${list}]\n` +
    `RETURN n, r, m`
  queryStore.executeQuery()
}

function showLinksOnGraph(m: SavedModel) {
  const links = ml.linksByModel[m.modelName] ?? []
  if (links.length === 0) {
    showToast('Suggest links first.', 'error')
    return
  }
  showNodesOnGraph(links.flatMap((l) => [l.node1, l.node2]))
}

const writingNow = ref<Set<string>>(new Set())
async function busy(modelName: string, work: () => Promise<void>) {
  if (writingNow.value.has(modelName)) return
  writingNow.value = new Set([...writingNow.value, modelName])
  try {
    await work()
  } catch (e) {
    showToast((e as Error).message, 'error')
  } finally {
    const next = new Set(writingNow.value)
    next.delete(modelName)
    writingNow.value = next
  }
}

async function onWriteToGraph(m: SavedModel) {
  if (m.taskType === 'LinkPrediction') {
    const type = linkWriteType.value[m.modelName]?.trim() || `PREDICTED_${m.targetRelationshipType}`
    await busy(m.modelName, async () => {
      const n = await ml.writeLinks(m.modelName, type, linkTopN(m))
      showToast(`Created ${n} ${type} relationship(s).`, 'success')
      void schema.fetchSchema()
    })
    return
  }
  await busy(m.modelName, async () => {
    const result = await ml.predictWrite(m, {
      writeProperty: m.targetProperty,
      overwriteExisting: false,
    })
    showToast(
      `Wrote ${result.nodesWritten} node(s); kept ${result.nodesSkipped} that already had ${result.writeProperty}.`,
      'success',
    )
    void schema.fetchSchema()
  })
}

// -- Link prediction ---------------------------------------------
const linkTopNByModel = ref<Record<string, number>>({})
const linkWriteType = ref<Record<string, string>>({})
function linkTopN(m: SavedModel): number {
  return linkTopNByModel.value[m.modelName] ?? 25
}
function setLinkTopN(m: SavedModel, value: string) {
  const n = Math.max(1, Math.min(10000, Math.round(Number(value) || 25)))
  linkTopNByModel.value = { ...linkTopNByModel.value, [m.modelName]: n }
}
function setLinkWriteType(m: SavedModel, value: string) {
  linkWriteType.value = { ...linkWriteType.value, [m.modelName]: value }
}

// -- Explain a prediction ----------------------------------------
const explainNode = ref<Record<string, string>>({})
const explainMethod = ref<Record<string, ExplainMethod>>({})
const explainingNow = ref<Set<string>>(new Set())
const EXPLAIN_METHODS: { value: ExplainMethod; label: string }[] = [
  { value: 'subgraphAudit', label: 'Which relationships carry it (audit)' },
  { value: 'counterfactual', label: 'What would change it (counterfactual)' },
  { value: 'pgExplainer', label: 'Most important relationships (PGExplainer)' },
]

async function onExplain(m: SavedModel) {
  const id = Number(explainNode.value[m.modelName])
  if (!Number.isInteger(id) || id < 0) {
    showToast('Enter the id of the node to explain.', 'error')
    return
  }
  if (explainingNow.value.has(m.modelName)) return
  explainingNow.value = new Set([...explainingNow.value, m.modelName])
  try {
    await ml.explain(m, id, explainMethod.value[m.modelName] ?? 'subgraphAudit')
  } catch (e) {
    showToast((e as Error).message, 'error')
  } finally {
    const next = new Set(explainingNow.value)
    next.delete(m.modelName)
    explainingNow.value = next
  }
}

function onShowExplanation(m: SavedModel) {
  const ex = ml.explanations[m.modelName]
  if (!ex) return
  showNodesOnGraph([
    ex.nodeId,
    ...ex.relationships.flatMap((r) => [r.sourceNodeId, r.targetNodeId]),
  ])
}

// -- Empty state: load the fraud-detection example ---------------
const loadingExample = ref(false)
const confirmReplace = ref(false)
const confirmReplaceInput = ref('')
const confirmReplaceInputEl = ref<HTMLInputElement | null>(null)
const FRAUD_EXAMPLE_ID = 'example-fraud-detection'
const FRAUD_NAME = 'Fraud Detection'
const confirmReplaceValid = computed(() => confirmReplaceInput.value === FRAUD_NAME)

async function onLoadFraudExample() {
  if (loadingExample.value) return
  // the active database has data: confirm before wiping it (as the Examples panel does)
  if (conn.nodeCount > 0) {
    confirmReplaceInput.value = ''
    confirmReplace.value = true
    nextTick(() => confirmReplaceInputEl.value?.focus())
    return
  }
  await doLoadFraudExample()
}

async function onConfirmReplace() {
  if (!confirmReplaceValid.value) return
  confirmReplace.value = false
  await doLoadFraudExample()
}

async function doLoadFraudExample() {
  const example = EXAMPLE_GRAPHS.find((e) => e.id === FRAUD_EXAMPLE_ID)
  if (!example) {
    showToast('Fraud Detection example is missing from this build.', 'error')
    return
  }
  loadingExample.value = true
  try {
    await api.importCypher('MATCH (n) DETACH DELETE n', conn.currentDatabase)
    const result = await api.importCypher(example.cypher, conn.currentDatabase)
    await conn.pollHealth()
    await schema.fetchSchema()
    styleStore.nodeStyles = {}
    styleStore.edgeStyles = {}
    styleStore.autoAssignMissing(schema.labels, settings.theme === 'dark')
    if (schema.labels.length > 0) {
      const pick = schema.labels[Math.floor(Math.random() * schema.labels.length)]
      styleStore.setNodeStyle(pick, { program: 'hexagon' })
    }
    api.save().catch(() => {})
    await ml.refreshTargetCandidates()
    preFillTarget('category')
    const parts: string[] = []
    if (result.stats.nodes_created) parts.push(`${result.stats.nodes_created} nodes`)
    if (result.stats.relationships_created)
      parts.push(`${result.stats.relationships_created} relationships`)
    showToast(`Loaded ${example.name}: ${parts.length > 0 ? parts.join(', ') : 'data'}`, 'success')
  } catch (e) {
    showToast(`Failed to load Fraud Detection: ${(e as Error).message}`, 'error')
  } finally {
    loadingExample.value = false
  }
}

// -- Mount -------------------------------------------------------
onMounted(async () => {
  await Promise.all([
    ml.listModels(),
    ml.refreshTargetCandidates(),
    ml.refreshDevices(),
    ml.refreshJobs(),
    schema.fetchSchema(),
  ])
  // Examples panel hand-off: open the wizard pre-selected
  const prefill = (window as { __traverseMlPrefill?: string }).__traverseMlPrefill
  if (prefill) {
    preFillTarget(prefill)
    delete (window as { __traverseMlPrefill?: string }).__traverseMlPrefill
  }
})

// new data (an import, say) can bring new targets
watch(
  () => schema.labelsDetail.length,
  () => {
    ml.refreshTargetCandidates()
  },
)

// -- Formatting --------------------------------------------------
function fmtTime(unix: number): string {
  if (!unix) return ''
  const ago = Math.floor(Date.now() / 1000 - unix)
  if (ago < 60) return `${ago}s ago`
  if (ago < 3600) return `${Math.floor(ago / 60)}m ago`
  if (ago < 86400) return `${Math.floor(ago / 3600)}h ago`
  return `${Math.floor(ago / 86400)}d ago`
}

function fmtArchitecture(kind: string): string {
  const k = kind.toLowerCase()
  if (k.startsWith('graphsage')) return 'GraphSAGE'
  if (k.startsWith('rgcn')) return 'R-GCN'
  if (k.startsWith('gat')) return 'GAT'
  if (k.startsWith('gcn')) return 'GCN'
  return kind
}

function fmtBatching(m: SavedModel): string {
  const b = m.batching
  if (!b || b.mode === 'fullGraph') return 'whole graph'
  if (b.mode === 'miniBatch') {
    const f = (b.fanouts ?? []).map((x) => (x < 0 ? 'all' : String(x))).join('/')
    return `mini-batches of ${b.batchSize ?? '?'}${f ? `, ${f} neighbors` : ''}`
  }
  return b.mode
}

const TASK_LABEL: Record<TaskKind, string> = {
  NodeClassification: 'category',
  NodeRegression: 'number',
  LinkPrediction: 'missing links',
}

function fmtNumber(v: number): string {
  return v.toLocaleString(undefined, { maximumFractionDigits: Math.abs(v) < 10 ? 3 : 1 })
}

function pct(v: number | undefined): string {
  return v === undefined ? '-' : `${(v * 100).toFixed(0)}%`
}

// -- Classifier predictions --------------------------------------
const expandedPredictions = ref<Set<string>>(new Set())
const predictingNow = ref<Set<string>>(new Set())

async function togglePredictions(m: SavedModel) {
  const name = m.modelName
  const have =
    m.taskType === 'NodeClassification'
      ? ml.predictByModel[name]?.size
      : m.taskType === 'NodeRegression'
        ? ml.valuesByModel[name]?.size
        : ml.linksByModel[name]?.length
  if (!have) {
    if (predictingNow.value.has(name)) return
    predictingNow.value = new Set([...predictingNow.value, name])
    try {
      await onPredict(m)
    } finally {
      const next = new Set(predictingNow.value)
      next.delete(name)
      predictingNow.value = next
    }
    return
  }
  const next = new Set(expandedPredictions.value)
  if (next.has(name)) next.delete(name)
  else next.add(name)
  expandedPredictions.value = next
}

function predictionEntries(modelName: string): {
  nodeId: number
  predictedClass: number
  confidence: number | null
}[] {
  const rows = ml.lastPredictions[modelName] ?? []
  return rows
    .map((r) => {
      const p = r.predictedProbabilities[r.predictedClass]
      return {
        nodeId: r.nodeId,
        predictedClass: r.predictedClass,
        confidence: typeof p === 'number' && Number.isFinite(p) ? p : null,
      }
    })
    .sort((a, b) => a.predictedClass - b.predictedClass || a.nodeId - b.nodeId)
}

function fmtConfidence(c: number | null): string {
  return c === null ? '' : `${Math.round(c * 100)}%`
}

function predictionCounts(modelName: string): { cls: number; count: number }[] {
  const map = ml.predictByModel[modelName]
  if (!map) return []
  const counts = new Map<number, number>()
  for (const cls of map.values()) counts.set(cls, (counts.get(cls) ?? 0) + 1)
  return Array.from(counts.entries())
    .map(([cls, count]) => ({ cls, count }))
    .sort((a, b) => a.cls - b.cls)
}

/** Classes the model actually predicts: the ones worth naming. */
function predictedClassesForModel(m: SavedModel): number[] {
  return predictionCounts(m.modelName).map((c) => c.cls)
}

/** `class N` unless the user named it (stored on the model as `classLabels`). */
function classDisplayName(m: SavedModel, cls: number): string {
  const label = m.classLabels[String(cls)]
  return label && label.trim().length > 0 ? label : `class ${cls}`
}

function hasAllClassNames(m: SavedModel): boolean {
  const classes = predictedClassesForModel(m)
  return classes.length > 0 && classes.every((c) => (m.classLabels[String(c)] ?? '').trim())
}

const editingClassNames = ref<Set<string>>(new Set())
function onClassNamesToggle(modelName: string, open: boolean) {
  const next = new Set(editingClassNames.value)
  if (open) next.add(modelName)
  else next.delete(modelName)
  editingClassNames.value = next
}

async function onClassNameInput(m: SavedModel, cls: number, value: string) {
  // optimistic: the predictions list reads the new name at once
  const next = { ...m.classLabels, [String(cls)]: value.trim() }
  for (const k of Object.keys(next)) if (!next[k]) delete next[k]
  m.classLabels = next
  try {
    await ml.setModelMetadata(m.modelName, 'classLabels', JSON.stringify(next))
  } catch (e) {
    showToast(`Saving class names failed: ${(e as Error).message}`, 'error')
  }
}

/** The graph view's community palette, so swatches match on-graph colours. */
function classColor(cls: number, modelName: string): string {
  const rank = new Map<number, number>()
  predictionCounts(modelName).forEach((c, i) => rank.set(c.cls, i))
  return colorForCommunity(cls, rank)
}

// -- Regression predictions --------------------------------------
function valueSummary(modelName: string): { n: number; min: number; max: number; mean: number } | null {
  const values = ml.valuesByModel[modelName]
  if (!values || values.size === 0) return null
  let min = Infinity
  let max = -Infinity
  let sum = 0
  for (const v of values.values()) {
    min = Math.min(min, v)
    max = Math.max(max, v)
    sum += v
  }
  return { n: values.size, min, max, mean: sum / values.size }
}

function valueEntries(modelName: string): { nodeId: number; value: number }[] {
  const values = ml.valuesByModel[modelName]
  if (!values) return []
  return Array.from(values.entries())
    .map(([nodeId, value]) => ({ nodeId, value }))
    .sort((a, b) => b.value - a.value)
    .slice(0, 200)
}
</script>

<template>
  <div class="ml-panel">
    <!-- Replace-database confirmation (load Fraud Detection over existing data) -->
    <Teleport to="body">
      <div
        v-if="confirmReplace"
        class="ml-modal-overlay"
        @click.self="confirmReplace = false"
      >
        <div class="ml-modal">
          <div class="ml-modal__title">Replace existing data?</div>
          <p class="ml-modal__desc">
            Loading <strong>Fraud Detection</strong> will clear the
            current graph in <strong>{{ conn.currentDatabase }}</strong>
            and replace it with the example.
          </p>
          <label class="ml-modal__desc">
            Type <strong>Fraud Detection</strong> to confirm:
          </label>
          <input
            ref="confirmReplaceInputEl"
            v-model="confirmReplaceInput"
            class="fg-input"
            placeholder="Fraud Detection"
            @keydown.enter="confirmReplaceValid && onConfirmReplace()"
            @keydown.escape="confirmReplace = false"
          />
          <div class="ml-modal__actions">
            <button class="fg-btn fg-btn--sm" @click="confirmReplace = false">
              Cancel
            </button>
            <button
              class="fg-btn fg-btn--sm fg-btn--danger"
              :disabled="!confirmReplaceValid"
              @click="onConfirmReplace"
            >
              Replace
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <!-- Delete-model confirmation modal -->
    <Teleport to="body">
      <div
        v-if="confirmDeleteModel"
        class="ml-modal-overlay"
        @click.self="confirmDeleteModel = null"
      >
        <div class="ml-modal">
          <div class="ml-modal__title">Delete model?</div>
          <p class="ml-modal__desc">
            <strong>{{ confirmDeleteModel }}</strong> and its saved file will be removed.
            Predictions already written to the graph stay.
          </p>
          <div class="ml-modal__actions">
            <button class="fg-btn fg-btn--sm" @click="confirmDeleteModel = null">
              Cancel
            </button>
            <button class="fg-btn fg-btn--sm fg-btn--danger" @click="onConfirmDelete">
              Delete
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <!-- Header -->
    <div class="ml-panel__header">
      <span class="ml-panel__title">ML Pipelines</span>
      <button
        class="fg-icon-btn"
        :disabled="ml.isLoadingModels"
        title="Refresh"
        @click="ml.listModels(); ml.refreshJobs(); ml.refreshDevices()"
      >
        <svg
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          stroke-linejoin="round"
          :class="{ spinning: ml.isLoadingModels }"
        >
          <path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" />
          <path d="M21 3v5h-5" />
          <path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" />
          <path d="M8 16H3v5" />
        </svg>
      </button>
    </div>

    <div class="ml-panel__body">
      <!-- Empty state - no models, nothing to predict yet -->
      <section
        v-if="ml.models.length === 0 && !canStartWizard"
        class="ml-panel__section"
      >
        <div class="ml-panel__empty">
          <p>
            You don't have any models or labelled data yet. The
            quickest way to see what this does is to load the
            <strong>Fraud Detection</strong> example.
          </p>
          <button
            class="fg-btn fg-btn--sm fg-btn--primary"
            :disabled="loadingExample"
            @click="onLoadFraudExample"
          >
            Show me how
          </button>
        </div>
      </section>

      <!-- Training runs started elsewhere: another tab, before a reload, from Cypher -->
      <section v-if="otherJobs.length > 0" class="ml-panel__section">
        <div class="ml-panel__section-title">Training now</div>
        <div
          v-for="j in otherJobs"
          :key="j.jobId"
          class="ml-panel__job"
        >
          <div class="ml-panel__job-main">
            <div class="ml-panel__model-name">{{ j.modelName }}</div>
            <div class="ml-panel__model-meta">
              <template v-if="j.status === 'queued'">
                waiting, {{ (j.queuePosition ?? 0) + 1 }} ahead
              </template>
              <template v-else>
                {{ fmtProgress(j.completedTrials, j.plannedTrials) }},
                {{ j.elapsedSeconds }}s on {{ j.device }}
              </template>
            </div>
            <div
              v-if="j.plannedTrials > 0"
              class="ml-panel__progress"
            >
              <div
                class="ml-panel__progress-bar"
                :style="{ width: `${(100 * j.completedTrials) / j.plannedTrials}%` }"
              />
            </div>
          </div>
          <button
            class="fg-btn fg-btn--sm fg-btn--ghost"
            title="Stop this training run"
            @click="onCancelJob(j.jobId)"
          >
            Cancel
          </button>
        </div>
      </section>

      <!-- Saved models -->
      <section v-if="ml.models.length > 0" class="ml-panel__section">
        <div class="ml-panel__section-title">Your models</div>
        <div class="ml-panel__model-list">
          <div
            v-for="m in ml.models"
            :key="m.modelName"
            class="ml-panel__model-row"
          >
            <div class="ml-panel__model-main">
              <div class="ml-panel__model-name">{{ m.modelName }}</div>
              <div class="ml-panel__model-meta">
                <template v-if="m.taskType === 'LinkPrediction'">
                  Predicts <strong>{{ m.targetRelationshipType }}</strong> links
                </template>
                <template v-else>
                  Predicts <strong>{{ m.targetProperty }}</strong> ({{ TASK_LABEL[m.taskType] }})
                </template>,
                {{ fmtArchitecture(m.architecture) }},
                <template v-if="m.device">{{ m.device }},</template>
                {{ fmtBatching(m) }},
                trained {{ fmtTime(m.trainedAtUnix) }}
              </div>

              <!-- Held-out test scores, per task -->
              <div
                v-if="m.taskType === 'NodeClassification' && m.accuracy > 0"
                class="ml-panel__model-acc"
              >
                <strong>{{ pct(m.accuracy) }}</strong>
                accuracy on {{ m.testSamples || m.metrics?.test_count || '?' }} held-out
                example(s)
                <span v-if="m.labelledNodes < 30" class="ml-panel__warn-inline">
                  (small training set, so the score is noisy)
                </span>
              </div>
              <div
                v-else-if="m.taskType === 'NodeRegression'"
                class="ml-panel__model-acc"
              >
                Typical error
                <strong>+/-{{ fmtNumber(m.testMetrics.ROOT_MEAN_SQUARED_ERROR ?? 0) }}</strong>
                (RMSE; mean absolute
                {{ fmtNumber(m.testMetrics.MEAN_ABSOLUTE_ERROR ?? 0) }})
                on {{ m.testSamples }} held-out node(s)
              </div>
              <div
                v-else-if="m.taskType === 'LinkPrediction'"
                class="ml-panel__model-acc"
              >
                <strong>{{ pct(m.testMetrics.AUCPR) }}</strong> AUCPR,
                {{ pct(m.testMetrics.ROC_AUC) }} ROC AUC on {{ m.testSamples }} held-out
                relationship(s)
              </div>
            </div>

            <!-- Classifier: per-class breakdown -->
            <details
              v-if="m.taskType === 'NodeClassification' && m.metrics"
              class="ml-panel__metrics"
            >
              <summary class="ml-panel__metrics-summary">
                F1 weighted {{ pct(m.metrics.f1_weighted) }},
                F1 macro {{ pct(m.metrics.f1_macro) }}
              </summary>
              <table class="ml-panel__metrics-table">
                <thead>
                  <tr>
                    <th>class</th>
                    <th>precision</th>
                    <th>recall</th>
                    <th>F1</th>
                    <th>support</th>
                  </tr>
                </thead>
                <tbody>
                  <tr
                    v-for="row in m.metrics.per_class"
                    :key="row.class"
                  >
                    <td>
                      <span
                        class="ml-panel__pred-swatch"
                        :style="{ background: classColor(row.class, m.modelName) }"
                      />
                      {{ classDisplayName(m, row.class) }}
                    </td>
                    <td>{{ pct(row.precision) }}</td>
                    <td>{{ pct(row.recall) }}</td>
                    <td>{{ pct(row.f1) }}</td>
                    <td>{{ row.support }}</td>
                  </tr>
                </tbody>
              </table>
            </details>

            <!-- Classifier: domain names for the classes (`0 = legit, 1 = fraud`) -->
            <details
              v-if="m.taskType === 'NodeClassification' && predictedClassesForModel(m).length > 0"
              class="ml-panel__class-names"
              :open="editingClassNames.has(m.modelName)"
              @toggle="(e: any) => onClassNamesToggle(m.modelName, e.target.open)"
            >
              <summary class="ml-panel__class-names-summary">
                Name the classes
                <span v-if="hasAllClassNames(m)" class="ml-panel__class-names-set">(set)</span>
              </summary>
              <div class="ml-panel__class-names-grid">
                <label
                  v-for="cls in predictedClassesForModel(m)"
                  :key="cls"
                  class="ml-panel__class-name-row"
                >
                  <span
                    class="ml-panel__pred-swatch"
                    :style="{ background: classColor(cls, m.modelName) }"
                  />
                  <span class="ml-panel__class-name-num">{{ cls }}</span>
                  <input
                    class="fg-input fg-input--sm"
                    :value="m.classLabels[String(cls)] ?? ''"
                    :placeholder="`class ${cls}`"
                    @change="(e: any) => onClassNameInput(m, cls, e.target.value)"
                  />
                </label>
              </div>
            </details>

            <!-- Predictions -->
            <div class="ml-panel__predictions">
              <button
                type="button"
                class="ml-panel__predictions-toggle"
                :disabled="predictingNow.has(m.modelName)"
                @click="togglePredictions(m)"
              >
                <template v-if="predictingNow.has(m.modelName)">
                  <span class="ml-panel__spinner" aria-hidden="true" />
                  Predicting...
                </template>
                <template
                  v-else-if="m.taskType === 'NodeClassification' && predictionCounts(m.modelName).length > 0"
                >
                  <DisclosureIcon :open="expandedPredictions.has(m.modelName)" />
                  Predictions:
                  <span
                    v-for="(c, i) in predictionCounts(m.modelName)"
                    :key="c.cls"
                    class="ml-panel__pred-pill"
                  >
                    <span class="ml-panel__pred-swatch" :style="{ background: classColor(c.cls, m.modelName) }" />
                    {{ classDisplayName(m, c.cls) }}: {{ c.count }}<span v-if="i < predictionCounts(m.modelName).length - 1">,&nbsp;</span>
                  </span>
                </template>
                <template v-else-if="m.taskType === 'NodeRegression' && valueSummary(m.modelName)">
                  <DisclosureIcon :open="expandedPredictions.has(m.modelName)" />
                  Predictions: {{ valueSummary(m.modelName)!.n }} node(s),
                  {{ fmtNumber(valueSummary(m.modelName)!.min) }} to
                  {{ fmtNumber(valueSummary(m.modelName)!.max) }}, mean
                  {{ fmtNumber(valueSummary(m.modelName)!.mean) }}
                </template>
                <template
                  v-else-if="m.taskType === 'LinkPrediction' && (ml.linksByModel[m.modelName]?.length ?? 0) > 0"
                >
                  <DisclosureIcon :open="expandedPredictions.has(m.modelName)" />
                  {{ ml.linksByModel[m.modelName].length }} suggested link(s)
                </template>
                <template v-else>
                  <DisclosureIcon :open="false" />
                  Predictions: not computed yet (click to compute)
                </template>
              </button>

              <template v-if="expandedPredictions.has(m.modelName)">
                <div
                  v-if="m.taskType === 'NodeClassification' && predictionEntries(m.modelName).length > 0"
                  class="ml-panel__pred-table"
                >
                  <div
                    v-for="row in predictionEntries(m.modelName)"
                    :key="row.nodeId"
                    class="ml-panel__pred-row"
                  >
                    <span class="ml-panel__pred-swatch" :style="{ background: classColor(row.predictedClass, m.modelName) }" />
                    <span class="ml-panel__pred-node">node {{ row.nodeId }}</span>
                    <span class="ml-panel__pred-class">&rarr; {{ classDisplayName(m, row.predictedClass) }}</span>
                    <span
                      v-if="row.confidence !== null"
                      class="ml-panel__pred-conf"
                      :title="`The model gave this class ${fmtConfidence(row.confidence)} probability`"
                    >
                      {{ fmtConfidence(row.confidence) }}
                    </span>
                  </div>
                </div>
                <div
                  v-else-if="m.taskType === 'NodeRegression' && valueEntries(m.modelName).length > 0"
                  class="ml-panel__pred-table"
                >
                  <div
                    v-for="row in valueEntries(m.modelName)"
                    :key="row.nodeId"
                    class="ml-panel__pred-row"
                  >
                    <span class="ml-panel__pred-node">node {{ row.nodeId }}</span>
                    <span class="ml-panel__pred-class">&rarr; {{ fmtNumber(row.value) }}</span>
                  </div>
                </div>
                <div
                  v-else-if="m.taskType === 'LinkPrediction'"
                  class="ml-panel__pred-table"
                >
                  <div
                    v-for="l in ml.linksByModel[m.modelName] ?? []"
                    :key="`${l.node1}-${l.node2}`"
                    class="ml-panel__pred-row"
                  >
                    <span class="ml-panel__pred-node">node {{ l.node1 }} - node {{ l.node2 }}</span>
                    <span class="ml-panel__pred-conf">{{ pct(l.probability) }}</span>
                  </div>
                </div>
              </template>
            </div>

            <!-- Link prediction: how many, and as what -->
            <div v-if="m.taskType === 'LinkPrediction'" class="ml-panel__row">
              <label class="ml-panel__field">
                <span class="ml-panel__label">Top</span>
                <input
                  class="fg-input fg-input--sm ml-panel__num"
                  type="number"
                  min="1"
                  :value="linkTopN(m)"
                  @change="(e: any) => setLinkTopN(m, e.target.value)"
                />
              </label>
              <label class="ml-panel__field ml-panel__grow">
                <span class="ml-panel__label">Write as</span>
                <input
                  class="fg-input fg-input--sm ml-panel__grow"
                  :value="linkWriteType[m.modelName] ?? ''"
                  :placeholder="`PREDICTED_${m.targetRelationshipType}`"
                  @change="(e: any) => setLinkWriteType(m, e.target.value)"
                />
              </label>
            </div>

            <!-- Classifier: why did it predict that? -->
            <details
              v-if="m.taskType === 'NodeClassification' && !/rgcn/i.test(m.architecture)"
              class="ml-panel__explain"
            >
              <summary class="ml-panel__metrics-summary">Explain a prediction</summary>
              <div class="ml-panel__row">
                <input
                  class="fg-input fg-input--sm ml-panel__num-wide"
                  placeholder="node id"
                  :value="explainNode[m.modelName] ?? ''"
                  @input="(e: any) => (explainNode[m.modelName] = e.target.value)"
                  @keydown.enter="onExplain(m)"
                />
                <FgSelect
                  :model-value="explainMethod[m.modelName] ?? 'subgraphAudit'"
                  :options="EXPLAIN_METHODS"
                  class="ml-panel__grow"
                  @update:model-value="(v: string) => (explainMethod[m.modelName] = v as ExplainMethod)"
                />
                <button
                  class="fg-btn fg-btn--sm"
                  :disabled="explainingNow.has(m.modelName)"
                  @click="onExplain(m)"
                >
                  <span v-if="explainingNow.has(m.modelName)" class="ml-panel__spinner" />
                  Explain
                </button>
              </div>
              <div v-if="ml.explanations[m.modelName]" class="ml-panel__explanation">
                <p class="ml-panel__help">{{ ml.explanations[m.modelName].summary }}</p>
                <div
                  v-for="(r, i) in ml.explanations[m.modelName].relationships"
                  :key="i"
                  class="ml-panel__pred-row"
                >
                  <span class="ml-panel__pred-node">
                    node {{ r.sourceNodeId }} - node {{ r.targetNodeId }}
                  </span>
                  <span v-if="r.score !== null" class="ml-panel__pred-conf">
                    {{ r.score.toFixed(2) }}
                  </span>
                </div>
                <button
                  v-if="ml.explanations[m.modelName].relationships.length > 0"
                  class="fg-btn fg-btn--sm fg-btn--ghost"
                  @click="onShowExplanation(m)"
                >
                  Show these on the graph
                </button>
              </div>
            </details>

            <div class="ml-panel__action-bar">
              <!-- Lucide `play` -->
              <button
                class="ml-panel__action"
                :title="m.taskType === 'LinkPrediction' ? 'Suggest the most likely missing links' : 'Predict again on the current graph'"
                @click="onPredict(m)"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z" />
                </svg>
                <span>{{ m.taskType === 'LinkPrediction' ? 'Suggest' : 'Predict' }}</span>
              </button>
              <!-- Lucide `eye` -->
              <button
                class="ml-panel__action"
                :class="{ 'ml-panel__action--active': isShowingOnGraph(m.modelName) }"
                :title="m.taskType === 'NodeClassification' ? 'Colour nodes by predicted class' : m.taskType === 'NodeRegression' ? 'Size nodes by predicted value' : 'Show the suggested pairs'"
                @click="onShowOnGraph(m)"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0" />
                  <circle cx="12" cy="12" r="3" />
                </svg>
                <span>{{ isShowingOnGraph(m.modelName) ? 'Hide' : 'Show' }}</span>
              </button>
              <!-- Lucide `square-pen` -->
              <button
                class="ml-panel__action"
                :disabled="writingNow.has(m.modelName)"
                :title="m.taskType === 'LinkPrediction' ? `Create the top ${linkTopN(m)} suggested relationships` : `Set n.${m.targetProperty} on nodes that don't have it yet`"
                @click="onWriteToGraph(m)"
              >
                <span v-if="writingNow.has(m.modelName)" class="ml-panel__spinner" />
                <svg v-else width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" />
                  <path d="M18.375 2.625a1 1 0 0 1 3 3l-9.013 9.014a2 2 0 0 1-.853.505l-2.873.84a.5.5 0 0 1-.62-.62l.84-2.873a2 2 0 0 1 .506-.852z" />
                </svg>
                <span>{{ writingNow.has(m.modelName) ? 'Writing...' : 'Write' }}</span>
              </button>
              <!-- Lucide `download` -->
              <button
                class="ml-panel__action"
                title="Download as .tvmodel"
                @click="onExportModel(m.modelName)"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M12 15V3" />
                  <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
                  <path d="m7 10 5 5 5-5" />
                </svg>
                <span>Export</span>
              </button>
              <!-- Lucide `trash-2` -->
              <button
                class="ml-panel__action ml-panel__action--danger"
                title="Delete model"
                @click="confirmDeleteModel = m.modelName"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M10 11v6" />
                  <path d="M14 11v6" />
                  <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" />
                  <path d="M3 6h18" />
                  <path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
                </svg>
                <span>Delete</span>
              </button>
            </div>
          </div>
        </div>
      </section>

      <!-- Train new model trigger -->
      <section
        v-if="!wizardOpen && canStartWizard"
        class="ml-panel__section"
      >
        <button
          class="fg-btn fg-btn--sm fg-btn--primary ml-panel__btn-wide"
          @click="wizardOpen = true"
        >
          + Train a new model
        </button>
      </section>

      <!-- Wizard -->
      <template v-if="wizardOpen">
        <!-- Step 1 -->
        <section class="ml-panel__section">
          <div class="ml-panel__section-title">1. What do you want to predict?</div>
          <div class="ml-panel__tasks">
            <button
              v-for="t in TASKS"
              :key="t.value"
              type="button"
              class="ml-panel__task"
              :class="{ 'ml-panel__task--selected': task === t.value }"
              @click="task = t.value"
            >
              <span class="ml-panel__task-title">{{ t.label }}</span>
              <span class="ml-panel__task-hint">{{ t.hint }}</span>
            </button>
          </div>
          <FgSelect
            v-model="target"
            :options="targetOptions"
            :placeholder="task === 'LinkPrediction' ? 'Pick a relationship type...' : 'Pick a property...'"
            :disabled="targetOptions.length === 0"
            class="ml-panel__grow"
          />

          <template v-if="task === 'NodeClassification'">
            <p v-if="ml.targets.length === 0" class="ml-panel__warn">
              No integer properties found on your nodes. Add an
              "answer" property to some of your nodes first - e.g.
              <code>MATCH (n:Customer {name:'Alice'}) SET n.category = 0</code>
            </p>
            <p
              v-else-if="visibleTargets.length === 0 && !showAllTargets"
              class="ml-panel__warn"
            >
              None of your integer properties look like class labels
              (they all have many distinct values or few examples).
              Mark a few nodes with a small label like 0 or 1 to teach
              the model what to predict - or
              <a href="#" @click.prevent="toggleShowAllTargets">show all properties anyway</a>.
            </p>
            <button
              v-if="hiddenTargetCount > 0"
              type="button"
              class="ml-panel__adv-toggle"
              @click="toggleShowAllTargets"
            >
              <DisclosureIcon :open="showAllTargets" />
              {{ showAllTargets ? 'Hide' : 'Show' }} {{ hiddenTargetCount }} more
              (continuous-looking properties - usually inputs, not predictions)
            </button>
          </template>
          <p
            v-else-if="task === 'NodeRegression' && ml.regressionTargets.length === 0"
            class="ml-panel__warn"
          >
            No numeric properties found on your nodes.
          </p>
          <template v-else-if="task === 'LinkPrediction'">
            <p class="ml-panel__help">
              The model learns from the relationships of this type that exist and suggests
              the most likely missing ones. Optionally narrow the pairs it considers:
            </p>
            <div class="ml-panel__row">
              <label class="ml-panel__field ml-panel__grow">
                <span class="ml-panel__label">From</span>
                <FgSelect v-model="sourceLabel" :options="labelOptions" class="ml-panel__grow" />
              </label>
              <label class="ml-panel__field ml-panel__grow">
                <span class="ml-panel__label">To</span>
                <FgSelect v-model="targetLabel" :options="labelOptions" class="ml-panel__grow" />
              </label>
            </div>
          </template>
          <p
            v-if="task !== 'LinkPrediction' && selectedTargetStats && selectedTargetStats.labelledNodes > 0"
            class="ml-panel__help"
          >
            The model will learn from these
            <strong>{{ selectedTargetStats.labelledNodes }}</strong>
            example(s) and predict the value for the remaining
            <strong>{{ selectedTargetStats.totalNodes - selectedTargetStats.labelledNodes }}</strong>
            node(s).
          </p>
        </section>

        <!-- Step 2 -->
        <section v-if="target" class="ml-panel__section">
          <div class="ml-panel__section-title">2. What should the model look at?</div>
          <p class="ml-panel__help">
            Pre-ticked properties are the numeric values on your
            nodes. Who connects to whom is always taken into account.
          </p>
          <div v-if="availableFeatureProperties.length === 0" class="ml-panel__empty">
            No numeric properties to use as inputs. Add at least one
            (a degree or PageRank score works well) for the model to learn from.
          </div>
          <div v-else class="ml-panel__chips">
            <label
              v-for="prop in availableFeatureProperties"
              :key="prop.name"
              class="ml-panel__chip"
              :class="{ 'ml-panel__chip--checked': checkedFeatures.has(prop.name) }"
            >
              <input
                type="checkbox"
                :checked="checkedFeatures.has(prop.name)"
                @change="toggleFeature(prop.name)"
              />
              <span class="ml-panel__chip-name">{{ prop.name }}</span>
              <span class="ml-panel__chip-type">{{ prop.type }}</span>
            </label>
          </div>
        </section>

        <!-- Step 3 - architecture cards -->
        <section v-if="target" class="ml-panel__section">
          <div class="ml-panel__section-title">3. Choose how the model learns</div>
          <div class="ml-panel__arch-list">
            <label
              class="ml-panel__arch-card"
              :class="{ 'ml-panel__arch-card--selected': architecture === 'Best' }"
            >
              <input type="radio" value="Best" v-model="architecture" />
              <div class="ml-panel__arch-main">
                <div class="ml-panel__arch-title">Compare and keep the best</div>
                <div class="ml-panel__arch-tagline">
                  <strong>Not sure? Start here.</strong>
                  Trains each model below, scores them with cross-validation and keeps the
                  winner. Takes a few times longer.
                </div>
              </div>
            </label>
            <label
              class="ml-panel__arch-card"
              :class="{ 'ml-panel__arch-card--selected': architecture === 'Gcn' }"
            >
              <input type="radio" value="Gcn" v-model="architecture" />
              <div class="ml-panel__arch-main">
                <div class="ml-panel__arch-title">GCN</div>
                <div class="ml-panel__arch-tagline">
                  <strong>A solid first try.</strong>
                  Averages over every neighbour, weighted by how connected they are.
                </div>
              </div>
            </label>
            <label
              class="ml-panel__arch-card"
              :class="{ 'ml-panel__arch-card--selected': architecture === 'GraphSage' }"
            >
              <input type="radio" value="GraphSage" v-model="architecture" />
              <div class="ml-panel__arch-main">
                <div class="ml-panel__arch-title">GraphSAGE</div>
                <div class="ml-panel__arch-tagline">
                  <strong>Best for graphs that keep growing.</strong>
                  Keeps a node's own data apart from its neighbours', so it generalizes to nodes
                  added after training.
                </div>
              </div>
            </label>
            <label
              class="ml-panel__arch-card"
              :class="{ 'ml-panel__arch-card--selected': architecture === 'Gat' }"
            >
              <input type="radio" value="Gat" v-model="architecture" />
              <div class="ml-panel__arch-main">
                <div class="ml-panel__arch-title">GAT</div>
                <div class="ml-panel__arch-tagline">
                  <strong>When some neighbours matter more than others.</strong>
                  Learns how much attention to pay to each one.
                </div>
              </div>
            </label>
            <label
              v-if="schema.relationshipTypes.length > 1"
              class="ml-panel__arch-card"
              :class="{ 'ml-panel__arch-card--selected': architecture === 'RGcn' }"
            >
              <input type="radio" value="RGcn" v-model="architecture" />
              <div class="ml-panel__arch-main">
                <div class="ml-panel__arch-title">R-GCN</div>
                <div class="ml-panel__arch-tagline">
                  <strong>When the relationship type carries meaning.</strong>
                  Learns separately from each type of relationship (owns, employs, knows...).
                </div>
              </div>
            </label>
          </div>
        </section>

        <!-- Train button + advanced -->
        <section v-if="target" class="ml-panel__section">
          <button
            class="fg-btn fg-btn--sm fg-btn--primary ml-panel__btn-wide"
            :disabled="!canTrain"
            @click="onTrain"
          >
            <template v-if="ml.isTraining">
              <span class="ml-panel__spinner" aria-hidden="true"></span>
              {{ ml.trainPhase.message }}
            </template>
            <template v-else>Train and predict</template>
          </button>
          <p v-if="blockReason && !ml.isTraining" class="ml-panel__help">
            {{ blockReason }}
          </p>
          <div v-if="ml.isTraining" class="ml-panel__job">
            <div class="ml-panel__job-main">
              <div class="ml-panel__model-meta">
                <template v-if="ml.trainPhase.step === 'training'">
                  {{ fmtProgress(ml.trainPhase.completedTrials, ml.trainPhase.plannedTrials) }},
                  {{ ml.trainPhase.seconds }}s
                </template>
                <template v-else>{{ ml.trainPhase.message }}</template>
              </div>
              <div
                v-if="ml.trainPhase.plannedTrials > 0"
                class="ml-panel__progress"
              >
                <div
                  class="ml-panel__progress-bar"
                  :style="{ width: `${(100 * ml.trainPhase.completedTrials) / ml.trainPhase.plannedTrials}%` }"
                />
              </div>
              <p class="ml-panel__help">
                Training runs on the server: you can keep using Studio, or close it.
              </p>
            </div>
            <button
              v-if="ml.trainPhase.jobId"
              class="fg-btn fg-btn--sm fg-btn--ghost"
              @click="onCancelTraining"
            >
              Cancel
            </button>
          </div>

          <button
            class="ml-panel__adv-toggle"
            type="button"
            @click="advancedOpen = !advancedOpen"
          >
            <DisclosureIcon :open="advancedOpen" /> Advanced settings
          </button>
          <div v-if="advancedOpen" class="ml-panel__advanced">
            <label class="ml-panel__field">
              <span class="ml-panel__label">Hidden size</span>
              <input
                v-model.number="hidden"
                type="number"
                min="4"
                class="fg-input fg-input--sm ml-panel__num-wide"
              />
            </label>
            <label v-if="architecture === 'Gat' || architecture === 'Best'" class="ml-panel__field">
              <span class="ml-panel__label">Attention heads</span>
              <input
                v-model.number="heads"
                type="number"
                min="1"
                class="fg-input fg-input--sm ml-panel__num-wide"
              />
            </label>
            <label class="ml-panel__field">
              <span class="ml-panel__label">Epochs</span>
              <input
                v-model.number="epochs"
                type="number"
                min="1"
                class="fg-input fg-input--sm ml-panel__num-wide"
              />
            </label>
            <label class="ml-panel__field">
              <span class="ml-panel__label">Learning rate</span>
              <input
                v-model.number="learningRate"
                type="number"
                step="0.001"
                class="fg-input fg-input--sm ml-panel__num-wide"
              />
            </label>
            <label class="ml-panel__field">
              <span class="ml-panel__label">Run on</span>
              <FgSelect v-model="device" :options="deviceOptions" class="ml-panel__grow" />
            </label>
            <p v-if="gpuNote" class="ml-panel__help">No GPU: {{ gpuNote }}</p>
            <label class="ml-panel__field">
              <span class="ml-panel__label">Training steps</span>
              <FgSelect
                :model-value="batching"
                :options="batchingOptions"
                class="ml-panel__grow"
                @update:model-value="(v: string) => (batching = v as BatchingMode)"
              />
            </label>
            <template v-if="batching === 'miniBatch'">
              <label class="ml-panel__field">
                <span class="ml-panel__label">Batch size</span>
                <input
                  v-model.number="batchSize"
                  type="number"
                  min="1"
                  class="fg-input fg-input--sm ml-panel__num-wide"
                />
              </label>
              <label class="ml-panel__field">
                <span class="ml-panel__label">Neighbors per hop</span>
                <input
                  v-model.number="fanout1"
                  type="number"
                  min="-1"
                  class="fg-input fg-input--sm ml-panel__num"
                  title="Neighbors sampled per node at the first hop (-1 = all)"
                />
                <input
                  v-model.number="fanout2"
                  type="number"
                  min="-1"
                  class="fg-input fg-input--sm ml-panel__num"
                  title="Neighbors sampled per node at the second hop (-1 = all)"
                />
              </label>
            </template>
            <label class="ml-panel__field">
              <span class="ml-panel__label">Save as</span>
              <input
                v-model="modelNameOverride"
                class="fg-input fg-input--sm ml-panel__grow"
                placeholder="auto-generated if blank"
              />
            </label>
          </div>

          <button
            class="ml-panel__cancel"
            type="button"
            @click="wizardOpen = false"
          >
            Close
          </button>

          <!-- Inline error if training failed -->
          <div v-if="ml.trainError" class="ml-panel__error">
            <strong>Training didn't finish</strong>
            <p>{{ ml.trainError }}</p>
            <button
              class="fg-btn fg-btn--sm fg-btn--ghost"
              @click="ml.resetTrainPhase()"
            >
              Dismiss
            </button>
          </div>
        </section>
      </template>
    </div>
  </div>
</template>

<style scoped>
.ml-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.ml-panel__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 12px;
  border-bottom: 1px solid var(--tv-border-default);
  height: 38px;
  flex-shrink: 0;
}

.ml-panel__title {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}
.spinning {
  animation: ml-panel-spin 0.6s linear infinite;
}

.ml-panel__body {
  flex: 1;
  overflow-y: auto;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}


.ml-panel__section {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.ml-panel__section-title {
  font-size: var(--tv-font-size-xs);
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
}

.ml-panel__help {
  margin: 0;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  line-height: 1.5;
}

.ml-panel__warn {
  margin: 0;
  padding: 8px 10px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-warn, #d97706);
  background: var(--tv-warn-subtle, rgba(217, 119, 6, 0.08));
  border: 1px solid var(--tv-warn, #d97706);
  border-radius: var(--tv-radius-sm);
  line-height: 1.5;
}
.ml-panel__warn code {
  font-family: var(--tv-font-mono, var(--tv-font-sans));
  font-size: 11px;
  padding: 1px 4px;
  background: rgba(0, 0, 0, 0.1);
  border-radius: 2px;
}

.ml-panel__empty {
  padding: 12px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  background: var(--tv-bg-base);
  border: 1px dashed var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  text-align: center;
  display: flex;
  flex-direction: column;
  gap: 10px;
  align-items: center;
  line-height: 1.5;
}

/* Saved-model list */
.ml-panel__model-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.ml-panel__model-row {
  display: block;
  width: 100%;
  padding: 10px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  background: var(--tv-bg-base);
  box-sizing: border-box;
}
.ml-panel__model-main {
  display: block;
  width: 100%;
  margin-bottom: 8px;
}
.ml-panel__model-name {
  display: block;
  width: 100%;
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
  font-family: var(--tv-font-mono, var(--tv-font-sans));
  overflow-wrap: anywhere;
  margin-bottom: 2px;
}
.ml-panel__model-meta {
  display: block;
  width: 100%;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  line-height: 1.5;
  margin-bottom: 2px;
}
.ml-panel__model-acc {
  display: block;
  width: 100%;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
}
.ml-panel__predictions {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin-top: 6px;
}
.ml-panel__predictions-toggle {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px;
  padding: 2px 0;
  background: transparent;
  border: none;
  color: var(--tv-text-secondary);
  font-size: var(--tv-font-size-xs);
  cursor: pointer;
  text-align: left;
}
.ml-panel__predictions-toggle:hover {
  color: var(--tv-text-primary);
}
.ml-panel__pred-pill {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}
.ml-panel__pred-swatch {
  display: inline-block;
  width: 10px;
  height: 10px;
  border-radius: 2px;
  flex-shrink: 0;
}
.ml-panel__pred-table {
  display: flex;
  flex-direction: column;
  gap: 2px;
  max-height: 240px;
  overflow-y: auto;
  padding: 4px 6px;
  background: var(--tv-bg-base);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  font-size: var(--tv-font-size-xs);
}
.ml-panel__pred-row {
  display: flex;
  align-items: center;
  gap: 6px;
  font-family: var(--tv-font-mono, var(--tv-font-sans));
}
.ml-panel__pred-node {
  color: var(--tv-text-primary);
}
.ml-panel__pred-class {
  color: var(--tv-text-muted);
  flex: 1;
}
.ml-panel__pred-conf {
  font-size: 11px;
  color: var(--tv-text-secondary);
  font-variant-numeric: tabular-nums;
  padding: 0 4px;
  border-radius: 4px;
  background: var(--tv-bg-base);
}

/* Class-name editor - disclosure on the saved-model card. */
.ml-panel__class-names {
  margin-top: 6px;
}
.ml-panel__class-names-summary {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  cursor: pointer;
  user-select: none;
}
.ml-panel__class-names-summary:hover {
  color: var(--tv-text-primary);
}
.ml-panel__class-names-set {
  margin-left: 4px;
  font-size: 10px;
  color: var(--tv-status-success);
}
.ml-panel__class-names-grid {
  display: flex;
  flex-direction: column;
  gap: 4px;
  /* Top/bottom margin so the inputs don't run into the
   * disclosure's border-top above or the model-actions row
   * border-top below when the panel is expanded. */
  margin: 6px 0 8px;
}
.ml-panel__class-name-row {
  display: flex;
  align-items: center;
  gap: 6px;
}
.ml-panel__class-name-num {
  font-family: var(--tv-font-mono, var(--tv-font-sans));
  font-size: 11px;
  color: var(--tv-text-muted);
  min-width: 18px;
}
.ml-panel__class-name-row .fg-input {
  flex: 1;
}

/* Inline "small training set" warning beside the accuracy line. */
.ml-panel__warn-inline {
  color: var(--tv-warn, #d97706);
  font-size: 11px;
}

/* Per-class metrics breakdown - collapsed by default. */
.ml-panel__metrics {
  margin-top: 6px;
}
.ml-panel__metrics-summary {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  cursor: pointer;
  user-select: none;
  font-variant-numeric: tabular-nums;
}
.ml-panel__metrics-summary:hover {
  color: var(--tv-text-primary);
}
.ml-panel__metrics-table {
  width: 100%;
  margin-top: 4px;
  font-size: 11px;
  font-family: var(--tv-font-mono, var(--tv-font-sans));
  border-collapse: collapse;
}
.ml-panel__metrics-table th,
.ml-panel__metrics-table td {
  text-align: left;
  padding: 2px 6px 2px 0;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}
.ml-panel__metrics-table th {
  color: var(--tv-text-muted);
  font-weight: 500;
}
.ml-panel__metrics-table td .ml-panel__pred-swatch {
  margin-right: 4px;
  vertical-align: middle;
}
/* Uniform action bar - 5 buttons in one row, every button has
 * the same compact icon+label form. Wraps to multiple rows only
 * when the panel is very narrow. */
.ml-panel__action-bar {
  display: grid;
  grid-template-columns: repeat(5, 1fr);
  gap: 2px;
  padding-top: 6px;
  border-top: 1px solid var(--tv-border-default);
}
.ml-panel__action {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 4px;
  height: 26px;
  padding: 0 4px;
  font-size: 11px;
  font-family: var(--tv-font-sans);
  color: var(--tv-text-muted);
  background: transparent;
  border: 1px solid transparent;
  border-radius: var(--tv-radius-sm);
  cursor: pointer;
  white-space: nowrap;
  min-width: 0;
  transition: color 0.12s, background 0.12s, border-color 0.12s;
}
.ml-panel__action svg {
  flex-shrink: 0;
}
.ml-panel__action span {
  overflow: hidden;
  text-overflow: ellipsis;
}
.ml-panel__action:hover:not(:disabled) {
  color: var(--tv-text-primary);
  background: var(--tv-bg-hover);
}
.ml-panel__action:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.ml-panel__action--active {
  color: var(--tv-accent);
  background: var(--tv-accent-subtle);
}
.ml-panel__action--active:hover:not(:disabled) {
  color: var(--tv-accent);
  background: var(--tv-accent-subtle);
}
.ml-panel__action--danger:hover:not(:disabled) {
  color: var(--tv-status-error, #dc2626);
  background: color-mix(in srgb, var(--tv-status-error, #dc2626) 10%, transparent);
}
/* Wrap to 3+2 when the panel is too narrow for 5 across. */
@container (max-width: 360px) {
  .ml-panel__action-bar {
    grid-template-columns: repeat(3, 1fr);
  }
}

/* Property chips */
.ml-panel__chips {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.ml-panel__chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 4px 10px;
  border: 1px solid var(--tv-border-default);
  border-radius: 999px;
  background: var(--tv-bg-base);
  cursor: pointer;
  font-size: var(--tv-font-size-xs);
}
.ml-panel__chip:hover {
  border-color: var(--tv-border-strong);
}
.ml-panel__chip--checked {
  border-color: var(--tv-border-focus);
  background: var(--tv-accent-subtle);
}
.ml-panel__chip input[type='checkbox'] {
  margin: 0;
  width: 12px;
  height: 12px;
  accent-color: var(--tv-accent);
}
.ml-panel__chip-name {
  font-weight: 500;
  color: var(--tv-text-primary);
}
.ml-panel__chip-type {
  color: var(--tv-text-muted);
  font-size: 10px;
}

/* Architecture cards */
.ml-panel__arch-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.ml-panel__arch-card {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 10px 12px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  background: var(--tv-bg-base);
  cursor: pointer;
}
.ml-panel__arch-card:hover {
  border-color: var(--tv-border-strong);
}
.ml-panel__arch-card--selected {
  border-color: var(--tv-border-focus);
  background: var(--tv-accent-subtle);
}
.ml-panel__arch-card input[type='radio'] {
  margin: 3px 0 0;
  accent-color: var(--tv-accent);
}
.ml-panel__arch-main {
  display: flex;
  flex-direction: column;
  gap: 3px;
}
.ml-panel__arch-title {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}
.ml-panel__arch-tagline {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  line-height: 1.5;
}
.ml-panel__arch-tagline strong {
  color: var(--tv-text-secondary);
}

/* Advanced toggle */
.ml-panel__adv-toggle {
  background: transparent;
  border: none;
  padding: 4px 0;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  cursor: pointer;
  text-align: left;
}
.ml-panel__advanced {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 10px;
  background: var(--tv-bg-base);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
}
.ml-panel__field {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.ml-panel__label {
  font-size: var(--tv-font-size-sm);
  color: var(--tv-text-secondary);
  flex-shrink: 0;
}
.ml-panel__grow {
  flex: 1;
  min-width: 0;
}
.ml-panel__num-wide {
  width: 96px;
  flex-shrink: 0;
  text-align: right;
}

/* Cancel link */
.ml-panel__cancel {
  background: transparent;
  border: none;
  padding: 6px 0;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  cursor: pointer;
  text-align: center;
}
.ml-panel__cancel:hover {
  color: var(--tv-text-primary);
}

.ml-panel__btn-wide {
  align-self: stretch;
}

/* Spinner */
.ml-panel__spinner {
  display: inline-block;
  width: 12px;
  height: 12px;
  margin-right: 6px;
  vertical-align: middle;
  border: 2px solid currentColor;
  border-right-color: transparent;
  border-radius: 50%;
  animation: ml-panel-spin 0.8s linear infinite;
}
@keyframes ml-panel-spin {
  to {
    transform: rotate(360deg);
  }
}

/* Inline error */
.ml-panel__error {
  margin-top: 8px;
  padding: 10px;
  background: rgba(220, 38, 38, 0.08);
  border: 1px solid #dc2626;
  border-radius: var(--tv-radius-md);
  color: var(--tv-text-primary);
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.ml-panel__error strong {
  font-size: var(--tv-font-size-sm);
  color: #dc2626;
}
.ml-panel__error p {
  margin: 0;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  line-height: 1.5;
  word-break: break-word;
}

/* Modal */
.ml-modal-overlay {
  position: fixed;
  inset: 0;
  z-index: 9999;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.5);
}
.ml-modal {
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
.ml-modal__title {
  font-size: var(--tv-font-size-md);
  font-weight: 600;
  color: var(--tv-text-primary);
}
.ml-modal__desc {
  margin: 0;
  font-size: var(--tv-font-size-sm);
  color: var(--tv-text-secondary);
  line-height: 1.5;
}
.ml-modal__actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 4px;
}

/* Task picker */
.ml-panel__tasks {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 6px;
}
.ml-panel__task {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 2px;
  padding: 8px 10px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  background: var(--tv-bg-base);
  cursor: pointer;
  text-align: left;
}
.ml-panel__task:hover {
  border-color: var(--tv-border-strong);
}
.ml-panel__task--selected {
  border-color: var(--tv-border-focus);
  background: var(--tv-accent-subtle);
}
.ml-panel__task-title {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}
.ml-panel__task-hint {
  font-size: 10px;
  color: var(--tv-text-muted);
}

/* Training jobs */
.ml-panel__job {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  background: var(--tv-bg-base);
}
.ml-panel__job-main {
  display: flex;
  flex-direction: column;
  gap: 4px;
  flex: 1;
  min-width: 0;
}
.ml-panel__progress {
  height: 4px;
  border-radius: 2px;
  background: var(--tv-border-default);
  overflow: hidden;
}
.ml-panel__progress-bar {
  height: 100%;
  background: var(--tv-accent);
  transition: width 0.3s ease;
}

/* Inline rows of fields */
.ml-panel__row {
  display: flex;
  align-items: center;
  gap: 6px;
}
.ml-panel__num {
  width: 64px;
  flex-shrink: 0;
  text-align: right;
}

/* Explanations */
.ml-panel__explain {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.ml-panel__explain[open] > .ml-panel__row {
  margin-top: 6px;
}
.ml-panel__explanation {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin-top: 6px;
}
</style>
