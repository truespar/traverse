# @truespar/traverse-http

Node.js client for the HTTP API of a [Traverse](https://github.com/truespar/traverse) server:
queries in openCypher or GQL, database and file management, aliases, schema, CSV and Cypher
import, and users. Node.js 22+, no dependencies (it uses the built-in `fetch`).

To run Traverse inside your own process instead, use
[@truespar/traverse-node](https://www.npmjs.com/package/@truespar/traverse-node).

## Installation

```bash
npm install @truespar/traverse-http
```

## Quick start

The server prints its API key at startup (`API key tvs_...`), or takes one from
`TRAVERSE_API_KEY`.

```js
import { TraverseHttpClient } from '@truespar/traverse-http';

const client = new TraverseHttpClient('http://localhost:7691', { apiKey: 'tvs_your_key' });

await client.query("CREATE (:Person {name: 'Alice', age: 34}), (:Person {name: 'Bob', age: 28})");

const result = await client.query(
  'MATCH (p:Person) WHERE p.age > $age RETURN p.name AS name ORDER BY name',
  { age: 30 },
);
console.log(result.columns, result.rows); // [ 'name' ] [ [ 'Alice' ] ]

// ISO/IEC 39075 GQL instead of openCypher: query(text, parameters, database, dialect)
await client.query('MATCH (p:Person) FILTER p.age > 30 RETURN p.name AS name', null, null, 'gql');

for (const db of await client.listDatabases()) console.log(db.name, db.aliases);

const schema = await client.getSchema();
console.log(schema.labels, schema.relationshipTypes);
```

Most calls take an optional database name to target a database other than the default one.

## Databases, files and aliases

```js
import { readFile } from 'node:fs/promises';

await client.createDatabase('staging');
await client.renameDatabase('staging', 'registry');
await client.dropDatabase('registry'); // unless the server runs with TRAVERSE_ALLOW_DROP_DATABASE=false

// upload a .tvdb file built offline (for example with @truespar/traverse-node), load it and point
// an alias at it; queries against the alias move to the new data in one step
await client.uploadFile('registry_v2', await readFile('registry_v2.tvdb'));
await client.loadDatabase('registry_v2', 'registry_v2.tvdb');
await client.createAlias('registry', 'registry_v2'); // later: alterAlias('registry', 'registry_v3')
```

## CSV import

```js
const people = 'id,name,age\n1,Alice,34\n2,Bob,28\n';
const nodes = await client.importCsv(people, 'Person', [
  { name: 'id', type: 'integer' },
  { name: 'name' },
  { name: 'age', type: 'integer' },
]);
console.log(nodes.nodesCreated); // 2

const knows = 'from,to,since\n1,2,2019\n';
const edges = await client.importCsvEdges(
  knows,
  'KNOWS',
  { label: 'Person', column: 'from', property: 'id' },
  { label: 'Person', column: 'to', property: 'id' },
  { columns: [{ name: 'since', type: 'integer' }] },
);
console.log(edges.edgesCreated); // 1
```

Column types are `string` (the default), `integer`, `float` and `boolean`; `property` stores a
column under another name. `ColumnMapping` and `EdgeEndpoint` classes are exported too.

## Errors

A failed request throws `TraverseHttpError`, with the HTTP status in `statusCode`.

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your
option.
