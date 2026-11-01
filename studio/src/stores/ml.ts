/**
 * ML store: a thin layer over the `traverse.ml.*` Cypher procedures. The
 * state lives on the server (models, training jobs, devices); this store
 * caches the latest lists and per-model results for the panel to render.
 *
 * Training is a job on the server: `trainAndPredict` queues one, follows
 * its progress, can cancel it, and predicts with the model once it's done.
 * Jobs keep running if the page goes away; `refreshJobs` finds them again.
 */

import { defineStore } from 'pinia'
import { ref, computed, type Ref } from 'vue'
import { api } from '../utils/api'
import { useConnectionStore } from './connection'

/** What a model predicts. */
export type TaskKind = 'NodeClassification' | 'NodeRegression' | 'LinkPrediction'

/** Procedure-name segment per task: `traverse.ml.pipeline.<segment>.*`. */
export const TASK_SEGMENT: Record<TaskKind, string> = {
  NodeClassification: 'nodeClassification',
  NodeRegression: 'nodeRegression',
  LinkPrediction: 'linkPrediction',
}

/** A model architecture the pipeline can try. */
export type ArchitectureChoice = 'Gcn' | 'GraphSage' | 'RGcn' | 'Gat'

/** How training steps see the graph: decided by the server, the whole
 *  graph every step, or sampled mini-batches. */
export type BatchingMode = 'auto' | 'fullGraph' | 'miniBatch'

/** A row from `targetCandidates()`. */
export interface TargetCandidate {
  propertyName: string
  totalNodes: number
  labelledNodes: number
  labelsWithProperty: string[]
  /** Distinct values among labelled nodes: few means class labels, many means a measurement. */
  distinctValues: number
}

export interface PerClassMetric {
  class: number
  precision: number
  recall: number
  f1: number
  support: number
}

export interface ClassificationMetrics {
  accuracy: number
  f1_weighted: number
  f1_macro: number
  per_class: PerClassMetric[]
  test_count: number
}

/** `{mode[, batchSize, fanouts]}` as the server reports it. */
export interface BatchingInfo {
  mode: string
  batchSize?: number
  fanouts?: number[]
}

/** A trained model in the catalog. */
export interface SavedModel {
  modelName: string
  taskType: TaskKind
  targetProperty: string
  targetRelationshipType: string
  architecture: string
  pipeline: string
  accuracy: number
  labelledNodes: number
  trainedAtUnix: number
  /** User-given class names, e.g. `{ "0": "legit", "1": "fraud" }`, stored on the model as
   *  `classLabels`. Empty until named; the UI falls back to `class N`. */
  classLabels: Record<string, string>
  /** Classification report on the held-out test set. */
  metrics: ClassificationMetrics | null
  /** Every held-out test metric, by GDS name (`ACCURACY`, `ROOT_MEAN_SQUARED_ERROR`, `AUCPR`...). */
  testMetrics: Record<string, number>
  trainSamples: number
  testSamples: number
  device: string
  batching: BatchingInfo | null
}

/** One row of `predict.stream` for a classifier. */
export interface PredictRow {
  nodeId: number
  predictedClass: number
  predictedProbabilities: number[]
}

/** A suggested relationship from a link-prediction model. */
export interface LinkSuggestion {
  node1: number
  node2: number
  probability: number
}

/** A row of `traverse.ml.devices()`. */
export interface DeviceInfo {
  device: string
  type: string
  name: string | null
  memoryBytes: number | null
  available: boolean
  reason: string | null
  isDefault: boolean
}

/** A row of `traverse.ml.trainJobs()`. */
export interface TrainJob {
  jobId: string
  status: 'queued' | 'running' | 'completed' | 'failed' | 'cancelled'
  modelName: string
  pipeline: string
  taskType: string
  device: string
  submittedAtUnix: number
  elapsedSeconds: number
  completedTrials: number
  plannedTrials: number
  queuePosition: number | null
  error: string
}

export type ExplainMethod = 'subgraphAudit' | 'counterfactual' | 'pgExplainer'

/** A relationship an explanation points at, with its score when the method gives one. */
export interface ExplainedRelationship {
  sourceNodeId: number
  targetNodeId: number
  score: number | null
}

export interface Explanation {
  method: ExplainMethod
  nodeId: number
  /** One sentence on what the explanation says. */
  summary: string
  relationships: ExplainedRelationship[]
}

/** What `trainAndPredict` needs. */
export interface TrainOptions {
  task: TaskKind
  /** The property to predict (node tasks) or the relationship type (link prediction). */
  target: string
  features: string[]
  /** One architecture, or several to compare (the best by cross-validation wins). */
  architectures: ArchitectureChoice[]
  hidden?: number
  /** Attention heads (GAT). */
  heads?: number
  epochs?: number
  learningRate?: number
  /** `''` uses the server default. */
  device?: string
  batching?: BatchingMode
  batchSize?: number
  fanouts?: [number, number]
  /** Link prediction: only pairs between these labels. */
  sourceLabel?: string
  targetLabel?: string
  modelNameOverride?: string
}

/** Progress shown while a model trains. */
export interface TrainPhase {
  step: 'idle' | 'preparing' | 'queued' | 'training' | 'predicting' | 'done' | 'failed'
  /** Seconds spent training so far. */
  seconds: number
  message: string
  /** Model-selection trials done and planned. */
  completedTrials: number
  plannedTrials: number
  /** Jobs ahead while queued. */
  queuePosition: number | null
  jobId: string | null
}

const IDLE: TrainPhase = {
  step: 'idle',
  seconds: 0,
  message: '',
  completedTrials: 0,
  plannedTrials: 0,
  queuePosition: null,
  jobId: null,
}

const num = (v: unknown, d = 0): number => (typeof v === 'number' && Number.isFinite(v) ? v : d)
const str = (v: unknown): string => (typeof v === 'string' ? v : '')

export const useMlStore = defineStore('ml', () => {
  const connection = useConnectionStore()

  async function callProc(
    cypher: string,
    parameters?: Record<string, unknown>,
  ): Promise<{ columns: string[]; rows: unknown[][] }> {
    const r = await api.query(cypher, parameters, connection.currentDatabase)
    return { columns: r.columns, rows: r.rows }
  }

  /** Rows as objects keyed by column. */
  function records(r: { columns: string[]; rows: unknown[][] }): Record<string, unknown>[] {
    return r.rows.map((row) => Object.fromEntries(r.columns.map((c, i) => [c, row[i]])))
  }

  // -- Target candidates -----------------------------------------
  /** Integer properties (class labels). */
  const targets = ref<TargetCandidate[]>([])
  /** Numeric properties (regression targets). */
  const regressionTargets = ref<TargetCandidate[]>([])
  const isLoadingTargets = ref(false)

  async function candidates(task: TaskKind): Promise<TargetCandidate[]> {
    const r = await callProc(
      `CALL traverse.ml.pipeline.${TASK_SEGMENT[task]}.targetCandidates()
         YIELD propertyName, totalNodes, labelledNodes, labelsWithProperty, distinctValues
       RETURN propertyName, totalNodes, labelledNodes, labelsWithProperty, distinctValues
       ORDER BY distinctValues ASC, labelledNodes DESC, propertyName`,
    )
    return records(r).map((x) => ({
      propertyName: str(x.propertyName),
      totalNodes: num(x.totalNodes),
      labelledNodes: num(x.labelledNodes),
      labelsWithProperty: ((x.labelsWithProperty as string[]) ?? []).slice(),
      distinctValues: num(x.distinctValues),
    }))
  }

  async function refreshTargetCandidates(): Promise<void> {
    isLoadingTargets.value = true
    try {
      const [cls, reg] = await Promise.all([
        candidates('NodeClassification').catch(() => []),
        candidates('NodeRegression').catch(() => []),
      ])
      targets.value = cls
      regressionTargets.value = reg
    } finally {
      isLoadingTargets.value = false
    }
  }

  // -- Devices ---------------------------------------------------
  const devices = ref<DeviceInfo[]>([])

  async function refreshDevices(): Promise<void> {
    try {
      const r = await callProc(
        `CALL traverse.ml.devices() YIELD device, type, name, memoryBytes, available, reason, isDefault
         RETURN device, type, name, memoryBytes, available, reason, isDefault`,
      )
      devices.value = records(r).map((x) => ({
        device: str(x.device),
        type: str(x.type),
        name: typeof x.name === 'string' ? x.name : null,
        memoryBytes: typeof x.memoryBytes === 'number' ? x.memoryBytes : null,
        available: x.available === true,
        reason: typeof x.reason === 'string' ? x.reason : null,
        isDefault: x.isDefault === true,
      }))
    } catch {
      devices.value = []
    }
  }

  // -- Training jobs ---------------------------------------------
  const jobs = ref<TrainJob[]>([])
  const activeJobs = computed(() =>
    jobs.value.filter((j) => j.status === 'queued' || j.status === 'running'),
  )

  async function refreshJobs(): Promise<void> {
    try {
      const r = await callProc(
        `CALL traverse.ml.trainJobs() YIELD jobId, status, modelName, pipeline, taskType, device,
           submittedAtUnix, elapsedSeconds, progress, error
         RETURN jobId, status, modelName, pipeline, taskType, device, submittedAtUnix,
           elapsedSeconds, progress, error`,
      )
      jobs.value = records(r).map((x) => {
        const p = (x.progress as Record<string, unknown>) ?? {}
        return {
          jobId: str(x.jobId),
          status: str(x.status) as TrainJob['status'],
          modelName: str(x.modelName),
          pipeline: str(x.pipeline),
          taskType: str(x.taskType),
          device: str(x.device),
          submittedAtUnix: num(x.submittedAtUnix),
          elapsedSeconds: num(x.elapsedSeconds),
          completedTrials: num(p.completedTrials),
          plannedTrials: num(p.plannedTrials),
          queuePosition: typeof p.queuePosition === 'number' ? p.queuePosition : null,
          error: str(x.error),
        }
      })
    } catch {
      jobs.value = []
    }
  }

  /** Asks the server to stop a job: a queued one is dropped, a running one stops at its next
   *  epoch or mini-batch. */
  async function cancelJob(jobId: string): Promise<void> {
    await callProc(
      `CALL traverse.ml.trainCancel($id) YIELD jobId, status, cancelRequested
       RETURN jobId, status, cancelRequested`,
      { id: jobId },
    )
    await refreshJobs()
  }

  // -- Train and predict -----------------------------------------
  const trainPhase = ref<TrainPhase>({ ...IDLE })
  const trainError = ref<string | null>(null)
  const isTraining = computed(() =>
    ['preparing', 'queued', 'training', 'predicting'].includes(trainPhase.value.step),
  )

  function autoName(target: string): { pipeline: string; model: string } {
    const ts = Math.floor(Date.now() / 1000)
    const safe = target.replace(/[^A-Za-z0-9_.-]/g, '_')
    return {
      pipeline: `studio-${safe}-${ts}`,
      model: `${safe}-${new Date().toISOString().slice(0, 10)}-${ts.toString(36).slice(-4)}`,
    }
  }

  /** The train config map for `opts`. */
  function trainConfig(opts: TrainOptions, pipeline: string, modelName: string) {
    const cfg: Record<string, unknown> = {
      pipeline,
      modelName,
      epochs: opts.epochs ?? 100,
      learningRate: opts.learningRate ?? 0.01,
    }
    if (opts.task === 'LinkPrediction') {
      cfg.targetRelationshipType = opts.target
      if (opts.sourceLabel) cfg.sourceNodeLabel = opts.sourceLabel
      if (opts.targetLabel) cfg.targetNodeLabel = opts.targetLabel
    } else {
      cfg.targetProperty = opts.target
    }
    if (opts.device) cfg.device = opts.device
    if (opts.batching === 'fullGraph') cfg.batching = 'fullGraph'
    if (opts.batching === 'miniBatch') {
      cfg.batching = 'miniBatch'
      if (opts.batchSize) cfg.batchSize = opts.batchSize
      if (opts.fanouts) cfg.fanouts = opts.fanouts
    }
    return cfg
  }

  /** Follows job `jobId` until it finishes; returns its final status row. */
  async function follow(jobId: string): Promise<Record<string, unknown>> {
    let wait = 400
    for (;;) {
      await new Promise((r) => setTimeout(r, wait))
      wait = Math.min(2000, Math.round(wait * 1.25))
      const r = records(
        await callProc(
          `CALL traverse.ml.trainStatus($id)
             YIELD jobId, status, elapsedSeconds, trainMillis, modelInfo, modelSelectionStats,
               error, progress
           RETURN jobId, status, elapsedSeconds, trainMillis, modelInfo, modelSelectionStats,
             error, progress`,
          { id: jobId },
        ),
      )[0]
      if (!r) throw new Error(`Training job ${jobId} disappeared`)
      const status = str(r.status)
      const p = (r.progress as Record<string, unknown>) ?? {}
      const queued = status === 'queued'
      trainPhase.value = {
        ...trainPhase.value,
        step: queued ? 'queued' : status === 'running' ? 'training' : trainPhase.value.step,
        seconds: num(r.elapsedSeconds),
        completedTrials: num(p.completedTrials),
        plannedTrials: num(p.plannedTrials),
        queuePosition: typeof p.queuePosition === 'number' ? p.queuePosition : null,
        message: queued
          ? `Waiting for ${num(p.queuePosition) + 1} job(s) ahead...`
          : 'Training... you can keep using Studio.',
      }
      if (status !== 'queued' && status !== 'running') return r
    }
  }

  /** Configures a pipeline, trains a model as a server job, then predicts with it. The pipeline
   *  is only the recipe for this run, so it's dropped afterwards; the model keeps everything it
   *  needs to predict. */
  async function trainAndPredict(opts: TrainOptions): Promise<{ modelName: string }> {
    if (!opts.target) throw new Error('Pick what to predict first')
    if (opts.features.length === 0) throw new Error('Pick at least one input property')
    if (opts.architectures.length === 0) throw new Error('Pick a model')
    const seg = TASK_SEGMENT[opts.task]
    const auto = autoName(opts.target)
    const modelName = opts.modelNameOverride?.trim() || auto.model
    const pipeline = auto.pipeline
    trainError.value = null
    trainPhase.value = { ...IDLE, step: 'preparing', message: 'Setting up...' }
    try {
      await callProc(
        `CALL traverse.ml.pipeline.${seg}.create($p) YIELD name RETURN name`,
        { p: pipeline },
      )
      await callProc(
        `CALL traverse.ml.pipeline.${seg}.selectFeatures($p, $props) YIELD name RETURN name`,
        { p: pipeline, props: opts.features },
      )
      for (const arch of opts.architectures) {
        const params: Record<string, unknown> = { hidden: opts.hidden ?? 32 }
        if (arch === 'Gat') params.heads = opts.heads ?? 4
        await callProc(
          `CALL traverse.ml.pipeline.${seg}.add${arch}($p, $cfg) YIELD name RETURN name`,
          { p: pipeline, cfg: params },
        )
      }
      const started = records(
        await callProc(
          `CALL traverse.ml.pipeline.${seg}.train($g, $cfg) YIELD jobId, status
           RETURN jobId, status`,
          { g: connection.currentDatabase, cfg: trainConfig(opts, pipeline, modelName) },
        ),
      )[0]
      const jobId = str(started?.jobId)
      if (!jobId) throw new Error('The server returned no training job')
      trainPhase.value = { ...trainPhase.value, step: 'queued', jobId, message: 'Queued...' }
      void refreshJobs()
      const done = await follow(jobId)
      const status = str(done.status)
      if (status === 'cancelled') throw new Error('Training was cancelled')
      if (status !== 'completed') throw new Error(str(done.error) || 'Training failed')
      const warning = str(done.error)
      trainPhase.value = { ...trainPhase.value, step: 'predicting', message: 'Predicting...' }
      await listModels()
      if (opts.task === 'NodeClassification') await predictStream(modelName)
      else if (opts.task === 'NodeRegression') await predictValues(modelName)
      else await suggestLinks(modelName)
      trainPhase.value = {
        ...trainPhase.value,
        step: 'done',
        message: warning || 'Done',
      }
      return { modelName }
    } catch (e) {
      trainError.value = (e as Error).message
      trainPhase.value = { ...trainPhase.value, step: 'failed', message: trainError.value }
      throw e
    } finally {
      // the recipe was for this run only
      callProc(`CALL traverse.ml.pipeline.drop($p) YIELD name RETURN name`, { p: pipeline }).catch(
        () => {},
      )
      void refreshJobs()
    }
  }

  async function cancelTraining(): Promise<void> {
    const id = trainPhase.value.jobId
    if (id) await cancelJob(id)
  }

  function resetTrainPhase(): void {
    trainPhase.value = { ...IDLE }
    trainError.value = null
  }

  /** Drops `key` from a per-model cache. */
  function forget<T>(cache: Ref<Record<string, T>>, key: string): void {
    if (!(key in cache.value)) return
    const next = { ...cache.value }
    delete next[key]
    cache.value = next
  }

  // -- Saved models ----------------------------------------------
  const models = ref<SavedModel[]>([])
  const isLoadingModels = ref(false)
  const modelsError = ref<string | null>(null)

  function parseClassLabels(raw: unknown): Record<string, string> {
    const out: Record<string, string> = {}
    if (typeof raw !== 'string' || !raw) return out
    try {
      const parsed = JSON.parse(raw)
      if (parsed && typeof parsed === 'object') {
        for (const [k, v] of Object.entries(parsed)) if (typeof v === 'string') out[k] = v
      }
    } catch {
      // not names: leave the fallback
    }
    return out
  }

  function parseMetrics(raw: unknown): ClassificationMetrics | null {
    if (typeof raw !== 'string' || !raw) return null
    try {
      const parsed = JSON.parse(raw) as ClassificationMetrics
      return parsed && Array.isArray(parsed.per_class) ? parsed : null
    } catch {
      return null
    }
  }

  async function listModels(): Promise<void> {
    isLoadingModels.value = true
    modelsError.value = null
    try {
      const r = await callProc(
        `CALL traverse.ml.model.list() YIELD modelName, taskType, targetProperty,
           targetRelationshipType, architecture, pipeline, accuracy, labelledNodes, trainedAtUnix,
           classLabels, metrics, testMetrics, trainSamples, testSamples, device, batching
         RETURN modelName, taskType, targetProperty, targetRelationshipType, architecture,
           pipeline, accuracy, labelledNodes, trainedAtUnix, classLabels, metrics, testMetrics,
           trainSamples, testSamples, device, batching`,
      )
      models.value = records(r).map((x) => ({
        modelName: str(x.modelName),
        taskType: (str(x.taskType) || 'NodeClassification') as TaskKind,
        targetProperty: str(x.targetProperty),
        targetRelationshipType: str(x.targetRelationshipType),
        architecture: str(x.architecture),
        pipeline: str(x.pipeline),
        accuracy: num(x.accuracy),
        labelledNodes: num(x.labelledNodes),
        trainedAtUnix: num(x.trainedAtUnix),
        classLabels: parseClassLabels(x.classLabels),
        metrics: parseMetrics(x.metrics),
        testMetrics: Object.fromEntries(
          Object.entries((x.testMetrics as Record<string, unknown>) ?? {}).map(([k, v]) => [
            k,
            num(v),
          ]),
        ),
        trainSamples: num(x.trainSamples),
        testSamples: num(x.testSamples),
        device: str(x.device),
        batching: (x.batching as BatchingInfo | null) ?? null,
      }))
    } catch (e) {
      modelsError.value = (e as Error).message
    } finally {
      isLoadingModels.value = false
    }
  }

  async function dropModel(name: string): Promise<void> {
    await callProc(
      `CALL traverse.ml.model.drop($n) YIELD modelName, dropped RETURN modelName, dropped`,
      { n: name },
    )
    forget(predictByModel, name)
    forget(lastPredictions, name)
    forget(valuesByModel, name)
    forget(linksByModel, name)
    forget(explanations, name)
    await listModels()
  }

  /** Downloads the model as a `.tvmodel` file (`GET /api/files/{name}?kind=model`), the same bytes
   *  an upload takes back. */
  async function exportModel(name: string): Promise<void> {
    await api.downloadModel(name)
  }

  /** Sets one entry of the model's metadata; Studio keeps class names there
   *  (`classLabels = '{"0":"legit","1":"fraud"}'`). */
  async function setModelMetadata(modelName: string, key: string, value: string): Promise<void> {
    await callProc(
      `CALL traverse.ml.model.setMetadata($n, $k, $v) YIELD modelName, key, value
       RETURN modelName, key, value`,
      { n: modelName, k: key, v: value },
    )
  }

  // -- Predictions -----------------------------------------------
  /** Classifier predictions: node id -> class, for the graph colouring. */
  const predictByModel = ref<Record<string, Map<number, number>>>({})
  /** Classifier predictions with probabilities. */
  const lastPredictions = ref<Record<string, PredictRow[]>>({})
  /** Regression predictions: node id -> value. */
  const valuesByModel = ref<Record<string, Map<number, number>>>({})
  /** Link-prediction suggestions, most likely first. */
  const linksByModel = ref<Record<string, LinkSuggestion[]>>({})
  const isPredicting = ref(false)
  const predictError = ref<string | null>(null)

  async function predicting<T>(work: () => Promise<T>): Promise<T> {
    isPredicting.value = true
    predictError.value = null
    try {
      return await work()
    } catch (e) {
      predictError.value = (e as Error).message
      throw e
    } finally {
      isPredicting.value = false
    }
  }

  async function predictStream(modelName: string): Promise<PredictRow[]> {
    return predicting(async () => {
      const r = await callProc(
        `CALL traverse.ml.pipeline.nodeClassification.predict.stream($g,
           {modelName: $model, includePredictedProbabilities: true})
           YIELD nodeId, predictedClass, predictedProbabilities
         RETURN nodeId, predictedClass, predictedProbabilities ORDER BY nodeId`,
        { g: connection.currentDatabase, model: modelName },
      )
      const out: PredictRow[] = records(r).map((x) => ({
        nodeId: num(x.nodeId),
        predictedClass: num(x.predictedClass),
        predictedProbabilities: ((x.predictedProbabilities as number[]) ?? []).slice(),
      }))
      predictByModel.value = {
        ...predictByModel.value,
        [modelName]: new Map(out.map((p) => [p.nodeId, p.predictedClass])),
      }
      lastPredictions.value = { ...lastPredictions.value, [modelName]: out }
      return out
    })
  }

  async function predictValues(modelName: string): Promise<Map<number, number>> {
    return predicting(async () => {
      const r = await callProc(
        `CALL traverse.ml.pipeline.nodeRegression.predict.stream($g, {modelName: $model})
           YIELD nodeId, predictedValue
         RETURN nodeId, predictedValue ORDER BY nodeId`,
        { g: connection.currentDatabase, model: modelName },
      )
      const values = new Map<number, number>(
        records(r).map((x) => [num(x.nodeId), num(x.predictedValue)]),
      )
      valuesByModel.value = { ...valuesByModel.value, [modelName]: values }
      return values
    })
  }

  async function suggestLinks(modelName: string, topN = 25): Promise<LinkSuggestion[]> {
    return predicting(async () => {
      const r = await callProc(
        `CALL traverse.ml.pipeline.linkPrediction.predict.stream($g, {modelName: $model, topN: $n})
           YIELD node1, node2, probability
         RETURN node1, node2, probability`,
        { g: connection.currentDatabase, model: modelName, n: topN },
      )
      const links = records(r).map((x) => ({
        node1: num(x.node1),
        node2: num(x.node2),
        probability: num(x.probability),
      }))
      linksByModel.value = { ...linksByModel.value, [modelName]: links }
      return links
    })
  }

  /** Writes a node model's predictions to its target property, keeping values already there
   *  unless `overwriteExisting`. */
  async function predictWrite(
    model: SavedModel,
    opts: { writeProperty?: string; overwriteExisting?: boolean } = {},
  ): Promise<{ nodesWritten: number; nodesSkipped: number; writeProperty: string }> {
    const cfg: Record<string, unknown> = { modelName: model.modelName }
    if (opts.writeProperty) cfg.writeProperty = opts.writeProperty
    if (opts.overwriteExisting !== undefined) cfg.overwriteExisting = opts.overwriteExisting
    const r = records(
      await callProc(
        `CALL traverse.ml.pipeline.${TASK_SEGMENT[model.taskType]}.predict.write($g, $cfg)
           YIELD nodesWritten, nodesSkipped, writeProperty
         RETURN nodesWritten, nodesSkipped, writeProperty`,
        { g: connection.currentDatabase, cfg },
      ),
    )[0] ?? {}
    return {
      nodesWritten: num(r.nodesWritten),
      nodesSkipped: num(r.nodesSkipped),
      writeProperty: str(r.writeProperty),
    }
  }

  /** Creates the `topN` most likely missing relationships as `relType`, with the probability as
   *  a property. */
  async function writeLinks(modelName: string, relType: string, topN: number): Promise<number> {
    const r = records(
      await callProc(
        `CALL traverse.ml.pipeline.linkPrediction.predict.write($g, $cfg)
           YIELD relationshipsWritten RETURN relationshipsWritten`,
        {
          g: connection.currentDatabase,
          cfg: { modelName, topN, writeRelationshipType: relType },
        },
      ),
    )[0]
    return num(r?.relationshipsWritten)
  }

  // -- Explanations ----------------------------------------------
  const explanations = ref<Record<string, Explanation>>({})

  function rels(raw: unknown, scoreKey: string | null): ExplainedRelationship[] {
    return ((raw as Record<string, unknown>[]) ?? []).map((m) => ({
      sourceNodeId: num(m.sourceNodeId),
      targetNodeId: num(m.targetNodeId),
      score: scoreKey && typeof m[scoreKey] === 'number' ? (m[scoreKey] as number) : null,
    }))
  }

  /** Why the classifier predicted what it did for `nodeId`, as relationships. */
  async function explain(
    model: SavedModel,
    nodeId: number,
    method: ExplainMethod,
  ): Promise<Explanation> {
    const g = connection.currentDatabase
    const name = (cls: number) => model.classLabels[String(cls)] || `class ${cls}`
    let explanation: Explanation
    if (method === 'subgraphAudit') {
      const x = records(
        await callProc(
          `CALL traverse.ml.explain.subgraphAudit($g, {modelName: $m, nodeId: $id,
             maxRelationships: 6})
             YIELD predictedClass, baselineProbability, explanationProbability, relationships
           RETURN predictedClass, baselineProbability, explanationProbability, relationships`,
          { g, m: model.modelName, id: nodeId },
        ),
      )[0] ?? {}
      const kept = rels(x.relationships, null)
      explanation = {
        method,
        nodeId,
        summary:
          `Predicted ${name(num(x.predictedClass))} (${Math.round(num(x.baselineProbability) * 100)}%). ` +
          `These ${kept.length} relationship(s) alone keep ` +
          `${Math.round(num(x.explanationProbability) * 100)}% of that confidence.`,
        relationships: kept,
      }
    } else if (method === 'counterfactual') {
      const x = records(
        await callProc(
          `CALL traverse.ml.explain.counterfactual($g, {modelName: $m, nodeId: $id})
             YIELD originalClass, counterfactualClass, flipped, removedRelationships
           RETURN originalClass, counterfactualClass, flipped, removedRelationships`,
          { g, m: model.modelName, id: nodeId },
        ),
      )[0] ?? {}
      const removed = rels(x.removedRelationships, 'maskValue')
      explanation = {
        method,
        nodeId,
        summary: x.flipped
          ? `Without these ${removed.length} relationship(s) the prediction changes from ` +
            `${name(num(x.originalClass))} to ${name(num(x.counterfactualClass))}.`
          : `No small set of relationships changes the prediction (${name(num(x.originalClass))}): ` +
            `it rests on the node's own properties or its wider neighborhood.`,
        relationships: removed,
      }
    } else {
      const rows = records(
        await callProc(
          `CALL traverse.ml.explain.pgExplainer($g, {modelName: $m, nodeIds: [$id], topK: 8})
             YIELD sourceNodeId, targetNodeId, importance
           RETURN sourceNodeId, targetNodeId, importance`,
          { g, m: model.modelName, id: nodeId },
        ),
      )
      explanation = {
        method,
        nodeId,
        summary: `The ${rows.length} relationship(s) that mattered most, by learned importance.`,
        relationships: rows.map((x) => ({
          sourceNodeId: num(x.sourceNodeId),
          targetNodeId: num(x.targetNodeId),
          score: num(x.importance),
        })),
      }
    }
    explanations.value = { ...explanations.value, [model.modelName]: explanation }
    return explanation
  }

  return {
    // targets
    targets,
    regressionTargets,
    isLoadingTargets,
    refreshTargetCandidates,
    // devices and jobs
    devices,
    refreshDevices,
    jobs,
    activeJobs,
    refreshJobs,
    cancelJob,
    // training
    trainPhase,
    trainError,
    isTraining,
    trainAndPredict,
    cancelTraining,
    resetTrainPhase,
    // models
    models,
    isLoadingModels,
    modelsError,
    listModels,
    dropModel,
    exportModel,
    setModelMetadata,
    // predictions
    predictByModel,
    lastPredictions,
    valuesByModel,
    linksByModel,
    isPredicting,
    predictError,
    predictStream,
    predictValues,
    suggestLinks,
    predictWrite,
    writeLinks,
    // explanations
    explanations,
    explain,
  }
})
