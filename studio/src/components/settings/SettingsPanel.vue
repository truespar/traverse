<script setup lang="ts">
import { useSettingsStore } from '../../stores/settings'
import { useTheme } from '../../composables/useTheme'

const settings = useSettingsStore()
const { theme, toggleTheme } = useTheme()
</script>

<template>
  <div class="settings-panel">
    <!-- Panel header -->
    <div class="settings-panel__header">
      <span class="settings-panel__title">Settings</span>
    </div>

    <div class="settings-panel__body">
      <!-- Graph Rendering -->
      <div class="settings-panel__section">
        <div class="settings-panel__section-title">Graph Rendering</div>

        <label class="settings-panel__field">
          <span class="settings-panel__label">Max nodes</span>
          <input
            type="number"
            class="fg-input fg-input--sm"
            :value="settings.maxNodes"
            @input="settings.maxNodes = Math.max(1, parseInt(($event.target as HTMLInputElement).value) || 3500)"
            min="1"
          />
        </label>

        <label class="settings-panel__field">
          <span class="settings-panel__label">Max edges</span>
          <input
            type="number"
            class="fg-input fg-input--sm"
            :value="settings.maxEdges"
            @input="settings.maxEdges = Math.max(1, parseInt(($event.target as HTMLInputElement).value) || 8500)"
            min="1"
          />
        </label>

        <p class="settings-panel__help">Graph view won't render results exceeding these limits.</p>
      </div>

      <!-- Editor -->
      <div class="settings-panel__section">
        <div class="settings-panel__section-title">Editor</div>

        <label class="settings-panel__field">
          <span class="settings-panel__label">Font size</span>
          <input
            type="number"
            class="fg-input fg-input--sm"
            :value="settings.editorFontSize"
            @input="settings.editorFontSize = Math.max(8, Math.min(32, parseInt(($event.target as HTMLInputElement).value) || 14))"
            min="8"
            max="32"
          />
        </label>
      </div>

      <!-- Theme -->
      <div class="settings-panel__section">
        <div class="settings-panel__section-title">Theme</div>

        <div class="settings-panel__theme-btns">
          <button
            class="fg-btn fg-btn--sm"
            :class="{ 'fg-btn--primary': theme === 'dark' }"
            @click="theme !== 'dark' && toggleTheme()"
          >Dark</button>
          <button
            class="fg-btn fg-btn--sm"
            :class="{ 'fg-btn--primary': theme === 'light' }"
            @click="theme !== 'light' && toggleTheme()"
          >Light</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.settings-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.settings-panel__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 12px;
  border-bottom: 1px solid var(--tv-border-default);
  height: 38px;
  flex-shrink: 0;
}

.settings-panel__title {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}

.settings-panel__body {
  flex: 1;
  overflow-y: auto;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.settings-panel__section {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.settings-panel__section-title {
  font-size: var(--tv-font-size-xs);
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
}

.settings-panel__field {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.settings-panel__label {
  font-size: var(--tv-font-size-sm);
  color: var(--tv-text-secondary);
}

.settings-panel__field .fg-input {
  width: 80px;
  text-align: right;
}

.settings-panel__toggle {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--tv-font-size-sm);
  color: var(--tv-text-secondary);
  cursor: pointer;
}

.settings-panel__toggle input[type="checkbox"] {
  accent-color: var(--tv-accent);
}

.settings-panel__help {
  margin: 0;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
}

.settings-panel__theme-btns {
  display: flex;
  gap: 6px;
}
</style>
