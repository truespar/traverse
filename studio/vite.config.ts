import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import { compression } from 'vite-plugin-compression2'

export default defineConfig(({ mode }) => ({
  plugins: [
    vue(),
    compression({
      algorithms: ['gzip'],
      include: /\.(js|css|html|json|svg|wasm)$/,
      // The bundled traverse-server's static handler tries `<asset>.gz`
      // first and serves it with `Content-Encoding: gzip`, so deleting
      // the originals saves ~60% of the embedded RustEmbed payload. The
      // standalone WASM build (`build:wasm`) goes on an ordinary static
      // host without that gz-first lookup, so it keeps both files and
      // the host serves the uncompressed asset (or compresses on the fly).
      deleteOriginalAssets: mode !== 'wasm',
    }),
  ],
  // Treat .wasm as an asset so vite copies it through unchanged. The
  // wasm-pack glue (`traverse_wasm.js`) loads the binary via
  // `new URL('traverse_wasm_bg.wasm', import.meta.url)` which vite
  // rewrites to point at the emitted asset.
  assetsInclude: ['**/*.wasm'],
  // Shared layout sources live outside Studio. Resolve their force solvers
  // from this app's pinned install, including inside the bundled worker, so
  // a normal `npm ci` does not require a second hidden node_modules tree.
  resolve: { dedupe: ['d3-force', 'graphology-layout-forceatlas2'] },
  // Skip dep-optimization for the WASM package - esbuild's pre-bundling
  // strips the `new URL(..., import.meta.url)` pattern needed to emit
  // the `.wasm` binary as a build asset. Excluding makes vite serve
  // the package as-is.
  optimizeDeps: {
    exclude: ['@truespar/traverse-wasm'],
  },
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://localhost:7691',
      '/ws': { target: 'ws://localhost:7691', ws: true },
      '/metrics': 'http://localhost:7691',
    },
    // Mirror the production cross-origin-isolation headers so dev-mode
    // SharedArrayBuffer (needed by the WASM thread pool) works on
    // http://localhost:5173/try.
    headers: {
      'Cross-Origin-Embedder-Policy': 'require-corp',
      'Cross-Origin-Opener-Policy': 'same-origin',
    },
    // `@truespar/traverse-wasm` is installed via `file:../crates/
    // traverse-wasm/js`, so its worker.js, wasm binary, and pkg/ files
    // live outside the Studio package root. Vite's dev server blocks
    // out-of-root reads by default; whitelist the workspace root so
    // those assets resolve via /@fs/... in dev. Production builds
    // bundle everything, so this only matters for `npm run dev`.
    fs: {
      allow: ['..'],
    },
  },
  build: {
    outDir: '../crates/traverse-server/static',
    emptyOutDir: true,
  },
}))
