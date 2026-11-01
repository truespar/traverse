<script setup lang="ts">
import { ref, onMounted, nextTick } from 'vue'
import { useConnectionStore } from '../../stores/connection'
import { shareUrl as makeShareUrl } from '../../utils/share'
import { useToast } from '../../composables/useToast'

const props = defineProps<{
  query: string
}>()

const emit = defineEmits<{
  close: []
}>()

const { showToast } = useToast()
const conn = useConnectionStore()

const title = ref('')
const titleInput = ref<HTMLInputElement | null>(null)
const shareUrl = ref('')
const sharing = ref(false)
const errorMsg = ref('')

onMounted(() => {
  nextTick(() => titleInput.value?.focus())
})

// the link carries the query itself: no service involved
function share() {
  shareUrl.value = makeShareUrl(props.query, title.value.trim() || undefined, conn.currentDatabase)
}

async function copyUrl() {
  try {
    await navigator.clipboard.writeText(shareUrl.value)
    showToast('URL copied')
  } catch {
    showToast('Copy failed', 'error')
  }
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Escape') {
    e.preventDefault()
    emit('close')
  } else if (e.key === 'Enter' && !shareUrl.value) {
    e.preventDefault()
    share()
  }
}
</script>

<template>
  <div class="share-dialog" @keydown="onKeydown">
    <!-- Compose state -->
    <template v-if="!shareUrl">
      <div class="share-dialog__query">{{ query }}</div>
      <div class="share-dialog__row">
        <input
          ref="titleInput"
          v-model="title"
          class="share-dialog__input fg-input"
          placeholder="Title (optional)"
        />
        <button
          class="share-dialog__btn fg-btn fg-btn--primary"
          :disabled="sharing"
          @click="share"
        >
          <span v-if="sharing" class="fg-spinner" style="width:12px;height:12px;border-width:1.5px" />
          <template v-else>Share</template>
        </button>
        <button class="share-dialog__btn fg-btn" @click="$emit('close')">Cancel</button>
      </div>
      <div v-if="errorMsg" class="share-dialog__error">{{ errorMsg }}</div>
    </template>

    <!-- Result state -->
    <template v-else>
      <div class="share-dialog__row">
        <input
          class="share-dialog__input share-dialog__url fg-input"
          :value="shareUrl"
          readonly
          @focus="($event.target as HTMLInputElement).select()"
        />
        <button class="share-dialog__btn fg-btn fg-btn--primary" @click="copyUrl">Copy</button>
        <button class="share-dialog__btn fg-btn" @click="$emit('close')">Close</button>
      </div>
    </template>
  </div>
</template>

<style scoped>
.share-dialog {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px 12px;
  background: var(--tv-bg-elevated);
  border-top: 1px solid var(--tv-border-default);
}

.share-dialog__query {
  font-family: var(--tv-font-mono);
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 100%;
}

.share-dialog__row {
  display: flex;
  align-items: center;
  gap: 8px;
}

.share-dialog__input {
  flex: 1;
  min-width: 0;
  font-size: var(--tv-font-size-sm);
}

.share-dialog__url {
  font-family: var(--tv-font-mono);
  font-size: var(--tv-font-size-xs);
}

.share-dialog__btn {
  flex-shrink: 0;
  font-size: var(--tv-font-size-xs);
  padding: 4px 12px;
  height: 28px;
}

.share-dialog__error {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-status-error);
}
</style>
