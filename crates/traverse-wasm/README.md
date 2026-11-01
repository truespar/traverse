# traverse-wasm

WebAssembly bindings for the [Traverse](https://truespar.com/traverse)
graph engine: Cypher and GQL queries and the graph-data-science
algorithms, in the browser, no server.

This crate is the source of the
[`@truespar/traverse-wasm`](https://www.npmjs.com/package/@truespar/traverse-wasm)
npm package, which runs these bindings in a Web Worker (`js/`). The
package README, [`js/README.md`](js/README.md), documents the JavaScript
API, hosting setup and OPFS persistence.

## Building

```sh
./build.sh             # single-threaded, stable Rust
./build.sh --threads   # threaded, nightly Rust
```

Either one runs wasm-pack and assembles a publish-ready package in `js/`
(wasm-pack's output lands in `js/pkg/`); run it from the workspace root
or from this directory. The nightly toolchain is pinned for this crate
only by `rust-toolchain.toml`. Use `build.sh` for the threaded build
rather than calling wasm-pack yourself: it sets the target features,
`-Z build-std` and the linker flags that threads need.

The threaded build needs `SharedArrayBuffer`, so the page must be
cross-origin isolated:

```
Cross-Origin-Embedder-Policy: require-corp
Cross-Origin-Opener-Policy:   same-origin
```

Recipes for common hosts are in `js/README.md` and
[`docs/reference/wasm-deployment.md`](../../docs/reference/wasm-deployment.md).
Pages that can't be isolated use the single-threaded build.

## Bindings

`TraverseDb` is the class the worker drives. Its methods are synchronous;
the package wraps them in an async API.

- `new TraverseDb()`: an empty graph.
- `query(text, paramsJson, timeoutMs, dialect)`: runs one statement.
  `paramsJson` is a JSON parameter map as a string, or `null`.
  `timeoutMs` defaults to 10 seconds and is capped at 30; it's checked
  between rows. `dialect: 'gql'` parses ISO GQL, anything else Cypher.
  Returns the HTTP `/api/query` shape: columns, rows, nodes and
  relationships, counts, query type and timings. Each query may buffer
  up to 128 MiB.
- `algorithms()`: the algorithm catalog, in the `/api/algorithms` shape.
- `schema()`: labels, relationship types, property keys, indexes and
  constraints, in the `/api/schema` shape.
- `exportTvdb()` / `loadTvdb(bytes)`: the graph as `.tvdb` bytes, the
  native server's file format, so files move between the two.
- `clear()`, `nodeCount()`, `edgeCount()`, `estimatedMemory()`.
- Static: `TraverseDb.splitStatements(script)` (cuts a script into
  statements, skipping `;` inside strings and comments) and
  `TraverseDb.version()`.
- The threaded build also exports `initThreadPool(n)`, which starts the
  worker pool; call it once before the first query.

The graph lives in one tab. Each query is a transaction of its own: a
write that fails or runs out of time leaves nothing behind.

## License

MIT OR Apache-2.0, like the rest of Traverse.
