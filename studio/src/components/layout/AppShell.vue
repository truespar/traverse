<script setup lang="ts">
import { ref, computed, watch, onMounted, onUnmounted } from 'vue'
import { useConnectionStore } from '../../stores/connection'
import { useSchemaStore } from '../../stores/schema'
import { useSettingsStore } from '../../stores/settings'
import { useStyleStore } from '../../stores/style'
import { useUsersStore } from '../../stores/users'
import { useQueryStore } from '../../stores/query'
import { useTheme } from '../../composables/useTheme'
import { useViewport } from '../../composables/useViewport'
import AppHeader from './AppHeader.vue'
import ActivityBar from './ActivityBar.vue'
import WasmModeBanner from './WasmModeBanner.vue'
import SchemaBrowser from '../schema/SchemaBrowser.vue'
import FileManagerPanel from '../files/FileManagerPanel.vue'
import UsersBrowser from '../users/UsersBrowser.vue'
import ImportPanel from '../import/ImportPanel.vue'
import ExamplesPanel from '../examples/ExamplesPanel.vue'
import MlPanel from '../ml/MlPanel.vue'
import SettingsPanel from '../settings/SettingsPanel.vue'
import CypherRefPanel from '../reference/CypherRefPanel.vue'
import DocsPanel from '../reference/DocsPanel.vue'
import QueryEditor from '../editor/QueryEditor.vue'
import ResultPanel from '../results/ResultPanel.vue'
import AppToast from '../ui/AppToast.vue'
import AuthGate from '../auth/AuthGate.vue'
import DashboardPanel from '../dashboard/DashboardPanel.vue'
import AiChatPanel from '../chat/AiChatPanel.vue'

const conn = useConnectionStore()
const schema = useSchemaStore()
const settings = useSettingsStore()
const styleStore = useStyleStore()
const usersStore = useUsersStore()
const queryStore = useQueryStore()
const { initTheme } = useTheme()
const { isMobile } = useViewport()


function closeSidePanel() {
  settings.activePanel = null
}
function closeChatPanel() {
  settings.aiChatOpen = false
}

// Panels that render in the main content area instead of the sidebar
const MAIN_PANELS = new Set(['dashboard', 'cypher-ref', 'docs'])
const sidebarPanel = computed(() =>
  settings.activePanel && !MAIN_PANELS.has(settings.activePanel) ? settings.activePanel : null
)
const mainPanel = computed(() =>
  settings.activePanel && MAIN_PANELS.has(settings.activePanel) ? settings.activePanel : null
)

function loadInt(key: string, fallback: number): number {
  const v = localStorage.getItem(key)
  if (!v) return fallback
  const n = parseInt(v, 10)
  return Number.isNaN(n) ? fallback : n
}

const sidePanelWidth = ref(loadInt('fg_sidebar_width', 260))
const editorHeight = ref(loadInt('fg_editor_height', 200))
const chatPanelWidth = ref(loadInt('tv_chat_panel_width', 380))
const isResizingSidePanel = ref(false)
const isResizingEditor = ref(false)
const isResizingChatPanel = ref(false)

watch(sidePanelWidth, (v) => localStorage.setItem('fg_sidebar_width', String(v)))
watch(editorHeight, (v) => localStorage.setItem('fg_editor_height', String(v)))
watch(chatPanelWidth, (v) => localStorage.setItem('tv_chat_panel_width', String(v)))

async function loadData() {
  await Promise.all([schema.fetchSchema(), styleStore.fetchStyles()])
  styleStore.autoAssignMissing(schema.labels, settings.theme === 'dark')
  usersStore.fetchUsers()
  queryStore.fetchSavedQueries()
}

onMounted(() => {
  initTheme()
  conn.startPolling()
})

// Load data once connected and authenticated (or auth not required)
watch(
  () => conn.isConnected && !conn.authRequired,
  (ready) => { if (ready) loadData() },
  { immediate: true },
)

// Re-fetch schema and styles when database changes
watch(() => conn.currentDatabase, async () => {
  if (!conn.isConnected || conn.authRequired) return
  await Promise.all([schema.fetchSchema(), styleStore.fetchStyles()])
  styleStore.autoAssignMissing(schema.labels, settings.theme === 'dark')
})

onUnmounted(() => {
  conn.stopPolling()
  // Clean up any active resize listeners
  for (const cleanup of resizeCleanups) cleanup()
  resizeCleanups.length = 0
})

// Track active resize cleanup functions so we can remove them on unmount
const resizeCleanups: Array<() => void> = []

function startResizeSidePanel(e: MouseEvent) {
  isResizingSidePanel.value = true
  const startX = e.clientX
  const startW = sidePanelWidth.value

  const onMove = (ev: MouseEvent) => {
    const delta = ev.clientX - startX
    sidePanelWidth.value = Math.max(180, Math.min(500, startW + delta))
  }
  const onUp = () => {
    isResizingSidePanel.value = false
    document.removeEventListener('mousemove', onMove)
    document.removeEventListener('mouseup', onUp)
    resizeCleanups.splice(resizeCleanups.indexOf(onUp), 1)
  }
  document.addEventListener('mousemove', onMove)
  document.addEventListener('mouseup', onUp)
  resizeCleanups.push(onUp)
}

function startResizeChatPanel(e: MouseEvent) {
  isResizingChatPanel.value = true
  const startX = e.clientX
  const startW = chatPanelWidth.value

  const onMove = (ev: MouseEvent) => {
    const delta = startX - ev.clientX // inverted: dragging left makes panel wider
    chatPanelWidth.value = Math.max(280, Math.min(600, startW + delta))
  }
  const onUp = () => {
    isResizingChatPanel.value = false
    document.removeEventListener('mousemove', onMove)
    document.removeEventListener('mouseup', onUp)
    resizeCleanups.splice(resizeCleanups.indexOf(onUp), 1)
  }
  document.addEventListener('mousemove', onMove)
  document.addEventListener('mouseup', onUp)
  resizeCleanups.push(onUp)
}

function startResizeEditor(e: MouseEvent) {
  isResizingEditor.value = true
  const startY = e.clientY
  const startH = editorHeight.value

  const onMove = (ev: MouseEvent) => {
    const delta = ev.clientY - startY
    editorHeight.value = Math.max(100, Math.min(600, startH + delta))
  }
  const onUp = () => {
    isResizingEditor.value = false
    document.removeEventListener('mousemove', onMove)
    document.removeEventListener('mouseup', onUp)
    resizeCleanups.splice(resizeCleanups.indexOf(onUp), 1)
  }
  document.addEventListener('mousemove', onMove)
  document.addEventListener('mouseup', onUp)
  resizeCleanups.push(onUp)
}
</script>

<template>
  <div class="app-shell" :class="{ 'app-shell--mobile': isMobile }">
    <!-- Activity bar - left rail on desktop, bottom tab bar on mobile -->
    <ActivityBar />

    <!-- Right of activity bar: header on top, content below -->
    <div class="app-shell__right">
      <AuthGate />
      <WasmModeBanner />
      <AppHeader />

      <div class="app-shell__body">
        <!-- Side panel (sidebar panels only) - overlay drawer on mobile -->
        <aside
          v-if="sidebarPanel"
          class="app-shell__side-panel"
          :style="isMobile ? undefined : { width: sidePanelWidth + 'px' }"
        >
          <SchemaBrowser v-if="sidebarPanel === 'schema'" />
          <FileManagerPanel v-if="sidebarPanel === 'files'" />
          <UsersBrowser v-if="sidebarPanel === 'users'" />
          <ImportPanel v-if="sidebarPanel === 'import'" />
          <ExamplesPanel v-if="sidebarPanel === 'examples'" />
          <MlPanel v-if="sidebarPanel === 'ml'" />
          <SettingsPanel v-if="sidebarPanel === 'settings'" />
        </aside>

        <!-- Backdrop for mobile drawer -->
        <div
          v-if="sidebarPanel && isMobile"
          class="app-shell__backdrop"
          @click="closeSidePanel"
        />

        <!-- Side panel resize handle (hidden on mobile) -->
        <div
          v-if="sidebarPanel && !isMobile"
          class="fg-resizer fg-resizer--v"
          :class="{ 'fg-resizer--active': isResizingSidePanel }"
          @mousedown="startResizeSidePanel"
        />

        <!-- Main content: full-width panels or editor+results -->
        <div class="app-shell__main">
          <template v-if="mainPanel">
            <DashboardPanel v-if="mainPanel === 'dashboard'" />
            <CypherRefPanel v-if="mainPanel === 'cypher-ref'" />
            <DocsPanel v-if="mainPanel === 'docs'" />
          </template>
          <template v-else>
            <!-- Editor pane -->
            <div
              class="app-shell__editor"
              :class="{ 'app-shell__editor--collapsed': isMobile && settings.editorMobileCollapsed }"
              :style="isMobile ? undefined : { height: editorHeight + 'px' }"
            >
              <QueryEditor />
            </div>

            <!-- Editor/Result resize handle (hidden on mobile) -->
            <div
              v-if="!isMobile"
              class="fg-resizer fg-resizer--h"
              :class="{ 'fg-resizer--active': isResizingEditor }"
              @mousedown="startResizeEditor"
            />

            <!-- Result pane -->
            <div class="app-shell__results">
              <ResultPanel />
            </div>
          </template>
        </div>

        <!-- AI Chat panel - right side on desktop, overlay on mobile -->
        <template v-if="settings.aiChatOpen && conn.aiChatAvailable">
          <div
            v-if="!isMobile"
            class="fg-resizer fg-resizer--v"
            :class="{ 'fg-resizer--active': isResizingChatPanel }"
            @mousedown="startResizeChatPanel"
          />
          <aside
            class="app-shell__chat-panel"
            :style="isMobile ? undefined : { width: chatPanelWidth + 'px' }"
          >
            <AiChatPanel />
          </aside>
          <div
            v-if="isMobile"
            class="app-shell__backdrop"
            @click="closeChatPanel"
          />
        </template>
      </div>
    </div>

    <AppToast />
  </div>
</template>

<style scoped>
.app-shell {
  display: flex;
  height: 100%;
  width: 100%;
  overflow: hidden;
}

.app-shell__right {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
  height: 100%;
  overflow: hidden;
}

.app-shell__body {
  display: flex;
  flex: 1;
  min-height: 0;
  overflow: hidden;
}

.app-shell__side-panel {
  flex-shrink: 0;
  height: 100%;
  overflow: hidden;
  background: var(--tv-bg-surface);
}

.app-shell__main {
  flex: 1;
  min-width: 0;
  height: 100%;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.app-shell__editor {
  flex-shrink: 0;
  overflow: hidden;
  position: relative;
}

.app-shell__results {
  flex: 1;
  min-height: 0;
  overflow: hidden;
}

.app-shell__chat-panel {
  flex-shrink: 0;
  height: 100%;
  overflow: hidden;
}

/* Backdrop behind side-panel / chat-panel overlays on mobile.
   Sits below the panel (z-index 70) and above the gds-drawer (50). */
.app-shell__backdrop {
  position: fixed;
  top: 0;
  left: 0;
  right: 0;
  bottom: 0;
  background: rgba(0, 0, 0, 0.4);
  z-index: 65;
}

/* ---------- Mobile layout (<=768px) ---------- */
@media (max-width: 768px) {
  /* Activity bar moves to the bottom of the screen as a tab bar.
     Using column-reverse keeps the activity bar last in the visual
     stack while preserving its position first in the DOM. */
  .app-shell {
    flex-direction: column-reverse;
  }

  /* The right pane (header + body) now takes full width. */
  .app-shell__right {
    flex: 1;
    min-height: 0;
  }

  /* Side panel becomes a slide-over drawer from the left, layered
     above the editor / results. z-index: 70 keeps it above the
     gds-drawer (z-index 50) so opening it doesn't get covered. */
  .app-shell__side-panel {
    position: fixed;
    top: 0;
    left: 0;
    bottom: var(--tv-activitybar-mobile-height, 56px);
    width: 85vw !important;
    max-width: 360px;
    z-index: 70;
    border-right: 1px solid var(--tv-border-default);
    box-shadow: var(--tv-shadow-lg);
  }

  /* AI chat panel takes the full screen on mobile. */
  .app-shell__chat-panel {
    position: fixed;
    top: 0;
    left: 0;
    right: 0;
    bottom: var(--tv-activitybar-mobile-height, 56px);
    width: 100% !important;
    z-index: 70;
  }

  /* Vertical editor height becomes a fraction of viewport rather than
     a persisted pixel value from desktop. Expanded ~ half the viewport
     so editing a multi-line query is reasonable; collapsed is just a
     single-line strip so the graph view gets the rest of the screen.
     `dvh` so iOS Safari's collapsing URL bar doesn't push the editor
     under it. `vh` falls back for browsers without dvh. */
  .app-shell__editor {
    height: 50vh !important;
    height: 50dvh !important;
    transition: height 0.18s ease;
  }
  .app-shell__editor--collapsed {
    /* Just the toolbar (with the Edit-query toggle + Run pill).
       Monaco is hidden in this state so the graph gets the rest. */
    height: 44px !important;
  }
}
</style>
