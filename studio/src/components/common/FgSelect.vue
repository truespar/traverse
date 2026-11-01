<script setup lang="ts">
import { ref, computed, watch, nextTick, onBeforeUnmount } from 'vue'

const props = defineProps<{
  modelValue: string
  options: { value: string; label: string }[]
  placeholder?: string
  disabled?: boolean
  small?: boolean
}>()

const emit = defineEmits<{
  'update:modelValue': [value: string]
}>()

const open = ref(false)
const triggerRef = ref<HTMLElement | null>(null)
const dropdownRef = ref<HTMLElement | null>(null)
const dropdownStyle = ref<Record<string, string>>({})

const selectedLabel = computed(() => {
  const opt = props.options.find(o => o.value === props.modelValue)
  return opt?.label || props.placeholder || ''
})

const isPlaceholder = computed(() => {
  return !props.options.some(o => o.value === props.modelValue)
})

function positionDropdown() {
  if (!triggerRef.value) return
  const rect = triggerRef.value.getBoundingClientRect()
  const dropHeight = Math.min(200, props.options.length * 28 + 8)
  const spaceBelow = window.innerHeight - rect.bottom - 8
  const openAbove = spaceBelow < dropHeight && rect.top > spaceBelow

  if (openAbove) {
    dropdownStyle.value = {
      position: 'fixed',
      left: `${rect.left}px`,
      bottom: `${window.innerHeight - rect.top + 4}px`,
      width: `${Math.max(rect.width, 100)}px`,
      maxHeight: `${Math.min(200, rect.top - 8)}px`,
    }
  } else {
    dropdownStyle.value = {
      position: 'fixed',
      left: `${rect.left}px`,
      top: `${rect.bottom + 4}px`,
      width: `${Math.max(rect.width, 100)}px`,
      maxHeight: `${Math.min(200, spaceBelow)}px`,
    }
  }
}

function toggle() {
  if (props.disabled) return
  open.value = !open.value
}

function select(value: string) {
  emit('update:modelValue', value)
  open.value = false
}

function onClickOutside(e: MouseEvent) {
  const target = e.target as HTMLElement
  if (triggerRef.value?.contains(target)) return
  if (dropdownRef.value?.contains(target)) return
  open.value = false
}

watch(open, (v) => {
  if (v) {
    nextTick(positionDropdown)
    document.addEventListener('mousedown', onClickOutside)
  } else {
    document.removeEventListener('mousedown', onClickOutside)
  }
})

onBeforeUnmount(() => {
  document.removeEventListener('mousedown', onClickOutside)
})
</script>

<template>
  <div class="fgs" :class="{ 'fgs--open': open, 'fgs--disabled': disabled, 'fgs--sm': small }">
    <button
      ref="triggerRef"
      class="fgs__trigger"
      type="button"
      :disabled="disabled"
      @click.stop="toggle"
    >
      <span class="fgs__label" :class="{ 'fgs__label--placeholder': isPlaceholder }">{{ selectedLabel }}</span>
      <svg class="fgs__chevron" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="m6 9 6 6 6-6" />
      </svg>
    </button>
    <Teleport to="body">
      <Transition name="fgs-drop">
        <div v-if="open" ref="dropdownRef" class="fgs__dropdown" :style="dropdownStyle">
          <button
            v-for="opt in options"
            :key="opt.value"
            class="fgs__option"
            :class="{ 'fgs__option--active': opt.value === modelValue }"
            @click.stop="select(opt.value)"
          >
            {{ opt.label }}
          </button>
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<style scoped>
.fgs {
  position: relative;
  display: inline-flex;
  min-width: 0;
}

.fgs__trigger {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 4px;
  width: 100%;
  height: 30px;
  padding: 0 8px 0 10px;
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-xs);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  background: var(--tv-bg-base);
  color: var(--tv-text-primary);
  cursor: pointer;
  outline: none;
  transition: border-color 0.15s ease;
}

.fgs--sm .fgs__trigger {
  height: 26px;
  font-size: var(--tv-font-size-xs);
}

.fgs__trigger:hover {
  border-color: var(--tv-border-strong);
}

.fgs--open .fgs__trigger {
  border-color: var(--tv-border-focus);
  box-shadow: 0 0 0 2px var(--tv-accent-subtle);
}

.fgs--disabled .fgs__trigger {
  opacity: 0.5;
  cursor: not-allowed;
}

.fgs__label {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
}

.fgs__label--placeholder {
  color: var(--tv-text-muted);
}

.fgs__chevron {
  flex-shrink: 0;
  color: var(--tv-text-muted);
  transition: transform 0.15s ease;
}

.fgs--open .fgs__chevron {
  transform: rotate(180deg);
}
</style>

<style>
/* Dropdown is teleported to body - cannot be scoped */
.fgs__dropdown {
  z-index: 9000;
  overflow-y: auto;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  box-shadow: var(--tv-shadow-lg);
  padding: 4px 0;
}

.fgs__option {
  display: block;
  width: 100%;
  padding: 5px 10px;
  border: none;
  background: none;
  text-align: left;
  font-family: var(--tv-font-sans);
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-primary);
  cursor: pointer;
  transition: background 0.1s;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  box-sizing: border-box;
}

.fgs__option:hover {
  background: var(--tv-bg-hover);
}

.fgs__option--active {
  background: var(--tv-accent-subtle);
  color: var(--tv-accent);
  font-weight: 500;
}

/* Transition */
.fgs-drop-enter-active,
.fgs-drop-leave-active {
  transition: opacity 0.12s ease, transform 0.12s ease;
}
.fgs-drop-enter-from,
.fgs-drop-leave-to {
  opacity: 0;
  transform: translateY(-4px);
}
</style>
