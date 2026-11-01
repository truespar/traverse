import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import { api, type NodeStyle, type EdgeStyle, type StylesData, type StyleRule, type StyleRuleOp } from '../utils/api'
import { useConnectionStore } from './connection'
import { colorForLabel } from '../composables/useGraphExplorer'
import { useToast } from '../composables/useToast'

/** Evaluate a single style rule against a properties object. Defensive - never throws. */
function evaluateRule(rule: StyleRule, properties: Record<string, unknown>): boolean {
  try {
    const op = rule.op
    if (op === 'exists') return rule.property in properties
    if (op === 'not_exists') return !(rule.property in properties)

    const propVal = properties[rule.property]
    if (propVal === undefined || propVal === null) return false
    const ruleVal = rule.value

    if (op === '=') return propVal == ruleVal // intentional loose equality for string/number coercion
    if (op === '!=') return propVal != ruleVal

    // Numeric comparisons - coerce both sides
    const numProp = Number(propVal)
    const numRule = Number(ruleVal)
    if (Number.isNaN(numProp) || Number.isNaN(numRule)) return false
    if (op === '>') return numProp > numRule
    if (op === '<') return numProp < numRule
    if (op === '>=') return numProp >= numRule
    if (op === '<=') return numProp <= numRule

    // String operations
    const strProp = String(propVal)
    const strRule = String(ruleVal ?? '')
    if (op === 'contains') return strProp.includes(strRule)
    if (op === 'starts_with') return strProp.startsWith(strRule)
  } catch {
    // Malformed rule - ignore
  }
  return false
}

/** Resolve a base style + rules into final style overrides. Last matching rule wins per-field. */
function resolveRules<T extends NodeStyle | EdgeStyle>(
  baseStyle: T | undefined,
  properties: Record<string, unknown>,
): Partial<T> {
  const result: Partial<T> = {}
  if (!baseStyle?.rules?.length) return result
  for (const rule of baseStyle.rules) {
    if (evaluateRule(rule, properties)) {
      Object.assign(result, rule.style)
    }
  }
  return result
}

export const useStyleStore = defineStore('style', () => {
  const nodeStyles = ref<Record<string, NodeStyle>>({})
  const edgeStyles = ref<Record<string, EdgeStyle>>({})

  let saveTimeout: ReturnType<typeof setTimeout> | null = null

  async function fetchStyles() {
    const conn = useConnectionStore()
    try {
      const data = await api.getStyles(conn.currentDatabase)
      nodeStyles.value = data.nodes || {}
      edgeStyles.value = data.edges || {}
    } catch {
      nodeStyles.value = {}
      edgeStyles.value = {}
    }
  }

  function debouncedSave() {
    if (saveTimeout) clearTimeout(saveTimeout)
    saveTimeout = setTimeout(() => {
      const conn = useConnectionStore()
      const styles: StylesData = {
        nodes: nodeStyles.value,
        edges: edgeStyles.value,
      }
      api.putStyles(styles, conn.currentDatabase).catch((e: any) => {
        // Silently ignore when running without a data directory (in-memory mode)
        if (e.message?.includes('no database path')) return
        const { showToast } = useToast()
        showToast(e.message || 'Failed to save styles', 'error')
      })
    }, 300)
  }

  function setNodeStyle(label: string, style: Partial<NodeStyle>) {
    const existing = nodeStyles.value[label] || {}
    nodeStyles.value = { ...nodeStyles.value, [label]: { ...existing, ...style } }
    debouncedSave()
  }

  function setEdgeStyle(type: string, style: Partial<EdgeStyle>) {
    const existing = edgeStyles.value[type] || {}
    edgeStyles.value = { ...edgeStyles.value, [type]: { ...existing, ...style } }
    debouncedSave()
  }

  function removeNodeStyle(label: string) {
    const copy = { ...nodeStyles.value }
    delete copy[label]
    nodeStyles.value = copy
    debouncedSave()
  }

  function removeEdgeStyle(type: string) {
    const copy = { ...edgeStyles.value }
    delete copy[type]
    edgeStyles.value = copy
    debouncedSave()
  }

  function autoAssignMissing(labels: string[], isDark: boolean) {
    let changed = false
    for (const label of labels) {
      if (!nodeStyles.value[label]?.color) {
        const existing = nodeStyles.value[label] || {}
        nodeStyles.value = { ...nodeStyles.value, [label]: { ...existing, color: colorForLabel(label, isDark) } }
        changed = true
      }
    }
    if (changed) debouncedSave()
  }

  const nodeColor = computed(() => (label: string) => nodeStyles.value[label]?.color)
  const nodeSize = computed(() => (label: string) => nodeStyles.value[label]?.size)
  const nodeProgram = computed(() => (label: string) => nodeStyles.value[label]?.program)
  const edgeColor = computed(() => (type: string) => edgeStyles.value[type]?.color)
  const edgeThickness = computed(() => (type: string) => edgeStyles.value[type]?.thickness)

  /** Resolve final node style (base + rule overrides) for a specific node's properties. */
  function resolveNode(label: string, properties: Record<string, unknown>): { color?: string; size?: number; program?: string } {
    const base = nodeStyles.value[label]
    return resolveRules(base, properties)
  }

  /** Resolve final edge style (base + rule overrides) for a specific edge's properties. */
  function resolveEdge(type: string, properties: Record<string, unknown>): { color?: string; thickness?: number } {
    const base = edgeStyles.value[type]
    return resolveRules(base, properties)
  }

  function addNodeRule(label: string, rule: StyleRule) {
    const existing = nodeStyles.value[label] || {}
    const rules = [...(existing.rules || []), rule]
    nodeStyles.value = { ...nodeStyles.value, [label]: { ...existing, rules } }
    debouncedSave()
  }

  function updateNodeRule(label: string, index: number, rule: StyleRule) {
    const existing = nodeStyles.value[label]
    if (!existing?.rules) return
    const rules = [...existing.rules]
    rules[index] = rule
    nodeStyles.value = { ...nodeStyles.value, [label]: { ...existing, rules } }
    debouncedSave()
  }

  function removeNodeRule(label: string, index: number) {
    const existing = nodeStyles.value[label]
    if (!existing?.rules) return
    const rules = existing.rules.filter((_, i) => i !== index)
    nodeStyles.value = { ...nodeStyles.value, [label]: { ...existing, rules: rules.length ? rules : undefined } }
    debouncedSave()
  }

  function moveNodeRule(label: string, from: number, to: number) {
    const existing = nodeStyles.value[label]
    if (!existing?.rules) return
    const rules = [...existing.rules]
    const [moved] = rules.splice(from, 1)
    rules.splice(to, 0, moved)
    nodeStyles.value = { ...nodeStyles.value, [label]: { ...existing, rules } }
    debouncedSave()
  }

  function addEdgeRule(type: string, rule: StyleRule) {
    const existing = edgeStyles.value[type] || {}
    const rules = [...(existing.rules || []), rule]
    edgeStyles.value = { ...edgeStyles.value, [type]: { ...existing, rules } }
    debouncedSave()
  }

  function updateEdgeRule(type: string, index: number, rule: StyleRule) {
    const existing = edgeStyles.value[type]
    if (!existing?.rules) return
    const rules = [...existing.rules]
    rules[index] = rule
    edgeStyles.value = { ...edgeStyles.value, [type]: { ...existing, rules } }
    debouncedSave()
  }

  function removeEdgeRule(type: string, index: number) {
    const existing = edgeStyles.value[type]
    if (!existing?.rules) return
    const rules = existing.rules.filter((_, i) => i !== index)
    edgeStyles.value = { ...edgeStyles.value, [type]: { ...existing, rules: rules.length ? rules : undefined } }
    debouncedSave()
  }

  function moveEdgeRule(type: string, from: number, to: number) {
    const existing = edgeStyles.value[type]
    if (!existing?.rules) return
    const rules = [...existing.rules]
    const [moved] = rules.splice(from, 1)
    rules.splice(to, 0, moved)
    edgeStyles.value = { ...edgeStyles.value, [type]: { ...existing, rules } }
    debouncedSave()
  }

  return {
    nodeStyles,
    edgeStyles,
    fetchStyles,
    autoAssignMissing,
    setNodeStyle,
    setEdgeStyle,
    removeNodeStyle,
    removeEdgeStyle,
    nodeColor,
    nodeSize,
    nodeProgram,
    edgeColor,
    edgeThickness,
    resolveNode,
    resolveEdge,
    addNodeRule,
    updateNodeRule,
    removeNodeRule,
    moveNodeRule,
    addEdgeRule,
    updateEdgeRule,
    removeEdgeRule,
    moveEdgeRule,
  }
})
