# WASM Deployment Guide

How to serve `@truespar/traverse-wasm` from your own webapp.

`traverse-server` already sends these headers for the Studio it
serves. This guide is for sites that embed `@truespar/traverse-wasm`
themselves.

## The two-header requirement

The multi-threaded WASM build of Traverse uses `SharedArrayBuffer` for
its thread pool (via `wasm-bindgen-rayon`). Browsers only
expose `SharedArrayBuffer` on **cross-origin-isolated** pages, which
requires the host serve both:

```
Cross-Origin-Embedder-Policy: require-corp
Cross-Origin-Opener-Policy:   same-origin
```

The `.wasm` file must also be served with:

```
Cross-Origin-Resource-Policy: same-origin
```

(or `cross-origin` if you're loading the WASM from a different
origin than the host page - only do this on a domain you control).

Without these headers, the WASM module loads but `initThreadPool`
throws with the message:

> Did you forget to enable atomics and bulk-memory features as
> outlined in wasm-bindgen-rayon README? - or - SharedArrayBuffer is
> not defined.

Browsers apply this requirement to every threaded WebAssembly
library.

## Deployment recipes

### Vercel

Add a `vercel.json` to the project root:

```json
{
  "headers": [
    {
      "source": "/(.*)",
      "headers": [
        { "key": "Cross-Origin-Embedder-Policy", "value": "require-corp" },
        { "key": "Cross-Origin-Opener-Policy",   "value": "same-origin" }
      ]
    }
  ]
}
```

### Netlify

Add a `_headers` file at the public-root of your build output (e.g.
`public/_headers` in a Next.js / Vite project):

```
/*
  Cross-Origin-Embedder-Policy: require-corp
  Cross-Origin-Opener-Policy: same-origin
```

### Cloudflare Pages / Workers

Add a `_headers` file like the Netlify recipe above. Or via a Worker:

```js
export default {
  async fetch(req, env) {
    const r = await env.ASSETS.fetch(req)
    const h = new Headers(r.headers)
    h.set('Cross-Origin-Embedder-Policy', 'require-corp')
    h.set('Cross-Origin-Opener-Policy',   'same-origin')
    return new Response(r.body, { status: r.status, headers: h })
  },
}
```

### nginx

In your server block:

```nginx
add_header Cross-Origin-Embedder-Policy "require-corp" always;
add_header Cross-Origin-Opener-Policy   "same-origin" always;

location ~ \.wasm$ {
    types { application/wasm wasm; }
    add_header Cross-Origin-Resource-Policy "same-origin" always;
}
```

### Apache

In `.htaccess` or your virtual host config:

```apache
Header always set Cross-Origin-Embedder-Policy "require-corp"
Header always set Cross-Origin-Opener-Policy   "same-origin"
<FilesMatch "\.wasm$">
    Header always set Cross-Origin-Resource-Policy "same-origin"
</FilesMatch>
```

### Express.js (Node)

```js
app.use((req, res, next) => {
  res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp')
  res.setHeader('Cross-Origin-Opener-Policy',   'same-origin')
  next()
})
```

### Local development

The npm package includes `serve.py` next to `index.js`, a small Python
static server that sends these headers, for trying the package before
integrating it:

```bash
cd node_modules/@truespar/traverse-wasm
python3 serve.py
# -> http://127.0.0.1:8000
```

For Vite / webpack dev servers, configure them to emit the headers:

#### Vite (`vite.config.js`)

```js
export default {
  server: {
    headers: {
      'Cross-Origin-Embedder-Policy': 'require-corp',
      'Cross-Origin-Opener-Policy':   'same-origin',
    },
  },
}
```

#### Next.js (`next.config.js`)

```js
export default {
  async headers() {
    return [
      {
        source: '/(.*)',
        headers: [
          { key: 'Cross-Origin-Embedder-Policy', value: 'require-corp' },
          { key: 'Cross-Origin-Opener-Policy',   value: 'same-origin' },
        ],
      },
    ]
  },
}
```

## Verification

To check a deployment, request the page that loads
`@truespar/traverse-wasm`:

```bash
curl -I https://your-site.example/
```

You should see:

```
Cross-Origin-Embedder-Policy: require-corp
Cross-Origin-Opener-Policy:   same-origin
```

Also load the page in a browser and run in the JS console:

```js
> crossOriginIsolated
true
> typeof SharedArrayBuffer
'function'
```

Both must report the values above. If `crossOriginIsolated` is
`false` your headers aren't reaching the browser correctly.

## Side effects of cross-origin-isolation

Once you opt into COEP/COOP, your page loses the ability to load
**unrelated** cross-origin resources without the embedded server
returning a CORP header. Common things this affects:

- **Third-party images / fonts** - they need to come from origins
  that emit `Cross-Origin-Resource-Policy: cross-origin`
  (e.g. CDN-hosted fonts via Google Fonts work; arbitrary HTTPS
  images may not).
- **Third-party iframes** - they need `Cross-Origin-Embedder-Policy:
  credentialless` or to opt-out via `frame-options`.
- **`window.open()` to other origins** - same-origin only.

Most CDNs (jsdelivr, unpkg, esm.sh, googleapis) send suitable CORP
headers, and static images from your own origin are unaffected.
traverse-wasm's Web Worker makes no cross-origin requests.

If the rest of your site can't be isolated, host
the Traverse-using page on a **dedicated subdomain** (e.g.
`graph.your-site.com`) that opts into isolation, and reach it via an
iframe or link from your main site.

## What if my page can't be isolated?

If the page can't send COEP/COOP headers, build the single-threaded
variant of traverse-wasm instead:

```bash
# In your build pipeline, replace:
wasm-pack build crates/traverse-wasm --target web --release --features threads
# with:
wasm-pack build crates/traverse-wasm --target web --release
```

The single-threaded build doesn't need `SharedArrayBuffer` and works
on any page, but the algorithms run on one core. The npm package
contains the threaded build; build the single-threaded one from source
with `crates/traverse-wasm/build.sh` (without `--threads`).

## Reference

- [web.dev: cross-origin-isolation explainer](https://web.dev/articles/coop-coep)
- [MDN: SharedArrayBuffer](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/SharedArrayBuffer)
- [wasm-bindgen-rayon README](https://github.com/RReverser/wasm-bindgen-rayon)
