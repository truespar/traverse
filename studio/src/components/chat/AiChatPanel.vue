<script setup lang="ts">
import { ref, watch, nextTick, computed, onMounted, onUnmounted } from 'vue'
import { useChatStore } from '../../stores/chat'
import { useConnectionStore } from '../../stores/connection'
import { useQueryStore } from '../../stores/query'
import { useSettingsStore } from '../../stores/settings'
import MarkdownRender from 'markstream-vue'
import 'markstream-vue/index.css'

const chat = useChatStore()
const conn = useConnectionStore()
const queryStore = useQueryStore()
const settings = useSettingsStore()

const inputText = ref('')
const messagesEl = ref<HTMLElement | null>(null)
const textareaEl = ref<HTMLTextAreaElement | null>(null)
const showClearModal = ref(false)

const canSend = computed(() => inputText.value.trim().length > 0 && !chat.isStreaming)

function send() {
  const text = inputText.value.trim()
  if (!text || chat.isStreaming) return
  inputText.value = ''
  chat.sendMessage(text)
  resetTextareaHeight()
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault()
    send()
  }
}

function autoResizeTextarea() {
  const ta = textareaEl.value
  if (!ta) return
  ta.style.height = 'auto'
  ta.style.height = Math.min(ta.scrollHeight, 120) + 'px'
}

function resetTextareaHeight() {
  const ta = textareaEl.value
  if (!ta) return
  ta.style.height = 'auto'
}

function newChat() {
  if (chat.messages.length === 0) return
  showClearModal.value = true
}

function confirmClear() {
  showClearModal.value = false
  chat.clearConversation()
}

function useQuery(code: string) {
  queryStore.currentQuery = code
}

// Event delegation for code block action buttons
function onMessagesClick(e: MouseEvent) {
  const target = e.target as HTMLElement
  const btn = target.closest('[data-code-action]') as HTMLElement | null
  if (!btn) return
  const wrapper = btn.closest('.ai-chat__code-block')
  const codeEl = wrapper?.querySelector('pre code') as HTMLElement | null
  const text = codeEl?.textContent || ''
  if (!text) return

  const action = btn.getAttribute('data-code-action')
  if (action === 'use') {
    useQuery(text)
  } else if (action === 'copy') {
    navigator.clipboard.writeText(text)
    btn.textContent = 'Copied!'
    setTimeout(() => { btn.textContent = 'Copy' }, 1500)
  }
}

// Auto-scroll during streaming
watch(
  () => chat.messages.length > 0 ? chat.messages[chat.messages.length - 1].content : '',
  () => {
    nextTick(() => {
      const el = messagesEl.value
      if (el) el.scrollTop = el.scrollHeight
    })
  },
)

watch(
  () => chat.messages.length,
  () => {
    nextTick(() => {
      const el = messagesEl.value
      if (el) el.scrollTop = el.scrollHeight
    })
  },
)

// Wrap a <pre data-language> with a header bar (language + Copy / Use Query).
function wrapPreElement(pre: HTMLElement) {
  if (pre.closest('.ai-chat__code-block')) return // already wrapped
  const lang = pre.getAttribute('data-language') || ''
  const isCypher = lang === 'cypher'

  const wrapper = document.createElement('div')
  wrapper.className = 'ai-chat__code-block'

  const header = document.createElement('div')
  header.className = 'ai-chat__code-header'

  const langLabel = document.createElement('span')
  langLabel.className = 'ai-chat__code-lang'
  langLabel.textContent = lang
  header.appendChild(langLabel)

  const actions = document.createElement('div')
  actions.className = 'ai-chat__code-actions'

  const copyBtn = document.createElement('button')
  copyBtn.className = 'ai-chat__code-btn'
  copyBtn.setAttribute('data-code-action', 'copy')
  copyBtn.textContent = 'Copy'
  actions.appendChild(copyBtn)

  if (isCypher) {
    const useBtn = document.createElement('button')
    useBtn.className = 'ai-chat__code-btn ai-chat__code-btn--primary'
    useBtn.setAttribute('data-code-action', 'use')
    useBtn.textContent = 'Use Query'
    actions.appendChild(useBtn)
  }

  header.appendChild(actions)
  pre.parentNode?.insertBefore(wrapper, pre)
  wrapper.appendChild(header)
  wrapper.appendChild(pre)
}

// Use MutationObserver to catch <pre> elements as markstream-vue renders them.
// This works for streaming, saved chats, and database switches.
let observer: MutationObserver | null = null

function startObserver() {
  const el = messagesEl.value
  if (!el || observer) return
  // Wrap any <pre> already in the DOM (e.g. restored from localStorage)
  el.querySelectorAll('pre[data-language]').forEach((pre) => wrapPreElement(pre as HTMLElement))

  observer = new MutationObserver((mutations) => {
    for (const mut of mutations) {
      for (const node of mut.addedNodes) {
        if (!(node instanceof HTMLElement)) continue
        // The added node itself might be a <pre>
        if (node.matches('pre[data-language]')) {
          wrapPreElement(node)
        }
        // Or it might contain <pre> descendants
        node.querySelectorAll?.('pre[data-language]')?.forEach((pre) => {
          wrapPreElement(pre as HTMLElement)
        })
      }
    }
  })
  observer.observe(el, { childList: true, subtree: true })
}

function stopObserver() {
  observer?.disconnect()
  observer = null
}

onMounted(() => nextTick(startObserver))
onUnmounted(stopObserver)

// Restart observer if the messages container remounts (e.g. v-if toggle)
watch(messagesEl, (el) => {
  stopObserver()
  if (el) nextTick(startObserver)
})

const suggestions = [
  'What labels and relationships are in this graph?',
  'Help me write a query to find connected nodes',
  'Explain the current query',
  'Find the shortest path between two nodes',
  'What is Traverse and what is it good at?',
  'How fast is Traverse for deep traversals?',
  'What openCypher features does Traverse support?',
  'Suggest an index to speed up my query',
]
</script>

<template>
  <div class="ai-chat">
    <!-- Header -->
    <div class="ai-chat__header">
      <span class="ai-chat__title">Graph Assistant <span class="ai-chat__db-name">{{ conn.currentDatabase }}</span></span>
      <div class="ai-chat__actions">
        <button
          class="ai-chat__action-btn"
          title="New Chat"
          @click="newChat"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 5v14M5 12h14" />
          </svg>
        </button>
        <button
          class="ai-chat__action-btn"
          title="Close"
          @click="settings.aiChatOpen = false"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M18 6 6 18M6 6l12 12" />
          </svg>
        </button>
      </div>
    </div>

    <!-- Messages -->
    <div
      ref="messagesEl"
      class="ai-chat__messages"
      @click="onMessagesClick"
    >
      <!-- Empty state -->
      <div v-if="chat.messages.length === 0" class="ai-chat__empty">
        <div class="ai-chat__empty-icon">
          <svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <path d="M20.341 6.484A10 10 0 0 1 10.266 21.85" />
            <path d="M3.659 17.516A10 10 0 0 1 13.74 2.152" />
            <circle cx="12" cy="12" r="3" />
            <circle cx="19" cy="5" r="2" />
            <circle cx="5" cy="19" r="2" />
          </svg>
        </div>
        <p class="ai-chat__empty-title">Ask about your graph</p>
        <p class="ai-chat__empty-subtitle">Get Cypher queries, explore your schema, or ask for help.</p>
        <p class="ai-chat__privacy-notice">Your query data is never sent - only the schema and execution times are shared with the assistant.</p>
        <div class="ai-chat__suggestions">
          <button
            v-for="s in suggestions"
            :key="s"
            class="ai-chat__suggestion"
            @click="inputText = s; send()"
          >
            {{ s }}
          </button>
        </div>
      </div>

      <!-- Message list -->
      <template v-else>
        <div
          v-for="(msg, i) in chat.messages"
          :key="i"
          class="ai-chat__msg"
          :class="'ai-chat__msg--' + msg.role"
        >
          <div v-if="msg.role === 'user'" class="ai-chat__msg-content ai-chat__msg-user">
            {{ msg.content }}
          </div>
          <div v-else class="ai-chat__msg-content ai-chat__msg-assistant">
            <MarkdownRender
              :content="msg.content"
              :final="!chat.isStreaming || i !== chat.messages.length - 1"
              :batch-rendering="chat.isStreaming && i === chat.messages.length - 1"
              :typewriter="false"
              :render-code-blocks-as-pre="true"
              :is-dark="settings.theme === 'dark'"
              class="ai-chat__markdown"
            />
          </div>
        </div>
        <div v-if="chat.isStreaming && chat.messages.length > 0 && !chat.messages[chat.messages.length - 1].content" class="ai-chat__typing">
          <span class="ai-chat__dot" /><span class="ai-chat__dot" /><span class="ai-chat__dot" />
        </div>
      </template>

      <!-- Error -->
      <div v-if="chat.streamError" class="ai-chat__error">
        {{ chat.streamError }}
      </div>
    </div>

    <!-- Input -->
    <div class="ai-chat__input-area">
      <textarea
        ref="textareaEl"
        v-model="inputText"
        class="ai-chat__textarea"
        placeholder="Ask about your graph..."
        rows="1"
        :disabled="chat.isStreaming"
        @keydown="onKeydown"
        @input="autoResizeTextarea"
      />
      <button
        v-if="chat.isStreaming"
        class="ai-chat__send ai-chat__send--stop"
        title="Stop"
        @click="chat.stopStreaming()"
      >
        <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
          <rect x="6" y="6" width="12" height="12" rx="2" />
        </svg>
      </button>
      <button
        v-else
        class="ai-chat__send"
        :disabled="!canSend"
        title="Send"
        @click="send"
      >
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="m5 12 7-7 7 7M12 19V5" />
        </svg>
      </button>
    </div>

    <!-- Clear chat confirmation modal -->
    <Teleport to="body">
      <Transition name="modal">
        <div v-if="showClearModal" class="fg-modal-overlay" @mousedown.self="showClearModal = false">
          <div class="drop-modal" role="dialog" aria-modal="true" aria-label="Clear conversation" @click.stop>
            <div class="drop-modal__header">Clear conversation</div>
            <div class="drop-modal__body">
              <p>This will delete the current conversation for <strong>{{ conn.currentDatabase }}</strong>. This cannot be undone.</p>
            </div>
            <div class="drop-modal__footer">
              <button class="fg-btn fg-btn--sm" @click="showClearModal = false">Cancel</button>
              <button class="fg-btn fg-btn--sm drop-modal__confirm-btn" @click="confirmClear">Clear Chat</button>
            </div>
          </div>
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<style scoped>
.ai-chat {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--tv-bg-base);
  border-left: 1px solid var(--tv-border-default);
}

.ai-chat__header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 12px;
  height: var(--tv-header-height);
  border-bottom: 1px solid var(--tv-border-default);
  flex-shrink: 0;
}

.ai-chat__title {
  font-size: 13px;
  font-weight: 600;
  color: var(--tv-text-primary);
}
.ai-chat__db-name {
  font-weight: 400;
  color: var(--tv-text-muted);
  font-size: 12px;
}
.ai-chat__db-name::before {
  content: '-';
  margin: 0 5px;
}

.ai-chat__actions {
  display: flex;
  gap: 4px;
}

.ai-chat__action-btn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: transparent;
  color: var(--tv-text-muted);
  cursor: pointer;
}
.ai-chat__action-btn:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}

.ai-chat__messages {
  flex: 1;
  overflow-y: auto;
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

/* Empty state */
.ai-chat__empty {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  flex: 1;
  text-align: center;
  padding: 24px 16px;
}
.ai-chat__empty-icon {
  color: var(--tv-text-muted);
  margin-bottom: 12px;
  opacity: 0.5;
}
.ai-chat__empty-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--tv-text-primary);
  margin: 0 0 4px;
}
.ai-chat__empty-subtitle {
  font-size: 12px;
  color: var(--tv-text-muted);
  margin: 0 0 16px;
}
.ai-chat__privacy-notice {
  font-size: 11px;
  color: var(--tv-text-muted);
  margin: 0 0 16px;
  opacity: 0.7;
}
.ai-chat__suggestions {
  display: flex;
  flex-direction: column;
  gap: 6px;
  width: 100%;
}
.ai-chat__suggestion {
  padding: 8px 12px;
  font-size: 12px;
  text-align: left;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-surface);
  color: var(--tv-text-secondary);
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s;
}
.ai-chat__suggestion:hover {
  background: var(--tv-bg-hover);
  border-color: var(--tv-accent);
}

/* Messages */
.ai-chat__msg {
  display: flex;
}
.ai-chat__msg--user {
  justify-content: flex-end;
}
.ai-chat__msg--assistant {
  justify-content: flex-start;
}

.ai-chat__msg-content {
  max-width: 90%;
  font-size: 13px;
  line-height: 1.5;
}

.ai-chat__msg-user {
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
  padding: 8px 12px;
  border-radius: 12px 12px 2px 12px;
  white-space: pre-wrap;
  word-break: break-word;
}

.ai-chat__msg-assistant {
  color: var(--tv-text-primary);
  padding: 4px 0;
  min-width: 0;
  overflow: hidden;
  /* Take the full message column so embedded tables / code blocks
     fill the available chat-panel width instead of shrinking the
     whole bubble to the table's intrinsic width (~241px). User
     messages keep `max-width: 90%` from the shared rule above - they
     stay right-aligned and chat-bubble-shaped. */
  width: 100%;
}

/* Typing indicator */
.ai-chat__typing {
  display: flex;
  gap: 4px;
  padding: 8px 12px;
}
.ai-chat__dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--tv-text-muted);
  animation: chat-bounce 1.4s infinite;
}
.ai-chat__dot:nth-child(2) { animation-delay: 0.2s; }
.ai-chat__dot:nth-child(3) { animation-delay: 0.4s; }

@keyframes chat-bounce {
  0%, 60%, 100% { opacity: 0.3; transform: translateY(0); }
  30% { opacity: 1; transform: translateY(-4px); }
}

/* Error */
.ai-chat__error {
  padding: 8px 12px;
  font-size: 12px;
  color: var(--tv-text-danger);
  background: var(--tv-bg-danger-subtle);
  border-radius: var(--tv-radius-sm);
}

/* Input area */
.ai-chat__input-area {
  display: flex;
  align-items: flex-end;
  gap: 8px;
  padding: 8px 12px;
  border-top: 1px solid var(--tv-border-default);
  flex-shrink: 0;
}

.ai-chat__textarea {
  flex: 1;
  resize: none;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-surface);
  color: var(--tv-text-primary);
  font-family: inherit;
  font-size: 13px;
  padding: 8px 10px;
  line-height: 1.4;
  outline: none;
  max-height: 120px;
}
.ai-chat__textarea:focus {
  border-color: var(--tv-accent);
}
.ai-chat__textarea:disabled {
  opacity: 0.5;
}

.ai-chat__send {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 32px;
  height: 32px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
  cursor: pointer;
  flex-shrink: 0;
  margin-bottom: 1px;
  transition: opacity 0.15s;
}
.ai-chat__send:disabled {
  opacity: 0.4;
  cursor: default;
}
.ai-chat__send--stop {
  background: var(--tv-text-danger);
}

</style>

<style>
/*
 * markstream-vue overrides for the Graph Assistant chat panel.
 *
 * markstream-vue ships Tailwind utility classes (bg-[hsl(var(--secondary))],
 * border-[var(--table-border,#cbd5e1)], etc.) that reference CSS variables we
 * don't define. We override everything here to use our --tv-* design tokens
 * so both light and dark themes work correctly.
 */

/* -- Root container ------------------------------------------------ */
.ai-chat__markdown.markstream-vue {
  font-size: 13px;
  line-height: 1.5;
  color: var(--tv-text-primary);
  /* Set CSS variables that markstream-vue's Tailwind classes reference */
  --table-border: var(--tv-border-default);
  --secondary: 0 0% 50%; /* fallback hsl for bg-[hsl(var(--secondary))] */
}

/* -- Kill all transitions/animations for snappy streaming ------ */
.ai-chat__markdown.markstream-vue *,
.ai-chat__markdown.markstream-vue *::before,
.ai-chat__markdown.markstream-vue *::after {
  transition: none !important;
  animation-duration: 0s !important;
}

.ai-chat__markdown .node-slot { display: block !important; }
.ai-chat__markdown .node-content { display: block !important; }
.ai-chat__markdown .typewriter-enter-from,
.ai-chat__markdown .typewriter-enter-to,
.ai-chat__markdown .typewriter-enter-active {
  opacity: 1 !important;
  transition: none !important;
}

/* -- Loading placeholders -------------------------------------- */
.ai-chat__markdown .node-placeholder,
.ai-chat__markdown .skeleton-line,
.ai-chat__markdown .loading-skeleton,
.ai-chat__markdown .code-loading-placeholder,
.ai-chat__markdown .code-height-placeholder {
  animation: none !important;
  background-image: none !important;
  opacity: 1 !important;
}

/* -- Headings -------------------------------------------------- */
.ai-chat__markdown .heading-node {
  margin-top: 1em !important;
  margin-bottom: 0.4em !important;
  font-weight: 600 !important;
  color: var(--tv-text-primary) !important;
}
.ai-chat__markdown .heading-node.heading-1 { font-size: 1.1rem !important; }
.ai-chat__markdown .heading-node.heading-2 { font-size: 1rem !important; }
.ai-chat__markdown .heading-node.heading-3 { font-size: 0.9375rem !important; }
.ai-chat__markdown .heading-node.heading-4,
.ai-chat__markdown .heading-node.heading-5,
.ai-chat__markdown .heading-node.heading-6 { font-size: 0.875rem !important; }

/* -- Paragraphs ------------------------------------------------ */
.ai-chat__markdown .paragraph-node {
  margin: 0.5em 0 !important;
  color: var(--tv-text-primary) !important;
}
.ai-chat__markdown .paragraph-node:first-child {
  margin-top: 0 !important;
}

/* -- Text nodes ------------------------------------------------ */
.ai-chat__markdown .text-node {
  color: var(--tv-text-primary) !important;
}

/* -- Strong / emphasis ----------------------------------------- */
.ai-chat__markdown .strong-node {
  font-weight: 600 !important;
  color: var(--tv-text-primary) !important;
}
.ai-chat__markdown .emphasis-node { font-style: italic; }

/* -- Lists ----------------------------------------------------- */
.ai-chat__markdown .list-node {
  margin: 0.5em 0 !important;
  padding-left: 1.25em !important;
  color: var(--tv-text-primary) !important;
}
.ai-chat__markdown .list-node.list-disc { list-style-type: disc !important; }
.ai-chat__markdown .list-node.list-decimal { list-style-type: decimal !important; }
.ai-chat__markdown .list-item {
  margin: 0.15em 0 !important;
  padding-left: 0 !important;
  line-height: 1.5 !important;
  color: var(--tv-text-primary) !important;
}
.ai-chat__markdown .list-item .paragraph-node { margin: 0 !important; }

/* -- Inline code ----------------------------------------------- */
/* Override Tailwind: bg-[hsl(var(--secondary))] px-1 py-0.5 text-[85%] */
.ai-chat__markdown code {
  font-family: var(--tv-font-mono) !important;
  font-size: 12px !important;
  padding: 1px 4px !important;
  background: var(--tv-bg-surface) !important;
  border-radius: 3px !important;
  color: var(--tv-text-primary) !important;
}

/* -- Code blocks (renderCodeBlocksAsPre) ----------------------- */
.ai-chat__markdown pre {
  margin: 0.5em 0 !important;
  padding: 0 !important;
  background: var(--tv-bg-surface) !important;
  border: 1px solid var(--tv-border-default) !important;
  border-radius: var(--tv-radius-sm) !important;
  overflow-x: auto !important;
}
.ai-chat__markdown pre code {
  display: block !important;
  padding: 8px 12px !important;
  background: none !important;
  font-size: 12px !important;
  line-height: 1.4 !important;
  color: var(--tv-text-primary) !important;
  white-space: pre !important;
  width: fit-content !important;
  min-width: 100% !important;
  box-sizing: border-box !important;
}

/* -- Tables ----------------------------------------------------- */
/* Override Tailwind: border-[var(--table-border,#cbd5e1)], my-8, text-sm,
   font-semibold, p-[calc(4/7*1em)] */
.ai-chat__markdown .table-node-wrapper {
  margin: 0.5em 0 !important;
  overflow-x: auto !important;
  /* Stretch to the message column so the table can use the full chat
     panel width before horizontally scrolling. */
  width: 100% !important;
}
.ai-chat__markdown .table-node {
  border-collapse: collapse !important;
  font-size: 12px !important;
  /* Fill the wrapper; if the table's natural width exceeds it, the
     wrapper's `overflow-x: auto` kicks in. */
  width: 100% !important;
  margin: 0 !important;
  color: var(--tv-text-primary) !important;
}
.ai-chat__markdown .table-node thead {
  border-color: var(--tv-border-default) !important;
}
.ai-chat__markdown .table-node tr {
  border-color: var(--tv-border-default) !important;
}
.ai-chat__markdown .table-node th,
.ai-chat__markdown .table-node td {
  padding: 4px 8px !important;
  border: 1px solid var(--tv-border-default) !important;
  text-align: left !important;
  color: var(--tv-text-primary) !important;
  background: transparent !important;
}
.ai-chat__markdown .table-node th {
  background: var(--tv-bg-surface) !important;
  font-weight: 600 !important;
}

/* -- Blockquotes ----------------------------------------------- */
.ai-chat__markdown .blockquote {
  margin: 0.5em 0 !important;
  padding: 0.375em 0.75em !important;
  border-left: 3px solid var(--tv-accent) !important;
  background: var(--tv-bg-surface) !important;
  color: var(--tv-text-secondary) !important;
}

/* -- Horizontal rules ------------------------------------------ */
.ai-chat__markdown .hr-node {
  margin: 1em 0 !important;
  border: none !important;
  border-top: 1px solid var(--tv-border-default) !important;
}

/* -- Links ----------------------------------------------------- */
.ai-chat__markdown .link-node {
  color: var(--tv-accent) !important;
  text-decoration: none !important;
}
.ai-chat__markdown .link-node:hover {
  text-decoration: underline !important;
}

/* -- Code block header bar (DOM-injected) ---------------------- */
.ai-chat__code-block {
  margin: 0.5em 0;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  overflow: hidden;
}
.ai-chat__code-block pre {
  margin: 0 !important;
  border: none !important;
  border-radius: 0 !important;
}
.ai-chat__code-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 4px 8px;
  background: var(--tv-bg-surface);
  border-bottom: 1px solid var(--tv-border-default);
  font-size: 11px;
}
.ai-chat__code-lang {
  color: var(--tv-text-muted);
  text-transform: lowercase;
}
.ai-chat__code-actions {
  display: flex;
  gap: 4px;
}
.ai-chat__code-btn {
  padding: 1px 8px;
  font-size: 11px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: transparent;
  color: var(--tv-text-secondary);
  cursor: pointer;
  transition: background 0.15s, color 0.15s, border-color 0.15s;
}
.ai-chat__code-btn:hover {
  background: var(--tv-bg-hover);
  color: var(--tv-text-primary);
}
.ai-chat__code-btn--primary {
  border-color: var(--tv-accent);
  color: var(--tv-accent);
}
.ai-chat__code-btn--primary:hover {
  background: var(--tv-accent);
  color: var(--tv-text-inverse);
}
</style>
