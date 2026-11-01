<script setup lang="ts">
import { ref, computed, watch, onBeforeUnmount } from 'vue'
import { useConnectionStore } from '../../stores/connection'
import type { DatabaseInfo } from '../../utils/api'
import { useQueryStore } from '../../stores/query'
import { useSchemaStore } from '../../stores/schema'
import { useTheme } from '../../composables/useTheme'
import { api, getApiKey, clearApiKey } from '../../utils/api'
import AliasPanel from '../aliases/AliasPanel.vue'

const conn = useConnectionStore()
const query = useQueryStore()
const schema = useSchemaStore()
const { theme, toggleTheme } = useTheme()

// Alias panel
const showAliasPanel = ref(false)

// Database management
const showDbMenu = ref(false)
const showCreateDb = ref(false)
const newDbName = ref('')
const dbError = ref('')
const dbBtnRef = ref<HTMLElement | null>(null)
const dbMenuPos = ref({ top: 0, left: 0 })

function toggleDbMenu() {
  showDbMenu.value = !showDbMenu.value
  if (showDbMenu.value && dbBtnRef.value) {
    const r = dbBtnRef.value.getBoundingClientRect()
    // Menu has a fixed width (see CSS). Clamp the left so its right
    // edge stays inside the viewport - otherwise opening it on the
    // right side of the screen or showing the "New database" form
    // pushes content off-screen.
    const menuWidth = Math.min(300, window.innerWidth - 16)
    const left = Math.min(
      Math.max(8, r.left),
      window.innerWidth - menuWidth - 8,
    )
    dbMenuPos.value = { top: r.bottom + 6, left }
  }
}

// Aliases: flat list fetched when dropdown opens
const aliasList = ref<{ alias: string; database: string }[]>([])

// Merged list: databases + aliases as top-level items
const dbMenuItems = computed(() => {
  const items: { name: string; isAlias: boolean; target?: string; db?: DatabaseInfo }[] = []
  for (const db of conn.databases) {
    items.push({ name: db.name, isAlias: false, db })
  }
  for (const { alias, database } of aliasList.value) {
    items.push({ name: alias, isAlias: true, target: database })
  }
  return items
})

function selectDatabase(name: string) {
  conn.currentDatabase = name
  showDbMenu.value = false
  conn.pollHealth()
}

async function createDatabase() {
  const name = newDbName.value.trim()
  if (!name) return
  dbError.value = ''
  try {
    await api.createDatabase(name)
    newDbName.value = ''
    showCreateDb.value = false
    showDbMenu.value = false
    conn.currentDatabase = name
    conn.pollHealth()
  } catch (e: any) {
    dbError.value = e.message || 'Failed to create database'
  }
}

// Close dropdown on click outside
function onClickOutside(e: MouseEvent) {
  const target = e.target as HTMLElement
  if (!target.closest('.header__db') && !target.closest('.header__db-menu')) {
    showDbMenu.value = false
  }
}

watch(showDbMenu, async (open) => {
  if (open) {
    dbError.value = ''
    document.addEventListener('click', onClickOutside)
    // Fetch aliases to show in the dropdown
    try {
      aliasList.value = await api.aliases()
    } catch {
      aliasList.value = []
    }
  } else {
    document.removeEventListener('click', onClickOutside)
    showCreateDb.value = false
  }
})

function formatBytes(bytes: number): string {
  const mb = bytes / (1024 * 1024)
  return mb < 1024 ? `${mb.toFixed(0)} MB` : `${(mb / 1024).toFixed(1)} GB`
}

const opfsDisplay = computed(() => {
  if (!conn.isWasmMode) return ''
  if (conn.opfsUsage == null || conn.opfsQuota == null) return ''
  return `${formatBytes(conn.opfsUsage)} / ${formatBytes(conn.opfsQuota)}`
})

const opfsPct = computed(() => {
  if (conn.opfsUsage == null || conn.opfsQuota == null || conn.opfsQuota === 0) return 0
  return Math.round((conn.opfsUsage / conn.opfsQuota) * 100)
})

const dbMemoryDisplay = computed(() => {
  if (!conn.dbMemory) return ''
  return formatBytes(conn.dbMemory)
})

const totalMemoryDisplay = computed(() => {
  if (!conn.memory) return ''
  return formatBytes(conn.memory)
})

const nodesDisplay = computed(() => conn.nodeCount.toLocaleString())
const edgesDisplay = computed(() => conn.edgeCount.toLocaleString())

const lastQueryTime = computed(() => {
  if (!query.timeMs) return ''
  return query.timeMs < 1 ? `${(query.timeMs * 1000).toFixed(0)} us` : `${query.timeMs.toFixed(1)} ms`
})

// Memory popover
// Helper: clamp the `right` offset for right-anchored popovers so
// their left edge stays on-screen. `width` is the popover's CSS width.
function clampRight(rect: DOMRect, width: number, gutter = 8): number {
  const raw = window.innerWidth - rect.right
  const maxRight = window.innerWidth - width - gutter
  return Math.max(gutter, Math.min(raw, maxRight))
}

const memoryRef = ref<HTMLElement | null>(null)
const showMemoryTip = ref(false)
const memoryTipPos = ref({ top: 0, right: 0 })

function toggleMemoryPopover() {
  if (showMemoryTip.value) {
    showMemoryTip.value = false
    return
  }
  if (memoryRef.value) {
    const rect = memoryRef.value.getBoundingClientRect()
    memoryTipPos.value = {
      top: rect.bottom + 8,
      right: clampRight(rect, 260),
    }
  }
  showMemoryTip.value = true
}

function onClickOutsideMemory(e: MouseEvent) {
  const target = e.target as HTMLElement
  if (!target.closest('.memtip') && !target.closest('.header__memory')) {
    showMemoryTip.value = false
  }
}

watch(showMemoryTip, (open) => {
  if (open) {
    document.addEventListener('click', onClickOutsideMemory)
  } else {
    document.removeEventListener('click', onClickOutsideMemory)
  }
})

// WASM-info popover. Same Teleport / click-outside pattern as the
// memory popover above. Clicking the WASM badge in the header opens
// a small panel showing which bundle variant is loaded, the thread
// count, and whether the browser-side features it depends on
// (SharedArrayBuffer, OPFS) are present.
const wasmBadgeRef = ref<HTMLElement | null>(null)
const showWasmTip = ref(false)
const wasmTipPos = ref({ top: 0, right: 0 })

function toggleWasmPopover() {
  if (showWasmTip.value) {
    showWasmTip.value = false
    return
  }
  if (wasmBadgeRef.value) {
    const rect = wasmBadgeRef.value.getBoundingClientRect()
    wasmTipPos.value = {
      top: rect.bottom + 8,
      right: clampRight(rect, 260),
    }
  }
  showWasmTip.value = true
}

function onClickOutsideWasm(e: MouseEvent) {
  const target = e.target as HTMLElement
  if (!target.closest('.wasmtip') && !target.closest('.header__wasm-badge')) {
    showWasmTip.value = false
  }
}

watch(showWasmTip, (open) => {
  if (open) {
    document.addEventListener('click', onClickOutsideWasm)
  } else {
    document.removeEventListener('click', onClickOutsideWasm)
  }
})

// Static capability values - computed once at component setup. The
// values don't change for the life of the tab.
const sabAvailable = typeof SharedArrayBuffer !== 'undefined'
const opfsAvailable = !!(
  typeof navigator !== 'undefined' &&
  navigator.storage &&
  typeof navigator.storage.getDirectory === 'function'
)
const threadCount = sabAvailable
  ? Math.min(navigator.hardwareConcurrency ?? 4, 16)
  : 1
const browserConcurrency = navigator.hardwareConcurrency ?? 1

onBeforeUnmount(() => {
  document.removeEventListener('click', onClickOutsideMemory)
})

const dbMemoryPct = computed(() => {
  if (!conn.dbMemory || !conn.memory) return 0
  return Math.round((conn.dbMemory / conn.memory) * 100)
})

const bytesPerEntity = computed(() => {
  const total = conn.nodeCount + conn.edgeCount
  if (!total || !conn.dbMemory) return ''
  const bpe = conn.dbMemory / total
  return bpe < 1024 ? `${bpe.toFixed(0)} B` : `${(bpe / 1024).toFixed(1)} KB`
})

// Logout - only shown when auth is enabled and user has a stored key
const showLogout = computed(() => !!getApiKey() && !conn.authRequired)

function logout() {
  clearApiKey()
  conn.authRequired = true
}
</script>

<template>
  <header class="header">
    <div class="header__left">
      <!-- Brand -->
      <span class="header__brand">Traverse Studio</span>
      <span class="header__sep" />

      <!-- Connection + Database (merged) -->
      <div v-if="conn.isConnected" class="header__db">
        <button ref="dbBtnRef" class="header__db-btn" @click="toggleDbMenu">
          <span class="fg-dot fg-dot--green" />
          <span class="header__db-name">{{ conn.currentDatabase }}</span>
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="m6 9 6 6 6-6" />
          </svg>
        </button>

        <!-- Database dropdown menu - teleported to body so the
             header's overflow-x: auto (mobile) doesn't clip it. -->
        <Teleport to="body">
        <div
          v-if="showDbMenu"
          class="header__db-menu"
          :style="{ top: dbMenuPos.top + 'px', left: dbMenuPos.left + 'px' }">
          <div class="header__db-menu-header">Databases</div>

          <button
            v-for="item in dbMenuItems"
            :key="item.name"
            class="header__db-menu-item"
            :class="{ 'header__db-menu-item--active': item.name === conn.currentDatabase }"
            @click="selectDatabase(item.name)"
          >
            <span class="header__db-menu-name">
              {{ item.name }}
              <span v-if="item.isAlias" class="header__db-menu-badge header__db-menu-badge--alias">alias</span>
              <span v-if="item.db?.default" class="header__db-menu-badge">default</span>
              <span v-if="item.db?.readOnly" class="header__db-menu-badge header__db-menu-badge--readonly">read-only</span>
            </span>
            <span class="header__db-menu-info">
              <template v-if="item.isAlias">&rarr; {{ item.target }}</template>
              <template v-else>{{ item.db!.nodes.toLocaleString() }} nodes, {{ item.db!.edges.toLocaleString() }} edges<template v-if="item.db!.memory_bytes">, {{ formatBytes(item.db!.memory_bytes) }}</template></template>
            </span>
          </button>

          <div class="header__db-menu-divider" />

          <!-- Manage aliases -->
          <button class="header__db-menu-action" @click="showAliasPanel = true; showDbMenu = false">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
              <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
            </svg>
            Manage aliases
          </button>

          <!-- Create new -->
          <button v-if="!showCreateDb" class="header__db-menu-action" @click.stop="showCreateDb = true">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M5 12h14" /><path d="M12 5v14" />
            </svg>
            New database
          </button>
          <div v-else class="header__db-menu-create" @click.stop>
            <input
              v-model="newDbName"
              class="fg-input"
              placeholder="Database name..."
              maxlength="50"
              @keydown.enter="createDatabase"
              @keydown.escape="showCreateDb = false"
            />
            <button class="fg-btn fg-btn--primary fg-btn--sm" @click="createDatabase">Create</button>
          </div>

          <div v-if="dbError" class="header__db-menu-error">{{ dbError }}</div>
        </div>
        </Teleport>
      </div>
      <span v-else class="header__item header__disconnected">
        <span class="fg-dot fg-dot--red" />
        Disconnected
      </span>
    </div>

    <div class="header__right">
      <!-- Node/edge counts -->
      <span v-if="conn.isConnected" class="header__item header__item--muted">
        {{ nodesDisplay }} nodes, {{ edgesDisplay }} edges<template v-if="schema.indexes.length">, {{ schema.indexes.length }} {{ schema.indexes.length === 1 ? 'index' : 'indexes' }}</template>
      </span>

      <!-- OPFS usage chip - WASM mode only. Visible reassurance that
           the user isn't about to hit a browser-storage cap, plus a
           clear "your data lives here" affordance. Color (amber at
           >=80% used) is the entire affordance - no native tooltip;
           Studio uses click-popovers elsewhere. -->
      <span
        v-if="opfsDisplay"
        class="header__item header__item--muted header__opfs"
        :class="{ 'header__opfs--warn': opfsPct >= 80 }"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <ellipse cx="12" cy="5" rx="9" ry="3" /><path d="M3 5v14a9 3 0 0 0 18 0V5" /><path d="M3 12a9 3 0 0 0 18 0" />
        </svg>
        {{ opfsDisplay }}
      </span>

      <!-- Memory: database / total. Click opens the .memtip popover
           below with per-DB detail; warn/danger color is the at-a-
           glance signal. The danger banner above the header carries
           the textual explanation when usage is critical. -->
      <span
        v-if="dbMemoryDisplay || totalMemoryDisplay"
        ref="memoryRef"
        class="header__item header__item--muted header__memory"
        :class="{
          'header__memory--warn': conn.wasmMemoryWarn && !conn.wasmMemoryDanger,
          'header__memory--danger': conn.wasmMemoryDanger,
        }"
        @click.stop="toggleMemoryPopover"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M6 19v2M10 19v2M14 19v2M18 19v2" /><rect x="4" y="6" width="16" height="13" rx="2" /><path d="M4 10h16" />
        </svg>
        <template v-if="dbMemoryDisplay && totalMemoryDisplay">{{ dbMemoryDisplay }} / {{ totalMemoryDisplay }}</template>
        <template v-else>{{ totalMemoryDisplay }}</template>
      </span>

      <Teleport to="body">
        <Transition name="memtip">
          <div
            v-if="showMemoryTip"
            class="memtip"
            :style="{ top: memoryTipPos.top + 'px', right: memoryTipPos.right + 'px' }"
            @click.stop
          >
            <div class="memtip__title">Memory</div>

            <template v-for="db in conn.databases" :key="db.name">
              <div class="memtip__row">
                <span class="memtip__label" :class="{ 'memtip__label--current': db.name === conn.currentDatabase }">{{ db.name }}</span>
                <span class="memtip__value">{{ db.memory_bytes ? formatBytes(db.memory_bytes) : '-' }}</span>
              </div>
            </template>
            <div v-if="conn.databases.length > 1 && totalMemoryDisplay" class="memtip__divider" />
            <div v-if="totalMemoryDisplay" class="memtip__row">
              <span class="memtip__label">Server total</span>
              <span class="memtip__value memtip__value--total">{{ totalMemoryDisplay }}</span>
            </div>

            <div v-if="bytesPerEntity" class="memtip__divider" />
            <div v-if="bytesPerEntity" class="memtip__row">
              <span class="memtip__label">Per entity</span>
              <span class="memtip__value">{{ bytesPerEntity }}</span>
            </div>
            <div v-if="bytesPerEntity" class="memtip__hint">
              {{ nodesDisplay }} nodes + {{ edgesDisplay }} edges
            </div>

            <!-- WASM-mode memory warning. The chip's amber/red color
                 is the at-a-glance signal; this is the explanation
                 once the popover is open. -->
            <template v-if="conn.wasmMemoryWarn || conn.wasmMemoryDanger">
              <div class="memtip__divider" />
              <div
                class="memtip__hint"
                :class="{ 'memtip__hint--danger': conn.wasmMemoryDanger }"
              >
                <template v-if="conn.wasmMemoryDanger">
                  Approaching the ~4 GB browser WASM memory ceiling.
                  Save your work (Schema &rarr; Save / Download .tvdb)
                  before the tab crashes.
                </template>
                <template v-else>
                  Memory usage is high. Browsers cap WASM linear
                  memory at ~4 GB; commit + close other tabs to
                  reduce pressure.
                </template>
              </div>
            </template>
          </div>
        </Transition>
      </Teleport>

      <!-- Last query time -->
      <span v-if="lastQueryTime" class="header__item header__item--muted">
        {{ lastQueryTime }}
      </span>

      <!-- WASM-mode chip - small badge that makes the in-browser
           engine unambiguous. Click opens an "about this WASM"
           popover with bundle/thread/feature info, mirroring DuckDB-
           Wasm's `.features` shell command. -->
      <span
        v-if="conn.isWasmMode"
        ref="wasmBadgeRef"
        class="header__wasm-badge"
        @click.stop="toggleWasmPopover"
      >WASM</span>

      <Teleport to="body">
        <Transition name="memtip">
          <div
            v-if="showWasmTip"
            class="memtip wasmtip"
            :style="{ top: wasmTipPos.top + 'px', right: wasmTipPos.right + 'px' }"
            @click.stop
          >
            <div class="memtip__title">About this WASM</div>

            <div class="memtip__row">
              <span class="memtip__label">Bundle</span>
              <span class="memtip__value">threaded</span>
            </div>
            <div class="memtip__row">
              <span class="memtip__label">Engine version</span>
              <span class="memtip__value">v{{ conn.version || '-' }}</span>
            </div>
            <div class="memtip__row">
              <span class="memtip__label">Threads</span>
              <span class="memtip__value">
                {{ threadCount }}<span class="memtip__hint--inline"> / {{ browserConcurrency }} cores</span>
              </span>
            </div>

            <div class="memtip__divider" />

            <div class="memtip__row">
              <span class="memtip__label">SharedArrayBuffer</span>
              <span class="memtip__value" :class="{ 'memtip__value--ok': sabAvailable }">
                {{ sabAvailable ? 'available' : 'blocked' }}
              </span>
            </div>
            <div class="memtip__row">
              <span class="memtip__label">OPFS</span>
              <span class="memtip__value" :class="{ 'memtip__value--ok': opfsAvailable }">
                {{ opfsAvailable ? 'available' : 'unavailable' }}
              </span>
            </div>

            <template v-if="opfsAvailable && conn.opfsUsage != null && conn.opfsQuota != null">
              <div class="memtip__divider" />
              <div class="memtip__row">
                <span class="memtip__label">OPFS storage</span>
                <span class="memtip__value">
                  {{ formatBytes(conn.opfsUsage) }} / {{ formatBytes(conn.opfsQuota) }}
                </span>
              </div>
            </template>

            <div class="memtip__divider" />
            <div class="memtip__hint">
              Engine runs entirely in this browser tab. Data lives in OPFS;
              nothing is shared with teammates.
            </div>
          </div>
        </Transition>
      </Teleport>

      <!-- Version -->
      <span
        v-if="conn.version"
        class="header__version header__item header__item--muted"
      >
        v{{ conn.version }}
      </span>

      <AliasPanel v-if="showAliasPanel" @close="showAliasPanel = false" />

      <!-- Theme toggle -->
      <button class="fg-icon-btn" :title="`Switch to ${theme === 'dark' ? 'light' : 'dark'} theme`" @click="toggleTheme">
        <svg v-if="theme === 'dark'" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="12" r="4" /><path d="M12 2v2" /><path d="M12 20v2" /><path d="m4.93 4.93 1.41 1.41" /><path d="m17.66 17.66 1.41 1.41" /><path d="M2 12h2" /><path d="M20 12h2" /><path d="m6.34 17.66-1.41 1.41" /><path d="m19.07 4.93-1.41 1.41" />
        </svg>
        <svg v-else width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z" />
        </svg>
      </button>

      <!-- Logout -->
      <button v-if="showLogout" class="fg-icon-btn" title="Logout" @click="logout">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><polyline points="16 17 21 12 16 7" /><line x1="21" y1="12" x2="9" y2="12" />
        </svg>
      </button>
    </div>
  </header>
</template>

<style scoped>
.header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  height: var(--tv-header-height);
  width: 100%;
  padding: 0 12px;
  background: var(--tv-bg-surface);
  border-bottom: 1px solid var(--tv-border-default);
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  flex-shrink: 0;
}

.header__left,
.header__right {
  display: flex;
  align-items: center;
  gap: 10px;
}

.header__item {
  display: flex;
  align-items: center;
  gap: 4px;
  white-space: nowrap;
}

.header__brand {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-secondary);
  white-space: nowrap;
  letter-spacing: -0.01em;
}

.header__disconnected {
  color: var(--tv-status-error);
}

.header__item--muted {
  color: var(--tv-text-muted);
  font-family: var(--tv-font-mono);
}

.header__memory {
  cursor: pointer;
  padding: 2px 6px;
  border-radius: var(--tv-radius-sm);
  transition: background 0.15s;
}
.header__memory:hover {
  background: var(--tv-bg-hover);
}

.header__sep {
  width: 1px;
  height: 16px;
  background: var(--tv-border-default);
}

/* Database selector button */
.header__db {
  position: relative;
}

.header__db-btn {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 2px 6px;
  height: 26px;
  border: 1px solid transparent;
  border-radius: var(--tv-radius-sm);
  background: none;
  color: var(--tv-text-primary);
  font-size: var(--tv-font-size-xs);
  font-family: var(--tv-font-sans);
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s;
}
.header__db-btn:hover {
  background: var(--tv-bg-hover);
  border-color: var(--tv-border-default);
}

.header__db-name {
  font-weight: 500;
  max-width: 120px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* Database dropdown menu */
.header__db-menu {
  /* Teleported to body - coordinates set via inline style.
     Fixed width so the menu doesn't reflow when its contents swap
     between the database list and the "New database" form. */
  position: fixed;
  z-index: 200;
  width: min(300px, calc(100vw - 16px));
  max-height: calc(100dvh - 80px);
  overflow-y: auto;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  box-shadow: var(--tv-shadow-lg);
}

.header__db-menu-header {
  padding: 8px 12px 4px;
  font-size: var(--tv-font-size-xs);
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
}

.header__db-menu-item {
  display: flex;
  flex-direction: column;
  gap: 1px;
  padding: 6px 12px;
  cursor: pointer;
  transition: background 0.1s;
  width: 100%;
  border: none;
  background: none;
  text-align: left;
  font-family: inherit;
}
.header__db-menu-item:hover {
  background: var(--tv-bg-hover);
}
.header__db-menu-item--active {
  background: var(--tv-bg-hover);
  border-left: 2px solid var(--tv-accent);
}

.header__db-menu-name {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: var(--tv-font-size-sm);
  font-weight: 500;
  color: var(--tv-text-primary);
}

.header__db-menu-badge {
  font-size: 11px;
  font-weight: 500;
  padding: 1px 6px;
  border-radius: 3px;
  background: var(--tv-bg-elevated);
  color: var(--tv-text-muted);
  line-height: 1.5;
}

.header__db-menu-badge--readonly {
  background: var(--tv-status-warning-subtle);
  color: var(--tv-status-warning);
}

.header__db-menu-info {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  white-space: nowrap;
}

.header__db-menu-badge--alias {
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
}

.header__db-menu-divider {
  height: 1px;
  margin: 4px 0;
  background: var(--tv-border-default);
}

.header__db-menu-action {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 12px;
  width: 100%;
  border: none;
  background: none;
  font-family: inherit;
  font-size: var(--tv-font-size-sm);
  color: var(--tv-text-secondary);
  cursor: pointer;
  text-align: left;
  transition: background 0.1s;
}
.header__db-menu-action:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}

.header__db-menu-create {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 12px;
}
.header__db-menu-create .fg-input {
  flex: 1;
  min-width: 0;
  height: 28px;
  font-size: var(--tv-font-size-xs);
}

.header__db-menu-error {
  padding: 4px 12px 8px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-status-error);
}
</style>

<style>
/* Memory tooltip (teleported to body, cannot be scoped) */
.memtip {
  position: fixed;
  z-index: 10000;
  width: 260px;
  padding: 10px 12px;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-lg);
  box-shadow: var(--tv-shadow-lg);
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
}

.memtip__title {
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-xs);
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
  margin-bottom: 8px;
}

.memtip__row {
  display: flex;
  justify-content: space-between;
  align-items: baseline;
  padding: 2px 0;
}

.memtip__label {
  font-family: var(--tv-font-sans);
  color: var(--tv-text-secondary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.memtip__label--current {
  font-weight: 500;
  color: var(--tv-text-primary);
}

.memtip__value--total {
  font-weight: 600;
}

.memtip__value {
  font-family: var(--tv-font-mono);
  font-weight: 500;
  color: var(--tv-text-primary);
}

.memtip__bar {
  height: 4px;
  margin: 6px 0;
  background: var(--tv-bg-base);
  border-radius: 2px;
  overflow: hidden;
}

.memtip__bar-fill {
  height: 100%;
  min-width: 6px;
  background: var(--tv-accent);
  border-radius: 2px;
  transition: width 0.3s ease;
}

.memtip__divider {
  height: 1px;
  margin: 8px 0;
  background: var(--tv-border-default);
}

.memtip__hint {
  font-family: var(--tv-font-sans);
  font-size: 11px;
  color: var(--tv-text-muted);
  padding-top: 2px;
}
.memtip__hint--danger {
  color: var(--tv-text-danger, #b91c1c);
  font-weight: 500;
}

/* Transition */
.memtip-enter-active,
.memtip-leave-active {
  transition: opacity 0.15s ease;
}
.memtip-enter-from,
.memtip-leave-to {
  opacity: 0;
}

/* WASM-mode badge */
.header__wasm-badge {
  display: inline-flex;
  align-items: center;
  padding: 2px 6px;
  font-size: 10px;
  font-weight: 600;
  letter-spacing: 0.04em;
  border-radius: var(--tv-radius-sm);
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
  border: 1px solid var(--tv-accent);
  font-family: var(--tv-font-mono);
  cursor: pointer;
  transition: background 0.15s;
}
.header__wasm-badge:hover {
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
}

/* WASM-info popover - additional value tints not used by the
   regular .memtip. `--ok` highlights the green checks for SAB /
   OPFS available; the inline hint pairs the thread count with a
   muted "/ N cores" suffix on the same line. */
.memtip__value--ok {
  color: var(--tv-text-success, #16a34a);
}
.memtip__hint--inline {
  font-size: 10px;
  color: var(--tv-text-muted);
  margin-left: 4px;
}

/* OPFS usage chip - turns amber when >=80% of quota used. */
.header__opfs {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}
.header__opfs--warn {
  color: var(--tv-text-warning, #b45309) !important;
}

/* Memory chip - gradient amber -> red as we approach the 4 GB
   wasm32 ceiling. Click still opens the memtip with per-DB detail. */
.header__memory--warn {
  color: var(--tv-text-warning, #b45309) !important;
}
.header__memory--danger {
  color: var(--tv-text-danger, #b91c1c) !important;
  font-weight: 600;
}

/* Version chip */
.header__version {
  display: flex;
  align-items: center;
  gap: 4px;
  white-space: nowrap;
  cursor: default;
}

/* ---------- Mobile ---------- */
@media (max-width: 768px) {
  /* Allow horizontal scroll so all chips remain reachable rather than
     overflowing / wrapping. The two flex groups stay side-by-side at
     their natural widths. Scrollbar hidden - touch users swipe. */
  .header {
    overflow-x: auto;
    overflow-y: hidden;
    -webkit-overflow-scrolling: touch;
    scrollbar-width: none;
    padding: 0 8px;
    gap: 8px;
    /* `space-between` becomes pointless once total width exceeds the
       container - switch to start so the left group is always visible. */
    justify-content: flex-start;
  }
  .header::-webkit-scrollbar {
    display: none;
  }
  .header__left,
  .header__right {
    flex-shrink: 0;
    gap: 6px;
  }

  /* Drop chips that aren't useful on a phone (informational only) to
     keep the header from being a wall of horizontal scroll. */
  .header__memory,
  .header__opfs,
  .header__version {
    display: none;
  }
}
</style>
