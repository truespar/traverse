# @truespar/traverse-node

The Traverse graph database embedded in Node.js: the engine runs in your process, no server.
Prebuilt for Windows x64 and Linux x64 (glibc); Node.js 22 or later.

```bash
npm install @truespar/traverse-node
```

```js
import { open } from '@truespar/traverse-node' // CommonJS: const { open } = require(...)

const db = open('graph.tvdb')

// one statement as a transaction of its own
db.execute('CREATE (:Person {name: $name})', { name: 'Ada' })

// off the event loop; an AbortSignal stops it
const { rows } = await db.executeAsync(
  'MATCH (p:Person) RETURN p.name AS name', undefined, undefined, AbortSignal.timeout(5000))

// the writer: one at a time, waits up to db.busyTimeout ms for the active one
const tx = db.begin()
tx.execute("MATCH (p:Person {name: 'Ada'}) SET p.born = 1815")
tx.commit()

// a snapshot reader: never waits, runs alongside the writer
const reader = db.beginRead()
console.log(reader.execute('MATCH (p:Person) RETURN count(p) AS n').rows[0].n)
reader.commit()

db.close()
```

- `execute(query, params, 'gql')` runs ISO/IEC 39075 GQL instead of openCypher.
- Results are `{ columns, rows, rowCount, stats }`; each row is an object keyed by column. Nodes
  come back as `{ _id, _labels, ...properties }`, relationships as
  `{ _id, _type, _source, _target, ...properties }`, paths as `{ nodes, edges }`, temporal values
  as ISO strings and byte arrays as base64 strings. Integers are numbers, exact up to 2^53.
- Errors carry `gqlStatus` (ISO GQLSTATUS) and, for the cases code branches on, `code`:
  `ERR_TRAVERSE_INTERRUPTED` (statement timeout, `tx.cancel()`, `TERMINATE TRANSACTIONS`) and
  `ERR_TRAVERSE_TRANSACTION_FAILED` (a statement failed after changing data: only `abort()` ends
  the transaction). An aborted `executeAsync` rejects with an `AbortError`.
- `db.bulkWriter()` loads nodes and edges in bulk; `db.statementTimeout` bounds every statement.

Types ship with the package (`index.d.ts`).

## Build from source

```bash
npm ci
npm run build        # release addon next to index.js; build:debug for a debug one
npm test
```

`NAPI_RS_NATIVE_LIBRARY_PATH` loads the addon from another file (a `.node` copy of the
`traverse_node` library cargo builds), which is how `../../run_binding_tests.sh node` tests it.

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your option.
