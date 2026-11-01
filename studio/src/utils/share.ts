// Share links that carry the query themselves: base64url JSON in the URL fragment, which the
// browser never sends to a server. `decodeShareHashOnBoot` in main.ts reads it on load and
// primes the editor (and switches to the database, when it exists there).

/** What a share link carries. */
export interface SharePayload {
  q: string
  t?: string
  db?: string
}

/** A link to this Studio that opens with `query` in the editor. */
export function shareUrl(query: string, title?: string, database?: string): string {
  const payload: SharePayload = { q: query }
  if (title) payload.t = title
  if (database) payload.db = database
  // URL-safe base64 (no `+` / `/`, no padding) survives chat and email clients that mangle those
  const b64 = btoa(unescape(encodeURIComponent(JSON.stringify(payload))))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/, '')
  return `${location.origin}${location.pathname}#q=${b64}`
}
