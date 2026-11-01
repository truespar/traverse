<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useConnectionStore } from '../../stores/connection'
import { api } from '../../utils/api'
import { useToast } from '../../composables/useToast'
import FgSelect from '../common/FgSelect.vue'

const emit = defineEmits<{ close: [] }>()

const conn = useConnectionStore()
const { showToast } = useToast()

interface AliasEntry {
  alias: string
  database: string
}

const aliases = ref<AliasEntry[]>([])
const loading = ref(false)
const error = ref('')

// Create form
const newAlias = ref('')
const newTarget = ref('')
const creating = ref(false)

// Edit state
const editingAlias = ref<string | null>(null)
const editTarget = ref('')
const saving = ref(false)

// Delete state (two-click)
const confirmingDelete = ref<string | null>(null)
const deleting = ref(false)

const dbOptions = computed(() =>
  conn.databases.map(db => ({ value: db.name, label: db.name }))
)

async function loadAliases() {
  loading.value = true
  error.value = ''
  try {
    aliases.value = await api.aliases()
  } catch (e: any) {
    error.value = e.message || 'Failed to load aliases'
  } finally {
    loading.value = false
  }
}

async function createAlias() {
  const alias = newAlias.value.trim()
  const database = newTarget.value
  if (!alias || !database) return
  creating.value = true
  error.value = ''
  try {
    await api.createAlias(alias, database)
    newAlias.value = ''
    newTarget.value = ''
    showToast(`Alias "${alias}" created`)
    await refresh()
  } catch (e: any) {
    error.value = e.message || 'Failed to create alias'
  } finally {
    creating.value = false
  }
}

function startEdit(entry: AliasEntry) {
  editingAlias.value = entry.alias
  editTarget.value = entry.database
}

function cancelEdit() {
  editingAlias.value = null
}

async function saveEdit() {
  if (!editingAlias.value) return
  saving.value = true
  error.value = ''
  try {
    await api.alterAlias(editingAlias.value, editTarget.value)
    showToast(`Alias "${editingAlias.value}" updated`)
    editingAlias.value = null
    await refresh()
  } catch (e: any) {
    error.value = e.message || 'Failed to update alias'
  } finally {
    saving.value = false
  }
}

async function deleteAlias(alias: string) {
  if (confirmingDelete.value !== alias) {
    confirmingDelete.value = alias
    return
  }
  deleting.value = true
  error.value = ''
  try {
    await api.dropAlias(alias)
    confirmingDelete.value = null
    showToast(`Alias "${alias}" deleted`)
    await refresh()
  } catch (e: any) {
    error.value = e.message || 'Failed to delete alias'
  } finally {
    deleting.value = false
  }
}

function cancelConfirm() {
  confirmingDelete.value = null
}

async function refresh() {
  await Promise.all([loadAliases(), conn.pollHealth()])
}

onMounted(loadAliases)
</script>

<template>
  <Teleport to="body">
    <Transition name="modal">
    <div class="fg-modal-overlay" @mousedown.self="emit('close')">
      <div class="ap-panel" role="dialog" aria-modal="true" aria-label="Database Aliases">
        <div class="ap-header">
          <span class="ap-title">Database Aliases</span>
          <button class="ap-close" aria-label="Close" @click="emit('close')">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M18 6 6 18" /><path d="m6 6 12 12" />
            </svg>
          </button>
        </div>

        <!-- Alias list -->
        <div class="ap-section">
          <div class="ap-section-title">
            Aliases
            <span v-if="aliases.length" class="ap-count">{{ aliases.length }}</span>
          </div>

          <div v-if="loading" class="ap-loading">
            <div class="ap-spinner" />
            <span>Loading aliases...</span>
          </div>

          <div v-else-if="aliases.length === 0" class="ap-empty">
            No aliases defined
          </div>

          <div v-else class="ap-list">
            <div
              v-for="entry in aliases"
              :key="entry.alias"
              class="ap-item"
              @mouseleave="entry.alias === confirmingDelete ? cancelConfirm() : null"
            >
              <!-- View mode -->
              <template v-if="editingAlias !== entry.alias">
                <div class="ap-item-info">
                  <span class="ap-alias-name">{{ entry.alias }}</span>
                  <span class="ap-arrow">
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                      <path d="M5 12h14" /><path d="m12 5 7 7-7 7" />
                    </svg>
                  </span>
                  <span class="ap-target-name">{{ entry.database }}</span>
                </div>
                <div class="ap-item-actions">
                  <button class="ap-edit-btn" title="Edit" @click="startEdit(entry)">
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                      <path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z" /><path d="m15 5 4 4" />
                    </svg>
                  </button>
                  <button
                    class="ap-delete-btn"
                    :class="{ 'ap-delete-btn--confirm': confirmingDelete === entry.alias }"
                    :disabled="deleting"
                    @click="deleteAlias(entry.alias)"
                  >
                    {{ confirmingDelete === entry.alias ? 'Confirm?' : 'Delete' }}
                  </button>
                </div>
              </template>

              <!-- Edit mode -->
              <template v-else>
                <div class="ap-edit-row">
                  <span class="ap-alias-name">{{ entry.alias }}</span>
                  <span class="ap-arrow">
                    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                      <path d="M5 12h14" /><path d="m12 5 7 7-7 7" />
                    </svg>
                  </span>
                  <FgSelect v-model="editTarget" :options="dbOptions" class="ap-edit-select" />
                </div>
                <div class="ap-item-actions">
                  <button class="ap-save-btn" :disabled="saving" @click="saveEdit">Save</button>
                  <button class="ap-cancel-btn" @click="cancelEdit">Cancel</button>
                </div>
              </template>
            </div>
          </div>
        </div>

        <!-- Create form -->
        <div class="ap-section">
          <div class="ap-section-title">Create Alias</div>
          <div class="ap-create-form">
            <input
              v-model="newAlias"
              class="fg-input"
              placeholder="Alias name..."
              maxlength="50"
              @keydown.enter="createAlias"
            />
            <FgSelect v-model="newTarget" :options="dbOptions" placeholder="Target database..." />
            <button
              class="ap-create-btn"
              :disabled="creating || !newAlias.trim() || !newTarget"
              @click="createAlias"
            >
              {{ creating ? 'Creating...' : 'Create' }}
            </button>
          </div>
        </div>

        <div v-if="error" class="ap-error">{{ error }}</div>
      </div>
    </div>
    </Transition>
  </Teleport>
</template>

<style scoped>
.ap-panel {
  width: 360px;
  max-height: 80vh;
  overflow-y: auto;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-lg);
  box-shadow: var(--tv-shadow-lg);
}

.ap-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 16px 20px 12px;
  border-bottom: 1px solid var(--tv-border-default);
}

.ap-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--tv-text-primary);
}

.ap-close {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: none;
  color: var(--tv-text-muted);
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}

.ap-close:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}

.ap-section {
  padding: 12px 20px;
  border-bottom: 1px solid var(--tv-border-default);
}

.ap-section:last-child {
  border-bottom: none;
}

.ap-section-title {
  font-size: var(--tv-font-size-xs);
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
  margin-bottom: 8px;
  display: flex;
  align-items: baseline;
  justify-content: space-between;
}

.ap-count {
  font-weight: 400;
  text-transform: none;
  letter-spacing: normal;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
}

.ap-loading {
  display: flex;
  align-items: center;
  gap: 8px;
  color: var(--tv-text-secondary);
  font-size: 12px;
  padding: 4px 0;
}

.ap-spinner {
  width: 14px;
  height: 14px;
  border: 2px solid var(--tv-border-default);
  border-top-color: var(--tv-accent);
  border-radius: 50%;
  animation: ap-spin 0.8s linear infinite;
}

@keyframes ap-spin {
  to { transform: rotate(360deg); }
}

.ap-empty {
  color: var(--tv-text-muted);
  font-size: 12px;
  padding: 4px 0;
}

.ap-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.ap-item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 6px 8px;
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-surface);
}

.ap-item-info {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
  flex: 1;
}

.ap-alias-name {
  font-size: var(--tv-font-size-sm);
  font-weight: 500;
  color: var(--tv-text-primary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.ap-arrow {
  flex-shrink: 0;
  color: var(--tv-text-muted);
  display: flex;
  align-items: center;
}

.ap-target-name {
  font-size: var(--tv-font-size-sm);
  color: var(--tv-text-secondary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.ap-item-actions {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-shrink: 0;
}

.ap-edit-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  border: none;
  border-radius: 4px;
  background: none;
  color: var(--tv-text-muted);
  cursor: pointer;
  transition: color 0.15s, background 0.15s;
}

.ap-edit-btn:hover {
  color: var(--tv-text-primary);
  background: var(--tv-bg-hover);
}

.ap-delete-btn {
  flex-shrink: 0;
  padding: 3px 8px;
  font-size: 11px;
  font-weight: 500;
  color: var(--tv-text-muted);
  background: none;
  border: 1px solid var(--tv-border-default);
  border-radius: 4px;
  cursor: pointer;
  transition: color 0.15s, border-color 0.15s, background 0.15s;
}

.ap-delete-btn:hover {
  color: var(--tv-status-error);
  border-color: var(--tv-status-error);
}

.ap-delete-btn--confirm {
  color: var(--tv-text-inverse);
  background: var(--tv-status-error);
  border-color: var(--tv-status-error);
}

.ap-delete-btn--confirm:hover {
  color: var(--tv-text-inverse);
  background: var(--tv-status-error-hover);
  border-color: var(--tv-status-error-hover);
}

.ap-delete-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

/* Edit mode */
.ap-edit-row {
  display: flex;
  align-items: center;
  gap: 6px;
  flex: 1;
  min-width: 0;
}

.ap-save-btn {
  padding: 3px 8px;
  font-size: 11px;
  font-weight: 500;
  color: var(--tv-text-inverse);
  background: var(--tv-accent);
  border: none;
  border-radius: 4px;
  cursor: pointer;
  transition: background 0.15s;
}

.ap-save-btn:hover {
  background: var(--tv-accent-hover);
}

.ap-save-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.ap-cancel-btn {
  padding: 3px 8px;
  font-size: 11px;
  font-weight: 500;
  color: var(--tv-text-muted);
  background: none;
  border: 1px solid var(--tv-border-default);
  border-radius: 4px;
  cursor: pointer;
  transition: color 0.15s, background 0.15s;
}

.ap-cancel-btn:hover {
  color: var(--tv-text-primary);
  background: var(--tv-bg-hover);
}

/* Create form */
.ap-create-form {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.ap-create-btn {
  align-self: flex-end;
  padding: 5px 14px;
  font-size: 12px;
  font-weight: 500;
  color: var(--tv-text-inverse);
  background: var(--tv-accent);
  border: none;
  border-radius: var(--tv-radius-sm);
  cursor: pointer;
  transition: background 0.15s;
}

.ap-create-btn:hover {
  background: var(--tv-accent-hover);
}

.ap-create-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.ap-error {
  padding: 8px 20px 12px;
  font-size: 12px;
  color: var(--tv-status-error);
}
</style>

<style>
/* Teleported to body - cannot be scoped */
.ap-create-form .fg-input,
.ap-create-form .fgs {
  width: 100%;
  box-sizing: border-box;
}

.ap-edit-row .fgs {
  min-width: 0;
  flex: 1;
}
</style>
