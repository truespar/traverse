<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch, nextTick } from 'vue'
import { useQueryStore } from '../../stores/query'
import { useSchemaStore } from '../../stores/schema'
import { useSettingsStore } from '../../stores/settings'
import { useConnectionStore } from '../../stores/connection'
import { useViewport } from '../../composables/useViewport'
import { registerCypherLanguage, updateSchemaCompletions } from './CypherLanguage'
import { registerGqlLanguage } from './GqlLanguage'
import { useToast } from '../../composables/useToast'
import { api } from '../../utils/api'
import QueryPalette from './QueryPalette.vue'
import SaveQueryDialog from './SaveQueryDialog.vue'
import ShareQueryDialog from './ShareQueryDialog.vue'
import FgSelect from '../common/FgSelect.vue'
import type * as Monaco from 'monaco-editor'

// Monaco web worker setup - must be configured before importing monaco-editor
// monaco-editor >= 0.56 exposes its ESM tree through an export map
// (`monaco-editor/<path>` -> `esm/vs/<path>.js`); deep `esm/vs/...` paths
// no longer resolve.
import editorWorker from 'monaco-editor/editor/editor.worker?worker'
import jsonWorker from 'monaco-editor/languages/features/json/json.worker?worker'
import cssWorker from 'monaco-editor/languages/features/css/css.worker?worker'
import htmlWorker from 'monaco-editor/languages/features/html/html.worker?worker'
import tsWorker from 'monaco-editor/languages/features/typescript/ts.worker?worker'

self.MonacoEnvironment = {
  getWorker(_: unknown, label: string) {
    if (label === 'json') return new jsonWorker()
    if (label === 'css' || label === 'scss' || label === 'less') return new cssWorker()
    if (label === 'html' || label === 'handlebars' || label === 'razor') return new htmlWorker()
    if (label === 'typescript' || label === 'javascript') return new tsWorker()
    return new editorWorker()
  },
}

const queryStore = useQueryStore()
/** While a query runs, Run turns into Stop (when the backend can stop a running query). */
const canStop = api.capabilities.cancelQuery
const stopping = computed(() => canStop && queryStore.isExecuting)
const schemaStore = useSchemaStore()
const settings = useSettingsStore()
const conn = useConnectionStore()
const { isMobile } = useViewport()
const { showToast } = useToast()

const dialectOptions = computed(() => {
  const out = [{ value: 'cypher', label: 'openCypher' }]
  // Server reports compile-time features via /api/health; only show
  // the GQL choice when the running binary was built with --features
  // gql. Hides the option entirely instead of letting the request
  // fail with "GQL dialect requested but this server was built
  // without the `gql` feature."
  if (conn.features.gql) {
    out.push({ value: 'gql', label: 'GQL (ISO 39075)' })
  }
  return out
})

// If the user previously had GQL selected (persisted in queryStore)
// but the server we're talking to now doesn't support it, drop back
// to cypher so the next Run doesn't fail with a confusing error.
watch(
  dialectOptions,
  (opts) => {
    const allowed = new Set(opts.map((o) => o.value))
    if (!allowed.has(queryStore.currentDialect)) {
      queryStore.currentDialect = 'cypher'
    }
  },
  { immediate: true },
)

function toggleMobileCollapse() {
  settings.editorMobileCollapsed = !settings.editorMobileCollapsed
  // Monaco caches its layout dimensions; tell it to re-measure after
  // the CSS height transition runs.
  nextTick(() => setTimeout(() => editor?.layout(), 200))
}

const editorContainer = ref<HTMLElement | null>(null)
let editor: Monaco.editor.IStandaloneCodeEditor | null = null
let monaco: typeof Monaco | null = null
const showPalette = ref(false)
const showSaveDialog = ref(false)
const showShareDialog = ref(false)

onMounted(async () => {
  // Dynamic import to avoid bundling monaco in main chunk
  monaco = await import('monaco-editor')

  if (!editorContainer.value) return

  registerCypherLanguage(monaco)
  registerGqlLanguage(monaco)

  // Define custom themes matching Studio palette
  monaco.editor.defineTheme('fg-dark', {
    base: 'vs-dark',
    inherit: true,
    rules: [
      { token: 'keyword', foreground: '38BDF8' },
      { token: 'string', foreground: 'D4A030' },
      { token: 'number', foreground: '5B9BD5' },
      { token: 'comment', foreground: '4A5565', fontStyle: 'italic' },
      { token: 'type', foreground: 'a78bfa' },
      { token: 'identifier', foreground: 'C9CDCF' },
      { token: 'operator', foreground: '7E8B97' },
    ],
    colors: {
      'editor.background': '#151D24',
      'editor.foreground': '#C9CDCF',
      'editor.lineHighlightBackground': '#1C262F',
      'editor.selectionBackground': '#0EA5E920',
      'editor.inactiveSelectionBackground': '#0EA5E910',
      'editorCursor.foreground': '#38BDF8',
      'editorLineNumber.foreground': '#3A4A58',
      'editorLineNumber.activeForeground': '#7E8B97',
      'editorGutter.background': '#151D24',
      'editorWidget.background': '#1C262F',
      'editorWidget.border': '#2A3A48',
      'input.background': '#0E1419',
      'input.border': '#2A3A48',
      'scrollbarSlider.background': '#2A3A4880',
      'scrollbarSlider.hoverBackground': '#3A4A5880',
    },
  })
  monaco.editor.defineTheme('fg-light', {
    base: 'vs',
    inherit: true,
    rules: [
      { token: 'keyword', foreground: '0369A1' },
      { token: 'string', foreground: 'B8860B' },
      { token: 'number', foreground: '2A6BB5' },
      { token: 'comment', foreground: '9AA1AB', fontStyle: 'italic' },
      { token: 'type', foreground: '7C3AED' },
      { token: 'identifier', foreground: '1A1D20' },
      { token: 'operator', foreground: '5A6370' },
    ],
    colors: {
      'editor.background': '#FFFFFF',
      'editor.foreground': '#1A1D20',
      'editor.lineHighlightBackground': '#F3F4F5',
      'editor.selectionBackground': '#0369A118',
      'editor.inactiveSelectionBackground': '#0369A10A',
      'editorCursor.foreground': '#0369A1',
      'editorLineNumber.foreground': '#B8BEC6',
      'editorLineNumber.activeForeground': '#5A6370',
      'editorGutter.background': '#FFFFFF',
      'editorWidget.background': '#FFFFFF',
      'editorWidget.border': '#D8DBDF',
      'input.background': '#F3F4F5',
      'input.border': '#D8DBDF',
      'scrollbarSlider.background': '#D8DBDF80',
      'scrollbarSlider.hoverBackground': '#B8BEC680',
    },
  })

  editor = monaco.editor.create(editorContainer.value, {
    value: queryStore.currentQuery,
    language: queryStore.currentDialect === 'gql' ? 'gql' : 'cypher',
    theme: settings.theme === 'dark' ? 'fg-dark' : 'fg-light',
    fontSize: settings.editorFontSize,
    minimap: { enabled: false },
    lineNumbers: 'on',
    wordWrap: 'on',
    scrollBeyondLastLine: false,
    automaticLayout: true,
    tabSize: 2,
    renderWhitespace: 'none',
    overviewRulerLanes: 0,
    hideCursorInOverviewRuler: true,
    scrollbar: {
      verticalScrollbarSize: 8,
      horizontalScrollbarSize: 8,
    },
    padding: { top: 8, bottom: 8 },
    quickSuggestions: true,
    suggestOnTriggerCharacters: true,
    wordBasedSuggestions: 'off',
  })

  // Ctrl+Enter to execute
  editor.addAction({
    id: 'execute-query',
    label: 'Execute Query',
    keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter],
    run: () => {
      queryStore.currentQuery = editor!.getValue()
      queryStore.executeQuery()
    },
  })

  // Ctrl+Shift+Enter to explain
  editor.addAction({
    id: 'explain-query',
    label: 'Explain Query',
    keybindings: [monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.Enter],
    run: () => {
      const q = editor!.getValue().trim()
      queryStore.currentQuery = q
      queryStore.executeQuery(`EXPLAIN ${q}`)
    },
  })


  // Sync editor content to store
  editor.onDidChangeModelContent(() => {
    queryStore.currentQuery = editor!.getValue()
  })

  // Update schema completions
  updateSchemaCompletions(
    monaco,
    schemaStore.labels,
    schemaStore.relationshipTypes,
    schemaStore.labelsDetail,
    schemaStore.relationshipTypesDetail,
    schemaStore.propertyKeys.map((pk) => pk.name),
  )

  // Load saved queries from server (skip if auth gate is showing)
  if (!conn.authRequired) queryStore.fetchSavedQueries()
})

// Sync store -> editor (for external updates like Cypher Reference "Use" button)
watch(() => queryStore.currentQuery, (v) => {
  if (editor && v !== editor.getValue()) {
    editor.setValue(v)
  }
})

// Watch theme changes
watch(() => settings.theme, (t) => {
  if (monaco && editor) {
    monaco.editor.setTheme(t === 'dark' ? 'fg-dark' : 'fg-light')
  }
})

// Swap the model language when the user toggles the dialect dropdown.
// `setModelLanguage` re-tokenises in place, so the user keeps their
// caret position, selection, and history.
watch(() => queryStore.currentDialect, (dialect) => {
  if (monaco && editor) {
    const model = editor.getModel()
    if (model) {
      monaco.editor.setModelLanguage(model, dialect === 'gql' ? 'gql' : 'cypher')
    }
  }
})

// Watch schema changes for completions
watch(
  () => [schemaStore.labels, schemaStore.relationshipTypes, schemaStore.propertyKeys, schemaStore.labelsDetail, schemaStore.relationshipTypesDetail],
  () => {
    if (monaco) {
      updateSchemaCompletions(
        monaco,
        schemaStore.labels,
        schemaStore.relationshipTypes,
        schemaStore.labelsDetail,
        schemaStore.relationshipTypesDetail,
        schemaStore.propertyKeys.map((pk) => pk.name),
      )
    }
  },
)

// Global keyboard shortcuts - must use window listener to prevent browser defaults
function onGlobalKeydown(e: KeyboardEvent) {
  const mod = e.ctrlKey || e.metaKey
  if (!mod) return

  if ((e.key === 's' || e.key === 'S') && e.shiftKey) {
    e.preventDefault()
    const q = editor?.getValue()?.trim() ?? ''
    if (!q) {
      showToast('Nothing to share', 'error')
      return
    }
    showShareDialog.value = true
  } else if (e.key === 's' || e.key === 'S') {
    e.preventDefault()
    const q = editor?.getValue()?.trim() ?? ''
    if (!q) {
      showToast('Nothing to save', 'error')
      return
    }
    showSaveDialog.value = true
  } else if (e.key === 'p' || e.key === 'P') {
    e.preventDefault()
    showPalette.value = !showPalette.value
  }
}

window.addEventListener('keydown', onGlobalKeydown)

onUnmounted(() => {
  window.removeEventListener('keydown', onGlobalKeydown)
  editor?.dispose()
})

/** Run, or Stop while a query runs. */
function runOrStop() {
  if (stopping.value) queryStore.cancelQuery()
  else execute()
}

function execute() {
  queryStore.currentQuery = editor?.getValue() ?? ''
  queryStore.executeQuery()
  // On phones, collapse the editor after firing so the result/graph
  // gets the screen. Desktop layout is unaffected.
  if (isMobile.value && !settings.editorMobileCollapsed) {
    settings.editorMobileCollapsed = true
    nextTick(() => setTimeout(() => editor?.layout(), 200))
  }
}

function handleSave(name: string) {
  const q = editor?.getValue()?.trim() ?? ''
  if (!q) return
  const finalName = name || `query_${new Date().toTimeString().slice(0, 8).replace(/:/g, '')}`
  queryStore.saveQuery(finalName, q)
  showToast(`Saved as '${finalName}'`)
  showSaveDialog.value = false
}

function handlePaletteSelect(payload: { query: string }) {
  if (editor) {
    editor.setValue(payload.query)
    queryStore.currentQuery = payload.query
  }
  showPalette.value = false
}
</script>

<template>
  <div class="query-editor">
    <!-- Mobile toolbar - single row above Monaco containing the
         expand/collapse toggle (left) and the Run pill (right).
         Always rendered so Run is reachable even when collapsed.
         Desktop uses the floating Run button inside the editor wrap. -->
    <div v-if="isMobile" class="query-editor__toolbar">
      <button
        class="query-editor__toolbar-toggle"
        :aria-label="settings.editorMobileCollapsed ? 'Expand editor' : 'Collapse editor'"
        @click="toggleMobileCollapse"
      >
        <svg
          width="14"
          height="14"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          stroke-linejoin="round"
          :style="settings.editorMobileCollapsed ? '' : 'transform: rotate(180deg)'"
        >
          <path d="m18 15-6-6-6 6" />
        </svg>
        {{ settings.editorMobileCollapsed ? 'Edit query' : 'Hide editor' }}
      </button>
      <button
        class="query-editor__toolbar-run"
        :class="{ 'query-editor__run--stop': stopping }"
        :disabled="queryStore.isExecuting && !stopping"
        @click="runOrStop"
        :aria-label="stopping ? 'Stop query' : 'Execute query'"
      >
        <svg v-if="stopping" width="12" height="12" viewBox="0 0 24 24" fill="currentColor">
          <rect x="6" y="6" width="12" height="12" rx="1" />
        </svg>
        <span v-else-if="queryStore.isExecuting" class="fg-spinner" style="width:12px;height:12px;border-width:1.5px" />
        <svg v-else width="12" height="12" viewBox="0 0 24 24" fill="currentColor">
          <path d="m6 3 14 9-14 9V3z" />
        </svg>
        {{ stopping ? 'Stop' : 'Run' }}
      </button>
    </div>

    <!-- Monaco wrapper. Hidden when the mobile editor is collapsed so
         the toolbar gets all the (small) height. -->
    <div
      v-show="!(isMobile && settings.editorMobileCollapsed)"
      class="query-editor__editor-wrap"
    >
      <div ref="editorContainer" class="query-editor__monaco" />

      <!-- Floating Run button - desktop only. -->
      <button
        v-if="!isMobile"
        class="query-editor__run-btn"
        :class="{ 'query-editor__run--stop': stopping }"
        :disabled="queryStore.isExecuting && !stopping"
        @click="runOrStop"
        :title="stopping ? 'Stop the running query' : 'Execute query'"
      >
        <svg v-if="stopping" width="12" height="12" viewBox="0 0 24 24" fill="currentColor">
          <rect x="6" y="6" width="12" height="12" rx="1" />
        </svg>
        <span v-else-if="queryStore.isExecuting" class="fg-spinner" style="width:12px;height:12px;border-width:1.5px" />
        <svg v-else width="12" height="12" viewBox="0 0 24 24" fill="currentColor">
          <path d="m6 3 14 9-14 9V3z" />
        </svg>
        <span class="query-editor__run-label query-editor__run-label--desktop">{{ stopping ? 'Stop' : 'Ctrl+Enter' }}</span>
        <span class="query-editor__run-label query-editor__run-label--mobile">{{ stopping ? 'Stop' : 'Run' }}</span>
      </button>

      <!-- Keyboard shortcut hints (clickable) -->
      <div class="query-editor__hints">
        <button class="query-editor__hint" @click="showSaveDialog = true"><kbd>Ctrl+S</kbd> Save</button>
        <button class="query-editor__hint" @click="showShareDialog = true"><kbd>Ctrl+Shift+S</kbd> Share</button>
        <button class="query-editor__hint" @click="showPalette = !showPalette"><kbd>Ctrl+P</kbd> Queries</button>
        <div
          v-if="dialectOptions.length > 1"
          class="query-editor__dialect"
          title="Select query dialect - openCypher is the default; GQL requires a server built with --features gql"
        >
          <span class="query-editor__dialect-label">Dialect</span>
          <FgSelect
            v-model="queryStore.currentDialect"
            :options="dialectOptions"
            small
            class="query-editor__dialect-select"
          />
        </div>
      </div>

      <QueryPalette v-if="showPalette" @close="showPalette = false" @select="handlePaletteSelect" />
    </div>

    <SaveQueryDialog
      v-if="showSaveDialog"
      @save="handleSave"
      @cancel="showSaveDialog = false"
    />

    <ShareQueryDialog
      v-if="showShareDialog"
      :query="queryStore.currentQuery.trim()"
      @close="showShareDialog = false"
    />
  </div>
</template>

<style scoped>
.query-editor {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--tv-bg-surface);
  position: relative;
}

.query-editor__editor-wrap {
  flex: 1;
  min-height: 0;
  position: relative;
}

.query-editor__monaco {
  width: 100%;
  height: 100%;
}

/* Floating Run button */
.query-editor__run-btn {
  position: absolute;
  bottom: 12px;
  right: 16px;
  z-index: 20;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 6px 14px;
  height: 32px;
  border: none;
  border-radius: 9999px;
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
  cursor: pointer;
  opacity: 0.75;
  transition: opacity 0.15s, background 0.15s;
  box-shadow: var(--tv-shadow-md);
}
.query-editor__run-btn:hover {
  opacity: 1;
  background: var(--tv-accent-hover);
}
.query-editor__run-btn:active {
  background: var(--tv-accent-active);
}
/* Stop: the run buttons while a query runs */
.query-editor__run--stop,
.query-editor__run--stop:hover,
.query-editor__run--stop:active {
  background: var(--tv-status-error);
  opacity: 1;
}
.query-editor__run--stop:hover {
  background: var(--tv-status-error-hover);
}
.query-editor__run-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.query-editor__run-label {
  font-size: 11px;
  opacity: 0.85;
}

/* Mobile editor toolbar - a single row above Monaco containing the
   expand/collapse toggle (left) and the Run pill (right). Always
   visible on mobile, collapsed or expanded. */
.query-editor__toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 6px 10px;
  height: 44px;
  background: var(--tv-bg-surface);
  border-bottom: 1px solid var(--tv-border-default);
  flex-shrink: 0;
}
.query-editor__toolbar-toggle {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 10px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-elevated);
  color: var(--tv-text-secondary);
  font-size: 12px;
  font-weight: 500;
  cursor: pointer;
}
.query-editor__toolbar-toggle:hover {
  color: var(--tv-text-primary);
  background: var(--tv-bg-hover);
}
.query-editor__toolbar-toggle svg {
  transition: transform 0.2s ease;
}
.query-editor__toolbar-run {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 16px;
  border: none;
  border-radius: 9999px;
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
  font-size: 12px;
  font-weight: 600;
  cursor: pointer;
  box-shadow: var(--tv-shadow-md);
}
.query-editor__toolbar-run:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}

/* Show only the desktop label by default; swap to the mobile label
   below the breakpoint. */
.query-editor__run-label--mobile {
  display: none;
}
@media (max-width: 640px) {
  .query-editor__run-label--desktop {
    display: none;
  }
  .query-editor__run-label--mobile {
    display: inline;
  }
}

/* Keyboard shortcut hints */
.query-editor__hints {
  position: absolute;
  bottom: 14px;
  left: 12px;
  z-index: 20;
  display: flex;
  gap: 4px;
  opacity: 0.4;
  transition: opacity 0.15s;
}
.query-editor__editor-wrap:hover .query-editor__hints {
  opacity: 0.7;
}

.query-editor__hint {
  font-family: var(--tv-font-sans);
  font-size: 11px;
  color: var(--tv-text-secondary);
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 3px 8px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: none;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.query-editor__hint:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}

.query-editor__hint kbd {
  font-family: var(--tv-font-sans);
  font-size: 10px;
  padding: 1px 5px;
  border: 1px solid var(--tv-border-default);
  border-radius: 3px;
  background: var(--tv-bg-elevated);
  color: var(--tv-text-secondary);
}

.query-editor__dialect {
  display: flex;
  align-items: center;
  gap: 6px;
}
.query-editor__dialect-label {
  font-family: var(--tv-font-sans);
  font-size: 11px;
  color: var(--tv-text-secondary);
}
.query-editor__dialect-select {
  min-width: 140px;
}

/* On narrow viewports (phones), hide the keyboard-shortcut hints so they
   don't collide with the floating Run button. The shortcuts still work for
   anyone with a physical keyboard; the buttons are reachable from the
   header/menu. */
@media (max-width: 640px) {
  .query-editor__hints {
    display: none;
  }
}
</style>

<style>
/* Monaco's iPadShowKeyboard widget - an invisible textarea Monaco floats
   in the editor's top-right corner on iOS to let users re-summon the soft
   keyboard. It overlaps our floating Run button and isn't needed: tapping
   the editor already focuses it and triggers the keyboard. Hide it
   globally (the rule must be unscoped because the widget is injected by
   Monaco into its own DOM subtree, outside Vue's scoped CSS). */
.iPadShowKeyboard,
textarea[widgetid="editor.contrib.ShowKeyboardWidget"] {
  display: none !important;
}

/* iOS Safari auto-zooms whenever a textarea/input with font-size < 16px
   gains focus, and frequently doesn't zoom back out cleanly. Monaco
   feeds keystrokes through a hidden <textarea class="inputarea"> that
   inherits the editor's fontSize (we use 14px). The visible code is
   drawn by Monaco itself in styled <span>s, NOT by the textarea - so
   bumping just the textarea to 16px disables the focus-zoom without
   touching how the code is rendered. */
.monaco-editor textarea.inputarea {
  font-size: 16px !important;
}
</style>
