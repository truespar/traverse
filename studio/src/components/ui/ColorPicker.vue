<script setup lang="ts">
import { ref, watch, nextTick, onMounted, onUnmounted } from 'vue'

const props = defineProps<{ modelValue: string }>()
const emit = defineEmits<{ 'update:modelValue': [color: string] }>()

const SWATCHES = [
  // Blues & teals
  '#0369A1', '#2563EB', '#38BDF8', '#06B6D4', '#0EA5E9', '#3B82F6', '#6366F1', '#818CF8',
  // Warm
  '#DC2626', '#EF4444', '#F97316', '#F59E0B', '#EAB308', '#D97706', '#B91C1C', '#C2410C',
  // Purple & pink
  '#7C3AED', '#8B5CF6', '#A78BFA', '#C084FC', '#D946EF', '#EC4899', '#F472B6', '#9333EA',
  // Green & gray
  '#059669', '#10B981', '#22C55E', '#84CC16', '#64748B', '#475569', '#94A3B8', '#CBD5E1',
]

const open = ref(false)
const hexInput = ref(props.modelValue || '#38BDF8')
const popoverRef = ref<HTMLElement | null>(null)
const triggerRef = ref<HTMLElement | null>(null)
const popoverStyle = ref<Record<string, string>>({})

watch(() => props.modelValue, (v) => { hexInput.value = v })

async function toggle() {
  open.value = !open.value
  if (open.value) {
    await nextTick()
    positionPopover()
  }
}

function positionPopover() {
  if (!triggerRef.value) return
  const rect = triggerRef.value.getBoundingClientRect()
  const popW = 224
  const popH = 180 // approximate
  // Try below; if it would overflow the viewport, place above
  let top = rect.bottom + 6
  if (top + popH > window.innerHeight) {
    top = rect.top - popH - 6
  }
  // Align left, but clamp to viewport
  let left = rect.left
  if (left + popW > window.innerWidth - 8) {
    left = window.innerWidth - popW - 8
  }
  popoverStyle.value = {
    top: `${top}px`,
    left: `${left}px`,
  }
}

function pickColor(color: string) {
  emit('update:modelValue', color)
  open.value = false
}

function onHexCommit() {
  const v = hexInput.value.trim()
  if (/^#[0-9a-fA-F]{6}$/.test(v)) {
    emit('update:modelValue', v)
    open.value = false
  }
}

function onClickOutside(e: MouseEvent) {
  if (!open.value) return
  const target = e.target as Node
  if (popoverRef.value?.contains(target) || triggerRef.value?.contains(target)) return
  open.value = false
}

onMounted(() => document.addEventListener('mousedown', onClickOutside))
onUnmounted(() => document.removeEventListener('mousedown', onClickOutside))
</script>

<template>
  <div class="color-picker" ref="triggerRef">
    <button
      class="color-picker__trigger"
      :style="{ background: modelValue }"
      @click="toggle"
    />
    <Teleport to="body">
      <div v-if="open" ref="popoverRef" class="color-picker__popover" :style="popoverStyle">
        <div class="color-picker__grid">
          <button
            v-for="c in SWATCHES"
            :key="c"
            class="color-picker__swatch"
            :class="{ 'color-picker__swatch--active': modelValue === c }"
            :style="{ background: c }"
            @click="pickColor(c)"
          />
        </div>
        <div class="color-picker__hex-row">
          <span class="color-picker__hex-preview" :style="{ background: hexInput }" />
          <input
            v-model="hexInput"
            class="color-picker__hex-input"
            maxlength="7"
            spellcheck="false"
            @keydown.enter="onHexCommit"
          />
        </div>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
.color-picker {
  position: relative;
  display: flex;
  align-items: center;
}

.color-picker__trigger {
  width: 16px;
  height: 16px;
  border-radius: 3px;
  border: 2px solid var(--tv-border-default);
  cursor: pointer;
  padding: 0;
  flex-shrink: 0;
}
.color-picker__trigger:hover {
  border-color: var(--tv-text-muted);
}
</style>

<!-- Teleported to body - cannot be scoped -->
<style>
.color-picker__popover {
  position: fixed;
  z-index: 10000;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  box-shadow: var(--tv-shadow-xl);
  padding: 10px;
  width: 224px;
}

.color-picker__grid {
  display: grid;
  grid-template-columns: repeat(8, 22px);
  justify-content: space-between;
  gap: 4px;
}

.color-picker__swatch {
  width: 22px;
  height: 22px;
  box-sizing: border-box;
  border-radius: 4px;
  border: 2px solid transparent;
  cursor: pointer;
  padding: 0;
  transition: border-color 0.1s, transform 0.1s;
}
.color-picker__swatch:hover {
  transform: scale(1.15);
  border-color: var(--tv-text-muted);
}
.color-picker__swatch--active {
  border-color: var(--tv-text-primary);
}

.color-picker__hex-row {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 8px;
  padding-top: 8px;
  border-top: 1px solid var(--tv-border-default);
}

.color-picker__hex-preview {
  width: 18px;
  height: 18px;
  border-radius: 50%;
  flex-shrink: 0;
  border: 1px solid var(--tv-border-default);
}

.color-picker__hex-input {
  flex: 1;
  min-width: 0;
  font-size: var(--tv-font-size-xs);
  font-family: var(--tv-font-mono);
  padding: 3px 6px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-base);
  color: var(--tv-text-primary);
  outline: none;
}
.color-picker__hex-input:focus {
  border-color: var(--tv-accent);
}
</style>
