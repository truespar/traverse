<script setup lang="ts">
import { ref } from 'vue'
import { useConnectionStore } from '../../stores/connection'
import { setApiKey } from '../../utils/api'

const conn = useConnectionStore()
const keyInput = ref('')
const error = ref('')
const loading = ref(false)

async function submit() {
  const key = keyInput.value.trim()
  if (!key) return

  error.value = ''
  loading.value = true

  setApiKey(key)

  try {
    await conn.pollHealth()
    if (conn.authRequired) {
      error.value = 'Invalid API key'
    }
  } catch {
    error.value = 'Connection failed'
  } finally {
    loading.value = false
  }
}
</script>

<template>
  <div v-if="conn.authRequired" class="auth-gate-overlay">
    <div class="auth-gate-card">
      <div class="auth-gate-logo">
        <div class="auth-gate-mark">
          <svg viewBox="0 0 512 512" fill="none" xmlns="http://www.w3.org/2000/svg">
            <path d="M227.3 371.862V136.3H277.435L278.989 182.548L269.662 179.05C271.216 171.018 275.621 163.505 282.876 156.509C290.13 149.255 298.94 143.425 309.303 139.021C319.926 134.616 331.067 132.414 342.726 132.414C360.863 132.414 376.927 137.078 390.918 146.405C404.909 155.473 415.92 168.039 423.952 184.103C431.984 199.907 436 218.173 436 238.9C436 259.109 431.984 277.246 423.952 293.309C415.92 309.373 404.909 322.068 390.918 331.396C376.927 340.464 360.992 344.998 343.115 344.998C331.197 344.998 319.797 342.666 308.915 338.002C298.033 333.339 288.835 327.38 281.321 320.125C273.807 312.611 269.014 304.58 266.941 296.03L279.378 290.589V427L227.3 371.862ZM331.844 299.916C342.467 299.916 351.795 297.325 359.827 292.143C368.118 286.961 374.595 279.836 379.259 270.768C383.922 261.441 386.254 250.818 386.254 238.9C386.254 226.723 383.922 216.1 379.259 207.032C374.595 197.705 368.247 190.45 360.215 185.268C352.183 180.087 342.726 177.496 331.844 177.496C321.222 177.496 311.765 180.087 303.474 185.268C295.442 190.45 289.094 197.705 284.43 207.032C279.767 216.1 277.435 226.723 277.435 238.9C277.435 250.818 279.767 261.441 284.43 270.768C289.094 279.836 295.442 286.961 303.474 292.143C311.765 297.325 321.222 299.916 331.844 299.916Z" fill="currentColor"/>
            <path d="M115.253 342.277V213.638V85H166.942V213.638V342.277H115.253ZM76 184.102V137.077H210.47V184.102H76Z" fill="currentColor"/>
          </svg>
        </div>
        <span>Traverse</span>
      </div>
      <p class="auth-gate-msg">API key required</p>
      <p class="auth-gate-sub">Enter the API key shown in the server startup banner to connect.</p>

      <form class="auth-gate-form" @submit.prevent="submit">
        <input
          v-model="keyInput"
          type="password"
          placeholder="tvs_..."
          class="auth-gate-input"
          autocomplete="off"
          spellcheck="false"
        />
        <button type="submit" class="auth-gate-btn" :disabled="loading || !keyInput.trim()">
          {{ loading ? 'Connecting...' : 'Connect' }}
        </button>
      </form>

      <p v-if="error" class="auth-gate-error">{{ error }}</p>
    </div>
  </div>
</template>

<style scoped>
.auth-gate-overlay {
  position: fixed;
  inset: 0;
  z-index: 9998;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--tv-bg-base);
}

.auth-gate-card {
  text-align: center;
  max-width: 420px;
  padding: 48px 32px;
}

.auth-gate-logo {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 12px;
  font-size: 24px;
  font-weight: 600;
  color: var(--tv-text-primary);
  margin-bottom: 24px;
}

.auth-gate-mark {
  width: 48px;
  height: 48px;
  padding: 4px;
  border-radius: var(--tv-radius-md);
  background: var(--tv-text-primary);
  color: var(--tv-bg-base);
  display: flex;
  align-items: center;
  justify-content: center;
}

.auth-gate-mark svg {
  width: 100%;
  height: 100%;
}

.auth-gate-msg {
  font-size: 16px;
  color: var(--tv-text-primary);
  margin-bottom: 8px;
}

.auth-gate-sub {
  font-size: 13px;
  color: var(--tv-text-secondary);
  margin-bottom: 24px;
}

.auth-gate-form {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.auth-gate-input {
  padding: 10px 14px;
  font-size: 14px;
  font-family: monospace;
  color: var(--tv-text-primary);
  background: var(--tv-bg-surface);
  border: 1px solid var(--tv-border-default);
  border-radius: 6px;
  outline: none;
  text-align: center;
}

.auth-gate-input:focus {
  border-color: var(--tv-accent);
}

.auth-gate-btn {
  display: inline-block;
  padding: 10px 28px;
  font-size: 14px;
  font-weight: 500;
  color: var(--tv-text-inverse);
  background: var(--tv-accent);
  border: none;
  border-radius: 6px;
  cursor: pointer;
  transition: background 0.15s;
}

.auth-gate-btn:hover:not(:disabled) {
  background: var(--tv-accent-hover);
}

.auth-gate-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.auth-gate-error {
  margin-top: 12px;
  color: var(--tv-status-error);
  font-size: 13px;
}
</style>
