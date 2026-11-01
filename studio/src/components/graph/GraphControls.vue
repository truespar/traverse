<script setup lang="ts">
import { ref, onBeforeUnmount } from 'vue'
import { LAYOUT_TYPES } from '../../composables/useGraphExplorer'

defineProps<{
  layout: string
  isAnimating: boolean
  gdsOpen?: boolean
}>()

const emit = defineEmits<{
  'zoom-in': []
  'zoom-out': []
  'zoom-fit': []
  relayout: []
  'switch-layout': [layout: string]
  export: []
  'toggle-gds': []
}>()

const tooltipKey = ref<string | null>(null)
const tooltipPos = ref({ top: 0, left: 0 })
let hoverTimer: ReturnType<typeof setTimeout> | null = null
let autoDismissTimer: ReturnType<typeof setTimeout> | null = null

// On touch devices, `mouseleave` never fires after a tap, so the
// tooltip would stick forever. Dismiss when the next pointerdown
// happens outside this control + on a short timer.
function onDocPointerDown(e: PointerEvent) {
  const target = e.target as HTMLElement
  if (!target.closest('.graph-controls__layout-btn') && !target.closest('.gc-tip')) {
    closeTooltip()
  }
}

function armAutoDismiss() {
  if (autoDismissTimer) clearTimeout(autoDismissTimer)
  autoDismissTimer = setTimeout(closeTooltip, 4500)
}

function closeTooltip() {
  if (hoverTimer) { clearTimeout(hoverTimer); hoverTimer = null }
  if (autoDismissTimer) { clearTimeout(autoDismissTimer); autoDismissTimer = null }
  tooltipKey.value = null
  document.removeEventListener('pointerdown', onDocPointerDown)
}

function onBtnEnter(key: string, event: MouseEvent) {
  if (hoverTimer) clearTimeout(hoverTimer)
  const el = event.currentTarget as HTMLElement
  hoverTimer = setTimeout(() => {
    const rect = el.getBoundingClientRect()
    const tipWidth = 220
    const centerX = rect.left + rect.width / 2
    const left = Math.min(centerX, window.innerWidth - tipWidth / 2 - 8)
    tooltipPos.value = {
      top: rect.bottom + 8,
      left,
    }
    tooltipKey.value = key
    document.addEventListener('pointerdown', onDocPointerDown)
    armAutoDismiss()
  }, 400)
}

function onBtnLeave() {
  closeTooltip()
}

onBeforeUnmount(closeTooltip)
</script>

<template>
  <div class="graph-controls">
    <button class="fg-icon-btn" title="Zoom in" @click="emit('zoom-in')">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /><line x1="11" y1="8" x2="11" y2="14" /><line x1="8" y1="11" x2="14" y2="11" />
      </svg>
    </button>
    <button class="fg-icon-btn" title="Zoom out" @click="emit('zoom-out')">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" /><line x1="8" y1="11" x2="14" y2="11" />
      </svg>
    </button>
    <button class="fg-icon-btn" title="Fit to screen" @click="emit('zoom-fit')">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M8 3H5a2 2 0 0 0-2 2v3" /><path d="M21 8V5a2 2 0 0 0-2-2h-3" /><path d="M3 16v3a2 2 0 0 0 2 2h3" /><path d="M16 21h3a2 2 0 0 0 2-2v-3" />
      </svg>
    </button>
    <div class="graph-controls__sep" />
    <button class="fg-icon-btn" :disabled="isAnimating" title="Re-layout" @click="emit('relayout')">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" /><path d="M21 3v5h-5" /><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" /><path d="M8 16H3v5" />
      </svg>
    </button>
    <div class="graph-controls__layout-group">
      <button
        class="graph-controls__layout-btn"
        :class="{ 'graph-controls__layout-btn--active': layout === LAYOUT_TYPES.FORCE_ATLAS_2 }"
        @click="emit('switch-layout', LAYOUT_TYPES.FORCE_ATLAS_2)"
        @mouseenter="onBtnEnter('fa2', $event)"
        @mouseleave="onBtnLeave"
      >FA2</button>
      <button
        class="graph-controls__layout-btn"
        :class="{ 'graph-controls__layout-btn--active': layout === LAYOUT_TYPES.D3_FORCE }"
        @click="emit('switch-layout', LAYOUT_TYPES.D3_FORCE)"
        @mouseenter="onBtnEnter('d3', $event)"
        @mouseleave="onBtnLeave"
      >D3</button>
    </div>
    <div class="graph-controls__sep" />
    <button class="fg-icon-btn" title="Export PNG" @click="emit('export')">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M12 15V3" /><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /><path d="m7 10 5 5 5-5" />
      </svg>
    </button>
    <div class="graph-controls__sep" />
    <button
      class="fg-icon-btn"
      :class="{ 'fg-icon-btn--active': gdsOpen }"
      title="Graph Data Science"
      @click="emit('toggle-gds')"
    >
      <!-- Concentric circles - alludes to centrality / community structure. -->
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="12" cy="12" r="3" />
        <path d="M12 3v3" /><path d="M12 18v3" /><path d="M3 12h3" /><path d="M18 12h3" />
        <path d="m5.6 5.6 2.1 2.1" /><path d="m16.3 16.3 2.1 2.1" />
        <path d="m5.6 18.4 2.1-2.1" /><path d="m16.3 7.7 2.1-2.1" />
      </svg>
    </button>

    <Teleport to="body">
      <Transition name="gc-tip">
        <div
          v-if="tooltipKey"
          class="gc-tip"
          :style="{ top: tooltipPos.top + 'px', left: tooltipPos.left + 'px' }"
          @mouseenter="tooltipKey = tooltipKey"
          @mouseleave="onBtnLeave"
        >
          <template v-if="tooltipKey === 'fa2'">
            <div class="gc-tip__title">ForceAtlas2</div>
            <div class="gc-tip__desc">Continuous force-directed layout optimized for community structure. Nodes repel, edges attract, gravity pulls toward center. Best for exploring clusters and dense graphs.</div>
          </template>
          <template v-else-if="tooltipKey === 'd3'">
            <div class="gc-tip__title">D3 Force</div>
            <div class="gc-tip__desc">Velocity-based physics simulation with charge repulsion and link springs. Produces evenly spaced layouts. Best for smaller graphs and tree-like structures.</div>
          </template>
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<style scoped>
.graph-controls {
  position: absolute;
  top: 8px;
  right: 8px;
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 4px;
  background: var(--tv-bg-surface);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-lg);
  box-shadow: var(--tv-shadow-sm);
}

.graph-controls__sep {
  width: 1px;
  height: 20px;
  background: var(--tv-border-default);
  margin: 0 2px;
}

.graph-controls__layout-group {
  display: flex;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  overflow: hidden;
}

.graph-controls__layout-btn {
  padding: 3px 8px;
  border: none;
  background: none;
  color: var(--tv-text-muted);
  font-size: 11px;
  font-family: var(--tv-font-sans);
  font-weight: 500;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.graph-controls__layout-btn:not(:last-child) {
  border-right: 1px solid var(--tv-border-default);
}
.graph-controls__layout-btn:hover {
  color: var(--tv-text-primary);
  background: var(--tv-bg-hover);
}
.graph-controls__layout-btn--active {
  color: var(--tv-text-inverse);
  background: var(--tv-accent);
}
.graph-controls__layout-btn--active:hover {
  background: var(--tv-accent-hover);
}

</style>

<style>
/* Layout tooltip (teleported to body, cannot be scoped) */
.gc-tip {
  position: fixed;
  z-index: 10000;
  width: 220px;
  padding: 8px 10px;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  box-shadow: var(--tv-shadow-lg);
  transform: translateX(-50%);
  pointer-events: none;
}

.gc-tip__title {
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-xs);
  font-weight: 600;
  color: var(--tv-text-primary);
  margin-bottom: 4px;
}

.gc-tip__desc {
  font-family: var(--tv-font-sans);
  font-size: 11px;
  line-height: 1.45;
  color: var(--tv-text-muted);
}

.gc-tip-enter-active,
.gc-tip-leave-active {
  transition: opacity 0.15s ease;
}
.gc-tip-enter-from,
.gc-tip-leave-to {
  opacity: 0;
}
</style>
