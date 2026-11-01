import { defineStore } from 'pinia'
import { ref, watch } from 'vue'

export type PanelId = 'dashboard' | 'schema' | 'files' | 'users' | 'import' | 'examples' | 'ml' | 'cypher-ref' | 'docs' | 'settings' | null

function initialTheme(): 'light' | 'dark' {
  const stored = localStorage.getItem('fg_theme')
  if (stored === 'light' || stored === 'dark') return stored
  // First-visit fallback: follow the system preference. Stored value
  // wins on subsequent visits - we only write to localStorage when the
  // user explicitly toggles via the header button.
  return window.matchMedia?.('(prefers-color-scheme: light)').matches ? 'light' : 'dark'
}

export const useSettingsStore = defineStore('settings', () => {
  const theme = ref<'light' | 'dark'>(initialTheme())
  const editorFontSize = ref(parseInt(localStorage.getItem('fg_editor_font_size') || '14'))

  // Active side panel
  const storedPanel = localStorage.getItem('fg_active_panel')
  const activePanel = ref<PanelId>(
    storedPanel === null ? 'dashboard' : (storedPanel || null) as PanelId
  )

  // AI Chat panel (right side, independent of sidebar).
  // On mobile, ignore the persisted "open" flag at boot - opening the
  // chat there is a full-screen overlay that would otherwise cover the
  // dashboard on first paint and confuse users who only opened the
  // chat once on desktop. The localStorage value is left as-is so
  // desktop sessions still auto-restore.
  const isMobileViewport = typeof window !== 'undefined'
    && window.matchMedia?.('(max-width: 768px)').matches
  const aiChatOpen = ref(
    isMobileViewport ? false : localStorage.getItem('tv_ai_chat_open') === 'true'
  )

  // Graph render limits
  const maxNodes = ref(parseInt(localStorage.getItem('fg_max_nodes') || '3500'))
  const maxEdges = ref(parseInt(localStorage.getItem('fg_max_edges') || '8500'))

  // Mobile: the Monaco editor pane is collapsed by default so the
  // graph / result area gets the screen. Stored separately because the
  // desktop layout is always expanded.
  const editorMobileCollapsed = ref(
    localStorage.getItem('tv_editor_mobile_collapsed') !== 'false'
  )

  watch(theme, (v) => {
    localStorage.setItem('fg_theme', v)
    document.documentElement.setAttribute('data-theme', v)
    document.documentElement.setAttribute('data-ag-theme-mode', v)
  })
  watch(editorFontSize, (v) => localStorage.setItem('fg_editor_font_size', String(v)))
  watch(activePanel, (v) => localStorage.setItem('fg_active_panel', v ?? ''))
  watch(aiChatOpen, (v) => localStorage.setItem('tv_ai_chat_open', String(v)))
  watch(maxNodes, (v) => localStorage.setItem('fg_max_nodes', String(v)))
  watch(maxEdges, (v) => localStorage.setItem('fg_max_edges', String(v)))
  watch(editorMobileCollapsed, (v) => localStorage.setItem('tv_editor_mobile_collapsed', String(v)))

  return { theme, editorFontSize, activePanel, aiChatOpen, maxNodes, maxEdges, editorMobileCollapsed }
})
