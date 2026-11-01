<script setup lang="ts">
// Slim header banner shown when Studio is running against the WASM
// backend. Clarifies the data-locality model up front - users
// otherwise have no way of knowing their imports won't be visible to
// teammates or persist beyond the browser tab's OPFS.
//
// Reads the active backend's capabilities flag (set on `api` at boot
// by `setApiClient`) - no reactive store dependency, since the
// backend swap happens once before any component mounts.
import { ref, onMounted, onBeforeUnmount } from 'vue'
import { api } from '../../utils/api'
import { useConnectionStore } from '../../stores/connection'
import { useViewport } from '../../composables/useViewport'

const conn = useConnectionStore()
const { isMobile } = useViewport()
const dismissed = ref(localStorage.getItem('tv-wasm-banner-dismissed') === '1')

/** Heuristic for "are we in WASM mode": serverFiles is the cleanest
 *  signal because it diverges between backends - the HTTP client
 *  always has server-side files, the WASM client never does. */
const isWasmMode = !api.capabilities.serverFiles

function dismiss() {
  localStorage.setItem('tv-wasm-banner-dismissed', '1')
  dismissed.value = true
}

// On mobile, vertical space is tight - auto-dismiss the welcome banner
// after a short read window and persist the dismissal so it doesn't
// return on the next visit. Desktop keeps the explicit-dismiss model
// because there's room for the banner to sit.
let autoDismissTimer: ReturnType<typeof setTimeout> | null = null
onMounted(() => {
  if (isMobile.value && isWasmMode && !dismissed.value) {
    autoDismissTimer = setTimeout(dismiss, 9000)
  }
})
onBeforeUnmount(() => {
  if (autoDismissTimer) clearTimeout(autoDismissTimer)
})
</script>

<template>
  <!-- Memory-pressure banner takes priority over the welcome notice.
       Cannot be dismissed - the user really needs to know the tab is
       about to die so they can commit / export before it does. -->
  <div
    v-if="isWasmMode && conn.wasmMemoryDanger"
    class="wasm-banner wasm-banner--danger"
    role="alert"
    aria-live="assertive"
  >
    <div class="wasm-banner__icon" aria-hidden="true">
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z" />
        <path d="M12 9v4" /><path d="M12 17h.01" />
      </svg>
    </div>
    <div class="wasm-banner__text">
      <strong>Memory limit approaching.</strong>
      Browsers cap WebAssembly at ~4 GB; the current graph is using over 2.5 GB. Save your work
      (Schema &rarr; Save / Download .tvdb) before the tab crashes.
    </div>
  </div>
  <div
    v-else-if="isWasmMode && !dismissed"
    class="wasm-banner"
    role="status"
    aria-live="polite"
  >
    <div class="wasm-banner__icon" aria-hidden="true">
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="12" cy="12" r="10" />
        <line x1="12" y1="8" x2="12" y2="12" />
        <line x1="12" y1="16" x2="12.01" y2="16" />
      </svg>
    </div>
    <div class="wasm-banner__text">
      <strong>WASM preview.</strong>
      Traverse is running entirely in this browser tab - your data lives in OPFS and is not shared.
      <a href="https://truespar.com/traverse/docs/v1/getting-started" target="_blank" rel="noopener">Run the Traverse server</a>
      for multi-user access, server-side persistence and more.
    </div>
    <button class="wasm-banner__close" title="Dismiss" @click="dismiss" aria-label="Dismiss WASM mode notice">
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <line x1="18" y1="6" x2="6" y2="18" />
        <line x1="6" y1="6" x2="18" y2="18" />
      </svg>
    </button>
  </div>
</template>

<style scoped>
.wasm-banner {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 14px;
  background: var(--tv-accent-subtle);
  color: var(--tv-text-primary);
  border-bottom: 1px solid var(--tv-border-default);
  font-size: 13px;
  line-height: 1.4;
}

/* Danger variant - memory-pressure warning. Non-dismissable, red. */
.wasm-banner--danger {
  background: var(--tv-bg-danger-subtle, #fef2f2);
  border-bottom-color: var(--tv-text-danger, #b91c1c);
  color: var(--tv-text-danger, #b91c1c);
}
.wasm-banner--danger .wasm-banner__icon {
  color: var(--tv-text-danger, #b91c1c);
}

.wasm-banner__icon {
  flex-shrink: 0;
  color: var(--tv-accent);
  display: flex;
  align-items: center;
}

.wasm-banner__text {
  flex: 1;
}
.wasm-banner__text strong {
  font-weight: 600;
  margin-right: 4px;
}
.wasm-banner__text a {
  color: var(--tv-accent);
  text-decoration: underline;
}

.wasm-banner__close {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 26px;
  height: 26px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: transparent;
  color: var(--tv-text-muted);
  cursor: pointer;
  transition: background 0.15s;
}
.wasm-banner__close:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}
</style>
