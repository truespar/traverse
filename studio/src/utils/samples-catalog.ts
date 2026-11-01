// Fetches the public sample-database catalog at
// `https://releases.truespar.com/traverse/samples/index.json` and
// exposes typed entries to the Studio. The catalog lists pre-built
// `.tvdb` files that visitors can load in seconds instead of
// rebuilding via Cypher.
//
// The same catalog drives both backends:
// - WASM mode: fetch the .tvdb -> `db.writeBytes` -> `db.load`
// - HTTP mode: fetch the .tvdb -> `api.uploadDatabase` -> `api.loadDatabase`
//
// Override the catalog URL at build time with
// `VITE_TRAVERSE_SAMPLES_URL` for staging / self-hosted forks.

import { ref } from 'vue'

const DEFAULT_CATALOG_URL =
  (import.meta.env.VITE_TRAVERSE_SAMPLES_URL as string | undefined) ??
  'https://releases.truespar.com/traverse/samples/index.json'

/** Module-scoped reactive state for the samples panel. AppShell
 *  mounts panels with `v-if`, so component-local refs are wiped
 *  every time the user switches the activity-bar tab. Hoisting the
 *  catalog state here means the cards keep their "Loaded" badge and
 *  the section stays populated across re-mounts; the catalog fetch
 *  only runs the first time anyone opens the panel in this tab. */
export const samplesState = {
  samples: ref<SampleEntry[]>([]),
  loading: ref(false),
  error: ref<string | null>(null),
  /** Per-sample status string for the in-progress flow (e.g.
   *  "Downloading 42%" / "Loading into memory..."). `null` when idle. */
  status: ref<Record<string, string | null>>({}),
  /** Last sample the user successfully loaded - keys the "Loaded"
   *  badge + starter-query list. */
  loadedId: ref<string | null>(null),
}

export interface SampleStarterQuery {
  label: string
  query: string
}

export interface SampleEntry {
  id: string
  name: string
  /** Free-form tag - used for grouping/coloring in the UI. */
  category: string
  description: string
  /** Approximate node + edge counts the catalog publishes. */
  nodes: number
  edges: number
  /** On-disk size of the `.tvdb` in bytes. */
  bytes: number
  /** Relative URL of the `.tvdb` (resolved against the catalog URL). */
  url: string
  tags?: string[]
  /** Demo queries shipped with the sample. Displayed under the
   *  sample card after load, and also persisted as saved queries so
   *  they appear in the Query Palette. Mirrors the
   *  `starterQueries` field on Studio's local `EXAMPLE_GRAPHS`. */
  starterQueries?: SampleStarterQuery[]
}

export interface SamplesCatalog {
  version: number
  samples: SampleEntry[]
}

/** Cache the catalog for the lifetime of the tab - it's a small JSON
 *  and won't change between user clicks. The releases site sets
 *  `Cache-Control: no-cache` on `index.json` so the next page reload
 *  pulls fresh content. */
let cached: { promise: Promise<SamplesCatalog>; url: string } | null = null

export async function fetchSamplesCatalog(): Promise<SamplesCatalog> {
  if (cached && cached.url === DEFAULT_CATALOG_URL) return cached.promise
  const promise = (async () => {
    const res = await fetch(DEFAULT_CATALOG_URL, {
      // CORS-safe - no credentials, no custom headers, GET only.
      // The releases.truespar.com web.config sets
      // `Access-Control-Allow-Origin: *` so any browser origin works.
      mode: 'cors',
    })
    if (!res.ok) {
      throw new Error(`samples catalog: HTTP ${res.status} ${res.statusText}`)
    }
    const json = (await res.json()) as SamplesCatalog
    if (!json || !Array.isArray(json.samples)) {
      throw new Error('samples catalog: malformed JSON (missing `samples` array)')
    }
    return json
  })()
  cached = { promise, url: DEFAULT_CATALOG_URL }
  return promise
}

/** Resolve a sample's relative URL against the catalog URL so each
 *  entry's `url: "movies.tvdb"` becomes the full
 *  `https://releases.truespar.com/traverse/samples/movies.tvdb`. */
export function sampleUrl(entry: SampleEntry): string {
  return new URL(entry.url, DEFAULT_CATALOG_URL).toString()
}

/** Download a sample's `.tvdb` bytes with progress reporting.
 *  Uses streaming `fetch` so the progress callback fires multiple
 *  times for large files. Throws on HTTP error or network failure. */
export async function downloadSampleBytes(
  entry: SampleEntry,
  onProgress?: (loaded: number, total: number) => void,
): Promise<Uint8Array> {
  const res = await fetch(sampleUrl(entry), { mode: 'cors' })
  if (!res.ok) {
    throw new Error(`download: HTTP ${res.status} ${res.statusText}`)
  }
  const total = Number(res.headers.get('content-length')) || entry.bytes || 0
  const reader = res.body?.getReader()
  if (!reader) {
    // Fallback when the browser doesn't expose a stream - just buffer.
    const buf = new Uint8Array(await res.arrayBuffer())
    onProgress?.(buf.byteLength, buf.byteLength)
    return buf
  }
  const chunks: Uint8Array[] = []
  let loaded = 0
  onProgress?.(0, total)
  while (true) {
    const { done, value } = await reader.read()
    if (done) break
    chunks.push(value)
    loaded += value.byteLength
    onProgress?.(loaded, total)
  }
  const out = new Uint8Array(loaded)
  let offset = 0
  for (const c of chunks) {
    out.set(c, offset)
    offset += c.byteLength
  }
  return out
}

export function formatSampleSize(bytes: number): string {
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`
}
