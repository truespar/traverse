<script setup lang="ts">
import { ref } from 'vue'
import { useConnectionStore } from '../../stores/connection'
import { useSchemaStore } from '../../stores/schema'
import { api } from '../../utils/api'
import type { ImportResult, CsvColumnMapping, CsvImportResult, CsvEdgeImportResult } from '../../utils/api'
import { useToast } from '../../composables/useToast'

const conn = useConnectionStore()
const schemaStore = useSchemaStore()
const { showToast } = useToast()

const WARN_SIZE = 50 * 1024 * 1024  // 50 MB
const MAX_SIZE = 512 * 1024 * 1024  // 512 MB

const importTab = ref<'cypher' | 'csv' | 'csv-edges'>('cypher')
const importText = ref('')
const importFile = ref<HTMLInputElement | null>(null)
const importing = ref(false)
const importResult = ref<ImportResult | null>(null)
const fileSizeWarning = ref('')

// CSV import state
const csvFile = ref<HTMLInputElement | null>(null)
const csvRaw = ref('')
const csvDelimiter = ref(',')
const csvLabel = ref('')
const csvPreviewHeaders = ref<string[]>([])
const csvPreviewRows = ref<string[][]>([])
const csvColumns = ref<(CsvColumnMapping & { include: boolean })[]>([])
const csvImportResult = ref<CsvImportResult | null>(null)

// CSV edge import state
const csvEdgeType = ref('')
const csvSourceLabel = ref('')
const csvSourceColumn = ref('')
const csvSourceProperty = ref('')
const csvTargetLabel = ref('')
const csvTargetColumn = ref('')
const csvTargetProperty = ref('')
const csvEdgeImportResult = ref<CsvEdgeImportResult | null>(null)

function formatSize(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

function checkFileSize(file: File): boolean {
  fileSizeWarning.value = ''
  if (file.size > MAX_SIZE) {
    showToast(`File too large (${formatSize(file.size)}). Maximum is 512 MB.`, 'error')
    return false
  }
  if (file.size > WARN_SIZE) {
    fileSizeWarning.value = `Large file (${formatSize(file.size)}) - import may take a while`
  }
  return true
}

function handleImportFile(event: Event) {
  const target = event.target as HTMLInputElement
  const file = target.files?.[0]
  if (!file) return
  if (!checkFileSize(file)) { target.value = ''; return }
  const reader = new FileReader()
  reader.onload = () => {
    importText.value = reader.result as string
  }
  reader.readAsText(file)
  target.value = ''
}

async function runImport() {
  const cypher = importText.value.trim()
  if (!cypher) return
  importing.value = true
  importResult.value = null
  try {
    const result = await api.importCypher(cypher, conn.currentDatabase)
    importResult.value = result
    conn.pollHealth()
    schemaStore.fetchSchema()
    if (result.errors.length === 0) {
      const parts: string[] = []
      if (result.stats.nodes_created) parts.push(`+${result.stats.nodes_created} nodes`)
      if (result.stats.relationships_created) parts.push(`+${result.stats.relationships_created} rels`)
      if (result.stats.properties_set) parts.push(`${result.stats.properties_set} props`)
      const summary = parts.length > 0 ? ` (${parts.join(', ')})` : ''
      showToast(`Imported ${result.executed} statements in ${result.time_ms.toFixed(0)} ms${summary}`, 'success')
      api.save().catch(() => showToast('Import succeeded but data may not be persisted to disk', 'error'))
    }
  } catch (e: any) {
    importResult.value = {
      total_statements: 0,
      executed: 0,
      errors: [{ statement: '', error: e.message || 'Import failed' }],
      stats: { nodes_created: 0, nodes_deleted: 0, relationships_created: 0, relationships_deleted: 0, properties_set: 0, labels_added: 0, labels_removed: 0 },
      time_ms: 0,
    }
  } finally {
    importing.value = false
  }
}

// CSV import functions

function parseCsvPreview(text: string, delimiter: string) {
  const delim = delimiter === '\\t' || delimiter === '\t' ? '\t' : delimiter
  const lines = text.split('\n').filter((l) => l.trim())
  if (lines.length === 0) {
    csvPreviewHeaders.value = []
    csvPreviewRows.value = []
    csvColumns.value = []
    return
  }

  const parseLine = (line: string): string[] => {
    const fields: string[] = []
    let current = ''
    let inQuotes = false
    for (let i = 0; i < line.length; i++) {
      const ch = line[i]
      if (inQuotes) {
        if (ch === '"' && line[i + 1] === '"') {
          current += '"'
          i++
        } else if (ch === '"') {
          inQuotes = false
        } else {
          current += ch
        }
      } else if (ch === '"') {
        inQuotes = true
      } else if (ch === delim) {
        fields.push(current)
        current = ''
      } else {
        current += ch
      }
    }
    fields.push(current)
    return fields
  }

  const headers = parseLine(lines[0])
  csvPreviewHeaders.value = headers

  const rows: string[][] = []
  for (let i = 1; i < Math.min(lines.length, 6); i++) {
    rows.push(parseLine(lines[i]))
  }
  csvPreviewRows.value = rows

  csvColumns.value = headers.map((h) => ({
    include: true,
    name: h,
    property: h,
    type: guessColumnType(rows, headers.indexOf(h)),
  }))
}

function guessColumnType(rows: string[][], colIdx: number): 'string' | 'integer' | 'float' | 'boolean' {
  const samples = rows.map((r) => r[colIdx]).filter((v) => v && v.trim())
  if (samples.length === 0) return 'string'
  const allInt = samples.every((v) => /^-?\d+$/.test(v.trim()))
  if (allInt) return 'integer'
  const allFloat = samples.every((v) => /^-?\d+\.?\d*$/.test(v.trim()))
  if (allFloat) return 'float'
  const allBool = samples.every((v) => /^(true|false|0|1|yes|no)$/i.test(v.trim()))
  if (allBool) return 'boolean'
  return 'string'
}

function handleCsvFile(event: Event) {
  const target = event.target as HTMLInputElement
  const file = target.files?.[0]
  if (!file) return
  if (!checkFileSize(file)) { target.value = ''; return }
  const reader = new FileReader()
  reader.onload = () => {
    csvRaw.value = reader.result as string
    parseCsvPreview(csvRaw.value, csvDelimiter.value)
    if (importTab.value === 'csv-edges') {
      autoFillEdgeImport(file.name)
    } else {
      if (!csvLabel.value) {
        const stem = file.name.replace(/\.[^.]+$/, '')
        csvLabel.value = stem
          .split(/[_\-\s]+/)
          .map((w) => w.charAt(0).toUpperCase() + w.slice(1).toLowerCase())
          .join('')
      }
    }
  }
  reader.readAsText(file)
  target.value = ''
}

function autoFillEdgeImport(filename: string) {
  const headers = csvPreviewHeaders.value.map((h) => h.toLowerCase())

  if (!csvSourceColumn.value) {
    const src = csvPreviewHeaders.value.find((_, i) => /^(source|src|from|source_id|from_id|src_id)$/i.test(headers[i]))
    if (src) csvSourceColumn.value = src
  }

  if (!csvTargetColumn.value) {
    const tgt = csvPreviewHeaders.value.find((_, i) => /^(target|tgt|to|target_id|to_id|tgt_id)$/i.test(headers[i]))
    if (tgt) csvTargetColumn.value = tgt
  }

  if (!csvEdgeType.value) {
    const typeColIdx = headers.findIndex((h) => /^(type|rel_type|relationship|edge_type)$/.test(h))
    if (typeColIdx >= 0 && csvPreviewRows.value.length > 0 && csvPreviewRows.value[0][typeColIdx]) {
      csvEdgeType.value = csvPreviewRows.value[0][typeColIdx].trim()
    } else {
      csvEdgeType.value = 'RELATED_TO'
    }
  }

  if (!csvSourceProperty.value) csvSourceProperty.value = 'id'
  if (!csvTargetProperty.value) csvTargetProperty.value = 'id'

  if (!csvSourceLabel.value || !csvTargetLabel.value) {
    const stem = filename.replace(/\.[^.]+$/, '').replace(/_?edges?/i, '')
    const label = stem
      .split(/[_\-\s]+/)
      .map((w) => w.charAt(0).toUpperCase() + w.slice(1).toLowerCase())
      .join('')
    if (!csvSourceLabel.value) csvSourceLabel.value = label
    if (!csvTargetLabel.value) csvTargetLabel.value = label
  }
}

function onDelimiterChange() {
  if (csvRaw.value) {
    parseCsvPreview(csvRaw.value, csvDelimiter.value)
  }
}

async function runCsvImport() {
  if (!csvRaw.value.trim() || !csvLabel.value.trim()) return
  importing.value = true
  csvImportResult.value = null
  try {
    const cols = csvColumns.value
      .filter((c) => c.include)
      .map(({ name, property, type }) => ({ name, property, type }))
    const result = await api.importCsv(
      csvRaw.value,
      csvLabel.value.trim(),
      cols,
      csvDelimiter.value,
      conn.currentDatabase,
    )
    csvImportResult.value = result
    conn.pollHealth()
    schemaStore.fetchSchema()
    if (result.errors.length === 0) {
      showToast(`Created ${result.nodes_created} nodes (${result.properties_set} properties) in ${result.time_ms.toFixed(0)} ms`, 'success')
      api.save().catch(() => showToast('Import succeeded but data may not be persisted to disk', 'error'))
    }
  } catch (e: any) {
    csvImportResult.value = {
      nodes_created: 0,
      properties_set: 0,
      labels_added: 0,
      errors: [{ error: e.message || 'CSV import failed' }],
      time_ms: 0,
    }
  } finally {
    importing.value = false
  }
}

async function runCsvEdgeImport() {
  if (!csvRaw.value.trim() || !csvEdgeType.value.trim() || !csvSourceLabel.value.trim() || !csvSourceColumn.value.trim() || !csvSourceProperty.value.trim() || !csvTargetLabel.value.trim() || !csvTargetColumn.value.trim() || !csvTargetProperty.value.trim()) return
  importing.value = true
  csvEdgeImportResult.value = null
  try {
    const cols = csvColumns.value
      .filter((c) => c.include && c.name !== csvSourceColumn.value && c.name !== csvTargetColumn.value)
      .map(({ name, property, type }) => ({ name, property, type }))
    const result = await api.importCsvEdges(
      csvRaw.value,
      csvEdgeType.value.trim(),
      { label: csvSourceLabel.value.trim(), column: csvSourceColumn.value, property: csvSourceProperty.value.trim() },
      { label: csvTargetLabel.value.trim(), column: csvTargetColumn.value, property: csvTargetProperty.value.trim() },
      cols,
      csvDelimiter.value,
      conn.currentDatabase,
    )
    csvEdgeImportResult.value = result
    conn.pollHealth()
    schemaStore.fetchSchema()
    if (result.errors.length === 0) {
      const parts: string[] = []
      parts.push(`+${result.edges_created} edges`)
      if (result.properties_set) parts.push(`${result.properties_set} props`)
      showToast(`Created ${result.edges_created} edges in ${result.time_ms.toFixed(0)} ms (${parts.join(', ')})`, 'success')
      api.save().catch(() => showToast('Import succeeded but data may not be persisted to disk', 'error'))
    }
  } catch (e: any) {
    csvEdgeImportResult.value = {
      edges_created: 0,
      properties_set: 0,
      errors: [{ error: e.message || 'CSV edge import failed' }],
      time_ms: 0,
    }
  } finally {
    importing.value = false
  }
}

</script>

<template>
  <div class="import-panel">
    <!-- Panel header -->
    <div class="import-panel__header">
      <span class="import-panel__title">Import</span>
    </div>

    <!-- Tab bar -->
    <div class="fg-tabs">
      <button class="fg-tab" :class="{ 'fg-tab--active': importTab === 'cypher' }" @click="importTab = 'cypher'">Cypher</button>
      <button class="fg-tab" :class="{ 'fg-tab--active': importTab === 'csv' }" @click="importTab = 'csv'">CSV</button>
      <button class="fg-tab" :class="{ 'fg-tab--active': importTab === 'csv-edges' }" @click="importTab = 'csv-edges'">CSV Edges</button>
    </div>

    <div class="import-panel__body">
      <div class="import-panel__db-label">
        Target: <strong>{{ conn.currentDatabase }}</strong>
      </div>

      <!-- File size warning -->
      <div v-if="fileSizeWarning" class="import-panel__size-warning">
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z" /><path d="M12 9v4" /><path d="M12 17h.01" />
        </svg>
        {{ fileSizeWarning }}
      </div>

      <!-- Cypher tab -->
      <template v-if="importTab === 'cypher'">
        <p class="import-panel__hint">
          Paste Cypher statements or load a .cypher file. Statements are separated by semicolons.
        </p>

        <div class="import-panel__actions-row">
          <button class="fg-btn fg-btn--sm" @click="importFile?.click()">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" />
              <path d="M14 2v4a2 2 0 0 0 2 2h4" />
            </svg>
            Load File
          </button>
          <input
            ref="importFile"
            type="file"
            accept=".cypher,.cql,.txt"
            style="display:none"
            @change="handleImportFile"
          />
        </div>

        <textarea
          v-model="importText"
          class="import-panel__textarea"
          placeholder="CREATE (a:Person {name: 'Alice'});&#10;CREATE (b:Person {name: 'Bob'});&#10;CREATE (a)-[:KNOWS]->(b);"
          spellcheck="false"
        />

        <button
          class="fg-btn fg-btn--primary"
          :disabled="!importText.trim() || importing"
          @click="runImport"
        >
          <span v-if="importing" class="fg-spinner" style="width:12px;height:12px;border-width:1.5px" />
          {{ importing ? 'Importing...' : 'Import' }}
        </button>

        <!-- Result summary -->
        <div v-if="importResult" class="import-panel__result">
          <div v-if="importResult.executed > 0" class="import-panel__result-success">
            Executed {{ importResult.executed }}/{{ importResult.total_statements }} statements
            in {{ importResult.time_ms.toFixed(0) }} ms
          </div>
          <div v-if="importResult.stats && Object.values(importResult.stats).some(v => v > 0)" class="import-panel__result-stats">
            <span v-if="importResult.stats.nodes_created">+{{ importResult.stats.nodes_created }} nodes</span>
            <span v-if="importResult.stats.relationships_created">+{{ importResult.stats.relationships_created }} rels</span>
            <span v-if="importResult.stats.properties_set">{{ importResult.stats.properties_set }} props</span>
            <span v-if="importResult.stats.nodes_deleted">-{{ importResult.stats.nodes_deleted }} nodes</span>
            <span v-if="importResult.stats.relationships_deleted">-{{ importResult.stats.relationships_deleted }} rels</span>
          </div>
          <div v-if="importResult.errors.length > 0" class="import-panel__result-errors">
            <div v-for="(err, i) in importResult.errors.slice(0, 5)" :key="i" class="import-panel__error-item">
              <span class="import-panel__error-stmt" v-if="err.statement">{{ err.statement.slice(0, 80) }}</span>
              <span class="import-panel__error-msg">{{ err.error }}</span>
            </div>
            <div v-if="importResult.errors.length > 5" class="import-panel__error-more">
              ...and {{ importResult.errors.length - 5 }} more errors
            </div>
          </div>
        </div>
      </template>

      <!-- CSV tab -->
      <template v-if="importTab === 'csv'">
        <div class="import-panel__actions-row">
          <button class="fg-btn fg-btn--sm" @click="csvFile?.click()">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" />
              <path d="M14 2v4a2 2 0 0 0 2 2h4" />
            </svg>
            Load CSV
          </button>
          <input
            ref="csvFile"
            type="file"
            accept=".csv,.tsv,.txt"
            style="display:none"
            @change="handleCsvFile"
          />
          <label class="import-panel__field">
            <span class="import-panel__field-label">Delimiter</span>
            <select v-model="csvDelimiter" class="fg-select fg-select--sm" @change="onDelimiterChange">
              <option value=",">, (comma)</option>
              <option value="&#9;">Tab</option>
              <option value=";">; (semicolon)</option>
              <option value="|">| (pipe)</option>
            </select>
          </label>
        </div>

        <!-- Preview table -->
        <div v-if="csvPreviewHeaders.length > 0" class="import-panel__preview-wrap">
          <div class="import-panel__preview-label">Preview (first {{ csvPreviewRows.length }} rows)</div>
          <div class="import-panel__preview-scroll">
            <table class="import-panel__preview">
              <thead>
                <tr><th v-for="h in csvPreviewHeaders" :key="h">{{ h }}</th></tr>
              </thead>
              <tbody>
                <tr v-for="(row, ri) in csvPreviewRows" :key="ri">
                  <td v-for="(cell, ci) in row" :key="ci">{{ cell }}</td>
                </tr>
              </tbody>
            </table>
          </div>
        </div>

        <div v-if="csvPreviewHeaders.length === 0 && !csvRaw" class="import-panel__hint">
          Select a CSV file to preview and configure the import mapping.
        </div>

        <!-- Mapping form -->
        <div v-if="csvColumns.length > 0" class="import-panel__mapping">
          <label class="import-panel__field">
            <span class="import-panel__field-label">Node label</span>
            <input v-model="csvLabel" class="fg-input fg-input--sm" placeholder="e.g. Person" />
          </label>

          <div class="import-panel__mapping-label">Column mapping</div>
          <table class="import-panel__mapping-table">
            <thead>
              <tr>
                <th style="width:32px"></th>
                <th>CSV Column</th>
                <th>Property</th>
                <th>Type</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(col, i) in csvColumns" :key="i">
                <td><input type="checkbox" v-model="col.include" /></td>
                <td class="import-panel__col-name">{{ col.name }}</td>
                <td><input v-model="col.property" class="fg-input fg-input--sm" /></td>
                <td>
                  <select v-model="col.type" class="fg-select fg-select--sm">
                    <option value="string">string</option>
                    <option value="integer">integer</option>
                    <option value="float">float</option>
                    <option value="boolean">boolean</option>
                  </select>
                </td>
              </tr>
            </tbody>
          </table>

          <button
            class="fg-btn fg-btn--primary"
            :disabled="!csvRaw.trim() || !csvLabel.trim() || importing"
            @click="runCsvImport"
          >
            <span v-if="importing" class="fg-spinner" style="width:12px;height:12px;border-width:1.5px" />
            {{ importing ? 'Importing...' : 'Import CSV' }}
          </button>
        </div>

        <!-- CSV Result summary -->
        <div v-if="csvImportResult" class="import-panel__result">
          <div v-if="csvImportResult.nodes_created > 0" class="import-panel__result-success">
            Created {{ csvImportResult.nodes_created }} nodes
            ({{ csvImportResult.properties_set }} properties)
            in {{ csvImportResult.time_ms.toFixed(0) }} ms
          </div>
          <div v-if="csvImportResult.errors.length > 0" class="import-panel__result-errors">
            <div v-for="(err, i) in csvImportResult.errors.slice(0, 5)" :key="i" class="import-panel__error-item">
              <span class="import-panel__error-stmt" v-if="err.row">Row {{ err.row }}</span>
              <span class="import-panel__error-msg">{{ err.error }}</span>
            </div>
            <div v-if="csvImportResult.errors.length > 5" class="import-panel__error-more">
              ...and {{ csvImportResult.errors.length - 5 }} more errors
            </div>
          </div>
        </div>
      </template>

      <!-- CSV Edges tab -->
      <template v-if="importTab === 'csv-edges'">
        <div class="import-panel__actions-row">
          <button class="fg-btn fg-btn--sm" @click="csvFile?.click()">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" />
              <path d="M14 2v4a2 2 0 0 0 2 2h4" />
            </svg>
            Load CSV
          </button>
          <input
            ref="csvFile"
            type="file"
            accept=".csv,.tsv,.txt"
            style="display:none"
            @change="handleCsvFile"
          />
          <label class="import-panel__field">
            <span class="import-panel__field-label">Delimiter</span>
            <select v-model="csvDelimiter" class="fg-select fg-select--sm" @change="onDelimiterChange">
              <option value=",">, (comma)</option>
              <option value="&#9;">Tab</option>
              <option value=";">; (semicolon)</option>
              <option value="|">| (pipe)</option>
            </select>
          </label>
        </div>

        <!-- Preview table -->
        <div v-if="csvPreviewHeaders.length > 0" class="import-panel__preview-wrap">
          <div class="import-panel__preview-label">Preview (first {{ csvPreviewRows.length }} rows)</div>
          <div class="import-panel__preview-scroll">
            <table class="import-panel__preview">
              <thead>
                <tr><th v-for="h in csvPreviewHeaders" :key="h">{{ h }}</th></tr>
              </thead>
              <tbody>
                <tr v-for="(row, ri) in csvPreviewRows" :key="ri">
                  <td v-for="(cell, ci) in row" :key="ci">{{ cell }}</td>
                </tr>
              </tbody>
            </table>
          </div>
        </div>

        <div v-if="csvPreviewHeaders.length === 0 && !csvRaw" class="import-panel__hint">
          Select a CSV file with edge data (source key, target key, and optional properties).
        </div>

        <!-- Edge config form -->
        <div v-if="csvColumns.length > 0" class="import-panel__mapping">
          <label class="import-panel__field">
            <span class="import-panel__field-label">Edge type</span>
            <input v-model="csvEdgeType" class="fg-input fg-input--sm" placeholder="e.g. KNOWS" />
          </label>

          <fieldset class="import-panel__endpoint">
            <legend>Source node</legend>
            <label class="import-panel__field">
              <span class="import-panel__field-label">Label</span>
              <input v-model="csvSourceLabel" class="fg-input fg-input--sm" placeholder="e.g. Person" />
            </label>
            <label class="import-panel__field">
              <span class="import-panel__field-label">CSV column</span>
              <select v-model="csvSourceColumn" class="fg-select fg-select--sm">
                <option value="" disabled>Select column...</option>
                <option v-for="h in csvPreviewHeaders" :key="h" :value="h">{{ h }}</option>
              </select>
            </label>
            <label class="import-panel__field">
              <span class="import-panel__field-label">Match property</span>
              <input v-model="csvSourceProperty" class="fg-input fg-input--sm" placeholder="e.g. name" />
            </label>
          </fieldset>

          <fieldset class="import-panel__endpoint">
            <legend>Target node</legend>
            <label class="import-panel__field">
              <span class="import-panel__field-label">Label</span>
              <input v-model="csvTargetLabel" class="fg-input fg-input--sm" placeholder="e.g. Person" />
            </label>
            <label class="import-panel__field">
              <span class="import-panel__field-label">CSV column</span>
              <select v-model="csvTargetColumn" class="fg-select fg-select--sm">
                <option value="" disabled>Select column...</option>
                <option v-for="h in csvPreviewHeaders" :key="h" :value="h">{{ h }}</option>
              </select>
            </label>
            <label class="import-panel__field">
              <span class="import-panel__field-label">Match property</span>
              <input v-model="csvTargetProperty" class="fg-input fg-input--sm" placeholder="e.g. name" />
            </label>
          </fieldset>

          <!-- Edge property columns -->
          <div v-if="csvColumns.filter(c => c.name !== csvSourceColumn && c.name !== csvTargetColumn).length > 0">
            <div class="import-panel__mapping-label">Edge property columns</div>
            <table class="import-panel__mapping-table">
              <thead>
                <tr>
                  <th style="width:32px"></th>
                  <th>CSV Column</th>
                  <th>Property</th>
                  <th>Type</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="(col, i) in csvColumns" :key="i" v-show="col.name !== csvSourceColumn && col.name !== csvTargetColumn">
                  <td><input type="checkbox" v-model="col.include" /></td>
                  <td class="import-panel__col-name">{{ col.name }}</td>
                  <td><input v-model="col.property" class="fg-input fg-input--sm" /></td>
                  <td>
                    <select v-model="col.type" class="fg-select fg-select--sm">
                      <option value="string">string</option>
                      <option value="integer">integer</option>
                      <option value="float">float</option>
                      <option value="boolean">boolean</option>
                    </select>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>

          <button
            class="fg-btn fg-btn--primary"
            :disabled="!csvRaw.trim() || !csvEdgeType.trim() || !csvSourceColumn || !csvSourceLabel.trim() || !csvSourceProperty.trim() || !csvTargetColumn || !csvTargetLabel.trim() || !csvTargetProperty.trim() || importing"
            @click="runCsvEdgeImport"
          >
            <span v-if="importing" class="fg-spinner" style="width:12px;height:12px;border-width:1.5px" />
            {{ importing ? 'Importing...' : 'Import Edges' }}
          </button>
        </div>

        <!-- CSV Edge Result summary -->
        <div v-if="csvEdgeImportResult" class="import-panel__result">
          <div v-if="csvEdgeImportResult.edges_created > 0" class="import-panel__result-success">
            Created {{ csvEdgeImportResult.edges_created }} edges
            ({{ csvEdgeImportResult.properties_set }} properties)
            in {{ csvEdgeImportResult.time_ms.toFixed(0) }} ms
          </div>
          <div v-if="csvEdgeImportResult.errors.length > 0" class="import-panel__result-errors">
            <div v-for="(err, i) in csvEdgeImportResult.errors.slice(0, 5)" :key="i" class="import-panel__error-item">
              <span class="import-panel__error-stmt" v-if="err.row">Row {{ err.row }}</span>
              <span class="import-panel__error-msg">{{ err.error }}</span>
            </div>
            <div v-if="csvEdgeImportResult.errors.length > 5" class="import-panel__error-more">
              ...and {{ csvEdgeImportResult.errors.length - 5 }} more errors
            </div>
          </div>
        </div>
      </template>

    </div>
  </div>
</template>

<style scoped>
.import-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.import-panel__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 12px;
  border-bottom: 1px solid var(--tv-border-default);
  height: 38px;
  flex-shrink: 0;
}

.import-panel__title {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}

.import-panel__body {
  flex: 1;
  overflow-y: auto;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.import-panel__db-label {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
}

.import-panel__hint {
  margin: 0;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
}

.import-panel__size-warning {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 10px;
  border-radius: var(--tv-radius-sm);
  background: color-mix(in srgb, var(--tv-status-warning) 12%, var(--tv-bg-elevated));
  border: 1px solid color-mix(in srgb, var(--tv-status-warning) 30%, transparent);
  color: var(--tv-status-warning);
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
}

.import-panel__actions-row {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.import-panel__textarea {
  width: 100%;
  min-height: 140px;
  padding: 8px;
  font-family: var(--tv-font-mono);
  font-size: var(--tv-font-size-xs);
  line-height: 1.5;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-base);
  color: var(--tv-text-primary);
  resize: vertical;
  outline: none;
}
.import-panel__textarea:focus {
  border-color: var(--tv-border-focus);
  box-shadow: 0 0 0 3px var(--tv-accent-subtle);
}
.import-panel__textarea::placeholder {
  color: var(--tv-text-muted);
}

.import-panel__result {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px;
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-elevated);
  font-size: var(--tv-font-size-xs);
}

.import-panel__result-success {
  color: var(--tv-status-success);
  font-weight: 500;
}

.import-panel__result-stats {
  display: flex;
  gap: 8px;
  color: var(--tv-text-secondary);
  font-size: var(--tv-font-size-xs);
}

.import-panel__result-errors {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.import-panel__error-item {
  display: flex;
  flex-direction: column;
  padding: 3px 6px;
  border-left: 2px solid var(--tv-status-error);
  font-size: var(--tv-font-size-xs);
}

.import-panel__error-stmt {
  font-family: var(--tv-font-mono);
  color: var(--tv-text-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.import-panel__error-msg {
  color: var(--tv-status-error);
}

.import-panel__error-more {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  padding-left: 8px;
}

/* Field / mapping styles */
.import-panel__field {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: var(--tv-font-size-xs);
}

.import-panel__field-label {
  color: var(--tv-text-muted);
  font-size: var(--tv-font-size-xs);
  white-space: nowrap;
}

.import-panel__preview-wrap {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.import-panel__preview-label {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
}

.import-panel__preview-scroll {
  overflow-x: auto;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
}

.import-panel__preview {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--tv-font-size-xs);
  font-family: var(--tv-font-mono);
}
.import-panel__preview th,
.import-panel__preview td {
  padding: 3px 6px;
  text-align: left;
  white-space: nowrap;
  border-bottom: 1px solid var(--tv-border-default);
  max-width: 120px;
  overflow: hidden;
  text-overflow: ellipsis;
}
.import-panel__preview th {
  background: var(--tv-bg-elevated);
  font-weight: 600;
  color: var(--tv-text-primary);
}
.import-panel__preview td {
  color: var(--tv-text-secondary);
}
.import-panel__preview tbody tr:last-child td {
  border-bottom: none;
}

.import-panel__mapping {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.import-panel__mapping-label {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  font-weight: 500;
}

.import-panel__mapping-table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--tv-font-size-xs);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
}
.import-panel__mapping-table th,
.import-panel__mapping-table td {
  padding: 3px 6px;
  text-align: left;
  border-bottom: 1px solid var(--tv-border-default);
}
.import-panel__mapping-table th {
  background: var(--tv-bg-elevated);
  font-weight: 500;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
}
.import-panel__mapping-table tbody tr:last-child td {
  border-bottom: none;
}

.import-panel__col-name {
  font-family: var(--tv-font-mono);
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
}

.import-panel__endpoint {
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  padding: 8px 10px;
  margin: 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.import-panel__endpoint legend {
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
  color: var(--tv-text-muted);
  padding: 0 4px;
}

</style>
