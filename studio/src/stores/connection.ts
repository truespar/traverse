import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'
import { api, type HealthResponse, type DatabaseInfo, getApiKey, clearApiKey, setAuthFailedHandler } from '../utils/api'

export const useConnectionStore = defineStore('connection', () => {
  const isConnected = ref(false)
  const databases = ref<DatabaseInfo[]>([])
  const currentDatabase = ref(localStorage.getItem('tv-studio-database') || 'traverse')
  const memory = ref(0)
  const dbMemory = ref(0)
  const nodeCount = ref(0)
  const edgeCount = ref(0)
  const uptime = ref(0)
  const version = ref('')
  const allowDropDatabase = ref(false)
  const authRequired = ref(true) // assume auth required until proven otherwise
  // Compile-time server features (e.g. gql, mcp, ml). Used to hide
  // UI for things the running server can't actually do.
  const features = ref<Record<string, boolean>>({})

  // Whether the active backend runs the engine in-tab (WASM) vs.
  // server-proxied (HTTP). Set once at boot via `api.capabilities`;
  // `serverFiles=false` is the canonical WASM indicator.
  const isWasmMode = ref(!api.capabilities.serverFiles)

  // The Graph Assistant: a server answers with the model it's configured
  // with (`ai_provider`), which health reports as `ai`. The browser build
  // has none.
  const aiChatAvailable = computed(
    () => api.capabilities.aiChat && (isWasmMode.value || features.value.ai === true),
  )

  // OPFS usage (WASM only). Polled alongside health so the header
  // chip stays current. Both values in bytes; `null` when the browser
  // doesn't expose `navigator.storage.estimate()`.
  const opfsUsage = ref<number | null>(null)
  const opfsQuota = ref<number | null>(null)

  // wasm32 caps linear memory at 4 GiB. Engine `estimated_memory()`
  // counts only node/edge/property/index storage - actual WASM heap
  // includes the executor's working set, intermediate allocations,
  // and serialization buffers, easily 2-3x the engine estimate. The
  // thresholds below are tuned conservatively against the engine
  // figure: `warn` ~ 1.5 GiB, `danger` ~ 2.5 GiB. Crossing `warn`
  // tints the memory chip amber; `danger` triggers the in-app banner
  // so the user has a chance to commit + close before the tab dies.
  const WASM_MEMORY_WARN_BYTES = 1_500_000_000
  const WASM_MEMORY_DANGER_BYTES = 2_500_000_000
  const wasmMemoryWarn = computed(
    () => isWasmMode.value && (dbMemory.value ?? 0) >= WASM_MEMORY_WARN_BYTES,
  )
  const wasmMemoryDanger = computed(
    () => isWasmMode.value && (dbMemory.value ?? 0) >= WASM_MEMORY_DANGER_BYTES,
  )

  // Track whether the stored API key has been validated against the server.
  // Reset on disconnect or when a server restart is detected (uptime drops).
  let keyValidated = false
  let lastUptime = -1

  // Global 401 handler - any request that gets 401 flips us back to the login screen.
  setAuthFailedHandler(() => {
    authRequired.value = true
    keyValidated = false
  })

  watch(currentDatabase, (db) => localStorage.setItem('tv-studio-database', db))

  let pollTimer: ReturnType<typeof setInterval> | null = null

  /** Refresh OPFS usage/quota - called alongside health when in WASM
   *  mode. Fails silently if the browser doesn't expose the storage
   *  estimate API (older Safari, locked-down profiles). */
  async function pollOpfsQuota() {
    if (!isWasmMode.value) return
    if (!('storage' in navigator) || typeof navigator.storage.estimate !== 'function') return
    try {
      const est = await navigator.storage.estimate()
      opfsUsage.value = est.usage ?? null
      opfsQuota.value = est.quota ?? null
    } catch {
      // Estimate can fail in incognito or under permission policies.
    }
  }

  async function pollHealth() {
    try {
      const data: HealthResponse = await api.health()

      // Detect server restart: uptime drops below previous value
      if (lastUptime >= 0 && data.uptime_secs < lastUptime) {
        keyValidated = false
      }
      lastUptime = data.uptime_secs

      // If auth is required and we have a stored key that hasn't been validated,
      // test it against a protected endpoint. If invalid (server restarted with
      // a new key), clear it so the login screen appears.
      if (data.auth_required && getApiKey() && !keyValidated) {
        try {
          await api.databases()
          keyValidated = true
        } catch {
          // 401 handler in request() already cleared the key
          // For any other error, don't invalidate - might be a transient failure
        }
      }

      const needsAuth = !!data.auth_required && !getApiKey()

      isConnected.value = true
      version.value = data.version
      uptime.value = data.uptime_secs
      memory.value = data.memory_bytes
      databases.value = data.databases
      allowDropDatabase.value = !!data.allow_drop_database
      features.value =
        data.features && typeof data.features === 'object'
          ? (data.features as Record<string, boolean>)
          : {}

      const db = data.databases.find((d) => d.name === currentDatabase.value) || data.databases[0]
      if (db) {
        currentDatabase.value = db.name
        nodeCount.value = db.nodes
        edgeCount.value = db.edges
        dbMemory.value = db.memory_bytes
      }

      // Set auth LAST - this is the gate that watchers check, so all other
      // state must be settled before flipping it.
      authRequired.value = needsAuth
    } catch {
      isConnected.value = false
      keyValidated = false
      lastUptime = -1
    }
  }

  function startPolling() {
    pollHealth()
    pollOpfsQuota()
    pollTimer = setInterval(() => {
      pollHealth()
      pollOpfsQuota()
    }, 5000)
  }

  function stopPolling() {
    if (pollTimer) {
      clearInterval(pollTimer)
      pollTimer = null
    }
  }

  return {
    isConnected,
    databases,
    currentDatabase,
    memory,
    dbMemory,
    nodeCount,
    edgeCount,
    uptime,
    version,
    allowDropDatabase,
    authRequired,
    features,
    isWasmMode,
    aiChatAvailable,
    opfsUsage,
    opfsQuota,
    wasmMemoryWarn,
    wasmMemoryDanger,
    pollHealth,
    pollOpfsQuota,
    startPolling,
    stopPolling,
  }
})
