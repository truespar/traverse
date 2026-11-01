<script setup lang="ts">
import { ref, nextTick } from 'vue'
import { useUsersStore } from '../../stores/users'

const store = useUsersStore()

const refreshing = ref(false)
async function handleRefresh() {
  refreshing.value = true
  const done = store.fetchUsers()
  const min = new Promise(r => setTimeout(r, 500))
  await Promise.all([done, min])
  refreshing.value = false
}

const expanded = ref(true)
const confirmingDelete = ref<string | null>(null)

// Modal state
const modalMode = ref<'create' | 'edit' | null>(null)
const modalUsername = ref('')
const modalPassword = ref('')
const modalRole = ref('reader')
const modalSaving = ref(false)
const modalError = ref('')

const usernameInput = ref<HTMLInputElement | null>(null)
const passwordInput = ref<HTMLInputElement | null>(null)

function openCreate() {
  modalMode.value = 'create'
  modalUsername.value = ''
  modalPassword.value = ''
  modalRole.value = 'reader'
  modalError.value = ''
  modalSaving.value = false
  nextTick(() => usernameInput.value?.focus())
}

function openEdit(username: string, role: string) {
  modalMode.value = 'edit'
  modalUsername.value = username
  modalPassword.value = ''
  modalRole.value = role
  modalError.value = ''
  modalSaving.value = false
  nextTick(() => passwordInput.value?.focus())
}

function closeModal() {
  modalMode.value = null
}

async function submitModal() {
  modalError.value = ''
  modalSaving.value = true
  try {
    if (modalMode.value === 'create') {
      if (!modalUsername.value.trim()) { modalError.value = 'Username is required'; return }
      if (!modalPassword.value) { modalError.value = 'Password is required'; return }
      await store.createUser(modalUsername.value.trim(), modalPassword.value, modalRole.value)
    } else {
      const updates: { password?: string; role?: string } = {}
      if (modalPassword.value) updates.password = modalPassword.value
      const current = store.users.find(u => u.username === modalUsername.value)
      if (current && modalRole.value !== current.role) updates.role = modalRole.value
      if (Object.keys(updates).length === 0) { closeModal(); return }
      await store.updateUser(modalUsername.value, updates)
    }
    closeModal()
  } catch (e: any) {
    modalError.value = e.message || 'Operation failed'
  } finally {
    modalSaving.value = false
  }
}

function clickDelete(username: string) {
  if (confirmingDelete.value === username) {
    store.deleteUser(username)
    confirmingDelete.value = null
  } else {
    confirmingDelete.value = username
    setTimeout(() => { if (confirmingDelete.value === username) confirmingDelete.value = null }, 3000)
  }
}

function roleBadgeClass(role: string): string {
  if (role === 'admin') return 'ub-role-badge ub-role-badge--accent'
  if (role === 'editor') return 'ub-role-badge ub-role-badge--blue'
  return 'ub-role-badge'
}
</script>

<template>
  <div class="users-browser">
    <div class="ub-toolbar">
      <span class="ub-title">Users</span>
      <button class="fg-icon-btn" :disabled="refreshing" title="Refresh" @click="handleRefresh()">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" :class="{ spinning: refreshing }">
          <path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" /><path d="M21 3v5h-5" /><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" /><path d="M8 16H3v5" />
        </svg>
      </button>
    </div>

    <div class="ub-section">
      <!-- Section header acts as a button row but isn't a `<button>`
           element - HTML forbids nested buttons. The expand toggle is
           bound to the row; the "Add user" inner button stops the
           click from propagating. role="button" + keyboard handling
           keep accessibility on par with a real button. -->
      <div
        class="ub-section__header"
        role="button"
        tabindex="0"
        @click="expanded = !expanded"
        @keydown.enter.prevent="expanded = !expanded"
        @keydown.space.prevent="expanded = !expanded"
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" :style="{ transform: expanded ? 'rotate(90deg)' : '' }">
          <path d="m9 18 6-6-6-6" />
        </svg>
        Users
        <span class="fg-badge">{{ store.users.length }}</span>
        <button class="ub-add-btn" title="Add user" @click.stop="openCreate">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M5 12h14" /><path d="M12 5v14" />
          </svg>
        </button>
      </div>

      <div v-if="expanded" class="ub-list">
        <div v-for="user in store.users" :key="user.username" class="ub-item">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="ub-item__icon">
            <path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2" /><circle cx="12" cy="7" r="4" />
          </svg>
          <span class="ub-item__name">{{ user.username }}</span>
          <span :class="roleBadgeClass(user.role)">{{ user.role }}</span>
          <button class="ub-item__action" title="Edit" @click="openEdit(user.username, user.role)">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z" />
              <path d="m15 5 4 4" />
            </svg>
          </button>
          <button
            class="ub-item__action"
            :class="{ 'ub-item__action--danger': confirmingDelete === user.username }"
            :title="confirmingDelete === user.username ? 'Click again to confirm' : 'Delete'"
            @click="clickDelete(user.username)"
          >
            <svg v-if="confirmingDelete !== user.username" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M3 6h18" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" /><path d="M10 11v6" /><path d="M14 11v6" />
            </svg>
            <span v-else class="ub-confirm-label">Confirm?</span>
          </button>
        </div>

        <div v-if="store.users.length === 0" class="ub-empty">
          No users configured
        </div>
      </div>
    </div>

    <!-- Modal -->
    <Teleport to="body">
      <Transition name="modal">
        <div v-if="modalMode" class="fg-modal-overlay" @mousedown.self="closeModal">
          <div class="ub-modal" role="dialog" aria-modal="true" :aria-label="modalMode === 'create' ? 'Create User' : 'Edit User'">
            <div class="ub-modal__header">
              <h3 class="ub-modal__title">{{ modalMode === 'create' ? 'Create User' : 'Edit User' }}</h3>
              <button class="fg-icon-btn" aria-label="Close" @click="closeModal">
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M18 6 6 18" /><path d="m6 6 12 12" />
                </svg>
              </button>
            </div>

            <form class="ub-modal__body" @submit.prevent="submitModal">
              <label class="ub-field">
                <span class="ub-field__label">Username</span>
                <input
                  ref="usernameInput"
                  v-model="modalUsername"
                  class="fg-input"
                  :disabled="modalMode === 'edit'"
                  autocomplete="off"
                />
              </label>

              <label class="ub-field">
                <span class="ub-field__label">Password</span>
                <input
                  ref="passwordInput"
                  v-model="modalPassword"
                  type="password"
                  class="fg-input"
                  :placeholder="modalMode === 'edit' ? 'Leave blank to keep current' : ''"
                  autocomplete="new-password"
                />
              </label>

              <div class="ub-field">
                <span class="ub-field__label">Role</span>
                <div class="ub-role-group">
                  <button
                    v-for="r in [
                      { value: 'reader', label: 'Reader', desc: 'Read-only access' },
                      { value: 'editor', label: 'Editor', desc: 'Read + write data' },
                      { value: 'admin', label: 'Admin', desc: 'Full control' },
                    ]"
                    :key="r.value"
                    type="button"
                    class="ub-role-option"
                    :class="{ 'ub-role-option--active': modalRole === r.value }"
                    @click="modalRole = r.value"
                  >
                    <span class="ub-role-option__label">{{ r.label }}</span>
                    <span class="ub-role-option__desc">{{ r.desc }}</span>
                  </button>
                </div>
              </div>

              <div v-if="modalError" class="ub-modal__error">{{ modalError }}</div>

              <div class="ub-modal__footer">
                <button type="button" class="fg-btn" @click="closeModal">Cancel</button>
                <button type="submit" class="fg-btn fg-btn--primary" :disabled="modalSaving">
                  {{ modalSaving ? 'Saving...' : (modalMode === 'create' ? 'Create User' : 'Save Changes') }}
                </button>
              </div>
            </form>
          </div>
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<style scoped>
/* -- Sidebar list ------------------------------------------- */
.ub-toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 8px 12px;
}

.ub-title {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-secondary);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.ub-section__header {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 8px 12px;
  border: none;
  background: none;
  color: var(--tv-text-primary);
  font-size: var(--tv-font-size-sm);
  font-weight: 500;
  cursor: pointer;
  text-align: left;
}
.ub-section__header:hover { background: var(--tv-bg-hover); }
.ub-section__header svg:first-child { transition: transform 0.15s; flex-shrink: 0; }

.ub-add-btn {
  margin-left: auto;
  border: none;
  background: none;
  color: var(--tv-text-muted);
  cursor: pointer;
  padding: 2px;
  border-radius: var(--tv-radius-sm);
  display: flex;
  align-items: center;
}
.ub-add-btn:hover { color: var(--tv-text-primary); background: var(--tv-bg-hover); }

.ub-list { padding: 0 0 4px; }

.ub-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 5px 12px 5px 32px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-secondary);
}
.ub-item:hover { background: var(--tv-bg-hover); color: var(--tv-text-primary); }

.ub-item__icon { flex-shrink: 0; }
.ub-item__name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.ub-item__action {
  opacity: 0;
  border: none;
  background: none;
  color: var(--tv-text-muted);
  cursor: pointer;
  padding: 4px;
  border-radius: var(--tv-radius-sm);
  flex-shrink: 0;
  display: flex;
  align-items: center;
}
.ub-item:hover .ub-item__action { opacity: 1; }
.ub-item__action:hover { color: var(--tv-text-primary); background: var(--tv-bg-hover); }
.ub-item__action--danger { opacity: 1 !important; color: var(--tv-status-error) !important; }

.ub-confirm-label { font-size: var(--tv-font-size-xs); font-weight: 600; white-space: nowrap; }

.ub-role-badge {
  font-size: 11px;
  padding: 2px 7px;
  border-radius: 3px;
  background: var(--tv-bg-elevated);
  color: var(--tv-text-muted);
  flex-shrink: 0;
  text-transform: capitalize;
}
.ub-role-badge--accent {
  background: color-mix(in srgb, var(--tv-accent) 20%, transparent);
  color: var(--tv-accent-text);
}
.ub-role-badge--blue {
  background: color-mix(in srgb, var(--tv-status-info) 20%, transparent);
  color: var(--tv-status-info);
}

.ub-empty {
  padding: 5px 12px 5px 32px;
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  font-style: italic;
}

/* -- Modal -------------------------------------------------- */
.ub-modal {
  width: 420px;
  max-width: calc(100vw - 48px);
  background: var(--tv-bg-surface);
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-xl);
  box-shadow: var(--tv-shadow-xl);
  overflow: hidden;
}

.ub-modal__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 20px 24px 0;
}

.ub-modal__title {
  font-size: var(--tv-font-size-lg);
  font-weight: 600;
  color: var(--tv-text-primary);
  margin: 0;
}

.ub-modal__body {
  padding: 20px 24px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.ub-field {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.ub-field__label {
  font-size: var(--tv-font-size-xs);
  font-weight: 500;
  color: var(--tv-text-secondary);
}

.ub-field .fg-input {
  width: 100%;
}
.ub-field .fg-input:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.ub-role-group {
  display: flex;
  gap: 6px;
}

.ub-role-option {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 3px;
  padding: 10px 6px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-md);
  background: var(--tv-bg-base);
  cursor: pointer;
  transition: border-color 0.15s, background 0.15s;
}
.ub-role-option:hover {
  border-color: var(--tv-border-strong);
}
.ub-role-option--active {
  border-color: var(--tv-accent);
  background: var(--tv-accent-subtle);
}

.ub-role-option__label {
  font-size: var(--tv-font-size-sm);
  font-weight: 600;
  color: var(--tv-text-primary);
}
.ub-role-option--active .ub-role-option__label {
  color: var(--tv-accent-text);
}

.ub-role-option__desc {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-text-muted);
  white-space: nowrap;
}

.ub-modal__error {
  font-size: var(--tv-font-size-xs);
  color: var(--tv-status-error);
  padding: 6px 10px;
  background: color-mix(in srgb, var(--tv-status-error) 10%, transparent);
  border-radius: var(--tv-radius-md);
}

.ub-modal__footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding-top: 4px;
}

@keyframes fg-spin { to { transform: rotate(360deg); } }
.spinning { animation: fg-spin 0.6s linear infinite; }
</style>
