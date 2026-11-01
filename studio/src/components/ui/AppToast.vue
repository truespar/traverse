<script setup lang="ts">
import { useToast } from '../../composables/useToast'

const { toasts, dismissToast } = useToast()
</script>

<template>
  <Teleport to="body">
    <div class="toast-container">
      <TransitionGroup name="toast">
        <div
          v-for="t in toasts"
          :key="t.id"
          class="toast-item"
          :class="'toast-item--' + t.type"
          @click="dismissToast(t.id)"
        >
          {{ t.message }}
        </div>
      </TransitionGroup>
    </div>
  </Teleport>
</template>

<style scoped>
.toast-container {
  position: fixed;
  bottom: 32px;
  right: 24px;
  z-index: 9999;
  display: flex;
  flex-direction: column;
  gap: 8px;
  pointer-events: none;
}

/* Mobile: lift the toast above the bottom tab bar + iOS safe area, and
   stretch full-width so messages stay readable on a phone. */
@media (max-width: 768px) {
  .toast-container {
    bottom: calc(var(--tv-activitybar-mobile-height, 56px) + env(safe-area-inset-bottom) + 12px);
    right: 12px;
    left: 12px;
    align-items: stretch;
  }
  .toast-item {
    max-width: none;
  }
}

.toast-item {
  pointer-events: auto;
  padding: 10px 16px;
  border-radius: var(--tv-radius-md);
  font-size: var(--tv-font-size-sm);
  font-weight: 500;
  cursor: pointer;
  box-shadow: var(--tv-shadow-lg);
  max-width: 360px;
  word-break: break-word;
  border: 1px solid transparent;
  backdrop-filter: blur(8px);
}

.toast-item--success {
  background: color-mix(in srgb, var(--tv-status-success) 12%, var(--tv-bg-elevated));
  border-color: color-mix(in srgb, var(--tv-status-success) 30%, transparent);
  color: var(--tv-status-success);
}

.toast-item--error {
  background: color-mix(in srgb, var(--tv-status-error) 12%, var(--tv-bg-elevated));
  border-color: color-mix(in srgb, var(--tv-status-error) 30%, transparent);
  color: var(--tv-status-error);
}

.toast-enter-active {
  transition: all 0.3s ease;
}
.toast-leave-active {
  transition: all 0.25s ease;
}
.toast-enter-from {
  opacity: 0;
  transform: translateX(40px);
}
.toast-leave-to {
  opacity: 0;
  transform: translateX(40px);
}
</style>
