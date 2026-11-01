import { createApp } from 'vue'
import { createPinia } from 'pinia'
import router from './router'
import App from './App.vue'
import '@fontsource-variable/inter'
import '@fontsource-variable/jetbrains-mono'
import './styles/base.css'
import './styles/components.css'
import { setApiClient } from './utils/api'

// Optional WASM boot mode - activated by any of:
//   - visiting `/try` (the public "Try Traverse" landing route)
//   - appending `?wasm=1` to any URL (developer override)
//   - building with `VITE_TRAVERSE_WASM=1` set
//
// In WASM mode the `WasmApiClient` runs the Traverse engine in-process
// via WebAssembly - no server, data lives in the browser tab's OPFS.
// Async dynamic-import keeps the WASM glue out of the default Studio
// bundle so the HTTP build stays small.
function shouldBootWasm(): boolean {
  return (
    location.pathname === '/try' ||
    location.pathname.startsWith('/try/') ||
    new URLSearchParams(location.search).get('wasm') === '1' ||
    import.meta.env.VITE_TRAVERSE_WASM === '1'
  )
}

async function bootApi(): Promise<void> {
  // Build-time gate. The server container builds Studio with
  // VITE_TRAVERSE_NO_WASM=1 so the dynamic `import('./utils/wasm-api')`
  // below gets tree-shaken - the server image never serves WASM mode
  // and shouldn't carry the wasm chunk. Vite replaces this expression
  // statically at build time so the whole branch becomes dead code.
  if (import.meta.env.VITE_TRAVERSE_NO_WASM === '1') return
  if (!shouldBootWasm()) return
  // SharedArrayBuffer is the gate for the threaded WASM build (rayon
  // pool). Browsers only expose it on cross-origin-isolated pages -
  // missing means either an old browser or the page is being served
  // without COEP/COOP. Detect early so we can show a helpful message
  // instead of a generic "WASM boot failed" with a wasm-bindgen stack.
  if (typeof SharedArrayBuffer === 'undefined') {
    throw new Error(
      'This browser blocks SharedArrayBuffer on the current page. ' +
      'Traverse needs cross-origin isolation - usually a Chrome/Edge/Firefox ' +
      'version from 2022 or later, with the page served via the bundled ' +
      'traverse-server or a host that sets Cross-Origin-Embedder-Policy: ' +
      'require-corp and Cross-Origin-Opener-Policy: same-origin.'
    )
  }
  // Surface a more informative loading state while the ~5 MB WASM
  // module + thread pool initialize. The static HTML scaffold shows a
  // spinner; we update its message as we progress through boot steps.
  setBootStatus('Loading Traverse runtime...')
  const { WasmApiClient } = await import('./utils/wasm-api')
  setBootStatus('Booting in-browser graph engine...')
  const client = await WasmApiClient.open()
  setApiClient(client)
  setBootStatus('Done.')
}

/** Update the pre-mount loading splash defined in `index.html`. No-op
 *  if the element has already been removed (e.g. when SSR / SPA hot-
 *  reload swaps the body). */
function setBootStatus(text: string): void {
  const el = document.getElementById('tv-boot-status')
  if (el) el.textContent = text
}

/** Remove the pre-mount loading splash. Called after the Vue app has
 *  taken over the `#app` element. */
function clearBootSplash(): void {
  const el = document.getElementById('tv-boot-splash')
  if (el) el.remove()
}

/** Decode a `#q=BASE64(JSON)` share-URL hash and stash the result
 *  in localStorage so the query store can pick it up on first mount
 *  via `tv-pending-share`. We touch localStorage rather than directly
 *  Pinia because Pinia stores aren't available until after the app
 *  mounts. The query store reads + clears the key on init.
 *
 *  Format is `SharePayload` from `utils/share.ts`:
 *      { q: string, t?: string, db?: string } */
function decodeShareHashOnBoot(): void {
  const m = location.hash.match(/[#&]q=([^&]+)/)
  if (!m) return
  try {
    const b64 = m[1].replace(/-/g, '+').replace(/_/g, '/')
    const padded = b64 + '='.repeat((4 - (b64.length % 4)) % 4)
    const json = decodeURIComponent(escape(atob(padded)))
    const payload = JSON.parse(json) as { q?: string; t?: string; db?: string }
    if (typeof payload.q === 'string' && payload.q.length > 0) {
      localStorage.setItem('tv-pending-share', JSON.stringify(payload))
    }
  } catch (err) {
    console.warn('Traverse: failed to decode share hash', err)
  }
  // Clear the hash from the URL bar so a refresh doesn't keep
  // re-priming the editor. `history.replaceState` keeps the path /
  // search params intact.
  history.replaceState(null, '', location.pathname + location.search)
}

decodeShareHashOnBoot()

// iOS soft-keyboard handling. When the keyboard slides up, the visual
// viewport shrinks but layout viewport (and `100vh`) doesn't, so any
// `position: fixed; bottom: 0` element stays anchored to the screen
// edge - under the keyboard. Push a CSS variable that bottom-fixed
// pieces (activity bar, chat input, Run button) can subtract from.
function trackKeyboardInset() {
  const vv = window.visualViewport
  if (!vv) return
  const update = () => {
    const inset = Math.max(0, window.innerHeight - vv.height - vv.offsetTop)
    document.documentElement.style.setProperty('--tv-keyboard-inset', `${inset}px`)
  }
  vv.addEventListener('resize', update)
  vv.addEventListener('scroll', update)
  update()
}
trackKeyboardInset()

bootApi()
  .catch((err) => {
    // Surface boot failures rather than silently falling through to
    // the HTTP client - WASM mode requires SharedArrayBuffer + COEP/COOP
    // and we want the user to see why it failed.
    console.error('Traverse WASM boot failed - falling back to HTTP client:', err)
    setBootStatus(`WASM boot failed: ${err?.message ?? err}. Falling back to HTTP client.`)
  })
  .finally(() => {
    const app = createApp(App)
    app.use(createPinia())
    app.use(router)
    app.mount('#app')
    clearBootSplash()
  })
