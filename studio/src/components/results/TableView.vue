<script setup lang="ts">
import { computed } from 'vue'
import { useQueryStore } from '../../stores/query'
import { formatValue } from '../../utils/format'

const query = useQueryStore()

function cellClass(value: unknown): string {
  if (value === null || value === undefined) return 'cell--null'
  if (typeof value === 'number') return 'cell--number'
  if (typeof value === 'boolean') return 'cell--boolean'
  return ''
}

// Detect if results should be flattened (all cells in a column are nodes/edges)
function isEntity(cell: unknown): cell is Record<string, unknown> {
  if (!cell || typeof cell !== 'object') return false
  const v = cell as Record<string, unknown>
  return v._type === 'node' || v._type === 'edge'
}

interface FlatColumn {
  key: string
  label: string
}

// Cap on rendered DOM rows. Larger results stay in memory (Copy/CSV export
// still see everything) but only the first N hydrate as <tr> nodes -
// otherwise 10 k rows x ~10 columns thrashes the browser.
const MAX_RENDERED_ROWS = 1000

const flattenedView = computed(() => {
  const { columns, rows } = query
  if (!rows.length) return null

  // Check which columns contain entities (nodes/edges)
  const entityCols = new Set<number>()
  for (let ci = 0; ci < columns.length; ci++) {
    const allEntities = rows.every((row) => isEntity(row[ci]))
    if (allEntities) entityCols.add(ci)
  }

  // If no entity columns, return null (use raw view)
  if (entityCols.size === 0) return null

  // Collect entity metadata (kind + property keys) once per entity column.
  // The previous implementation recomputed this inside the per-row loop,
  // turning a 10 k-row x N-column result into an O(rows^2 x cols) walk that
  // OOM'd the browser.
  const entityMeta = new Map<number, { isNode: boolean; isEdge: boolean; sortedKeys: string[] }>()
  for (const ci of entityCols) {
    let isNode = false
    let isEdge = false
    const propKeys = new Set<string>()
    for (const row of rows) {
      const v = row[ci] as Record<string, unknown>
      if (v._type === 'node') isNode = true
      else if (v._type === 'edge') isEdge = true
      const props = v.properties as Record<string, unknown> | undefined
      if (props) for (const k of Object.keys(props)) propKeys.add(k)
    }
    entityMeta.set(ci, { isNode, isEdge, sortedKeys: [...propKeys].sort() })
  }

  // Build flattened columns: for each entity column, expand into id + labels/type + properties
  const flatCols: FlatColumn[] = []

  for (let ci = 0; ci < columns.length; ci++) {
    const name = columns[ci]
    if (entityCols.has(ci)) {
      const meta = entityMeta.get(ci)!
      flatCols.push({ key: `${name}._id`, label: `id(${name})` })
      if (meta.isNode) flatCols.push({ key: `${name}._labels`, label: `${name}.labels` })
      if (meta.isEdge) flatCols.push({ key: `${name}._type`, label: `${name}.type` })
      for (const pk of meta.sortedKeys) {
        flatCols.push({ key: `${name}.${pk}`, label: `${name}.${pk}` })
      }
    } else {
      flatCols.push({ key: name, label: name })
    }
  }

  // Build rows (using pre-computed entityMeta - O(rows x cols) instead of O(rows^2 x cols))
  const renderRows = rows.length > MAX_RENDERED_ROWS ? rows.slice(0, MAX_RENDERED_ROWS) : rows
  const flatRows = renderRows.map((row) => {
    const cells: { display: string; cls: string }[] = []
    for (let ci = 0; ci < columns.length; ci++) {
      if (entityCols.has(ci)) {
        const v = row[ci] as Record<string, unknown>
        const props = (v.properties as Record<string, unknown>) || {}
        const meta = entityMeta.get(ci)!

        cells.push({ display: String(v.id), cls: 'cell--number' })
        if (meta.isNode) {
          cells.push({ display: ((v.labels as string[]) || []).join(':'), cls: 'cell--label' })
        }
        if (meta.isEdge) {
          cells.push({ display: String(v.type || ''), cls: 'cell--label' })
        }
        for (const pk of meta.sortedKeys) {
          const val = props[pk]
          cells.push({ display: formatValue(val), cls: cellClass(val) })
        }
      } else {
        cells.push({ display: formatValue(row[ci]), cls: cellClass(row[ci]) })
      }
    }
    return cells
  })

  return { columns: flatCols, rows: flatRows, hiddenRows: rows.length - flatRows.length }
})

// Fallback: raw columns/rows for non-entity results. Same row cap as the
// flattened view.
const rawRows = computed(() => {
  const src = query.rows
  const renderRows = src.length > MAX_RENDERED_ROWS ? src.slice(0, MAX_RENDERED_ROWS) : src
  return renderRows.map((row) =>
    row.map((cell) => ({ display: formatValue(cell), cls: cellClass(cell) }))
  )
})

const displayColumns = computed(() =>
  flattenedView.value ? flattenedView.value.columns.map((c) => c.label) : query.columns
)

const displayRows = computed(() =>
  flattenedView.value ? flattenedView.value.rows : rawRows.value
)

const hiddenRowCount = computed(() => {
  if (flattenedView.value) return flattenedView.value.hiddenRows
  return Math.max(0, query.rows.length - rawRows.value.length)
})

// Build a row of strings for Copy / Export - runs over ALL query rows
// (not just the visible MAX_RENDERED_ROWS slice).
function fullDisplayRow(row: unknown[]): string[] {
  if (!flattenedView.value) {
    return row.map((cell) => formatValue(cell))
  }
  // Mirror the flattened layout using the metadata we already computed.
  const { columns } = query
  const cells: string[] = []
  for (let ci = 0; ci < columns.length; ci++) {
    const v = row[ci]
    if (v && typeof v === 'object' && ((v as Record<string, unknown>)._type === 'node'
        || (v as Record<string, unknown>)._type === 'edge')) {
      const ent = v as Record<string, unknown>
      const props = (ent.properties as Record<string, unknown>) || {}
      cells.push(String(ent.id))
      if (ent._type === 'node') cells.push(((ent.labels as string[]) || []).join(':'))
      if (ent._type === 'edge') cells.push(String(ent.type || ''))
      // Re-derive sorted keys from the flat column list to stay in sync.
      const name = columns[ci]
      const prefix = `${name}.`
      const keyCols = flattenedView.value!.columns
        .filter((c) => c.key.startsWith(prefix) && !c.key.endsWith('._id') && c.key !== `${name}._labels` && c.key !== `${name}._type`)
        .map((c) => c.key.slice(prefix.length))
      for (const pk of keyCols) cells.push(formatValue(props[pk]))
    } else {
      cells.push(formatValue(v))
    }
  }
  return cells
}

function copyTable() {
  const header = displayColumns.value.join('\t')
  const body = query.rows.map((row) => fullDisplayRow(row).join('\t')).join('\n')
  navigator.clipboard.writeText(header + '\n' + body)
}

function exportCsv() {
  const escape = (s: string) => {
    if (s.includes(',') || s.includes('"') || s.includes('\n')) {
      return `"${s.replace(/"/g, '""')}"`
    }
    return s
  }
  const header = displayColumns.value.map(escape).join(',')
  const body = query.rows.map((row) => fullDisplayRow(row).map(escape).join(',')).join('\n')
  const blob = new Blob([header + '\n' + body], { type: 'text/csv' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = 'results.csv'
  a.click()
  URL.revokeObjectURL(url)
}
</script>

<template>
  <div class="table-view">
    <div class="table-view__actions">
      <button class="fg-btn fg-btn--sm" @click="copyTable">Copy</button>
      <button class="fg-btn fg-btn--sm" @click="exportCsv">Export CSV</button>
      <span v-if="hiddenRowCount > 0" class="table-view__notice">
        Showing first {{ displayRows.length.toLocaleString() }} rows
        ({{ hiddenRowCount.toLocaleString() }} more - use Copy / Export CSV for the full result).
      </span>
    </div>
    <div class="table-view__scroll">
      <table class="table-view__table">
        <thead>
          <tr>
            <th v-for="col in displayColumns" :key="col">{{ col }}</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(row, ri) in displayRows" :key="ri">
            <td v-for="(cell, ci) in row" :key="ci" :class="cell.cls">
              {{ cell.display }}
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </div>
</template>

<style scoped>
.table-view {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.table-view__actions {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 4px 8px;
  flex-shrink: 0;
}

.table-view__notice {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  margin-left: 4px;
}

.table-view__scroll {
  flex: 1;
  overflow: auto;
}

.table-view__table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--tv-font-size-sm);
  font-family: var(--tv-font-mono);
}

.table-view__table th {
  position: sticky;
  top: 0;
  background: var(--tv-bg-elevated);
  padding: 4px 10px;
  text-align: left;
  font-weight: 600;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  border-bottom: 1px solid var(--tv-border-default);
  white-space: nowrap;
  font-family: var(--tv-font-sans);
}

.table-view__table td {
  padding: 3px 10px;
  border-bottom: 1px solid var(--tv-border-default);
  max-width: 400px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--tv-text-primary);
}

.table-view__table tr:hover td {
  background: var(--tv-bg-hover);
}

.cell--null { color: var(--tv-text-muted); font-style: italic; }
.cell--number { color: var(--tv-node-1); }
.cell--boolean { color: var(--tv-node-5); }
.cell--label { color: var(--tv-node-2); font-weight: 500; }
</style>
