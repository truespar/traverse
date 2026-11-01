<script setup lang="ts">
import { ref, computed } from 'vue'
import { useSchemaStore } from '../../stores/schema'
import { useStyleStore } from '../../stores/style'
import { useConnectionStore } from '../../stores/connection'
import { useSettingsStore } from '../../stores/settings'
import { api, type StyleRule, type StyleRuleOp } from '../../utils/api'
import { colorForLabel } from '../../composables/useGraphExplorer'
import ColorPicker from '../ui/ColorPicker.vue'
import FgSelect from '../common/FgSelect.vue'

const schema = useSchemaStore()
const styleStore = useStyleStore()
const conn = useConnectionStore()
const settings = useSettingsStore()
const refreshing = ref(false)
async function handleRefresh() {
  refreshing.value = true
  const done = schema.fetchSchema()
  const min = new Promise(r => setTimeout(r, 500))
  await Promise.all([done, min])
  refreshing.value = false
}

const showDropModal = ref(false)
const showDisabledModal = ref(false)
const dropConfirmInput = ref('')
const dropError = ref('')
const dropping = ref(false)

const showRenameModal = ref(false)
const renameValue = ref('')
const renameError = ref('')
const renaming = ref(false)

function openRenameModal() {
  renameValue.value = conn.currentDatabase
  renameError.value = ''
  showRenameModal.value = true
}

async function submitRename() {
  const oldName = conn.currentDatabase
  const newName = renameValue.value.trim()
  if (!newName || newName === oldName) {
    showRenameModal.value = false
    return
  }
  renaming.value = true
  renameError.value = ''
  try {
    await api.renameDatabase(oldName, newName)
    showRenameModal.value = false
    conn.currentDatabase = newName
    conn.pollHealth()
  } catch (e: any) {
    renameError.value = e.message || 'Failed to rename database'
  } finally {
    renaming.value = false
  }
}

function openDropModal() {
  dropConfirmInput.value = ''
  dropError.value = ''
  showDropModal.value = true
}

async function dropCurrentDatabase() {
  if (dropConfirmInput.value !== 'delete') return
  dropping.value = true
  dropError.value = ''
  const name = conn.currentDatabase
  try {
    await api.dropDatabase(name)
    showDropModal.value = false
    // Navigate to another database
    const next = conn.databases.find(d => d.default && d.name !== name)
      || conn.databases.find(d => d.name !== name)
    if (next) {
      conn.currentDatabase = next.name
    }
    conn.pollHealth()
  } catch (e: any) {
    dropError.value = e.message || 'Failed to delete database'
  } finally {
    dropping.value = false
  }
}

const expandedSections = ref<Set<string>>(new Set(['labels', 'types', 'indexes']))
const editingNode = ref<string | null>(null)
const editingEdge = ref<string | null>(null)
const expandedLabels = ref<Set<string>>(new Set())
const expandedEdgeTypes = ref<Set<string>>(new Set())

function toggleLabel(label: string) {
  if (expandedLabels.value.has(label)) {
    expandedLabels.value.delete(label)
  } else {
    expandedLabels.value.add(label)
  }
}

function toggleEdgeType(type: string) {
  if (expandedEdgeTypes.value.has(type)) {
    expandedEdgeTypes.value.delete(type)
  } else {
    expandedEdgeTypes.value.add(type)
  }
}

function toggleSection(section: string) {
  if (expandedSections.value.has(section)) {
    expandedSections.value.delete(section)
  } else {
    expandedSections.value.add(section)
  }
}

function insertInEditor(text: string) {
  window.dispatchEvent(new CustomEvent('fg-insert-text', { detail: text }))
}

function toggleNodeEditor(label: string, event: Event) {
  event.stopPropagation()
  editingNode.value = editingNode.value === label ? null : label
}

function toggleEdgeEditor(type: string, event: Event) {
  event.stopPropagation()
  editingEdge.value = editingEdge.value === type ? null : type
}

function setNodeColor(label: string, color: string) {
  styleStore.setNodeStyle(label, { color })
}

function onNodeSizeChange(label: string, event: Event) {
  const size = parseFloat((event.target as HTMLInputElement).value)
  styleStore.setNodeStyle(label, { size })
}

function resetNodeStyle(label: string) {
  styleStore.removeNodeStyle(label)
  editingNode.value = null
}

function setEdgeColor(type: string, color: string) {
  styleStore.setEdgeStyle(type, { color })
}

function onEdgeThicknessChange(type: string, event: Event) {
  const thickness = parseFloat((event.target as HTMLInputElement).value)
  styleStore.setEdgeStyle(type, { thickness })
}

function resetEdgeStyle(type: string) {
  styleStore.removeEdgeStyle(type)
  editingEdge.value = null
}

function getNodeDotColor(label: string): string {
  return styleStore.nodeColor(label) || colorForLabel(label, settings.theme === 'dark')
}

function getNodeSize(label: string): number {
  return styleStore.nodeSize(label) ?? 6
}

function getNodeProgram(label: string): string {
  return styleStore.nodeProgram(label) ?? 'circle'
}

function getEdgeThickness(type: string): number {
  return styleStore.edgeThickness(type) ?? 1.5
}

function onNodeHexInput(label: string, event: Event) {
  const v = (event.target as HTMLInputElement).value.trim()
  if (/^#[0-9a-fA-F]{6}$/.test(v)) {
    setNodeColor(label, v)
  }
}

function onEdgeHexInput(type: string, event: Event) {
  const v = (event.target as HTMLInputElement).value.trim()
  if (/^#[0-9a-fA-F]{6}$/.test(v)) {
    setEdgeColor(type, v)
  }
}

// --- Conditional style rules ---

const ALL_OPS: { value: StyleRuleOp; label: string }[] = [
  { value: '=', label: '=' },
  { value: '!=', label: '!=' },
  { value: '>', label: '>' },
  { value: '<', label: '<' },
  { value: '>=', label: '>=' },
  { value: '<=', label: '<=' },
  { value: 'contains', label: 'contains' },
  { value: 'starts_with', label: 'starts with' },
  { value: 'exists', label: 'exists' },
  { value: 'not_exists', label: 'not exists' },
]

const NUMERIC_OPS = new Set<StyleRuleOp>(['=', '!=', '>', '<', '>=', '<=', 'exists', 'not_exists'])
const STRING_OPS = new Set<StyleRuleOp>(['=', '!=', 'contains', 'starts_with', 'exists', 'not_exists'])
const NO_VALUE_OPS = new Set<StyleRuleOp>(['exists', 'not_exists'])

function opsForType(propType: string): { value: string; label: string }[] {
  const lower = propType.toLowerCase()
  if (lower === 'integer' || lower === 'float' || lower === 'long' || lower === 'double' || lower === 'number') {
    return ALL_OPS.filter(o => NUMERIC_OPS.has(o.value))
  }
  if (lower === 'boolean') {
    return ALL_OPS.filter(o => o.value === '=' || o.value === '!=' || NO_VALUE_OPS.has(o.value))
  }
  // String and fallback - all ops
  return ALL_OPS
}

function propsForLabel(label: string): { value: string; label: string }[] {
  const ld = schema.labelsDetail.find(l => l.name === label)
  return ld?.properties.map(p => ({ value: p.name, label: p.name })) || []
}

function propsForEdgeType(type: string): { value: string; label: string }[] {
  const rd = schema.relationshipTypesDetail.find(r => r.name === type)
  return rd?.properties.map(p => ({ value: p.name, label: p.name })) || []
}

function propTypeForLabel(label: string, propName: string): string {
  const ld = schema.labelsDetail.find(l => l.name === label)
  return ld?.properties.find(p => p.name === propName)?.type || 'String'
}

function propTypeForEdgeType(type: string, propName: string): string {
  const rd = schema.relationshipTypesDetail.find(r => r.name === type)
  return rd?.properties.find(p => p.name === propName)?.type || 'String'
}

function nodeRules(label: string): StyleRule[] {
  return styleStore.nodeStyles[label]?.rules || []
}

function edgeRules(type: string): StyleRule[] {
  return styleStore.edgeStyles[type]?.rules || []
}

function addNewNodeRule(label: string) {
  const props = propsForLabel(label)
  const prop = props[0]?.value || ''
  styleStore.addNodeRule(label, { property: prop, op: '=', value: '', style: {} })
}

function addNewEdgeRule(type: string) {
  const props = propsForEdgeType(type)
  const prop = props[0]?.value || ''
  styleStore.addEdgeRule(type, { property: prop, op: '=', value: '', style: {} })
}

function updateRuleProperty(kind: 'node' | 'edge', key: string, index: number, property: string) {
  const rules = kind === 'node' ? nodeRules(key) : edgeRules(key)
  const rule = rules[index]
  if (!rule) return
  const updated = { ...rule, property, value: '' }
  if (kind === 'node') styleStore.updateNodeRule(key, index, updated)
  else styleStore.updateEdgeRule(key, index, updated)
}

function updateRuleOp(kind: 'node' | 'edge', key: string, index: number, op: string) {
  const rules = kind === 'node' ? nodeRules(key) : edgeRules(key)
  const rule = rules[index]
  if (!rule) return
  const updated = { ...rule, op: op as StyleRuleOp }
  if (NO_VALUE_OPS.has(op as StyleRuleOp)) {
    delete (updated as any).value
  }
  if (kind === 'node') styleStore.updateNodeRule(key, index, updated)
  else styleStore.updateEdgeRule(key, index, updated)
}

function updateRuleValue(kind: 'node' | 'edge', key: string, index: number, event: Event) {
  const rules = kind === 'node' ? nodeRules(key) : edgeRules(key)
  const rule = rules[index]
  if (!rule) return
  const raw = (event.target as HTMLInputElement).value
  // Try to coerce to number if it looks numeric
  let value: string | number | boolean = raw
  if (raw === 'true') value = true
  else if (raw === 'false') value = false
  else if (raw !== '' && !isNaN(Number(raw))) value = Number(raw)
  const updated = { ...rule, value }
  if (kind === 'node') styleStore.updateNodeRule(key, index, updated)
  else styleStore.updateEdgeRule(key, index, updated)
}

function updateRuleStyleColor(kind: 'node' | 'edge', key: string, index: number, color: string) {
  const rules = kind === 'node' ? nodeRules(key) : edgeRules(key)
  const rule = rules[index]
  if (!rule) return
  const updated = { ...rule, style: { ...rule.style, color } }
  if (kind === 'node') styleStore.updateNodeRule(key, index, updated)
  else styleStore.updateEdgeRule(key, index, updated)
}

function updateRuleStyleSize(kind: 'node' | 'edge', key: string, index: number, event: Event) {
  const rules = kind === 'node' ? nodeRules(key) : edgeRules(key)
  const rule = rules[index]
  if (!rule) return
  const val = parseFloat((event.target as HTMLInputElement).value)
  const sizeKey = kind === 'node' ? 'size' : 'thickness'
  const updated = { ...rule, style: { ...rule.style, [sizeKey]: val } }
  if (kind === 'node') styleStore.updateNodeRule(key, index, updated)
  else styleStore.updateEdgeRule(key, index, updated)
}

function updateRuleStyleProgram(kind: 'node', key: string, index: number, program: string) {
  const rules = nodeRules(key)
  const rule = rules[index]
  if (!rule) return
  const updated = { ...rule, style: { ...rule.style, program } }
  styleStore.updateNodeRule(key, index, updated)
}

function moveRule(kind: 'node' | 'edge', key: string, index: number, dir: -1 | 1) {
  if (kind === 'node') styleStore.moveNodeRule(key, index, index + dir)
  else styleStore.moveEdgeRule(key, index, index + dir)
}

function removeRule(kind: 'node' | 'edge', key: string, index: number) {
  if (kind === 'node') styleStore.removeNodeRule(key, index)
  else styleStore.removeEdgeRule(key, index)
}

function ruleSummary(rule: StyleRule): string {
  const parts: string[] = []
  if (rule.style.color) parts.push(rule.style.color)
  if ('size' in rule.style && rule.style.size != null) parts.push(`size ${rule.style.size}`)
  if ('thickness' in rule.style && (rule.style as any).thickness != null) parts.push(`width ${(rule.style as any).thickness}`)
  if ('program' in rule.style && rule.style.program) parts.push(rule.style.program)
  return parts.join(', ') || 'no style set'
}
</script>

<template>
  <div class="schema-browser">
    <div class="schema-browser__toolbar">
      <span class="schema-browser__title">Schema</span>
      <button
        class="fg-icon-btn"
        :disabled="refreshing"
        title="Refresh schema"
        @click="handleRefresh()"
      >
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" :class="{ spinning: refreshing }">
          <path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" /><path d="M21 3v5h-5" /><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" /><path d="M8 16H3v5" />
        </svg>
      </button>
    </div>

    <div class="schema-browser__content">

    <!-- Nodes -->
    <div class="schema-section">
      <button class="schema-section__header" @click="toggleSection('labels')">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" :style="{ transform: expandedSections.has('labels') ? 'rotate(90deg)' : '' }">
          <path d="m9 18 6-6-6-6" />
        </svg>
        Nodes
        <span class="fg-badge">{{ schema.labelsDetail.length }}</span>
      </button>
      <div v-if="expandedSections.has('labels')" class="schema-section__items">
        <div
          v-for="ld in schema.labelsDetail"
          :key="ld.name"
          class="schema-card"
          :style="{ '--card-accent': getNodeDotColor(ld.name) }"
        >
          <div class="schema-card__header" @click="toggleLabel(ld.name)">
            <svg v-if="ld.properties.length > 0" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="schema-card__chevron" :class="{ 'schema-card__chevron--expanded': expandedLabels.has(ld.name) }">
              <path d="m9 18 6-6-6-6" />
            </svg>
            <span
              class="schema-card__dot"
              :class="{ 'schema-card__dot--hexagon': getNodeProgram(ld.name) === 'hexagon', 'schema-card__dot--pulse': getNodeProgram(ld.name) === 'pulse' }"
              :style="{ background: getNodeDotColor(ld.name) }"
            />
            <span class="schema-card__name" @click.stop="insertInEditor(':' + ld.name)">{{ ld.name }}</span>
            <span v-if="ld.properties.length > 0" class="schema-card__count">{{ ld.properties.length }}</span>
            <button
              v-if="editingNode === ld.name"
              class="schema-card__action schema-card__action--visible"
              title="Reset to default"
              @click.stop="resetNodeStyle(ld.name)"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" /><path d="M21 3v5h-5" /><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" /><path d="M8 16H3v5" />
              </svg>
            </button>
            <button
              class="schema-card__action"
              :class="{ 'schema-card__action--active': editingNode === ld.name }"
              title="Edit style"
              @click.stop="toggleNodeEditor(ld.name, $event)"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M12 22a1 1 0 0 1 0-20 10 9 0 0 1 10 9 5 5 0 0 1-5 5h-2.25a1.75 1.75 0 0 0-1.4 2.8l.3.4a1.75 1.75 0 0 1-1.4 2.8z" />
                <circle cx="13.5" cy="6.5" r=".5" fill="currentColor" /><circle cx="17.5" cy="10.5" r=".5" fill="currentColor" /><circle cx="6.5" cy="12.5" r=".5" fill="currentColor" /><circle cx="8.5" cy="7.5" r=".5" fill="currentColor" />
              </svg>
            </button>
          </div>
          <!-- Node style editor -->
          <div v-if="editingNode === ld.name" class="style-editor">
            <div class="style-editor__row">
              <span class="style-editor__label">Color</span>
              <div class="style-editor__field">
                <ColorPicker
                  :model-value="getNodeDotColor(ld.name)"
                  @update:model-value="setNodeColor(ld.name, $event)"
                />
                <input
                  class="style-editor__hex-input"
                  :value="getNodeDotColor(ld.name)"
                  maxlength="7"
                  spellcheck="false"
                  @change="onNodeHexInput(ld.name, $event)"
                  @keydown.enter="onNodeHexInput(ld.name, $event)"
                />
              </div>
            </div>
            <div class="style-editor__row">
              <span class="style-editor__label">Size</span>
              <div class="style-editor__field">
                <input
                  type="range"
                  class="style-editor__slider"
                  min="2" max="20" step="1"
                  :value="getNodeSize(ld.name)"
                  @input="onNodeSizeChange(ld.name, $event)"
                />
                <span class="style-editor__value">{{ getNodeSize(ld.name) }}</span>
              </div>
            </div>
            <div class="style-editor__row">
              <span class="style-editor__label">Shape</span>
              <div class="style-editor__field" style="gap: 4px">
                <button
                  class="style-editor__shape"
                  :class="{ 'style-editor__shape--active': getNodeProgram(ld.name) === 'circle' }"
                  title="Circle"
                  @click="styleStore.setNodeStyle(ld.name, { program: 'circle' as any })"
                >
                  <svg width="16" height="16" viewBox="0 0 16 16"><circle cx="8" cy="8" r="7" fill="currentColor" /></svg>
                </button>
                <button
                  class="style-editor__shape"
                  :class="{ 'style-editor__shape--active': getNodeProgram(ld.name) === 'hexagon' }"
                  title="Hexagon"
                  @click="styleStore.setNodeStyle(ld.name, { program: 'hexagon' as any })"
                >
                  <svg width="16" height="16" viewBox="0 0 16 16"><polygon points="8,1 14.5,4.5 14.5,11.5 8,15 1.5,11.5 1.5,4.5" fill="currentColor" /></svg>
                </button>
                <button
                  class="style-editor__shape"
                  :class="{ 'style-editor__shape--active': getNodeProgram(ld.name) === 'pulse' }"
                  title="Pulse"
                  @click="styleStore.setNodeStyle(ld.name, { program: 'pulse' as any })"
                >
                  <svg width="16" height="16" viewBox="0 0 16 16">
                    <circle cx="8" cy="8" r="4" fill="currentColor" />
                    <circle cx="8" cy="8" r="7" fill="none" stroke="currentColor" stroke-width="1" opacity="0.4" />
                  </svg>
                </button>
              </div>
            </div>
            <!-- Conditional rules -->
            <div class="style-rules">
              <div class="style-rules__header">
                <span class="style-rules__title">Rules</span>
                <span v-if="nodeRules(ld.name).length" class="style-rules__count">{{ nodeRules(ld.name).length }}</span>
              </div>
              <div v-for="(rule, ri) in nodeRules(ld.name)" :key="ri" class="style-rule">
                <div class="style-rule__condition">
                  <span class="style-rule__if">IF</span>
                  <FgSelect
                    :model-value="rule.property"
                    :options="propsForLabel(ld.name)"
                    placeholder="property"
                    small
                    class="style-rule__prop-select"
                    @update:model-value="updateRuleProperty('node', ld.name, ri, $event)"
                  />
                  <FgSelect
                    :model-value="rule.op"
                    :options="opsForType(propTypeForLabel(ld.name, rule.property))"
                    small
                    class="style-rule__op-select"
                    @update:model-value="updateRuleOp('node', ld.name, ri, $event)"
                  />
                  <input
                    v-if="!NO_VALUE_OPS.has(rule.op)"
                    class="fg-input fg-input--sm style-rule__value-input"
                    :value="rule.value ?? ''"
                    placeholder="value"
                    @change="updateRuleValue('node', ld.name, ri, $event)"
                  />
                </div>
                <div class="style-rule__style">
                  <span class="style-rule__then">THEN</span>
                  <div class="style-rule__style-fields">
                    <div class="style-rule__style-row">
                      <ColorPicker
                        :model-value="(rule.style.color as string) || getNodeDotColor(ld.name)"
                        @update:model-value="updateRuleStyleColor('node', ld.name, ri, $event)"
                      />
                      <input
                        type="range"
                        class="style-editor__slider style-rule__slider"
                        min="2" max="20" step="1"
                        :value="(rule.style as any).size ?? getNodeSize(ld.name)"
                        @input="updateRuleStyleSize('node', ld.name, ri, $event)"
                      />
                      <span class="style-editor__value">{{ (rule.style as any).size ?? getNodeSize(ld.name) }}</span>
                      <div class="style-rule__shapes">
                        <button
                          class="style-editor__shape style-editor__shape--sm"
                          :class="{ 'style-editor__shape--active': ((rule.style as any).program || 'circle') === 'circle' }"
                          title="Circle"
                          @click="updateRuleStyleProgram('node', ld.name, ri, 'circle')"
                        >
                          <svg width="12" height="12" viewBox="0 0 16 16"><circle cx="8" cy="8" r="7" fill="currentColor" /></svg>
                        </button>
                        <button
                          class="style-editor__shape style-editor__shape--sm"
                          :class="{ 'style-editor__shape--active': (rule.style as any).program === 'hexagon' }"
                          title="Hexagon"
                          @click="updateRuleStyleProgram('node', ld.name, ri, 'hexagon')"
                        >
                          <svg width="12" height="12" viewBox="0 0 16 16"><polygon points="8,1 14.5,4.5 14.5,11.5 8,15 1.5,11.5 1.5,4.5" fill="currentColor" /></svg>
                        </button>
                        <button
                          class="style-editor__shape style-editor__shape--sm"
                          :class="{ 'style-editor__shape--active': (rule.style as any).program === 'pulse' }"
                          title="Pulse"
                          @click="updateRuleStyleProgram('node', ld.name, ri, 'pulse')"
                        >
                          <svg width="12" height="12" viewBox="0 0 16 16"><circle cx="8" cy="8" r="4" fill="currentColor" /><circle cx="8" cy="8" r="7" fill="none" stroke="currentColor" stroke-width="1" opacity="0.4" /></svg>
                        </button>
                      </div>
                    </div>
                  </div>
                </div>
                <div class="style-rule__actions">
                  <button v-if="ri > 0" class="fg-icon-btn style-rule__move" title="Move up" @click="moveRule('node', ld.name, ri, -1)">
                    <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="m18 15-6-6-6 6"/></svg>
                  </button>
                  <button v-if="ri < nodeRules(ld.name).length - 1" class="fg-icon-btn style-rule__move" title="Move down" @click="moveRule('node', ld.name, ri, 1)">
                    <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="m6 9 6 6 6-6"/></svg>
                  </button>
                  <button class="fg-icon-btn style-rule__delete" title="Remove rule" @click="removeRule('node', ld.name, ri)">
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>
                  </button>
                </div>
              </div>
              <button v-if="propsForLabel(ld.name).length > 0" class="style-rules__add" @click="addNewNodeRule(ld.name)">+ Add rule</button>
              <div v-else class="style-rules__no-props">No properties available</div>
            </div>
          </div>
          <!-- Nested properties -->
          <div v-if="expandedLabels.has(ld.name) && ld.properties.length > 0" class="schema-card__props">
            <div
              v-for="prop in ld.properties"
              :key="prop.name"
              class="schema-card__prop"
              @click="insertInEditor(prop.name)"
            >
              <span class="schema-card__prop-name">{{ prop.name }}</span>
              <span class="schema-card__prop-type">{{ prop.type }}</span>
            </div>
          </div>
        </div>
        <div v-if="schema.labelsDetail.length === 0" class="schema-card__empty">
          No nodes found
        </div>
      </div>
    </div>

    <!-- Edges -->
    <div class="schema-section">
      <button class="schema-section__header" @click="toggleSection('types')">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" :style="{ transform: expandedSections.has('types') ? 'rotate(90deg)' : '' }">
          <path d="m9 18 6-6-6-6" />
        </svg>
        Edges
        <span class="fg-badge">{{ schema.relationshipTypesDetail.length }}</span>
      </button>
      <div v-if="expandedSections.has('types')" class="schema-section__items">
        <div
          v-for="rd in schema.relationshipTypesDetail"
          :key="rd.name"
          class="schema-card schema-card--edge"
          :style="{ '--card-accent': styleStore.edgeColor(rd.name) || 'var(--tv-text-muted)' }"
        >
          <div class="schema-card__header" @click="toggleEdgeType(rd.name)">
            <svg v-if="rd.properties.length > 0" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="schema-card__chevron" :class="{ 'schema-card__chevron--expanded': expandedEdgeTypes.has(rd.name) }">
              <path d="m9 18 6-6-6-6" />
            </svg>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="var(--card-accent)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" style="flex-shrink:0">
              <path d="M5 12h14" /><path d="m12 5 7 7-7 7" />
            </svg>
            <span class="schema-card__name" @click.stop="insertInEditor('[:' + rd.name + ']')">{{ rd.name }}</span>
            <span v-if="rd.properties.length > 0" class="schema-card__count">{{ rd.properties.length }}</span>
            <button
              v-if="editingEdge === rd.name"
              class="schema-card__action schema-card__action--visible"
              title="Reset to default"
              @click.stop="resetEdgeStyle(rd.name)"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" /><path d="M21 3v5h-5" /><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" /><path d="M8 16H3v5" />
              </svg>
            </button>
            <button
              class="schema-card__action"
              :class="{ 'schema-card__action--active': editingEdge === rd.name }"
              title="Edit style"
              @click.stop="toggleEdgeEditor(rd.name, $event)"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M12 22a1 1 0 0 1 0-20 10 9 0 0 1 10 9 5 5 0 0 1-5 5h-2.25a1.75 1.75 0 0 0-1.4 2.8l.3.4a1.75 1.75 0 0 1-1.4 2.8z" />
                <circle cx="13.5" cy="6.5" r=".5" fill="currentColor" /><circle cx="17.5" cy="10.5" r=".5" fill="currentColor" /><circle cx="6.5" cy="12.5" r=".5" fill="currentColor" /><circle cx="8.5" cy="7.5" r=".5" fill="currentColor" />
              </svg>
            </button>
          </div>
          <!-- Edge style editor -->
          <div v-if="editingEdge === rd.name" class="style-editor">
            <div class="style-editor__row">
              <span class="style-editor__label">Color</span>
              <div class="style-editor__field">
                <ColorPicker
                  :model-value="styleStore.edgeColor(rd.name) || '#2A3A48'"
                  @update:model-value="setEdgeColor(rd.name, $event)"
                />
                <input
                  class="style-editor__hex-input"
                  :value="styleStore.edgeColor(rd.name) || '#2A3A48'"
                  maxlength="7"
                  spellcheck="false"
                  @change="onEdgeHexInput(rd.name, $event)"
                  @keydown.enter="onEdgeHexInput(rd.name, $event)"
                />
              </div>
            </div>
            <div class="style-editor__row">
              <span class="style-editor__label">Width</span>
              <div class="style-editor__field">
                <input
                  type="range"
                  class="style-editor__slider"
                  min="0.5" max="5" step="0.5"
                  :value="getEdgeThickness(rd.name)"
                  @input="onEdgeThicknessChange(rd.name, $event)"
                />
                <span class="style-editor__value">{{ getEdgeThickness(rd.name) }}</span>
              </div>
            </div>
            <!-- Conditional rules -->
            <div class="style-rules">
              <div class="style-rules__header">
                <span class="style-rules__title">Rules</span>
                <span v-if="edgeRules(rd.name).length" class="style-rules__count">{{ edgeRules(rd.name).length }}</span>
              </div>
              <div v-for="(rule, ri) in edgeRules(rd.name)" :key="ri" class="style-rule">
                <div class="style-rule__condition">
                  <span class="style-rule__if">IF</span>
                  <FgSelect
                    :model-value="rule.property"
                    :options="propsForEdgeType(rd.name)"
                    placeholder="property"
                    small
                    class="style-rule__prop-select"
                    @update:model-value="updateRuleProperty('edge', rd.name, ri, $event)"
                  />
                  <FgSelect
                    :model-value="rule.op"
                    :options="opsForType(propTypeForEdgeType(rd.name, rule.property))"
                    small
                    class="style-rule__op-select"
                    @update:model-value="updateRuleOp('edge', rd.name, ri, $event)"
                  />
                  <input
                    v-if="!NO_VALUE_OPS.has(rule.op)"
                    class="fg-input fg-input--sm style-rule__value-input"
                    :value="rule.value ?? ''"
                    placeholder="value"
                    @change="updateRuleValue('edge', rd.name, ri, $event)"
                  />
                </div>
                <div class="style-rule__style">
                  <span class="style-rule__then">THEN</span>
                  <div class="style-rule__style-fields">
                    <div class="style-rule__style-row">
                      <ColorPicker
                        :model-value="(rule.style.color as string) || styleStore.edgeColor(rd.name) || '#2A3A48'"
                        @update:model-value="updateRuleStyleColor('edge', rd.name, ri, $event)"
                      />
                      <input
                        type="range"
                        class="style-editor__slider style-rule__slider"
                        min="0.5" max="5" step="0.5"
                        :value="(rule.style as any).thickness ?? getEdgeThickness(rd.name)"
                        @input="updateRuleStyleSize('edge', rd.name, ri, $event)"
                      />
                      <span class="style-editor__value">{{ (rule.style as any).thickness ?? getEdgeThickness(rd.name) }}</span>
                    </div>
                  </div>
                </div>
                <div class="style-rule__actions">
                  <button v-if="ri > 0" class="fg-icon-btn style-rule__move" title="Move up" @click="moveRule('edge', rd.name, ri, -1)">
                    <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="m18 15-6-6-6 6"/></svg>
                  </button>
                  <button v-if="ri < edgeRules(rd.name).length - 1" class="fg-icon-btn style-rule__move" title="Move down" @click="moveRule('edge', rd.name, ri, 1)">
                    <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="m6 9 6 6 6-6"/></svg>
                  </button>
                  <button class="fg-icon-btn style-rule__delete" title="Remove rule" @click="removeRule('edge', rd.name, ri)">
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>
                  </button>
                </div>
              </div>
              <button v-if="propsForEdgeType(rd.name).length > 0" class="style-rules__add" @click="addNewEdgeRule(rd.name)">+ Add rule</button>
              <div v-else class="style-rules__no-props">No properties available</div>
            </div>
          </div>
          <!-- Nested properties -->
          <div v-if="expandedEdgeTypes.has(rd.name) && rd.properties.length > 0" class="schema-card__props">
            <div
              v-for="prop in rd.properties"
              :key="prop.name"
              class="schema-card__prop"
              @click="insertInEditor(prop.name)"
            >
              <span class="schema-card__prop-name">{{ prop.name }}</span>
              <span class="schema-card__prop-type">{{ prop.type }}</span>
            </div>
          </div>
        </div>
        <div v-if="schema.relationshipTypesDetail.length === 0" class="schema-card__empty">
          No edges found
        </div>
      </div>
    </div>

    <!-- Indexes -->
    <div class="schema-section">
      <button class="schema-section__header" @click="toggleSection('indexes')">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" :style="{ transform: expandedSections.has('indexes') ? 'rotate(90deg)' : '' }">
          <path d="m9 18 6-6-6-6" />
        </svg>
        Indexes
        <span class="fg-badge">{{ schema.indexes.length }}</span>
      </button>
      <div v-if="expandedSections.has('indexes')" class="schema-section__items">
        <div
          v-for="idx in schema.indexes"
          :key="idx.name"
          class="schema-item"
        >
          <span class="schema-item__index-name truncate">{{ idx.name }}</span>
          <span class="schema-item__index-detail">{{ idx.label }}.{{ idx.property }}</span>
          <span v-if="idx.entries != null" class="schema-item__index-entries">{{ idx.entries.toLocaleString() }}</span>
        </div>
        <div v-if="schema.indexes.length === 0" class="schema-item schema-item--empty">
          No indexes found
        </div>
      </div>
    </div>

    <!-- Constraints -->
    <div class="schema-section">
      <button class="schema-section__header" @click="toggleSection('constraints')">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" :style="{ transform: expandedSections.has('constraints') ? 'rotate(90deg)' : '' }">
          <path d="m9 18 6-6-6-6" />
        </svg>
        Constraints
        <span class="fg-badge">{{ schema.constraints.length }}</span>
      </button>
      <div v-if="expandedSections.has('constraints')" class="schema-section__items">
        <div
          v-for="c in schema.constraints"
          :key="c.name"
          class="schema-item"
        >
          <span class="schema-item__index-name truncate">{{ c.name }}</span>
          <span class="schema-item__index-detail">{{ c.type }} on {{ c.label }}.{{ c.property }}</span>
        </div>
        <div v-if="schema.constraints.length === 0" class="schema-item schema-item--empty">
          No constraints
        </div>
      </div>
    </div>
    </div>

    <!-- Database actions -->
    <div class="schema-browser__actions">
      <button
        class="schema-browser__action-btn"
        title="Rename this database"
        @click="openRenameModal"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" />
          <path d="m15 5 4 4" />
        </svg>
        Rename
      </button>
      <!--
        Delete is always visible: the confirmation modal and the
        `allowDropDatabase` gate (in the click handler below) are the
        safety nets. Hiding it with one database left would leave WASM
        users (one seeded database) and fresh servers no way to find it;
        the backend refuses to delete the last database if that applies.
      -->
      <button
        class="schema-browser__action-btn schema-browser__action-btn--danger"
        title="Delete this database"
        @click="conn.allowDropDatabase ? openDropModal() : (showDisabledModal = true)"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M3 6h18" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /><path d="M10 11v6" /><path d="M14 11v6" />
        </svg>
        Delete
      </button>
    </div>

    <!-- Rename modal -->
    <Teleport to="body">
      <Transition name="modal">
        <div v-if="showRenameModal" class="fg-modal-overlay" @mousedown.self="showRenameModal = false">
          <div class="drop-modal" role="dialog" aria-modal="true" aria-label="Rename database" @click.stop>
            <div class="drop-modal__header drop-modal__header--info">Rename database</div>
            <div class="drop-modal__body">
              <p>Rename <strong>"{{ conn.currentDatabase }}"</strong> - this also renames the <code>.tvdb</code> file on disk.</p>
              <label class="drop-modal__label">New name</label>
              <input
                v-model="renameValue"
                class="fg-input drop-modal__input"
                placeholder="New database name..."
                maxlength="50"
                autocomplete="off"
                spellcheck="false"
                @keydown.enter="submitRename"
              />
              <div v-if="renameError" class="drop-modal__error">{{ renameError }}</div>
            </div>
            <div class="drop-modal__footer">
              <button class="fg-btn fg-btn--sm" @click="showRenameModal = false">Cancel</button>
              <button
                class="fg-btn fg-btn--sm fg-btn--primary"
                :disabled="!renameValue.trim() || renameValue.trim() === conn.currentDatabase || renaming"
                @click="submitRename"
              >Rename</button>
            </div>
          </div>
        </div>
      </Transition>
    </Teleport>

    <!-- Deletion disabled modal -->
    <Teleport to="body">
      <Transition name="modal">
        <div v-if="showDisabledModal" class="fg-modal-overlay" @mousedown.self="showDisabledModal = false">
          <div class="drop-modal" role="dialog" aria-modal="true" aria-label="Database deletion disabled" @click.stop>
            <div class="drop-modal__header drop-modal__header--info">Database deletion disabled</div>
            <div class="drop-modal__body">
              <p>The server is not configured to allow deleting databases. To enable this feature, restart the server with:</p>
              <pre class="drop-modal__code">TRAVERSE_ALLOW_DROP_DATABASE=true</pre>
            </div>
            <div class="drop-modal__footer">
              <button class="fg-btn fg-btn--sm fg-btn--primary" @click="showDisabledModal = false">OK</button>
            </div>
          </div>
        </div>
      </Transition>
    </Teleport>

    <!-- Delete confirmation modal -->
    <Teleport to="body">
      <Transition name="modal">
        <div v-if="showDropModal" class="fg-modal-overlay" @mousedown.self="showDropModal = false">
          <div class="drop-modal" role="dialog" aria-modal="true" aria-label="Delete database" @click.stop>
            <div class="drop-modal__header">Delete database</div>
            <div class="drop-modal__body">
              <p>This will permanently delete <strong>"{{ conn.currentDatabase }}"</strong> and remove its <code>.tvdb</code> file from disk. This action cannot be undone.</p>
              <label class="drop-modal__label">Type <strong>delete</strong> to confirm</label>
              <input
                v-model="dropConfirmInput"
                class="fg-input drop-modal__input"
                placeholder="delete"
                autocomplete="off"
                spellcheck="false"
                @keydown.enter="dropCurrentDatabase"
              />
              <div v-if="dropError" class="drop-modal__error">{{ dropError }}</div>
            </div>
            <div class="drop-modal__footer">
              <button class="fg-btn fg-btn--sm" @click="showDropModal = false">Cancel</button>
              <button
                class="fg-btn fg-btn--sm drop-modal__confirm-btn"
                :disabled="dropConfirmInput !== 'delete' || dropping"
                @click="dropCurrentDatabase"
              >Delete database</button>
            </div>
          </div>
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<style scoped>
.schema-browser {
  display: flex;
  flex-direction: column;
  height: 100%;
  padding: 0;
}

.schema-browser__content {
  flex: 1;
  overflow-y: auto;
  min-height: 0;
}

.schema-browser__toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 8px 12px;
}

.schema-browser__title {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-secondary);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.schema-section__header {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 8px 12px;
  border: none;
  background: none;
  color: var(--tv-text-primary);
  font-size: var(--tv-font-size-sm);
  font-weight: 500;
  cursor: pointer;
  text-align: left;
}
.schema-section__header:hover {
  background: var(--tv-bg-hover);
}

.schema-section__header svg {
  transition: transform 0.15s;
  flex-shrink: 0;
}

.schema-section__items {
  padding: 4px 8px 8px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

/* -- Schema card (node label / edge type) ------------------ */
.schema-card {
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  overflow: hidden;
  transition: border-color 0.15s;
}
.schema-card:hover {
  border-color: var(--tv-border-strong);
}

.schema-card__header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 8px 7px 10px;
  cursor: pointer;
  transition: background 0.1s;
}
.schema-card__header:hover {
  background: var(--tv-bg-hover);
}

.schema-card__chevron {
  flex-shrink: 0;
  transition: transform 0.15s;
  color: var(--tv-text-muted);
}
.schema-card__chevron--expanded {
  transform: rotate(90deg);
}

.schema-card__dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  flex-shrink: 0;
}
.schema-card__dot--hexagon {
  border-radius: 0;
  clip-path: polygon(50% 0%, 100% 25%, 100% 75%, 50% 100%, 0% 75%, 0% 25%);
}
.schema-card__dot--pulse {
  box-shadow: 0 0 0 2px var(--tv-bg-elevated), 0 0 0 3px var(--card-accent);
}

/* Index / constraint rows (simpler, no card) */
.schema-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 10px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
}

.schema-item--empty {
  color: var(--tv-text-muted);
  font-style: italic;
}

.schema-item__index-name {
  font-family: var(--tv-font-mono);
  max-width: 120px;
}

.schema-item__type {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  margin-left: auto;
  font-family: var(--tv-font-mono);
}

.schema-card__name {
  cursor: pointer;
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
  color: var(--tv-text-primary);
}

.schema-card__count {
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
  color: var(--tv-text-secondary);
  background: var(--tv-bg-base);
  border-radius: 9px;
  padding: 1px 7px;
  line-height: 18px;
  font-family: var(--tv-font-mono);
  flex-shrink: 0;
}

.schema-card__action {
  border: none;
  background: none;
  color: var(--tv-text-muted);
  cursor: pointer;
  padding: 4px;
  border-radius: var(--tv-radius-sm);
  flex-shrink: 0;
  display: flex;
  align-items: center;
  transition: color 0.15s, background 0.15s;
}
.schema-card__action:hover {
  color: var(--tv-text-primary);
  background: var(--tv-bg-hover);
}
.schema-card__action--active {
  color: var(--tv-accent);
}

.schema-card__empty {
  padding: 8px 12px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  font-style: italic;
}

/* Properties inside card */
.schema-card__props {
  border-top: 1px solid var(--tv-border-default);
  padding: 4px 0;
}

.schema-card__prop {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 3px 10px 3px 32px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  cursor: pointer;
}
.schema-card__prop:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}

.schema-card__prop-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.schema-card__prop-type {
  font-size: 11px;
  color: var(--tv-text-muted);
  font-family: var(--tv-font-mono);
  flex-shrink: 0;
}

.schema-item__index-detail {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  margin-left: auto;
}

.schema-item__index-entries {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  font-family: var(--tv-font-mono);
  margin-left: 8px;
  flex-shrink: 0;
}

/* Style editor panel (inside card) */
.style-editor {
  padding: 8px 10px 8px 32px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  border-top: 1px solid var(--tv-border-default);
  min-width: 0;
}

.style-editor__row {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}

.style-editor__label {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  width: 34px;
  flex-shrink: 0;
  margin-right: 4px;
}

.style-editor__field {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: 1;
  min-width: 0;
}

.style-editor__hex-input {
  flex: 1;
  min-width: 0;
  font-size: 11px;
  font-family: var(--tv-font-mono);
  color: var(--tv-text-secondary);
  background: transparent;
  border: none;
  outline: none;
  padding: 0;
}
.style-editor__hex-input:focus {
  color: var(--tv-text-primary);
}

.style-editor__slider {
  flex: 1;
  min-width: 0;
  height: 18px;
  cursor: pointer;
  -webkit-appearance: none;
  appearance: none;
  background: transparent;
  outline: none;
  margin: 0;
}
.style-editor__slider::-webkit-slider-runnable-track {
  height: 8px;
  border-radius: 4px;
  background: var(--tv-bg-base);
  border: 1px solid var(--tv-border-default);
}
.style-editor__slider::-webkit-slider-thumb {
  -webkit-appearance: none;
  width: 6px;
  height: 16px;
  border-radius: 3px;
  background: var(--tv-accent);
  border: none;
  box-shadow: 0 0 0 1px var(--tv-border-default);
  cursor: grab;
  margin-top: -4px;
}
.style-editor__slider::-webkit-slider-thumb:active {
  cursor: grabbing;
}
.style-editor__slider::-moz-range-track {
  height: 8px;
  border-radius: 4px;
  background: var(--tv-bg-base);
  border: 1px solid var(--tv-border-default);
}
.style-editor__slider::-moz-range-thumb {
  width: 6px;
  height: 16px;
  border-radius: 3px;
  background: var(--tv-accent);
  border: none;
  box-shadow: 0 0 0 1px var(--tv-border-default);
  cursor: grab;
}
.style-editor__slider::-moz-range-thumb:active {
  cursor: grabbing;
}

.style-editor__value {
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
  color: var(--tv-text-secondary);
  width: 20px;
  text-align: right;
  font-family: var(--tv-font-mono);
  flex-shrink: 0;
}

.style-editor__shape {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  padding: 0;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-base);
  color: var(--tv-text-muted);
  cursor: pointer;
  transition: border-color 0.15s, color 0.15s, background 0.15s;
}
.style-editor__shape:hover {
  border-color: var(--tv-border-strong);
  color: var(--tv-text-primary);
}
.style-editor__shape--active {
  background: var(--tv-accent-subtle);
  border-color: var(--tv-accent);
  color: var(--tv-accent);
}

/* Conditional style rules */
.style-rules {
  margin-top: 4px;
  padding-top: 6px;
  border-top: 1px dashed var(--tv-border-default);
}

.style-rules__header {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-bottom: 6px;
}

.style-rules__title {
  font-size: 10px;
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
}

.style-rules__count {
  font-size: 10px;
  color: var(--tv-text-muted);
  font-family: var(--tv-font-mono);
}

.style-rule {
  background: var(--tv-bg-base);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  padding: 6px 8px;
  margin-bottom: 4px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.style-rule__condition {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-wrap: wrap;
}

.style-rule__if,
.style-rule__then {
  font-size: 10px;
  font-weight: 600;
  color: var(--tv-accent);
  text-transform: uppercase;
  letter-spacing: 0.03em;
  flex-shrink: 0;
  width: 30px;
}

.style-rule__prop-select {
  flex: 1;
  min-width: 60px;
  max-width: 100px;
}

.style-rule__op-select {
  flex-shrink: 0;
  min-width: 50px;
  max-width: 85px;
}

.style-rule__value-input {
  flex: 1;
  min-width: 40px;
  max-width: 70px;
  font-size: 11px !important;
  height: 26px !important;
  padding: 0 6px !important;
}

.style-rule__style {
  display: flex;
  align-items: flex-start;
  gap: 4px;
}

.style-rule__style-fields {
  flex: 1;
  min-width: 0;
}

.style-rule__style-row {
  display: flex;
  align-items: center;
  gap: 6px;
}

.style-rule__slider {
  flex: 1;
  min-width: 40px;
}

.style-rule__shapes {
  display: flex;
  gap: 2px;
  flex-shrink: 0;
}

.style-editor__shape--sm {
  width: 22px;
  height: 22px;
}

.style-rule__actions {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 2px;
}

.style-rule__move {
  width: 18px;
  height: 18px;
  padding: 0;
}

.style-rule__delete {
  width: 18px;
  height: 18px;
  padding: 0;
}

.style-rule__delete:hover {
  color: var(--tv-status-error) !important;
}

.style-rules__add {
  display: block;
  width: 100%;
  padding: 4px 0;
  border: none;
  background: none;
  color: var(--tv-accent);
  font-size: 11px;
  font-weight: 500;
  font-family: var(--tv-font-sans);
  cursor: pointer;
  text-align: left;
  transition: color 0.15s;
}

.style-rules__add:hover {
  color: var(--tv-accent-hover);
}

.style-rules__no-props {
  font-size: 10px;
  color: var(--tv-text-muted);
  font-style: italic;
}

/* Database actions */
.schema-browser__actions {
  flex-shrink: 0;
  border-top: 1px solid var(--tv-border-default);
  padding: 8px 12px;
  display: flex;
  gap: 6px;
}

.schema-browser__action-btn {
  display: flex;
  align-items: center;
  gap: 6px;
  flex: 1;
  justify-content: center;
  padding: 6px 8px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: none;
  color: var(--tv-text-secondary);
  font-size: var(--tv-font-size-xs);
  font-family: var(--tv-font-sans);
  cursor: pointer;
  transition: background 0.15s, color 0.15s, border-color 0.15s;
}
.schema-browser__action-btn:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}

.schema-browser__action-btn--danger {
  border-color: var(--tv-status-error);
  color: var(--tv-status-error);
}
.schema-browser__action-btn--danger:hover {
  background: var(--tv-status-error-subtle);
  color: var(--tv-status-error);
}



@keyframes fg-spin {
  to { transform: rotate(360deg); }
}
.spinning {
  animation: fg-spin 0.6s linear infinite;
}
</style>

<style>
/* Style rule selects (FgSelect inside scoped component - cannot be scoped) */
.style-rule__prop-select .fgs__trigger,
.style-rule__op-select .fgs__trigger {
  font-size: 11px;
  height: 24px;
  padding: 0 4px 0 6px;
}

/* Database modals (teleported to body, cannot be scoped) */
.drop-modal {
  width: 400px;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-lg);
  box-shadow: var(--tv-shadow-xl);
  overflow: hidden;
}

.drop-modal__header {
  padding: 12px 16px;
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-status-error);
  border-bottom: 1px solid var(--tv-border-default);
}

.drop-modal__header--info {
  color: var(--tv-text-primary);
}

.drop-modal__body {
  padding: 16px;
}

.drop-modal__body p {
  margin: 0 0 12px;
  font-size: var(--tv-font-size-sm);
  line-height: 1.5;
  color: var(--tv-text-secondary);
}

.drop-modal__body code {
  font-family: var(--tv-font-mono);
  font-size: var(--tv-font-size-xs);
  padding: 1px 4px;
  border-radius: 3px;
  background: var(--tv-bg-base);
  color: var(--tv-text-secondary);
}

.drop-modal__code {
  margin: 0;
  padding: 8px 10px;
  font-family: var(--tv-font-mono);
  font-size: var(--tv-font-size-xs);
  background: var(--tv-bg-base);
  border-radius: var(--tv-radius-sm);
  color: var(--tv-text-primary);
  overflow-x: auto;
}

.drop-modal__label {
  display: block;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  margin-bottom: 6px;
}

.drop-modal__input {
  width: 100%;
  box-sizing: border-box;
}

.drop-modal__error {
  margin-top: 8px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-status-error);
}

.drop-modal__footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 16px;
  border-top: 1px solid var(--tv-border-default);
}

.drop-modal__confirm-btn {
  background: var(--tv-status-error) !important;
  color: var(--tv-text-inverse) !important;
  border-color: var(--tv-status-error) !important;
}

.drop-modal__confirm-btn:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}
</style>
