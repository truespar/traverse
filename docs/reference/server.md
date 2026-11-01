# Traverse Server Reference

Traverse Server is the main database server process. It exposes a Bolt protocol endpoint for client connections, an HTTP API with WebSocket support for Studio and programmatic access, and optionally a gRPC endpoint.

## Quick Start

```bash
# Start with a single database (HTTP API + Studio enabled by default on port 7691)
traverse-server --data mydb.tvdb

# Start with a directory of databases
traverse-server --data ./databases/

# Start with Bolt only (disable HTTP API and Studio)
traverse-server --data mydb.tvdb --http-listen none

# Start with a config file
traverse-server --config traverse.toml
```

## Configuration

Settings are resolved in this order (highest priority first):

1. **CLI arguments** (`--listen`, `--data`, etc.)
2. **Environment variables** (`TRAVERSE_LISTEN`, `TRAVERSE_DATA`, etc.)
3. **Config file** (`--config path` or auto-detected `traverse.toml` in CWD)
4. **Built-in defaults**

### Configuration Reference

| CLI Flag | Environment Variable | TOML Key | Type | Default | Description |
|----------|---------------------|----------|------|---------|-------------|
| `-c`, `--config` | - | - | path | *(none)* | Path to TOML configuration file (auto-detects `traverse.toml` in CWD) |
| `--listen` | `TRAVERSE_LISTEN` | `listen` | string | `0.0.0.0:7690` | Bolt protocol listen address |
| `--http-listen` | `TRAVERSE_HTTP_LISTEN` | `http_listen` | string | `0.0.0.0:7691` | HTTP API and WebSocket listen address (use `none` to disable) |
| `--data` | `TRAVERSE_DATA` | `data` | string | *(none)* | Path to `.tvdb` file or directory of `.tvdb` files |
| `--default-db` | `TRAVERSE_DEFAULT_DB` | `default_db` | string | `traverse` | Default database name |
| `--memory-limit` | `TRAVERSE_MEMORY_LIMIT` | `memory_limit` | string | 90% of system RAM | Process memory budget (e.g. `4GB`, `512MB`, `1073741824`) |
| `--query-memory-limit` | `TRAVERSE_QUERY_MEMORY_LIMIT` | `query_memory_limit` | string | 25% of system RAM | Per-query memory budget (e.g. `8GB`, `512MB`, `0` = unlimited); see [Memory Management](#memory-management) |
| `--auth` | `TRAVERSE_AUTH` | `auth` | string | *(none)* | Seed admin user credentials (`user:pass`) |
| `--query-timeout` | `TRAVERSE_QUERY_TIMEOUT` | `query_timeout` | integer | *(none)* | Statement time limit in milliseconds; see [Stopping work](#seeing-and-stopping-running-work) |
| `--tx-idle-timeout` | `TRAVERSE_TX_IDLE_TIMEOUT` | `tx_idle_timeout` | integer | `60` | Seconds an explicit transaction may sit idle before it's rolled back (`0` = no limit); see [Transactions](#transactions) |
| `--slow-query-threshold` | `TRAVERSE_SLOW_QUERY_THRESHOLD` | `slow_query_threshold` | integer | *(none)* | Slow query logging threshold in milliseconds |
| `--allowed-ips` | `TRAVERSE_ALLOWED_IPS` | `allowed_ips` | string/list | *(all allowed)* | IP allowlist in CIDR notation |
| `--allow-drop-database` | `TRAVERSE_ALLOW_DROP_DATABASE` | `allow_drop_database` | bool | `true` | Allow database deletion via Studio and HTTP API (`false` locks it down) |
| `--durability` | `TRAVERSE_DURABILITY` | `durability` | string | `snapshot` | Durability mode: `snapshot`, `wal-async` or `wal` (see [Durability](#durability)) |
| `--wal-fsync-ms` | `TRAVERSE_WAL_FSYNC_MS` | `wal_fsync_ms` | integer | `1000` | `wal-async` only: milliseconds between background syncs of the redo log, which bounds what a crash can lose |
| `--checkpoint-interval` | `TRAVERSE_CHECKPOINT_INTERVAL` | `checkpoint_interval` | integer | `300` | Seconds between background saves of changed databases (`0` = off): bounds crash loss in `snapshot` mode and log replay in the WAL modes |
| `--ml-device` | `TRAVERSE_ML_DEVICE` | `ml_device` | string | `auto` | Where ML training and inference run when a call names no `device`: `auto` (the first usable GPU, else the CPU), `cpu`, `gpu` or `gpu:<n>`. GPUs need a build with the `ml-cuda` feature |
| `--ml-max-jobs` | `TRAVERSE_ML_MAX_JOBS` | `ml_max_jobs` | integer | `1` | ML training jobs that may run at once; the rest queue |
| `--api-key` | `TRAVERSE_API_KEY` | `api_key` | string | *(auto-generated)* | API key for HTTP API authentication (`tvs_` prefix required) |
| - | `TRAVERSE_QUIET` | `quiet` | bool | `false` | Leave out the startup banner, which prints the API key (for deployments that ship stdout to logs) |
| `--ai-provider` | `TRAVERSE_AI_PROVIDER` | `ai_provider` | string | *(off)* | Studio's Graph Assistant: `anthropic` (the Claude API) or `openai` (any OpenAI-compatible endpoint: OpenAI, Azure OpenAI, Ollama, LM Studio, vLLM). Off, and hidden in Studio, when unset |
| - | `TRAVERSE_AI_API_KEY` | `ai_api_key` | string | *(none)* | The provider's API key; it stays on the server. Not needed for a local model |
| `--ai-model` | `TRAVERSE_AI_MODEL` | `ai_model` | string | `claude-opus-5-5` on `anthropic` | Model to answer with; required on `openai` |
| `--ai-base-url` | `TRAVERSE_AI_BASE_URL` | `ai_base_url` | string | the provider's | API base URL, for a gateway, Azure OpenAI or a local model (e.g. `http://localhost:11434/v1`) |
| `--ai-max-tokens` | `TRAVERSE_AI_MAX_TOKENS` | `ai_max_tokens` | integer | `64000` on `anthropic` | Output tokens per answer |
| `--ai-effort` | `TRAVERSE_AI_EFFORT` | `ai_effort` | string | the model's | Claude API effort: `low`, `medium`, `high`, `xhigh` or `max` |
| `--log-level` | `TRAVERSE_LOG_LEVEL` | `log_level` | string | `info` | `trace`, `debug`, `info`, `warn` or `error` (see [Logging](#logging)) |
| `--log-dir` | `TRAVERSE_LOG_DIR` | `log_dir` | string | *(none: console)* | Directory for daily log files (`traverse.log.<date>`) |
| `--server-agent` | `TRAVERSE_SERVER_AGENT` | - | string | `Traverse/<version>` | Bolt server agent announced to clients, for libraries that only accept a particular server |
| `--tls-cert` | `TRAVERSE_TLS_CERT` | `tls_cert` | string | *(none)* | TLS certificate file path (PEM) |
| `--tls-key` | `TRAVERSE_TLS_KEY` | `tls_key` | string | *(none)* | TLS private key file path (PEM) |
| `--grpc-listen` | `TRAVERSE_GRPC_LISTEN` | `grpc_listen` | string | `0.0.0.0:50051` | gRPC listen address |
| `--service-install`* | - | - | flag | - | Install as a Windows service (`TraverseServer`) |
| `--service-uninstall`* | - | - | flag | - | Uninstall the Windows service |

\* Windows only.

## Durability

Traverse offers three durability modes:

**`snapshot` (default).** Writes are applied in memory at full speed and
persisted to the `.tvdb` file on `POST /api/save`, on graceful shutdown
(SIGTERM/Ctrl+C) and by the periodic checkpoint, which saves every changed
database every `checkpoint_interval` seconds (default 300, `0` turns it
off). A crash loses the writes made since the last save or checkpoint.
This is the classic in-memory-database model and the mode all published
benchmark numbers reflect.

**`wal-async`.** Every commit is appended to the redo log before it is
acknowledged, and a background task syncs the log to disk every
`wal_fsync_ms` milliseconds (default 1000). A crash loses at most that
interval of acknowledged commits, at close to `snapshot` write throughput.

**`wal`.** Every commit is appended to a redo log (`<db>.tvdb.rlog.*`
segments next to the database file) and `fdatasync`'d before the client
receives its acknowledgment - an acknowledged write survives a crash or
power loss. Group commit shares one disk sync across concurrent
committers, so write throughput scales with concurrency but is
bounded by disk syncs (a lone writer pays about one fsync latency per
commit). Other queries see a commit once it is durable, so nothing a
reader observed can be lost in a crash. On startup, unapplied log segments
are replayed automatically; each snapshot save (`POST /api/save`, the
periodic checkpoint, shutdown) deletes the log segments it covers.

Switching modes is safe in every direction: segments left behind by a
previous `wal` or `wal-async` run are replayed on the next startup
**regardless of the current mode**, and the recovery position is stamped into the next
snapshot so nothing is applied twice.

```bash
# opt into per-commit durability
TRAVERSE_DURABILITY=wal traverse-server --data /var/lib/traverse
# or: traverse-server --durability wal --data /var/lib/traverse
# or in traverse.toml: durability = "wal"
```

### Example Config File

```toml
# traverse.toml

# Network
listen = "0.0.0.0:7690"
http_listen = "0.0.0.0:7691"  # HTTP API + Studio (enabled by default; set to "none" to disable)

# Data
data = "/var/lib/traverse/databases"
default_db = "main"

# Memory
memory_limit = "8GB"
query_memory_limit = "2GB"

# Authentication
auth = "admin:changeme"

# Query limits
query_timeout = 30000
slow_query_threshold = 1000

# Security
allowed_ips = ["10.0.0.0/8", "192.168.0.0/16"]
allow_drop_database = false
# api_key = "tvs_your_key_here"  # Auto-generated if not set

# TLS
# tls_cert = "/etc/traverse/cert.pem"
# tls_key = "/etc/traverse/key.pem"

# gRPC
# grpc_listen = "0.0.0.0:50051"
```

Note: In the TOML file, `allowed_ips` is an array of strings. When set via CLI or environment variable, it is a comma-separated string (e.g. `10.0.0.0/8,192.168.0.0/16`).

## Transactions

Every query runs with **snapshot isolation**: it sees every transaction
committed before it started, nothing committed after, and never uncommitted
state. Reads never wait for writes, and writes never wait for reads.

**Auto-commit queries** (a `RUN` outside `BEGIN`) write under the single
writer lock and commit when they finish.

A GQL **transaction block** over the HTTP API (`START TRANSACTION ... COMMIT`
with `X-Query-Dialect: gql`) runs its statements in order as one such
unit: all of them commit, or none. `READ ONLY` refuses statements that
write, and a block ending in `ROLLBACK` can't contain writes; use an
explicit transaction to roll back work.

**Explicit transactions** (Bolt `BEGIN`/`COMMIT`, gRPC `BeginTransaction`,
the drivers' `execute_write` / `execute_read`) are optimistic: they read
their snapshot plus their own writes and apply everything at `COMMIT`, so
an open transaction blocks no one. If another transaction committed a change
to a node or relationship this one changed or deleted, `COMMIT` fails with
`Neo.TransientError.Transaction.Outdated` (gRPC: `ABORTED`); the official
drivers retry transient errors in managed transactions automatically. The
same goes for a commit that a concurrent one would make break a unique
constraint (two `MERGE`s of the same key): one commits, the other retries and
finds the node.

- The isolation level is snapshot isolation, like PostgreSQL's
  `REPEATABLE READ`: two transactions that read the same data but change
  *different* objects can both commit (write skew).
- `BEGIN` with `mode: "r"` (driver read sessions) refuses writes.
- Index and constraint changes in an explicit transaction take effect at
  commit; until then the transaction plans against the schema it started
  with.
- A transaction idle longer than `--tx-idle-timeout` (default 60 s) is
  rolled back, and the next message in it fails with
  `Neo.TransientError.Transaction.Terminated`. An open transaction keeps the
  data versions it can see in memory, so don't leave transactions open.

## Seeing and stopping running work

Every statement runs as part of a transaction listed by `SHOW TRANSACTIONS`:
an explicit transaction for its life, an auto-commit statement while it
runs. The listing spans every session and protocol (Bolt, HTTP, WebSocket,
gRPC, MCP, and the embedded bindings in the same process, listed under the
database's file name). With authentication on, users see and stop their own
transactions; admins see everyone's.

```cypher
// what's running: database, transactionId, currentQueryId, connectionId,
// clientAddress, username, currentQuery, startTime, status, elapsedTime
SHOW TRANSACTIONS

// every column, filtered and sorted
SHOW TRANSACTIONS YIELD * WHERE elapsedTime > duration('PT30S') ORDER BY elapsedTime DESC

// specific ids (comma-separated, or a STRING / LIST<STRING> parameter)
SHOW TRANSACTIONS 'neo4j-transaction-12', 'neo4j-transaction-15'

// stop one; columns transactionId, username, message
TERMINATE TRANSACTIONS 'neo4j-transaction-12'

// stop everything running longer than a minute
SHOW TRANSACTIONS YIELD transactionId AS id, elapsedTime
WHERE elapsedTime > duration('PT1M')
TERMINATE TRANSACTIONS id YIELD message
RETURN id, message
```

`YIELD *` adds `metaData` (Bolt `tx_metadata`), `parameters`, `protocol`,
`requestUri`, `currentQueryStartTime`, `currentQueryStatus`, `idleTime`,
`currentQueryElapsedTime` and `currentQueryAllocatedBytes`; counters
Traverse doesn't track (CPU time, locks) are null, and the page-cache
counters are 0. In a chain each command runs per row of the one before it
and its `YIELD` keeps the earlier variables, so rename clashing columns
(`YIELD transactionId AS id`).

A terminated transaction's running statement stops at its next check
(within milliseconds), its writes are rolled back, and its next message
fails with `Neo.TransientError.Transaction.Terminated` (gRPC `ABORTED`).

Statements also stop when:

- **Their time limit passes.** `--query-timeout` applies to every
  statement. A Bolt `tx_timeout` limits the whole transaction, counted from
  `BEGIN`; when it governs, the error is
  `Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration`,
  otherwise `Neo.TransientError.Transaction.TransactionTimedOut`. gRPC
  requests take `timeout_ms` (`DEADLINE_EXCEEDED`). Waiting for the writer
  lock counts too.
- **Their client goes away.** A Bolt `RESET` stops the running statement
  (it's answered with `IGNORED`) and everything queued before the `RESET`;
  a closed connection stops it too. An HTTP request whose client
  disconnects or aborts stops, as does a gRPC call whose client cancels or
  drops the response stream.
- **A WebSocket client cancels it**: `{"type": "cancel", "id": "<query
  id>"}` on `/ws` answers the query with an error and keeps the socket open.

Long statements run off the server's async workers, so one heavy query
never holds up other connections, timers or the cancels above.

## Data Persistence

Traverse is an **in-memory** database. Data is loaded from `.tvdb` files at startup and persisted back on graceful shutdown, by the periodic checkpoint and on request.

### Data Path Modes

- **Single file** (`--data mydb.tvdb`): Opens the file as the default database. Creates it on shutdown if it doesn't exist.
- **Directory** (`--data ./databases/`): Scans for all `*.tvdb` files. Each file stem becomes a database name (e.g., `users.tvdb` becomes the `users` database).
- **No path**: Runs with an empty in-memory database, no persistence.

### Saving Data

Data is saved by the periodic checkpoint (`checkpoint_interval`, every 300 seconds by default), at graceful shutdown (SIGTERM or SIGINT: `kill $PID` or Ctrl+C) and on request: `POST http://localhost:7691/api/save` saves all databases immediately.

**Important:** `kill -9` (SIGKILL) skips the save handler and will lose unsaved data.

## Authentication

### Setting Up Users

Seed the first admin user with `--auth`:

```bash
traverse-server --data mydb.tvdb --auth admin:secretpassword
```

The seed user is only created when the auth catalog is empty (no existing users). Once users exist, the seed is ignored.

### Roles

| Role | Permissions |
|------|-------------|
| `Admin` | Full access: read, write, user management |
| `Editor` | Read and write access |
| `Reader` | Read-only access |

### User Management

Users are managed via Cypher commands through the CLI or Bolt protocol:

```cypher
CREATE USER alice SET PASSWORD 'secret' SET ROLE Editor;
ALTER USER alice SET ROLE Admin;
DROP USER alice;
SHOW USERS;
```

Studio's Users panel and the HTTP API (`/api/users`) manage the same users.

When no users are configured, authentication is disabled and all connections have full access.

Users live in the default database's `.tvdb` file. Each change is saved, crash-safely, before the command returns; one that can't be saved fails and isn't made. If the stored users can't be read at startup, the server refuses to start rather than run without authentication; start it with `--auth user:pass` to replace them with that admin.

## HTTP API Key Authentication

When the HTTP API is enabled (default on port 7691), all sensitive endpoints require an API key. The key is auto-generated on startup if not explicitly configured.

### Setting an API Key

```bash
# Auto-generated (shown in startup banner)
traverse-server --data mydb.tvdb

# Explicit key (must start with tvs_ prefix)
traverse-server --data mydb.tvdb --api-key tvs_mykey123
```

### Passing the API Key

Three methods are supported:

1. **`X-API-Key` header** (recommended):
   ```bash
   curl -H "X-API-Key: tvs_..." http://localhost:7691/api/schema
   ```

2. **`Authorization: Bearer` header**:
   ```bash
   curl -H "Authorization: Bearer tvs_..." http://localhost:7691/api/schema
   ```

3. **`?token=` query parameter** (for WebSocket connections):
   ```
   ws://localhost:7691/ws?token=tvs_...
   ```

### Public Endpoints

The following endpoints are exempt from API key authentication:

- `GET /api/health` - Health check
- `GET /api/ready` - Readiness probe
- `GET /api/docs` - API documentation (Scalar UI)
- `GET /api/openapi.json` - OpenAPI specification
- `GET /metrics` - Prometheus metrics
- Static files (Studio SPA assets)

### Studio

When Studio detects that the server requires an API key, it displays a prompt for the key. The key is stored in the browser's localStorage and included in all subsequent requests.

## TLS

To enable TLS:

```bash
traverse-server --tls-cert /path/to/cert.pem --tls-key /path/to/key.pem
```

Both the Bolt and HTTP endpoints will use TLS when configured. Certificates must be in PEM format.

## IP Allowlisting

Restrict connections to specific IP ranges using CIDR notation:

```bash
# CLI
traverse-server --allowed-ips "10.0.0.0/8,192.168.1.0/24,172.16.0.0/12"

# Environment variable
TRAVERSE_ALLOWED_IPS="10.0.0.0/8,192.168.1.0/24" traverse-server
```

```toml
# TOML
allowed_ips = ["10.0.0.0/8", "192.168.1.0/24", "172.16.0.0/12"]
```

When no allowlist is set, all IPs are permitted. The allowlist applies to both Bolt and HTTP connections.

## Memory Management

Traverse is an in-memory engine, so memory is governed at two levels. Both
limits accept human-readable sizes (`TB`, `GB`, `MB`, `KB`, case-insensitive)
or raw bytes; `0` / `unlimited` disables a limit.

### Process budget (`--memory-limit`)

Resident set size of the whole process against a ceiling.

- **Default**: 90% of system RAM. The default also applies to the CLI and the
  embedded bindings, which have no configuration step.
- When the ceiling is exceeded, write operations are rejected and running
  queries are aborted until memory usage drops below the limit.

### Per-query budget (`--query-memory-limit`)

Every query tracks an estimate of the memory its own buffers hold - a sorted
or aggregated input, a hash-join table, the rows an `Eager` barrier holds, a
variable-length expansion, the result set. A query that exceeds its budget
fails with

```
query exceeded its memory limit: 9.1 GB used, limit 8.0 GB (TRAVERSE_QUERY_MEMORY_LIMIT) ...
```

while the server, and every other query, keep running. This is the
equivalent of Neo4j's `db.memory.transaction.max` (also the name the setting
has in `SHOW SETTINGS`).

- **Default**: 25% of system RAM.
- Raise it for deliberately large analytical queries; lower it on shared
  instances so one client's query cannot take a disproportionate share.

Most statements never approach the budget because the executor streams:
`CREATE`, `SET`, `REMOVE` and `MERGE` apply one row at a time, joins hold one
side, aggregates hold their groups, and a statement without `RETURN` keeps no
rows at all. The planner inserts an `Eager` operator (visible in `EXPLAIN`)
only where Cypher's clause-at-a-time semantics require a later clause to see
all of an earlier clause's writes - `MATCH (n:Person) CREATE (:Person)`, for
instance - and that buffer is what the budget protects against.

## Logging

Logging is controlled via the `RUST_LOG` environment variable (standard `tracing-subscriber` `EnvFilter` syntax):

```bash
# Default level: info
traverse-server

# Debug logging for the server crate
RUST_LOG=traverse_server=debug traverse-server

# Trace all crates
RUST_LOG=trace traverse-server

# Selective logging
RUST_LOG="traverse_server=debug,traverse_cypher=info" traverse-server
```

When `--slow-query-threshold` is set, queries exceeding the threshold are logged at the `WARN` level with their execution time and query text.

## Windows Service (Windows only)

Traverse can run as a native Windows Service. This requires Administrator privileges.

### Install

```cmd
traverse-server.exe --service-install
```

This registers a `TraverseServer` service with automatic startup. The service reads configuration from `traverse.toml` in the executable directory and environment variables.

### Start / Stop

```cmd
sc start TraverseServer
sc stop TraverseServer
```

### Uninstall

```cmd
traverse-server.exe --service-uninstall
```

### Logging

When running as a service, logs are written to daily files `traverse.log.<date>` in `TRAVERSE_LOG_DIR`, by default the executable's directory (no console output). Set `RUST_LOG` as a system environment variable to control log level. Everything else works as in the console: the same settings, listeners, checkpoints and durability modes.

## Endpoints

| Protocol | Default Address | Purpose |
|----------|----------------|---------|
| Bolt | `0.0.0.0:7690` | Client connections (CLI, drivers) |
| HTTP | `0.0.0.0:7691` | REST API, WebSocket, Studio UI (use `--http-listen none` to disable) |
| gRPC | `0.0.0.0:50051` | gRPC API, in builds with the `grpc` feature (the release binaries have it); `--grpc-listen` sets the address |

### HTTP API Endpoints

#### Core

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/` | Studio web UI (embedded static assets) |
| `GET` | `/api/health` | Health status (uptime, memory, database stats) |
| `GET` | `/api/ready` | Readiness probe (`{"status": "ready"}`) |
| `GET` | `/api/schema` | Database schema (labels, relationship types, property keys) |
| `POST` | `/api/query` | Execute a Cypher query |
| `GET` | `/metrics` | Prometheus-format metrics |
| `GET` | `/ws` | WebSocket connection for real-time queries |

#### Data Management

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/save` | Persist all databases to disk |
| `POST` | `/api/quiesce` | Before a planned stop: waits for background tasks, saves every changed database and returns a node/edge inventory to check after the restart (`?timeout_ms=`, default 120000) |
| `POST` | `/api/import` | Import data from Cypher statements (512 MB body limit) |
| `POST` | `/api/import/csv` | Bulk CSV node import (512 MB body limit) |
| `POST` | `/api/import/csv/edges` | Bulk CSV edge import (512 MB body limit) |

#### Database Management

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/databases` | List all databases |
| `POST` | `/api/databases` | Create a new database |
| `DELETE` | `/api/databases/{name}` | Drop a database (requires `allow_drop_database`) |
| `POST` | `/api/databases/{name}/rename` | Rename a database |
| `POST` | `/api/databases/{name}/unload` | Unload a database from memory (keeps `.tvdb` on disk) |
| `POST` | `/api/databases/load` | Load a `.tvdb` file from disk into memory |

#### File Management

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/files` | List all `.tvdb` files in the data directory |
| `GET` | `/api/files/{name}` | Download a `.tvdb` file (persists loaded database first) |
| `PUT` | `/api/files/{name}` | Upload a `.tvdb` file to the data directory (no body limit) |
| `DELETE` | `/api/files/{name}` | Delete a `.tvdb` file from disk (must not be loaded) |

#### Alias Management

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/aliases` | List all database aliases |
| `POST` | `/api/aliases` | Create an alias (`{"alias": "...", "database": "..."}`) |
| `PUT` | `/api/aliases/{name}` | Change the target database for an alias |
| `DELETE` | `/api/aliases/{name}` | Delete an alias |

#### Async Tasks

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/tasks/{id}` | Get status of an async task (load/unload operations) |

Some endpoints (database load/unload) accept `?async=true` to run in the background. The response includes a `taskId` that can be polled via `/api/tasks/{id}`.

#### User Management

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/users` | List all users |
| `POST` | `/api/users` | Create a user |
| `PUT` | `/api/users/{username}` | Update a user (password, role) |
| `DELETE` | `/api/users/{username}` | Delete a user |

#### Studio

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/studio/queries` | List saved queries |
| `POST` | `/api/studio/queries` | Save a query |
| `DELETE` | `/api/studio/queries/{name}` | Delete a saved query |
| `GET` | `/api/studio/styles` | Get graph visualization styles |
| `PUT` | `/api/studio/styles` | Update graph visualization styles |

#### AI Chat

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/ai/chat` | Graph Assistant (SSE stream, answered by the model set with `ai_provider`) |

#### MCP (Model Context Protocol)

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/mcp` | MCP JSON-RPC 2.0 endpoint (Streamable HTTP transport) |
| `GET` | `/api/mcp` | Returns `405` (SSE streaming not supported) |
| `DELETE` | `/api/mcp` | Returns `405` (session termination not supported) |

Traverse has an [MCP](https://modelcontextprotocol.io) endpoint (protocol version `2025-11-25`) through which AI assistants such as Claude query and explore its databases. It is available whenever the HTTP API is, in builds with the `mcp` feature.

**Available tools:**

| Tool | Description |
|------|-------------|
| `query` | Run a Cypher query (read/write, parameterized, 500 row limit) |
| `schema` | Get labels, relationship types, properties, indexes, constraints |
| `databases` | List all databases with node/edge counts |
| `search_nodes` | Find nodes by label + optional property filter (100 result limit) |
| `shortest_path` | Find shortest path between two node IDs |
| `neighbors` | Get neighbors of a node by direction/edge type (50 result limit) |

**Claude Desktop configuration:**

```json
{
  "mcpServers": {
    "traverse": {
      "url": "http://localhost:7691/api/mcp",
      "headers": {
        "Authorization": "Bearer tvs_your_api_key"
      }
    }
  }
}
```

**Example usage:**

```bash
# Initialize
curl -X POST http://localhost:7691/api/mcp \
  -H "Authorization: Bearer tvs_your_key" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize",
       "params":{"protocolVersion":"2025-11-25",
         "capabilities":{},
         "clientInfo":{"name":"my-app","version":"1.0"}}}'

# Run a query
curl -X POST http://localhost:7691/api/mcp \
  -H "Authorization: Bearer tvs_your_key" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call",
       "params":{"name":"query",
         "arguments":{"cypher":"MATCH (n) RETURN labels(n), count(*)"}}}'
```

#### Documentation

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/docs` | Interactive API reference (Scalar UI) |
| `GET` | `/api/openapi.json` | OpenAPI 3.1 specification |
