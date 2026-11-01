<script setup lang="ts">
import { ref, onMounted, nextTick } from 'vue'

const emit = defineEmits<{
  save: [name: string]
  cancel: []
}>()

const nameInput = ref<HTMLInputElement | null>(null)
const name = ref('')

onMounted(() => {
  nextTick(() => nameInput.value?.focus())
})

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Enter') {
    e.preventDefault()
    emit('save', name.value.trim())
  } else if (e.key === 'Escape') {
    e.preventDefault()
    emit('cancel')
  }
}
</script>

<template>
  <div class="save-dialog">
    <input
      ref="nameInput"
      v-model="name"
      class="save-dialog__input fg-input"
      placeholder="Untitled query"
      @keydown="onKeydown"
    />
    <button class="save-dialog__btn save-dialog__btn--save fg-btn fg-btn--primary" @click="$emit('save', name.trim())">
      Save
    </button>
    <button class="save-dialog__btn fg-btn" @click="$emit('cancel')">
      Cancel
    </button>
  </div>
</template>

<style scoped>
.save-dialog {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  background: var(--tv-bg-elevated);
  border-top: 1px solid var(--tv-border-default);
}

.save-dialog__input {
  flex: 1;
  min-width: 0;
  font-size: var(--tv-font-size-sm);
}

.save-dialog__btn {
  flex-shrink: 0;
  font-size: var(--tv-font-size-xs);
  padding: 4px 12px;
  height: 28px;
}
</style>
