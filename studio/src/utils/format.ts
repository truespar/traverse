/** Shared formatting utilities used across the Studio UI. */

export function formatValue(v: unknown): string {
  if (v === null || v === undefined) return 'null'
  if (typeof v === 'object') return JSON.stringify(v)
  return String(v)
}

export function inferType(v: unknown): string {
  if (v === null || v === undefined) return 'Null'
  if (typeof v === 'boolean') return 'Boolean'
  if (typeof v === 'number') return Number.isInteger(v) ? 'Integer' : 'Float'
  if (typeof v === 'string') return 'String'
  if (Array.isArray(v)) return 'List'
  return 'Map'
}
