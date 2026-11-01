<script setup lang="ts">
import { ref, watch } from 'vue'
import { api } from '../../utils/api'
import { useConnectionStore } from '../../stores/connection'
import { useSchemaStore } from '../../stores/schema'
import { useToast } from '../../composables/useToast'

const conn = useConnectionStore()
const schema = useSchemaStore()
const { showToast } = useToast()

interface FileEntry {
  name: string
  file: string
  bytes: number
  loaded: boolean
  /** `database` for `.tvdb`, `model` for `.tvmodel`. Older
   *  servers omit this - the UI defaults to `database`. */
  kind?: 'database' | 'model'
}

const files = ref<FileEntry[]>([])
const loading = ref(false)
const uploadFileInput = ref<HTMLInputElement | null>(null)
const uploadSelectedFile = ref<File | null>(null)
const uploadDbName = ref('')
const uploadProgress = ref(0)
const uploadPhase = ref<'idle' | 'uploading' | 'loading' | 'done' | 'error'>('idle')
const uploadError = ref('')
const busyFile = ref<string | null>(null)
const busyAction = ref<string | null>(null) // 'loading' | 'unloading' | 'deleting' | 'downloading'
const unloadTarget = ref<string | null>(null)

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`
}

async function fetchFiles() {
  loading.value = true
  try {
    files.value = await api.listFiles()
  } catch (e: any) {
    showToast(e.message || 'Failed to list files', 'error')
  } finally {
    loading.value = false
  }
}

/** Detect whether the chosen file is a `.tvmodel` (model upload)
 *  or anything else (treated as a `.tvdb` database). The upload
 *  panel uses this to skip the lowercase/sanitisation we apply
 *  to DB names - model names preserve the user's casing. */
function uploadKind(): 'database' | 'model' {
  const f = uploadSelectedFile.value
  if (f && /\.tvmodel$/i.test(f.name)) return 'model'
  return 'database'
}

function handleUploadFile(event: Event) {
  const target = event.target as HTMLInputElement
  const file = target.files?.[0]
  if (!file) return
  uploadSelectedFile.value = file
  uploadPhase.value = 'idle'
  uploadError.value = ''
  const stem = file.name.replace(/\.[^.]+$/, '')
  if (/\.tvmodel$/i.test(file.name)) {
    // Model names keep casing and special characters (rejected
    // server-side via `is_safe_model_name` if unsafe).
    uploadDbName.value = stem
  } else {
    uploadDbName.value = stem.toLowerCase().replace(/[^a-z0-9_]/g, '_').replace(/_+/g, '_').replace(/^_|_$/g, '')
  }
  target.value = ''
}

function clearUpload() {
  uploadSelectedFile.value = null
  uploadDbName.value = ''
  uploadPhase.value = 'idle'
  uploadProgress.value = 0
  uploadError.value = ''
}

async function runUpload() {
  const file = uploadSelectedFile.value
  const name = uploadDbName.value.trim()
  if (!file || !name) return

  uploadPhase.value = 'idle'
  uploadProgress.value = 0
  uploadError.value = ''

  const kind = uploadKind()
  // Model uploads bypass the database-limit + DB-naming checks
  // since they're conceptually separate artifacts.
  if (kind === 'model') {
    const nameExists = files.value.some((f) => f.kind === 'model' && f.name === name)
    if (nameExists) {
      uploadPhase.value = 'error'
      uploadError.value = `Model "${name}" already exists - delete it first.`
      return
    }
    uploadPhase.value = 'uploading'
    try {
      await api.uploadModel(name, file, (loaded, total) => {
        uploadProgress.value = Math.round((loaded / total) * 100)
      })
      uploadPhase.value = 'done'
      showToast(`Model "${name}" uploaded`, 'success')
      await fetchFiles()
      clearUpload()
    } catch (e: any) {
      uploadPhase.value = 'error'
      uploadError.value = e.message || 'Upload failed'
    }
    return
  }

  // Check if a database/file with this name already exists
  const nameExists = files.value.some(f => f.name === name && f.kind !== 'model') || conn.databases.some(d => d.name === name)
  if (nameExists) {
    uploadPhase.value = 'error'
    uploadError.value = `Database "${name}" already exists - drop or delete it first.`
    return
  }

  uploadPhase.value = 'uploading'

  try {
    const uploadResult = await api.uploadDatabase(name, file, (loaded, total) => {
      uploadProgress.value = Math.round((loaded / total) * 100)
    })

    // If the server already loaded it into memory (no data dir mode), skip the load step
    if (!uploadResult.loaded) {
      uploadPhase.value = 'loading'
      await api.loadDatabase(name, `${name}.tvdb`)
    }

    uploadPhase.value = 'done'
    await conn.pollHealth()
    conn.currentDatabase = name
    schema.fetchSchema()
    showToast(`Database "${name}" uploaded and loaded`, 'success')
    await fetchFiles()
    clearUpload()
  } catch (e: any) {
    uploadPhase.value = 'error'
    uploadError.value = e.message || 'Upload failed'
    if (uploadProgress.value === 100) {
      api.deleteFile(name).catch(() => {})
    }
  }
}

async function handleDownload(name: string, kind: 'database' | 'model' = 'database') {
  busyFile.value = name
  busyAction.value = 'downloading'
  try {
    if (kind === 'model') {
      await api.downloadModel(name)
    } else {
      await api.downloadDatabase(name)
    }
  } catch (e: any) {
    showToast(e.message || 'Download failed', 'error')
  } finally {
    busyFile.value = null
    busyAction.value = null
  }
}

async function handleLoad(name: string) {
  busyFile.value = name
  busyAction.value = 'loading'
  try {
    await api.loadDatabase(name, `${name}.tvdb`)
    await conn.pollHealth()
    schema.fetchSchema()
    await fetchFiles()
    showToast(`Database "${name}" loaded`, 'success')
  } catch (e: any) {
    showToast(e.message || 'Failed to load database', 'error')
  } finally {
    busyFile.value = null
    busyAction.value = null
  }
}

function confirmUnload(name: string) {
  unloadTarget.value = name
}

async function executeUnload() {
  const name = unloadTarget.value
  if (!name) return
  unloadTarget.value = null
  busyFile.value = name
  busyAction.value = 'unloading'
  try {
    await api.unloadDatabase(name)
    await conn.pollHealth()
    await fetchFiles()
    showToast(`Database "${name}" unloaded`, 'success')
  } catch (e: any) {
    showToast(e.message || 'Failed to unload database', 'error')
  } finally {
    busyFile.value = null
    busyAction.value = null
  }
}

async function handleDelete(name: string, kind: 'database' | 'model' = 'database') {
  busyFile.value = name
  busyAction.value = 'deleting'
  try {
    await api.deleteFile(name, kind)
    await fetchFiles()
    const ext = kind === 'model' ? 'tvmodel' : 'tvdb'
    showToast(`File "${name}.${ext}" deleted`, 'success')
  } catch (e: any) {
    showToast(e.message || 'Failed to delete file', 'error')
  } finally {
    busyFile.value = null
    busyAction.value = null
  }
}

// Fetch files once connected and authenticated (not on mount - auth may not be ready yet)
watch(
  () => conn.isConnected && !conn.authRequired,
  (ready) => { if (ready) fetchFiles() },
  { immediate: true },
)
</script>

<template>
  <div class="file-manager">
    <!-- Header -->
    <div class="file-manager__header">
      <span class="file-manager__title">Files</span>
      <button
        class="fg-icon-btn"
        :disabled="loading"
        title="Refresh file list"
        @click="fetchFiles"
      >
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" :class="{ spinning: loading }">
          <path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" /><path d="M21 3v5h-5" /><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" /><path d="M8 16H3v5" />
        </svg>
      </button>
    </div>

    <div class="file-manager__body">
      <!-- Upload section -->
      <div class="file-manager__section">
        <button class="fg-btn fg-btn--sm file-manager__upload-btn" @click="uploadFileInput?.click()">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 13V3" /><path d="m7 8 5-5 5 5" /><path d="M20 21H4" /><path d="M20 17v4" /><path d="M4 17v4" />
          </svg>
          Upload .tvdb or .tvmodel
        </button>
        <input
          ref="uploadFileInput"
          type="file"
          accept=".tvdb,.tvmodel"
          style="display:none"
          @change="handleUploadFile"
        />

        <!-- Upload form -->
        <div v-if="uploadSelectedFile" class="file-manager__upload-form">
          <div class="file-manager__upload-file-info">
            <span class="file-manager__upload-file-name">{{ uploadSelectedFile.name }}</span>
            <span class="file-manager__upload-file-size">({{ formatSize(uploadSelectedFile.size) }})</span>
            <button v-if="uploadPhase === 'idle' || uploadPhase === 'error'" class="fg-icon-btn file-manager__upload-clear" title="Cancel" @click="clearUpload">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M18 6 6 18" /><path d="m6 6 12 12" />
              </svg>
            </button>
          </div>

          <label class="file-manager__field">
            <span class="file-manager__field-label">Name</span>
            <input
              v-model="uploadDbName"
              class="fg-input fg-input--sm"
              placeholder="e.g. my_database"
              :disabled="uploadPhase === 'uploading' || uploadPhase === 'loading'"
            />
          </label>

          <!-- Progress -->
          <div v-if="uploadPhase === 'uploading'" class="file-manager__progress">
            <div class="file-manager__progress-bar">
              <div class="file-manager__progress-fill" :style="{ width: uploadProgress + '%' }" />
            </div>
            <span class="file-manager__progress-text">
              <span class="fg-spinner" style="width:10px;height:10px;border-width:1.5px" />
              Uploading {{ uploadProgress }}%
            </span>
          </div>

          <div v-if="uploadPhase === 'loading'" class="file-manager__progress">
            <span class="file-manager__progress-text">
              <span class="fg-spinner" style="width:10px;height:10px;border-width:1.5px" />
              Loading into memory...
            </span>
          </div>

          <!-- Error -->
          <div v-if="uploadError" class="file-manager__error">{{ uploadError }}</div>

          <!-- Upload button -->
          <button
            v-if="uploadPhase === 'idle' || uploadPhase === 'error'"
            class="fg-btn fg-btn--primary fg-btn--sm"
            :disabled="!uploadDbName.trim()"
            @click="runUpload"
          >
            Upload &amp; Load
          </button>
        </div>
      </div>

      <!-- Divider -->
      <div class="file-manager__divider">
        <span>Files on server</span>
      </div>

      <!-- Loading spinner -->
      <div v-if="loading && files.length === 0" class="file-manager__loading">
        <span class="fg-spinner" style="width:16px;height:16px;border-width:2px" />
      </div>

      <!-- Empty state -->
      <div v-else-if="files.length === 0" class="file-manager__empty">
        No files found on server
      </div>

      <!-- File list -->
      <div v-else class="file-manager__list">
        <div
          v-for="f in files"
          :key="`${f.kind ?? 'database'}:${f.name}`"
          class="file-manager__card"
          :class="{ 'file-manager__card--loaded': f.loaded }"
        >
          <div class="file-manager__card-info">
            <span class="file-manager__card-name">{{ f.name }}</span>
            <div class="file-manager__card-meta">
              <span
                class="file-manager__kind"
                :class="f.kind === 'model' ? 'file-manager__kind--model' : 'file-manager__kind--database'"
              >
                {{ f.kind === 'model' ? '.tvmodel' : '.tvdb' }}
              </span>
              <span class="file-manager__card-size">{{ formatSize(f.bytes) }}</span>
              <span
                v-if="f.kind !== 'model'"
                class="file-manager__status"
                :class="f.loaded ? 'file-manager__status--loaded' : 'file-manager__status--disk'"
              >
                {{ f.loaded ? 'Loaded' : 'On disk' }}
              </span>
            </div>
          </div>
          <div class="file-manager__card-actions">
            <!-- Busy state: show spinner + label instead of buttons -->
            <template v-if="busyFile === f.name">
              <span class="file-manager__busy">
                <span class="fg-spinner" style="width:12px;height:12px;border-width:1.5px" />
                {{ busyAction === 'loading' ? 'Loading...' : busyAction === 'unloading' ? 'Unloading...' : busyAction === 'deleting' ? 'Deleting...' : '' }}
              </span>
            </template>
            <template v-else>
              <!-- Download -->
              <button
                class="fg-icon-btn"
                title="Download"
                :disabled="!!busyFile"
                @click="handleDownload(f.name, f.kind ?? 'database')"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /><polyline points="7 10 12 15 17 10" /><line x1="12" x2="12" y1="15" y2="3" />
                </svg>
              </button>
              <!-- Load (databases only, when not loaded) -->
              <button
                v-if="f.kind !== 'model' && !f.loaded"
                class="fg-icon-btn"
                title="Load into memory"
                :disabled="!!busyFile"
                @click="handleLoad(f.name)"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <polygon points="6 3 20 12 6 21 6 3" />
                </svg>
              </button>
              <!-- Unload (databases only, when loaded) -->
              <button
                v-if="f.kind !== 'model' && f.loaded"
                class="fg-icon-btn file-manager__unload-btn"
                title="Unload from memory"
                :disabled="!!busyFile"
                @click="confirmUnload(f.name)"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M4 12l8-8 8 8" /><path d="M12 4v16" />
                </svg>
              </button>
              <!-- Delete: databases require unloaded; models always allowed -->
              <button
                v-if="f.kind === 'model' || !f.loaded"
                class="fg-icon-btn file-manager__delete-btn"
                title="Delete file"
                :disabled="!!busyFile"
                @click="handleDelete(f.name, f.kind ?? 'database')"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M3 6h18" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
                </svg>
              </button>
            </template>
          </div>
        </div>
      </div>
    </div>

    <!-- Unload confirmation modal -->
    <Teleport to="body">
      <Transition name="modal">
        <div v-if="unloadTarget" class="fg-modal-overlay" @mousedown.self="unloadTarget = null">
          <div class="fm-modal" role="dialog" aria-modal="true" aria-label="Unload database" @click.stop>
            <div class="fm-modal__header">Unload database</div>
            <div class="fm-modal__body">
              <p>This will save <strong>"{{ unloadTarget }}"</strong> to disk and remove it from memory. Active connections to this database will be disconnected.</p>
              <p>The <code>.tvdb</code> file remains on disk and can be loaded again later.</p>
            </div>
            <div class="fm-modal__footer">
              <button class="fg-btn fg-btn--sm" @click="unloadTarget = null">Cancel</button>
              <button class="fg-btn fg-btn--sm fg-btn--primary" @click="executeUnload">Unload</button>
            </div>
          </div>
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<style scoped>
.file-manager {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.file-manager__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 8px 12px;
  flex-shrink: 0;
}

.file-manager__title {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-secondary);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.file-manager__body {
  flex: 1;
  overflow-y: auto;
  min-height: 0;
  padding: 0 12px 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

/* Upload section */
.file-manager__section {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.file-manager__upload-btn {
  align-self: flex-start;
}

.file-manager__upload-form {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  min-width: 0;
}

.file-manager__upload-file-info {
  display: flex;
  align-items: center;
  gap: 6px;
}

.file-manager__upload-file-name {
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
  color: var(--tv-text-primary);
  font-family: var(--tv-font-mono);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
  flex: 1;
}

.file-manager__upload-file-size {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  flex-shrink: 0;
}

.file-manager__upload-clear {
  flex-shrink: 0;
  margin-left: auto;
}

.file-manager__field {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: var(--tv-font-size-xs);
  min-width: 0;
}

.file-manager__field-label {
  color: var(--tv-text-muted);
  font-size: var(--tv-font-size-xs);
  white-space: nowrap;
  flex-shrink: 0;
}

.file-manager__field .fg-input {
  flex: 1;
  min-width: 0;
}

.file-manager__progress {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.file-manager__progress-bar {
  height: 6px;
  border-radius: 3px;
  background: var(--tv-bg-base);
  border: 1px solid var(--tv-border-default);
  overflow: hidden;
}

.file-manager__progress-fill {
  height: 100%;
  background: var(--tv-accent);
  border-radius: 3px;
  transition: width 0.2s ease;
}

.file-manager__progress-text {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
}

.file-manager__error {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-status-error);
  padding: 4px 0;
}

/* Divider */
.file-manager__divider {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
  font-weight: 500;
}

.file-manager__divider::after {
  content: '';
  flex: 1;
  height: 1px;
  background: var(--tv-border-default);
}

/* Loading / empty */
.file-manager__loading {
  display: flex;
  justify-content: center;
  padding: 24px 0;
}

.file-manager__empty {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  font-style: italic;
  padding: 12px 0;
}

/* File list */
.file-manager__list {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.file-manager__card {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  transition: border-color 0.15s;
}

.file-manager__card:hover {
  border-color: var(--tv-border-strong);
}

.file-manager__card-info {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.file-manager__card-name {
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
  color: var(--tv-text-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.file-manager__card-meta {
  display: flex;
  align-items: center;
  gap: 8px;
}

.file-manager__card-size {
  font-size: 11px;
  color: var(--tv-text-muted);
  font-family: var(--tv-font-mono);
}

.file-manager__status {
  font-size: 10px;
  font-weight: 600;
  padding: 1px 6px;
  border-radius: 9px;
  line-height: 16px;
}

.file-manager__status--loaded {
  color: var(--tv-status-success);
  background: color-mix(in srgb, var(--tv-status-success) 12%, transparent);
}

.file-manager__status--disk {
  color: var(--tv-text-muted);
  background: var(--tv-bg-base);
}

/* Extension badge - `.tvdb` vs `.tvmodel`. Sits next to the size
 * inside the card meta row so a glance tells you what each file is. */
.file-manager__kind {
  font-size: 10px;
  font-weight: 600;
  font-family: var(--tv-font-mono, var(--tv-font-sans));
  padding: 1px 6px;
  border-radius: 9px;
  line-height: 16px;
}
.file-manager__kind--database {
  color: var(--tv-text-secondary);
  background: var(--tv-bg-base);
}
.file-manager__kind--model {
  color: var(--tv-accent);
  background: var(--tv-accent-subtle);
}

.file-manager__card-actions {
  display: flex;
  align-items: center;
  gap: 2px;
  flex-shrink: 0;
}

.file-manager__delete-btn {
  color: var(--tv-text-muted);
}
.file-manager__delete-btn:hover {
  color: var(--tv-status-error) !important;
}

.file-manager__busy {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  white-space: nowrap;
}

/* Unload button */
.file-manager__unload-btn {
  color: var(--tv-text-muted);
}
.file-manager__unload-btn:hover {
  color: var(--tv-status-warning) !important;
}

@keyframes fg-spin {
  to { transform: rotate(360deg); }
}
.spinning {
  animation: fg-spin 0.6s linear infinite;
}
</style>

<style>
/* Unload modal (teleported to body, cannot be scoped) */
.fm-modal {
  width: 380px;
  background: var(--tv-bg-elevated);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-lg);
  box-shadow: var(--tv-shadow-xl);
  overflow: hidden;
}

.fm-modal__header {
  padding: 12px 16px;
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
  border-bottom: 1px solid var(--tv-border-default);
}

.fm-modal__body {
  padding: 16px;
}

.fm-modal__body p {
  margin: 0 0 8px;
  font-size: var(--tv-font-size-sm);
  line-height: 1.5;
  color: var(--tv-text-secondary);
}

.fm-modal__body p:last-child {
  margin-bottom: 0;
}

.fm-modal__body code {
  font-family: var(--tv-font-mono);
  font-size: var(--tv-font-size-xs);
  padding: 1px 4px;
  border-radius: 3px;
  background: var(--tv-bg-base);
  color: var(--tv-text-secondary);
}

.fm-modal__footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 16px;
  border-top: 1px solid var(--tv-border-default);
}
</style>
