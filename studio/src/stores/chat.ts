import { defineStore } from 'pinia'
import { ref, computed, watch } from 'vue'
import { useConnectionStore } from './connection'
import { useSchemaStore } from './schema'
import { useQueryStore } from './query'
import { useGdsStore } from './gds'
import { streamAiChat, type AiChatMessage, type AiChatMetadata } from '../utils/api'

const STORAGE_KEY = 'tv_chat_conversations'

export interface ChatMessage {
  role: 'user' | 'assistant'
  content: string
}

function loadConversations(): Map<string, ChatMessage[]> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (!raw) return new Map()
    const entries: [string, ChatMessage[]][] = JSON.parse(raw)
    return new Map(entries)
  } catch {
    return new Map()
  }
}

function saveConversations(map: Map<string, ChatMessage[]>) {
  try {
    // Only persist conversations that have content
    const entries = Array.from(map.entries()).filter(([, msgs]) => msgs.length > 0)
    localStorage.setItem(STORAGE_KEY, JSON.stringify(entries))
  } catch {
    // localStorage quota exceeded - ignore
  }
}

export const useChatStore = defineStore('chat', () => {
  const conn = useConnectionStore()
  const schema = useSchemaStore()
  const query = useQueryStore()
  const gds = useGdsStore()

  // Per-database conversations (loaded from localStorage)
  const conversations = ref<Map<string, ChatMessage[]>>(loadConversations())
  const isStreaming = ref(false)
  const streamError = ref<string | null>(null)

  let abortController: AbortController | null = null

  const messages = computed(() => {
    return conversations.value.get(conn.currentDatabase) ?? []
  })

  function ensureConversation(): ChatMessage[] {
    const db = conn.currentDatabase
    if (!conversations.value.has(db)) {
      conversations.value.set(db, [])
    }
    return conversations.value.get(db)!
  }

  function buildMetadata(): AiChatMetadata {
    // Build graph schema string from schema store
    const lines: string[] = []

    if (schema.labelsDetail.length > 0) {
      lines.push('Node labels:')
      for (const label of schema.labelsDetail) {
        const props = label.properties.map((p) => p.name).join(', ')
        lines.push(`  ${label.name}${props ? ` (${props})` : ''}`)
      }
    }

    if (schema.relationshipTypesDetail.length > 0) {
      lines.push('Relationship types:')
      for (const rel of schema.relationshipTypesDetail) {
        const props = rel.properties.map((p) => p.name).join(', ')
        lines.push(`  ${rel.name}${props ? ` (${props})` : ''}`)
      }
    }

    if (schema.indexes.length > 0) {
      lines.push('Indexes:')
      for (const idx of schema.indexes) {
        lines.push(`  ${idx.name}: ${idx.label}.${idx.property} (${idx.entries} entries)`)
      }
    }

    const metadata: AiChatMetadata = {
      graphSchema: lines.join('\n'),
    }

    metadata.queryDialect = query.currentDialect
    if (query.currentQuery) {
      metadata.currentQuery = query.currentQuery
    }
    if (query.timeMs > 0) {
      metadata.lastQueryTimeMs = query.timeMs
    }
    if (query.planningTimeMs > 0) {
      metadata.lastQueryPlanningMs = query.planningTimeMs
    }
    if (query.executionTimeMs > 0) {
      metadata.lastQueryExecutionMs = query.executionTimeMs
    }

    // GDS catalog - one line per algorithm, grouped by category. Sent
    // so the AI can suggest exact algorithm names + supported modes
    // instead of hallucinating them.
    if (gds.availableAlgorithms.length > 0) {
      const gdsLines: string[] = []
      for (const group of gds.algorithmsByCategory) {
        gdsLines.push(`[${group.category}]`)
        for (const a of group.algorithms) {
          const modes = a.modes.join('/')
          gdsLines.push(`  ${a.name} (${a.displayName}, modes: ${modes}) - ${a.description}`)
        }
      }
      metadata.gdsAlgorithms = gdsLines.join('\n')
    }

    return metadata
  }

  async function sendMessage(content: string) {
    const msgs = ensureConversation()
    msgs.push({ role: 'user', content })
    msgs.push({ role: 'assistant', content: '' })

    // Trigger reactivity
    conversations.value = new Map(conversations.value)

    isStreaming.value = true
    streamError.value = null
    abortController = new AbortController()

    // Ensure the GDS catalog is loaded so it can be included in the
    // metadata payload - the AI uses it to recommend exact algorithm
    // names without hallucinating. Cached on subsequent calls.
    if (gds.availableAlgorithms.length === 0) {
      try { await gds.loadAlgorithms() } catch { /* non-fatal */ }
    }

    const apiMessages: AiChatMessage[] = msgs
      .filter((m) => m.role === 'user' || (m.role === 'assistant' && m.content))
      .map((m) => ({ role: m.role, content: m.content }))

    const metadata = buildMetadata()

    try {
      const assistantMsg = msgs[msgs.length - 1]
      await streamAiChat(
        apiMessages,
        metadata,
        (delta) => {
          assistantMsg.content += delta
          // Trigger reactivity
          conversations.value = new Map(conversations.value)
        },
        abortController.signal,
      )
    } catch (e: unknown) {
      if (e instanceof DOMException && e.name === 'AbortError') {
        // User cancelled - keep partial response
      } else {
        const errMsg = e instanceof Error ? e.message : 'Unknown error'
        streamError.value = errMsg
        // Remove the empty assistant message if nothing was streamed
        const last = msgs[msgs.length - 1]
        if (last.role === 'assistant' && !last.content) {
          msgs.pop()
          conversations.value = new Map(conversations.value)
        }
      }
    } finally {
      isStreaming.value = false
      abortController = null
    }
  }

  function stopStreaming() {
    abortController?.abort()
  }

  function clearConversation() {
    conversations.value.set(conn.currentDatabase, [])
    conversations.value = new Map(conversations.value)
    streamError.value = null
  }

  // Persist to localStorage whenever conversations change
  watch(conversations, (map) => saveConversations(map), { deep: true })

  return {
    messages,
    isStreaming,
    streamError,
    sendMessage,
    stopStreaming,
    clearConversation,
  }
})
