# Traverse

Traverse is GQL/openCypher graph in-memory database with transactional support and strong analytical focus. Supports extreme read-heavy applications. As a server, embedded or through the web browser. MCP, API, Bolt, aliasing, hot-swapping and built-in Studio. Used daily by frontier financial institutions.

## Features

### Database files

A database is a single `.tvdb` file. It holds the nodes, relationships and properties together
with the indexes and constraints. The server, the CLI, the embedded SDKs, Studio and the browser
build all read and write this format. Every file is validated when it is opened, and a damaged
file is rejected with an error.

### Building a database with the SDKs

The embedded SDKs for .NET, Java, Python, Node.js, Go and C open a `.tvdb` file inside your own
process. An application can create a file, fill it with the bulk writer or with Cypher and GQL
statements, and close it. The finished file can be queried in the same process, opened with the
CLI or uploaded to a server. The bulk writer skips the redo log and writes the file once, when
it commits. See [Building a database file](#building-a-database-file) for an example in each
language.

`traverse-admin` builds a file from CSV files or a Cypher script, for data that needs no code.

### Replacing a database on a running server

A server runs several databases, one for each `.tvdb` file in its data directory. The data
behind an application can be replaced without restarting the server:

1. Upload the new file with `PUT /api/files/{name}`. The upload is streamed to disk.
2. Load it with `POST /api/databases/load`. With `?async=true` the load runs as a background
   task. A database loaded this way is read-only.
3. Point the application's alias at the new database with `PUT /api/aliases/{alias}`, or with
   `ALTER ALIAS ... SET DATABASE ...` over Bolt. The change is atomic and is saved to disk. New
   transactions on the alias use the new database; transactions that are already running finish
   on the old one.
4. Unload the old database with `POST /api/databases/{name}/unload` to free its memory.

An alias is used in place of a database name: in the Bolt session, in the `database` field of
the HTTP and WebSocket APIs, with `traverse-cli --database`, or with `USE` in a query. A database
that an alias points to can't be unloaded, renamed or dropped. Databases can also be created,
renamed and dropped while the server runs. The commands are under
[Replacing a database: commands](#replacing-a-database-commands).

### Query languages

- GQL (ISO/IEC 39075) and openCypher use the same planner and executor. All 40 mandatory and 60
  optional features listed in [CONFORMANCE.md](crates/traverse-gql/CONFORMANCE.md) are supported,
  and all 3,893 scenarios of the openCypher TCK pass. Bolt clients use openCypher; the HTTP API
  (with `X-Query-Dialect: gql`), Studio, the SDKs and the browser build accept both.
- Recent Cypher additions are supported, including `SEARCH`, vector functions,
  `CALL { ... } IN TRANSACTIONS`, composable `SHOW` commands, dynamic labels and types, and
  quantified path patterns.
- APOC (all 246 functions and 190 procedures of the core catalog) and GDS (graph projections and
  32 algorithms in stream, stats, write and mutate modes) are implemented natively. See
  [APOC & GDS compatibility](docs/reference/apoc-gds-compat.md).

### Transactions, memory and durability

- All data is held in memory. Reads use MVCC snapshots and don't wait for writers or other
  readers. Large read queries run in parallel on several cores.
- An explicit transaction reads a snapshot and its own writes. If its commit conflicts with
  another transaction, it fails with a transient error that the drivers retry.
- Each query has a memory limit (`--query-memory-limit`). A query that goes over it fails, and
  the server keeps running.
- There are three durability modes. `snapshot` (the default) keeps writes in memory and saves
  them on request, at shutdown and at a regular checkpoint. `wal-async` writes every commit to a
  log and syncs the log in the background. `wal` syncs every commit to disk before acknowledging
  it, and groups concurrent commits into one sync. Saves are made from a snapshot, so queries and
  writes continue during a save.

### Indexes, constraints and graph ML

- Indexes: range, composite and relationship property indexes; full-text (BM25, with analyzers);
  vector (HNSW, cosine or Euclidean); point (R-tree); text (substring).
- Constraints: unique, node key, existence and property type.
- Graph ML: graph neural networks (GCN, GraphSAGE, GAT, R-GCN) are trained and run inside the
  engine with the `traverse.ml.*` procedures, for node classification, node regression and link
  prediction. Model selection, self-supervised embeddings (GraphMAE) and explanations are
  included. Neither Python nor a separate service is needed, and trained models can also run in
  the browser. See [traverse-ml](crates/traverse-ml/README.md).

### Interfaces

- Server: Bolt 5.1 to 6.0 for any Bolt driver, an HTTP API, WebSocket, gRPC, and an
  [MCP](https://modelcontextprotocol.io) endpoint for AI assistants such as Claude Desktop and
  Claude Code.
- Embedded SDKs: .NET (`Traverse.Embedded`), Java (`com.truespar:traverse-java`), Python
  (`traverse-embedded`), Node.js (`@truespar/traverse-node`), Go (`github.com/truespar/traverse/go`)
  and C (`crates/traverse-ffi/traverse.h`).
- Browser: the engine compiles to WebAssembly (`@truespar/traverse-wasm`). Studio can run on it
  without a server and keeps its databases in the browser's private file system.

### Studio

Studio is served by the server on port 7691. It has a query editor for GQL and Cypher; graph,
table and JSON result views; a schema browser; CSV and Cypher import; file, database and alias
management; GDS and ML panels; a benchmark dashboard; and a Graph Assistant that uses a model you
configure (the Claude API or any OpenAI-compatible endpoint, including local models).

### Operations

- Users with admin, editor and reader roles; API keys for the HTTP API; TLS; IP allowlists.
- `SHOW TRANSACTIONS` and `TERMINATE TRANSACTIONS` on all protocols, statement time limits and
  rollback of idle transactions.
- Prometheus metrics, health and readiness endpoints, and a quiesce endpoint for planned
  shutdowns.
- A Docker image, a Windows service, and a TOML config file whose settings can be overridden with
  environment variables and CLI flags.

## Getting Started

### Start the Server

```bash
# An empty in-memory database (not saved)
traverse-server

# One database file
traverse-server --data mydb.tvdb

# A directory: each .tvdb file in it is a database named after the file
traverse-server --data ./databases/
```

Bolt listens on port **7690**, the HTTP API, WebSocket and Studio on **7691**
(`--http-listen none` turns them off) and gRPC on **50051**.

### Connect with the CLI

```bash
# Interactive REPL
traverse-cli

# Connect to a specific server and database
traverse-cli --bolt bolt://192.168.1.10:7690 --database registry

# Run a query directly
traverse-cli "MATCH (n:Person) RETURN n.name LIMIT 10"

# Open a database file directly (no server needed)
traverse-cli --file mydb.tvdb
```

### Open Studio

Open [http://localhost:7691](http://localhost:7691) in your browser and log in with the API key
shown in the server output.

### Docker

```bash
docker run -p 7690:7690 -p 7691:7691 -v traverse-data:/data truespar/traverse
```

The container generates an API key at startup and prints it in its log (`docker logs`). For a
key that stays the same across restarts, pass your own: `-e TRAVERSE_API_KEY=tvs_...`.

## Building a database file

Each example creates `registry.tvdb` with two companies, an ownership relationship and an index
on the company name. `Commit` writes the data to the file.

**C#** (`dotnet add package Traverse.Embedded`). Property values are given as strings, and their
types are inferred:

```csharp
using Traverse;

using var db = Database.Open("registry.tvdb");
using var bulk = db.BeginBulkWrite();
long acme = bulk.CreateNode(["Company"], ["name", "founded"], ["Acme", "1998"]);
long globex = bulk.CreateNode(["Company"], ["name", "founded"], ["Globex", "2004"]);
bulk.CreateEdge(acme, globex, "OWNS", ["share"], ["0.6"]);
bulk.CreateIndex("company_name", "Company", "name");
bulk.Commit();
```

**Java** (`com.truespar:traverse-java`):

```java
try (Database db = Database.open("registry.tvdb");
     BulkWriter bulk = db.bulkWriter()) {
    long acme = bulk.createNode(new String[] {"Company"}, Map.of("name", "Acme", "founded", 1998));
    long globex = bulk.createNode(new String[] {"Company"}, Map.of("name", "Globex", "founded", 2004));
    bulk.createEdge(acme, globex, "OWNS", Map.of("share", 0.6));
    bulk.createIndex("company_name", "Company", "name");
    bulk.commit();
}
```

**Python** (`pip install traverse-embedded`):

```python
import traverse

with traverse.open("registry.tvdb") as db:
    bulk = db.bulk_writer()
    acme = bulk.create_node(["Company"], {"name": "Acme", "founded": 1998})
    globex = bulk.create_node(["Company"], {"name": "Globex", "founded": 2004})
    bulk.create_edge(acme, globex, "OWNS", {"share": 0.6})
    bulk.create_index("company_name", "Company", "name")
    bulk.commit()
```

**Node.js** (`npm install @truespar/traverse-node`):

```js
const { open } = require('@truespar/traverse-node')

const db = open('registry.tvdb')
const bulk = db.bulkWriter()
const acme = bulk.createNode(['Company'], { name: 'Acme', founded: 1998 })
const globex = bulk.createNode(['Company'], { name: 'Globex', founded: 2004 })
bulk.createEdge(acme, globex, 'OWNS', { share: 0.6 })
bulk.createIndex('company_name', 'Company', 'name')
bulk.commit()
db.close()
```

**Go** (`go get github.com/truespar/traverse/go`). The package loads the native library from
`TRAVERSE_FFI_LIB`, from the executable's directory or from the system library path; the
[GitHub release](https://github.com/truespar/traverse/releases) has it for Windows and Linux x64
(`traverse-ffi-<version>-x86_64-windows.zip`, `traverse-ffi-<version>-x86_64-linux.tar.gz`).
Property values are given as strings:

```go
db, err := traverse.Open("registry.tvdb")
check(err)
bulk, err := db.BulkWriter()
check(err)
acme, err := bulk.CreateNode([]string{"Company"}, map[string]string{"name": "Acme", "founded": "1998"})
check(err)
globex, err := bulk.CreateNode([]string{"Company"}, map[string]string{"name": "Globex", "founded": "2004"})
check(err)
_, err = bulk.CreateEdge(acme, globex, "OWNS", map[string]string{"share": "0.6"})
check(err)
check(bulk.CreateIndex("company_name", "Company", "name"))
check(bulk.Commit())
check(db.Close())
```

The C API in `crates/traverse-ffi/traverse.h` has the same calls; the `traverse-ffi` archives on
the release hold the header with the library (and, for Windows, its import library
`traverse_ffi.dll.lib`). Besides the bulk writer, each SDK runs Cypher and GQL statements in
transactions, for example `MERGE`, `CREATE INDEX` and `CREATE CONSTRAINT`.

Without code, `traverse-admin` builds a file from CSV files (see the
[admin reference](docs/reference/admin.md) for the format):

```bash
traverse-admin import \
  --nodes Company=companies.csv --nodes Person=people.csv \
  --edges OWNS=ownership.csv --edges BOARD_MEMBER=board.csv \
  --output registry.tvdb
```

## Replacing a database: commands

These commands upload a new file to a server started with a data directory, load it and move
the alias to it:

```bash
H="X-API-Key: $TRAVERSE_API_KEY"
URL=http://localhost:7691

# 1. Upload (saved as registry_2026_11.tvdb in the data directory)
curl -H "$H" -T registry_2026_11.tvdb $URL/api/files/registry_2026_11

# 2. Load it next to the live database, in the background
curl -H "$H" -H "Content-Type: application/json" \
  -d '{"name": "registry_2026_11", "file": "registry_2026_11.tvdb"}' \
  "$URL/api/databases/load?async=true"      # -> {"taskId": "..."}
curl -H "$H" $URL/api/tasks/<taskId>         # repeat until "status" is "completed"

# 3. Point the alias at it (the first time: POST /api/aliases with {"alias", "database"})
curl -X PUT -H "$H" -H "Content-Type: application/json" \
  -d '{"database": "registry_2026_11"}' $URL/api/aliases/registry

# 4. Unload the previous database
curl -X POST -H "$H" $URL/api/databases/registry_2026_10/unload
```

Clients connected to `registry` use the new data from their next transaction. The same alias
commands over Bolt, for example in `traverse-cli`:

```cypher
CREATE ALIAS registry FOR DATABASE registry_2026_10
ALTER ALIAS registry SET DATABASE registry_2026_11
SHOW ALIASES
```

Studio's Files and Aliases panels do the same in the browser. The full API is in the
[server reference](docs/reference/server.md#http-api-endpoints).

## Build from source

Rust 1.99.1 or newer and Node.js 22.12 or newer.

```bash
# Studio, embedded into the server binary
cd studio && npm ci && npm run build:server && cd ..

# Server with every feature, plus the CLI and admin tools
cargo build --release -p traverse-server --features grpc,tls,mcp,ml -p traverse-cli -p traverse-admin

# Native libraries behind the .NET, Go, Java and Node.js embedded packages
cargo build --release -p traverse-ffi -p traverse-java -p traverse-node
```

The server binary embeds the Studio bundle committed under
`crates/traverse-server/static`. Rebuilding that bundle (`npm run build:server`)
also needs the WASM engine package, produced by `crates/traverse-wasm/build.sh
--threads` on a nightly toolchain with wasm-pack. It is only needed when changing
Studio.

Each embedded package bundles its native library: .NET (`dotnet/`) takes
`traverse_ffi` from `dotnet/src/Traverse.Embedded/runtimes/<rid>/native/`, Java
(`java/`) takes `traverse_java` from `java/src/main/resources/natives/<platform>/`,
and Node.js (`node/embedded/`) takes the `traverse_node` addon from
`node/embedded/npm/<platform>/`. Copy the built libraries there before packing.
To use a library built from source instead, point `TRAVERSE_FFI_LIB` (.NET, Go),
`TRAVERSE_JAVA_LIB` (Java) or `NAPI_RS_NATIVE_LIBRARY_PATH` (Node.js) at its
file; `run_binding_tests.sh` tests each binding that way against a debug build.
The Python binding builds with maturin from `crates/traverse-python`.

## Included Tools

| Binary | Description |
|--------|-------------|
| `traverse-server` | Graph database server (Bolt, HTTP, WebSocket, gRPC, MCP and Studio) |
| `traverse-cli` | Interactive Cypher REPL and scripting client, for a server or a `.tvdb` file |
| `traverse-admin` | Builds `.tvdb` files from CSV files or a Cypher script |
| `traverse-bench` | Benchmark suite and dataset import tool |

## Configuration

Traverse is configured with CLI arguments, environment variables or a TOML config file. Copy
`traverse.example.toml` to `traverse.toml` and edit it.

**Precedence:** CLI args > environment variables > config file > built-in defaults

### Key Settings

| Environment Variable | CLI Flag | Default | Description |
|---------------------|----------|---------|-------------|
| `TRAVERSE_LISTEN` | `--listen` | `0.0.0.0:7690` | Bolt protocol address |
| `TRAVERSE_HTTP_LISTEN` | `--http-listen` | `0.0.0.0:7691` | HTTP API + Studio address (`none` disables it) |
| `TRAVERSE_DATA` | `--data` | *(none)* | Path to `.tvdb` file or directory |
| `TRAVERSE_MEMORY_LIMIT` | `--memory-limit` | 90% of system RAM | Process memory budget (e.g. `8GB`) |
| `TRAVERSE_QUERY_MEMORY_LIMIT` | `--query-memory-limit` | 25% of system RAM | Per-query memory budget (e.g. `2GB`, `0` = unlimited) |
| `TRAVERSE_AUTH` | `--auth` | *(none)* | Seed admin credentials (`user:pass`) |
| `TRAVERSE_DURABILITY` | `--durability` | `snapshot` | `snapshot` (in memory, saved on request, at shutdown and at each checkpoint), `wal-async` (every commit logged, log synced in the background) or `wal` (every commit on disk before it is acknowledged) |
| `TRAVERSE_CHECKPOINT_INTERVAL` | `--checkpoint-interval` | `300` | Seconds between background saves of changed databases (`0` = off) |
| `TRAVERSE_API_KEY` | `--api-key` | *(auto-generated)* | API key for HTTP authentication |
| `TRAVERSE_AI_PROVIDER` | `--ai-provider` | *(off)* | Studio's Graph Assistant: `anthropic` (the Claude API) or `openai` (any OpenAI-compatible endpoint, including local models); its key is `TRAVERSE_AI_API_KEY` |

`traverse.example.toml` and the [server reference](docs/reference/server.md#configuration-reference)
list every setting, including TLS, gRPC, query timeouts, IP allowlists and the Graph Assistant's
model settings.

To use your own AI assistant instead (Claude Desktop, Claude Code or another MCP client), point it
at the server's MCP endpoint, `/api/mcp` (built with the `mcp` feature).

## Documentation

The `docs/` directory has the reference documentation:

- [Server](docs/reference/server.md) - configuration, durability, transactions, TLS, gRPC, authentication, HTTP API
- [Studio](docs/reference/studio.md) - web UI, query editor, graph visualization, import
- [CLI](docs/reference/cli.md) - interactive shell, embedded mode, scripting
- [Admin](docs/reference/admin.md) - building `.tvdb` files from CSV or Cypher
- [Bench](docs/reference/bench.md) - benchmarking, dataset download, comparison testing
- [WASM deployment](docs/reference/wasm-deployment.md) - hosting the in-browser build
- [APOC & GDS compatibility](docs/reference/apoc-gds-compat.md) - Neo4j APOC (100% core catalog: 246 functions, 190 procedures) and GDS (materialized projections, 32 algorithms, mutate pipelines), implemented natively
- [GQL conformance](crates/traverse-gql/CONFORMANCE.md) - ISO/IEC 39075 feature by feature

## Contributing

Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md). Report security problems
privately, as [SECURITY.md](SECURITY.md) describes.

## License

Copyright 2026 The Intelligence Company AB (publ). Traverse is developed by Truespar,
the research arm of The Intelligence Company AB (publ).

Traverse is dual-licensed under the [MIT license](LICENSE-MIT) and the
[Apache License 2.0](LICENSE-APACHE), at your option. See [LICENSE](LICENSE).

Third-party components remain under their own licenses, collected in
[THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
Traverse by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
