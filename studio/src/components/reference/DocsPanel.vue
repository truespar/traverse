<script setup lang="ts">
import { ref, computed } from 'vue'
import { useConnectionStore } from '../../stores/connection'

const conn = useConnectionStore()
const searchQuery = ref('')

/** Categories whose content only makes sense against a running
 *  traverse-server (CLI flags, HTTP API endpoints, Bolt shell, the
 *  client libraries, the bench harness). In WASM mode the engine
 *  runs in the browser tab - none of these apply. We don't hide the
 *  sections (visitors evaluating the product still want to learn
 *  what they get when they deploy) but each category gets a
 *  "Server only" badge so it's clear which bits are aspirational. */
const SERVER_ONLY_CATEGORIES = new Set([
  'start',
  'server',
  'api',
  'shell',
  'lib',
  'bench',
])

function isServerOnly(cat: string): boolean {
  return conn.isWasmMode && SERVER_ONLY_CATEGORIES.has(cat)
}

interface DocEntry {
  id: string
  name: string
  category: 'start' | 'server' | 'api' | 'shell' | 'import' | 'lib' | 'bench'
  summary: string
  syntax: string
  description: string
  examples: { label: string; code: string }[]
}

const docs: DocEntry[] = [
  // -- Getting Started ------------------------------------
  {
    id: 'quickstart',
    name: 'Getting started',
    category: 'start',
    summary: 'Start the server and run your first query.',
    syntax: 'traverse-server --data <PATH> --http-listen 0.0.0.0:7691',
    description:
      '1. Start the server - point --data at a .tvdb file or a directory of .tvdb files. Studio and the HTTP API are enabled by default on port 7691 (use --http-listen none to disable).' +
      '\n\n2. Open Studio at http://localhost:7691 in your browser, or connect via the CLI shell: traverse-cli --bolt bolt://localhost:7690.' +
      '\n\n3. Run Cypher queries. Data is held in memory and auto-persisted to .tvdb files on graceful shutdown (Ctrl+C) or via the /api/save endpoint.' +
      '\n\nAn API key is auto-generated on startup and printed in the console banner (tvs_... format). Use it to authenticate HTTP requests.',
    examples: [
      { label: 'Single database', code: 'traverse-server --data mydb.tvdb \\\n  --http-listen 0.0.0.0:7691' },
      { label: 'Directory of databases', code: 'traverse-server --data ./databases/ \\\n  --http-listen 0.0.0.0:7691 \\\n  --auth admin:changeme' },
      { label: 'Connect via CLI', code: 'traverse-cli --bolt bolt://localhost:7690' },
    ],
  },

  // -- Server ---------------------------------------------
  {
    id: 'server-flags',
    name: 'Server flags',
    category: 'server',
    summary: 'All traverse-server CLI flags.',
    syntax: 'traverse-server [OPTIONS]',
    description:
      '--config, -c <FILE>     Path to traverse.toml (auto-detects in CWD)' +
      '\n--data <PATH>           .tvdb file or directory of .tvdb files' +
      '\n--listen <ADDR>         Bolt protocol address (default 0.0.0.0:7690)' +
      '\n--http-listen <ADDR>    HTTP API + Studio address (default 0.0.0.0:7691, "none" to disable)' +
      '\n--default-db <NAME>     Default database name (default "traverse")' +
      '\n--memory-limit <SIZE>   Memory budget, e.g. 4GB (default 90% of RAM)' +
      '\n--auth <USER:PASS>      Seed admin user on first startup' +
      '\n--query-memory-limit <SIZE>  Per-query memory budget (default 25% of RAM, 0 = unlimited)' +
      '\n--query-timeout <MS>    Per-query timeout in milliseconds' +
      '\n--tx-idle-timeout <S>   Roll back an explicit transaction idle this long (default 60, 0 = off)' +
      '\n--slow-query-threshold <MS>  Log queries slower than this' +
      '\n--allowed-ips <CIDR,..> Comma-separated CIDR allowlist' +
      '\n--allow-drop-database   Enable DROP DATABASE in Studio and API' +
      '\n--api-key <KEY>         HTTP API key (tvs_..., auto-generated if omitted)' +
      '\n--durability <MODE>     snapshot (default), wal-async or wal' +
      '\n--wal-fsync-ms <MS>     wal-async: background log sync interval (default 1000)' +
      '\n--checkpoint-interval <S>  Background save of changed databases (default 300, 0 = off)' +
      '\n--ml-device <DEVICE>    Graph ML device: auto, cpu, gpu or gpu:<n> (default auto)' +
      '\n--ml-max-jobs <N>       Graph ML training jobs at once (default 1)' +
      '\n--log-level <LEVEL>     trace, debug, info (default), warn or error' +
      '\n--log-dir <DIR>         Daily log files instead of the console' +
      '\n--server-agent <AGENT>  Bolt server agent announced to clients' +
      '\n--ai-provider <NAME>    Graph Assistant model provider: anthropic or openai (any OpenAI-compatible endpoint); off when unset' +
      '\n--ai-model <MODEL>      Graph Assistant model (default claude-opus-5-5 on anthropic)' +
      '\n--ai-base-url <URL>     Graph Assistant API base URL (a gateway, Azure OpenAI, Ollama, ...)' +
      '\n--ai-max-tokens <N>     Output tokens per answer' +
      '\n--ai-effort <LEVEL>     Claude API effort: low, medium, high, xhigh, max' +
      '\n--tls-cert <FILE>       TLS certificate path (PEM)' +
      '\n--tls-key <FILE>        TLS private key path (PEM)' +
      '\n--grpc-listen <ADDR>    gRPC listen address' +
      '\n--service-install       Install as a Windows service' +
      '\n--service-uninstall     Uninstall the Windows service',
    examples: [
      { label: 'Production', code: 'traverse-server \\\n  --config traverse.toml \\\n  --data /var/lib/traverse/ \\\n  --listen 0.0.0.0:7690 \\\n  --http-listen 0.0.0.0:7691 \\\n  --memory-limit 16GB \\\n  --auth admin:changeme \\\n  --query-timeout 30000 \\\n  --allow-drop-database' },
      { label: 'Auth + TLS', code: 'traverse-server --data db.tvdb \\\n  --auth admin:secret \\\n  --tls-cert cert.pem --tls-key key.pem' },
      { label: 'IP allowlist', code: 'traverse-server --data db.tvdb \\\n  --allowed-ips 10.0.0.0/8,192.168.0.0/16' },
    ],
  },
  {
    id: 'server-config',
    name: 'Configuration',
    category: 'server',
    summary: 'TOML config file and environment variables.',
    syntax: 'Precedence: CLI flags  >  environment variables  >  traverse.toml  >  defaults',
    description:
      'TOML config' +
      '\nThe server auto-detects traverse.toml in the working directory, or specify --config <path>. All CLI flags have TOML equivalents with underscores (e.g. --http-listen -> http_listen).' +
      '\n\nEnvironment variables' +
      '\nEvery config option can be set via TRAVERSE_* env var:' +
      '\n  TRAVERSE_LISTEN, TRAVERSE_HTTP_LISTEN, TRAVERSE_DATA,' +
      '\n  TRAVERSE_DEFAULT_DB, TRAVERSE_MEMORY_LIMIT, TRAVERSE_AUTH,' +
      '\n  TRAVERSE_QUERY_TIMEOUT, TRAVERSE_SLOW_QUERY_THRESHOLD,' +
      '\n  TRAVERSE_ALLOWED_IPS, TRAVERSE_ALLOW_DROP_DATABASE,' +
      '\n  TRAVERSE_API_KEY,' +
      '\n  TRAVERSE_AI_PROVIDER, TRAVERSE_AI_API_KEY, TRAVERSE_AI_MODEL,' +
      '\n  TRAVERSE_AI_BASE_URL, TRAVERSE_AI_MAX_TOKENS, TRAVERSE_AI_EFFORT,' +
      '\n  TRAVERSE_TLS_CERT, TRAVERSE_TLS_KEY, TRAVERSE_GRPC_LISTEN,' +
      '\n  TRAVERSE_DURABILITY, TRAVERSE_WAL_FSYNC_MS, TRAVERSE_CHECKPOINT_INTERVAL,' +
      '\n  TRAVERSE_QUERY_MEMORY_LIMIT, TRAVERSE_TX_IDLE_TIMEOUT, TRAVERSE_ML_DEVICE,' +
      '\n  TRAVERSE_ML_MAX_JOBS, TRAVERSE_LOG_LEVEL, TRAVERSE_LOG_DIR, TRAVERSE_QUIET,' +
      '\n  TRAVERSE_SERVER_AGENT' +
      '\n\nDurability' +
      '\nsnapshot (default): writes are in-memory at full speed; data persists on save, at graceful shutdown and by the periodic checkpoint (every 300 s by default).' +
      '\nwal-async: every commit is logged before the ACK and the log is synced in the background (every 1000 ms by default), which bounds what a crash can lose.' +
      '\nwal: every commit is fdatasync-ed to a redo log before the ACK (group commit under concurrency) - acknowledged writes survive crashes, at disk-sync-bound write throughput. WAL segments from a previous wal run are replayed on startup in either mode.',
    examples: [
      { label: 'traverse.toml', code: '# traverse.toml\nlisten = "0.0.0.0:7690"\nhttp_listen = "0.0.0.0:7691"\ndata = "/var/lib/traverse/databases"\ndefault_db = "traverse"\nmemory_limit = "8GB"\nauth = "admin:changeme"\nquery_timeout = 30000\nslow_query_threshold = 1000\nallowed_ips = ["10.0.0.0/8"]\nallow_drop_database = false\n# "snapshot" (default) or "wal" (per-commit durability)\ndurability = "snapshot"' },
      { label: 'Environment variables', code: 'TRAVERSE_DATA=/data \\\nTRAVERSE_LISTEN=0.0.0.0:7690 \\\nTRAVERSE_HTTP_LISTEN=0.0.0.0:7691 \\\nTRAVERSE_MEMORY_LIMIT=4GB \\\nTRAVERSE_AUTH=admin:secret \\\n  traverse-server' },
    ],
  },

  // -- APOC & GDS compatibility --------------------------
  {
    id: 'apoc-gds',
    name: 'APOC & GDS compatibility',
    category: 'lib',
    summary: 'Neo4j APOC (100% core catalog) and GDS work natively.',
    syntax: 'CALL apoc.<ns>.<fn>(...)   |   CALL gds.<algo>.<mode>(graphName, config)',
    description:
      'APOC: all 246 functions and 190 procedures of the APOC core catalog - triggers, periodic jobs, export/import (JSON/CSV/Cypher/GraphML/Arrow), dynamic Cypher, virtual entities, load.jdbc (SQLite/PostgreSQL), geocoding.' +
      '\n\nGDS: gds.graph.project builds a materialized snapshot; 32 algorithms in stream/stats/write/mutate modes; mutate chains (fastRP.mutate -> knn on the embedding) run entirely on the projection; gds.util.* and gds.linkprediction.* functions.' +
      '\n\nBehavior follows APOC core 2026.05 and GDS 2.x.',
    examples: [
      { label: 'APOC', code: "CALL apoc.periodic.iterate(\n  'MATCH (n:Old) RETURN n',\n  'DELETE n', {batchSize: 1000});\nRETURN apoc.text.levenshteinDistance('kitten', 'sitting');" },
      { label: 'GDS pipeline', code: "CALL gds.graph.project('g', 'User', 'FRIEND');\nCALL gds.fastRP.mutate('g', {mutateProperty: 'emb', embeddingDimension: 128});\nCALL gds.knn.stream('g', {nodeProperties: ['emb'], topK: 10})\nYIELD node1, node2, similarity\nRETURN gds.util.asNode(node1).name, similarity;" },
    ],
  },

  // -- HTTP API -------------------------------------------
  {
    id: 'http-api',
    name: 'HTTP API',
    category: 'api',
    summary: 'REST endpoints with interactive Scalar docs.',
    syntax: '/api/docs           Interactive API reference (no key required)\n/api/openapi.json   OpenAPI 3.1 spec',
    description:
      'The HTTP API listens on --http-listen (port 7691 by default). /api/docs serves an interactive reference (Scalar) where every endpoint can be tried.' +
      '\n\nAuthentication' +
      '\nMost endpoints require the API key via X-API-Key header or Authorization: Bearer token. The key is printed in the server startup banner.' +
      '\nPublic (no key): /api/health, /api/ready, /api/docs, /api/openapi.json, /metrics' +
      '\n\nEndpoints' +
      '\n  Core            /api/health  /api/ready  /api/schema  /api/query  /metrics' +
      '\n  Data import     /api/import  /api/import/csv  /api/import/csv/edges  /api/save' +
      '\n  Databases       /api/databases  (list, create, drop, rename, load, unload)' +
      '\n  Files           /api/files  (.tvdb databases and .tvmodel ML models - pass ?kind=model)' +
      '\n  Aliases         /api/aliases  (list, create, alter, delete)' +
      '\n  Users           /api/users  (list, create, update, delete)' +
      '\n  Studio          /api/studio/queries  /api/studio/styles' +
      '\n  Tasks           /api/tasks/{id}  (poll async operations)' +
      '\n  MCP             /api/mcp  (Model Context Protocol)' +
      '\n\nQuery dialect' +
      '\nPOST /api/query runs openCypher by default. Send X-Query-Dialect: gql (header) or "dialect": "gql" (body field) to run ISO 39075 GQL instead - the header wins when both are set. Requires a server built with the gql feature (on by default); see the GQL dialect entry below.' +
      '\n\nFile kinds' +
      '\n/api/files lists both .tvdb databases and .tvmodel trained ML models; each entry carries a kind field ("database" or "model"). Upload, download and delete take ?kind=model to target a .tvmodel - uploading a model requires a server built with the ml feature (503 otherwise).',
    examples: [
      { label: 'Open API docs', code: 'open http://localhost:7691/api/docs' },
      { label: 'Health check', code: 'curl http://localhost:7691/api/health' },
      { label: 'Run a query', code: 'curl -X POST http://localhost:7691/api/query \\\n  -H "X-API-Key: tvs_your_key" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"query": "MATCH (n) RETURN n LIMIT 10"}\'' },
      { label: 'List databases', code: 'curl http://localhost:7691/api/databases \\\n  -H "X-API-Key: tvs_your_key"' },
    ],
  },

  {
    id: 'gql-dialect',
    name: 'GQL dialect (ISO 39075)',
    category: 'api',
    summary: 'Run ISO/IEC 39075 GQL instead of openCypher.',
    syntax: 'X-Query-Dialect: gql   (header)      "dialect": "gql"   (body field)      Studio: dialect picker in the query editor',
    description:
      'Traverse runs two query languages: openCypher (the default) and ISO/IEC 39075 GQL. GQL has its own parser; it is lowered into the Cypher AST and runs on the same planner and executor, so the language does not change plans or performance.' +
      '\n\nConformance' +
      '\nAll 100 ISO 39075 conformance features are implemented - the 40 mandatory features (section 24.2 minimum conformance) and all 60 optional feature IDs. Per-feature status is tracked in crates/traverse-gql/CONFORMANCE.md. GQL-specific clauses such as FILTER and LET work alongside the shared MATCH/RETURN core.' +
      '\n\nSelecting the dialect' +
      '\nHTTP: set the X-Query-Dialect: gql header or a "dialect": "gql" body field on POST /api/query (the header wins when both are set).' +
      '\nStudio: pick "GQL (ISO 39075)" from the dialect dropdown in the query editor.' +
      '\nClients: every official HTTP client (Python, Node.js, Go, Java, .NET - see the Libraries section) exposes a dialect option on its query method.' +
      '\n\nCatalog statements' +
      '\nCREATE GRAPH <name> [IF NOT EXISTS] and DROP GRAPH <name> [IF EXISTS] map onto database create/drop (bare or open ANY graph type; single-segment names). USE is accepted as a no-op - the request\'s "database" field selects the graph.' +
      '\n\nAvailability' +
      '\nGQL is compile-time gated by the gql cargo feature, which is on by default; lean/embedded builds may omit it. GET /api/health reports it under features.gql - Studio hides the dialect picker when it is absent, and the API returns 400 if gql is requested on a server built without it.',
    examples: [
      { label: 'GQL over HTTP (header)', code: 'curl -X POST http://localhost:7691/api/query \\\n  -H "X-API-Key: tvs_your_key" \\\n  -H "X-Query-Dialect: gql" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"query": "MATCH (n:Person) FILTER n.age > 30 RETURN n.name"}\'' },
      { label: 'Dialect in body', code: 'curl -X POST http://localhost:7691/api/query \\\n  -H "X-API-Key: tvs_your_key" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"query": "MATCH (n) RETURN count(n)", "dialect": "gql"}\'' },
      { label: 'Catalog statements', code: 'CREATE GRAPH IF NOT EXISTS socialnet\nDROP GRAPH IF EXISTS socialnet' },
    ],
  },

  {
    id: 'mcp-server',
    name: 'MCP Server',
    category: 'api',
    summary: 'Model Context Protocol - connect AI agents to Traverse.',
    syntax: 'POST /api/mcp          JSON-RPC 2.0 (Streamable HTTP transport)',
    description:
      'The server has an MCP (Model Context Protocol) endpoint, /api/mcp, through which AI assistants such as Claude query and explore its databases. It is available whenever the HTTP API is, in builds with the mcp feature (the release binaries have it).' +
      '\n\nAuthentication' +
      '\nUses the same API key as the HTTP API: pass via Authorization: Bearer header or X-API-Key header.' +
      '\n\nProtocol' +
      '\nMCP uses JSON-RPC 2.0 over HTTP POST. The client sends initialize, then calls tools via tools/call.' +
      '\n\nAvailable tools:' +
      '\n  query           Run a Cypher query (read/write, parameterized)' +
      '\n  schema          Get labels, types, properties, indexes, constraints' +
      '\n  databases       List all databases with node/edge counts' +
      '\n  search_nodes    Find nodes by label + property filter' +
      '\n  shortest_path   Find shortest path between two node IDs' +
      '\n  neighbors       Get neighbors of a node by direction/type',
    examples: [
      { label: 'Claude Desktop config', code: '// claude_desktop_config.json\n{\n  "mcpServers": {\n    "traverse": {\n      "url": "http://localhost:7691/api/mcp",\n      "headers": {\n        "Authorization": "Bearer tvs_your_api_key"\n      }\n    }\n  }\n}' },
      { label: 'Initialize handshake', code: 'curl -X POST http://localhost:7691/api/mcp \\\n  -H "Authorization: Bearer tvs_your_key" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"jsonrpc":"2.0","id":1,\n       "method":"initialize",\n       "params":{"protocolVersion":"2025-11-25",\n         "capabilities":{},\n         "clientInfo":{"name":"my-app","version":"1.0"}}}\'' },
      { label: 'List tools', code: 'curl -X POST http://localhost:7691/api/mcp \\\n  -H "Authorization: Bearer tvs_your_key" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"jsonrpc":"2.0","id":2,\n       "method":"tools/list","params":{}}\'' },
      { label: 'Run a query', code: 'curl -X POST http://localhost:7691/api/mcp \\\n  -H "Authorization: Bearer tvs_your_key" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"jsonrpc":"2.0","id":3,\n       "method":"tools/call",\n       "params":{"name":"query",\n         "arguments":{"cypher":"MATCH (n) RETURN labels(n), count(*)"}}}\'' },
    ],
  },

  // -- Cypher Shell ---------------------------------------
  {
    id: 'cli',
    name: 'traverse-cli',
    category: 'shell',
    summary: 'Interactive Cypher shell and scripting.',
    syntax: 'traverse-cli [OPTIONS] [QUERY]',
    description:
      'Connect to any Bolt-compatible server (Traverse, Neo4j, Memgraph) and run Cypher queries interactively. Supports syntax highlighting, tab completion, and multiple output formats.' +
      '\n\n--bolt, -b <URI>        Bolt URI (default bolt://localhost:7690)' +
      '\n                        Supports bolt:// and bolt+s:// (TLS)' +
      '\n--file, -f <PATH>       Open a .tvdb file directly (no server needed)' +
      '\n--script, -s <FILE>     Run a .cypher script file' +
      '\n--format <FMT>          Output: table, json, or csv (default table)' +
      '\n--database, -d <DB>     Select a named database' +
      '\n--user, -u <USER>       Username for authentication' +
      '\n--password, -p <PASS>   Password for authentication' +
      '\n--no-highlight          Disable syntax highlighting' +
      '\n--tls-insecure          Skip TLS cert verification' +
      '\n[QUERY]                 Execute non-interactively and exit' +
      '\n\nEmbedded mode: Use --file to open a .tvdb database directly without a running server. Useful for quick inspection and offline queries.',
    examples: [
      { label: 'Interactive session', code: 'traverse-cli --bolt bolt://localhost:7690' },
      { label: 'Run a query', code: 'traverse-cli "MATCH (n) RETURN count(n)"' },
      { label: 'Script + JSON output', code: 'traverse-cli --script import.cypher --format json' },
      { label: 'Embedded mode', code: 'traverse-cli --file mydb.tvdb "MATCH (n) RETURN count(n)"' },
    ],
  },
  {
    id: 'cli-repl',
    name: 'REPL commands',
    category: 'shell',
    summary: 'Meta-commands and administration in the shell.',
    syntax: ':help  :quit  :format  :param  :source  :use  :clear  :update',
    description:
      'Meta-commands (prefix with colon):' +
      '\n  :help                  Show help' +
      '\n  :quit / :exit / :q     Exit the shell' +
      '\n  :format <table|json|csv>  Change output format' +
      '\n  :param key => value    Set a query parameter' +
      '\n  :params                Show all parameters' +
      '\n  :params clear          Clear all parameters' +
      '\n  :source <file.cypher>  Run a Cypher script file' +
      '\n  :use <database>        Switch to a different database' +
      '\n  :clear                 Clear the screen' +
      '\n\nTransactions (Bolt mode):' +
      '\n  BEGIN;  COMMIT;  ROLLBACK;' +
      '\n\nUser management (admin):' +
      '\n  CREATE USER <name> SET PASSWORD \'...\' [SET ROLE ADMIN|EDITOR|READER]' +
      '\n  ALTER USER <name> SET PASSWORD \'...\' | SET ROLE <role>' +
      '\n  DROP USER <name>' +
      '\n  SHOW USERS' +
      '\n\nDatabase management:' +
      '\n  SHOW DATABASES   CREATE DATABASE <name>   DROP DATABASE <name>' +
      '\n  LOAD CSV WITH HEADERS FROM \'file:///path.csv\' AS row ...',
    examples: [
      { label: 'Parameterised query', code: ':param name => \'Alice\'\nMATCH (n:Person {name: $name}) RETURN n' },
      { label: 'Switch database', code: ':use analytics' },
      { label: 'Run script', code: ':source seed_data.cypher' },
      { label: 'Export as CSV', code: ':format csv\nMATCH (n:Person) RETURN n.name, n.age' },
    ],
  },

  // -- Data Import ----------------------------------------
  {
    id: 'import-csv',
    name: 'CSV import',
    category: 'import',
    summary: 'Bulk CSV import into a .tvdb file (offline).',
    syntax: 'traverse-admin import --nodes LABEL=file.csv [--edges TYPE=file.csv]\n  --output db.tvdb [OPTIONS]',
    description:
      'Builds a new .tvdb database file from CSV node and edge files, with no server running. For large datasets it is faster than importing through Studio.' +
      '\n\n--nodes LABEL=FILE      Node CSV (repeatable)' +
      '\n--edges TYPE=FILE       Edge CSV (repeatable)' +
      '\n--output, -o <FILE>     Output .tvdb file' +
      '\n--id-type <string|integer>  ID column type (default string)' +
      '\n--delimiter <CHAR>      Field delimiter (default comma)' +
      '\n--skip-bad-rows         Skip unparseable rows instead of aborting' +
      '\n\nNode CSVs must have an id column. Edge CSVs must have start_id and end_id columns referencing node IDs.',
    examples: [
      { label: 'Nodes only', code: 'traverse-admin import \\\n  --nodes Person=people.csv \\\n  --output social.tvdb' },
      { label: 'Nodes + edges', code: 'traverse-admin import \\\n  --nodes Person=people.csv \\\n  --nodes Company=companies.csv \\\n  --edges WORKS_AT=employment.csv \\\n  --output social.tvdb' },
      { label: 'TSV with integer IDs', code: 'traverse-admin import \\\n  --nodes User=users.tsv \\\n  --delimiter "\\t" --id-type integer \\\n  --skip-bad-rows --output users.tvdb' },
    ],
  },
  {
    id: 'import-cypher',
    name: 'Cypher import',
    category: 'import',
    summary: 'Import a .cypher file into a .tvdb file (offline).',
    syntax: 'traverse-admin cypher-import --input data.cypher --output db.tvdb [OPTIONS]',
    description:
      'Import a .cypher file (one statement per line) into a new .tvdb database file. Runs offline - no server needed.' +
      '\n\n--input, -i <FILE>      Input .cypher file' +
      '\n--output, -o <FILE>     Output .tvdb file' +
      '\n--skip-errors           Skip failing statements instead of aborting' +
      '\n--index LABEL:PROP      Pre-create indexes (repeatable)' +
      '\n\nIf --index is omitted, indexes are auto-created when the first MATCH requiring a lookup is seen.',
    examples: [
      { label: 'Basic', code: 'traverse-admin cypher-import \\\n  --input data.cypher --output mydb.tvdb' },
      { label: 'With indexes', code: 'traverse-admin cypher-import \\\n  --input data.cypher --output mydb.tvdb \\\n  --index Person:name --index Company:id' },
      { label: 'Lenient import', code: 'traverse-admin cypher-import \\\n  --input messy_data.cypher --output mydb.tvdb \\\n  --skip-errors' },
    ],
  },

  // -- Libraries --------------------------------------------
  {
    id: 'bolt-driver',
    name: 'Traverse.Bolt',
    category: 'lib',
    summary: '.NET Bolt driver for any Bolt server.',
    syntax: 'dotnet add package Traverse.Bolt',
    description:
      'Bolt driver for .NET, with no dependencies. It connects to any Bolt server: Traverse, Neo4j or Memgraph.' +
      '\n\nConnection:' +
      '\n  BoltClient.ConnectAsync(host, port, options)' +
      '\n  BoltClient.ConnectAsync("bolt://host:port", options)' +
      '\n  BoltPool.Create(host, port, options, poolOptions)' +
      '\n\nQueries (auto-commit):' +
      '\n  RunAsync(cypher, params)           Full materialized result' +
      '\n  RunAndConsumeAsync(cypher, params)  Drain without materializing' +
      '\n  RunAndDiscardAsync(cypher, params)  Server-side DISCARD' +
      '\n  RunStreamAsync(cypher, params)      IAsyncEnumerable<BoltRecord>' +
      '\n\nTransactions:' +
      '\n  BeginTransactionAsync(bookmarks, options)' +
      '\n  tx.RunAsync / tx.CommitAsync / tx.RollbackAsync' +
      '\n\nResult types: BoltResult (columns + records + summary), BoltRecord (typed getters), BoltResultCursor (streaming).' +
      '\nGraph types: BoltNode, BoltRelationship, BoltPath.' +
      '\nTemporal/spatial: BoltDuration, BoltZonedDateTime, BoltPoint2D/3D.' +
      '\n\nPooling: BoltPool with max size, idle timeout, lifetime rotation, liveness probes.' +
      '\nRetry: BoltRetry.ExecuteAsync with exponential backoff + jitter.' +
      '\nTLS: bolt+s:// (standard) and bolt+ssc:// (self-signed).' +
      '\nAuth: basic, bearer, kerberos, custom schemes.',
    examples: [
      { label: 'Connect + query', code: 'using Traverse.Bolt;\n\nawait using var client = await BoltClient.ConnectAsync(\n  "localhost", 7687);\nvar result = await client.RunAsync(\n  "MATCH (p:Person) RETURN p.name LIMIT 10");\n\nforeach (var record in result.Records)\n    Console.WriteLine(record.GetString("p.name"));' },
      { label: 'Transactions', code: 'await using var tx = await client\n  .BeginTransactionAsync();\nawait tx.RunAndConsumeAsync(\n  "CREATE (:User {name: $name})",\n  new() { ["name"] = "Alice" });\nvar result = await tx.RunAsync(\n  "MATCH (u:User) RETURN u.name");\nawait tx.CommitAsync();' },
      { label: 'Connection pool + retry', code: 'var pool = BoltPool.Create("localhost", 7687,\n  new BoltClientOptions { Database = "mydb" },\n  new BoltPoolOptions { MaxSize = 50 });\n\nawait BoltRetry.ExecuteAsync(pool, async conn =>\n{\n    var r = await conn.RunAsync(\n      "MATCH (n) RETURN count(n) AS c");\n    Console.WriteLine(r.Records[0].GetInt64("c"));\n});' },
      { label: 'Streaming large results', code: 'var cursor = await client.RunStreamAsync(\n  "MATCH (n) RETURN n.name");\n\nawait foreach (var record in cursor)\n    Console.WriteLine(record.GetString(0));' },
    ],
  },
  {
    id: 'embed-dotnet',
    name: 'Traverse.Embedded',
    category: 'lib',
    summary: 'NuGet package - embed Traverse in .NET apps.',
    syntax: 'dotnet add package Traverse.Embedded',
    description:
      'Runs Traverse inside a .NET application, with no server. One NuGet package with the native library bundled (Windows x64, Linux x64; elsewhere set TRAVERSE_FFI_LIB to a library built from source). Open a .tvdb file, run Cypher or GQL, close.' +
      '\n\nCore API:' +
      '\n  Database.Open(path)          Open or create a .tvdb database' +
      '\n  db.Query(cypher)             One statement as a transaction of its own' +
      '\n  db.BeginTransaction()        The writer (one at a time, waits up to BusyTimeout)' +
      '\n  db.BeginReadTransaction()    A snapshot reader: never waits' +
      '\n  tx.Query(cypher, token?)     Execute openCypher, returns QueryResult' +
      '\n  tx.QueryGql(query, token?)   Execute ISO 39075 GQL through the same engine' +
      '\n  tx.Commit() / tx.Abort()     Finish the transaction; tx.Cancel() stops its statement' +
      '\n  db.BeginBulkWrite()          Bulk load: a write transaction without the redo log' +
      '\n  db.Flush()                   Persist to disk' +
      '\n  db.StatementTimeout          Bound every statement' +
      '\n\nQueryResult implements IEnumerable<Row>. Each Row has typed getters: GetString, GetInt64, GetDouble, GetBool, GetValue (by index or column name).' +
      '\n\nBulkWriter: CreateNode(labels, keys, values), CreateEdge(src, tgt, type, keys, values), CreateIndex(name, label, prop), CreateEdgeIndex(name, edgeType, property?), DropEdgeIndex(name). Infers property types from the strings.' +
      '\n\nErrors are TraverseException with ErrorCode and GqlStatus. All classes implement IDisposable; a transaction aborts on dispose unless committed, and disposing the database saves it (throwing if that save fails).',
    examples: [
      { label: 'Open + query', code: 'using Traverse;\n\nusing var db = Database.Open("social.tvdb");\nusing var tx = db.BeginReadTransaction();\nusing var result = tx.Query(\n  "MATCH (p:Person) RETURN p.name, p.age LIMIT 10");\n\nforeach (var row in result)\n{\n    Console.WriteLine(\n      $"{row.GetString("p.name")}, {row.GetInt64("p.age")}");\n}\ntx.Commit();' },
      { label: 'Bulk load', code: 'using var db = Database.Open("new.tvdb");\nusing var bulk = db.BeginBulkWrite();\n\nvar id1 = bulk.CreateNode(\n  ["Person"],\n  ["name", "age"], ["Alice", "30"]);\nvar id2 = bulk.CreateNode(\n  ["Person"],\n  ["name", "age"], ["Bob", "25"]);\nbulk.CreateEdge(id1, id2, "KNOWS");\nbulk.CreateIndex("idx_name", "Person", "name");\nbulk.Commit();' },
      { label: 'ASP.NET Core', code: '// Program.cs - register as singleton\nvar db = Database.Open("app.tvdb");\nbuilder.Services.AddSingleton(db);\n\n// Controller - inject and query\npublic class GraphController(Database db)\n{\n    [HttpGet("/people")]\n    public IActionResult People(CancellationToken ct)\n    {\n        using var tx = db.BeginReadTransaction();\n        using var r = tx.Query(\n          "MATCH (p:Person) RETURN p.name", ct);\n        var names = r.Select(\n          row => row.GetString(0)).ToList();\n        tx.Commit();\n        return Ok(names);\n    }\n}' },
    ],
  },
  {
    id: 'embed-c-ffi',
    name: 'C FFI',
    category: 'lib',
    summary: 'C header for embedding from any language.',
    syntax: '#include "traverse.h"\n// Link: -ltraverse_ffi',
    description:
      'A shared library (libtraverse_ffi.so / .dll / .dylib) with a C ABI, for embedding Traverse from any language that can call C.' +
      '\n\nShip the shared library alongside your application and include traverse.h (52 functions).' +
      '\n\nLifecycle:' +
      '\n  traverse_open(path) -> TvDatabase*' +
      '\n  traverse_begin(db) -> TvTransaction*' +
      '\n  traverse_query(tx, cypher) -> TvResult*' +
      '\n  traverse_query_gql(tx, gql) -> TvResult*   (ISO 39075 GQL, same engine)' +
      '\n  traverse_commit(tx) / traverse_abort(tx)' +
      '\n  traverse_flush(db) / traverse_close(db)' +
      '\n\nResult iteration:' +
      '\n  traverse_result_column_count / _column_name' +
      '\n  traverse_result_next (advance cursor)' +
      '\n  traverse_result_get_string / _get_int / _get_double / _get_bool' +
      '\n  traverse_result_is_null / _get_type' +
      '\n  traverse_result_free' +
      '\n\nBulk writer:' +
      '\n  traverse_bulk_begin / _create_node / _create_edge' +
      '\n  traverse_bulk_create_index / _create_edge_index' +
      '\n  traverse_bulk_drop_edge_index / _commit / _abort' +
      '\n\nError handling: Thread-local (SQLite pattern).' +
      '\n  traverse_last_error_code() -> TvError enum' +
      '\n  traverse_last_error() -> const char*',
    examples: [
      { label: 'C example', code: '#include "traverse.h"\n\nTvDatabase *db = traverse_open("test.tvdb");\nTvTransaction *tx = traverse_begin(db);\nTvResult *r = traverse_query(tx,\n  "MATCH (n) RETURN n.name LIMIT 5");\n\nwhile (traverse_result_next(r)) {\n  const char *name =\n    traverse_result_get_string(r, 0);\n  printf("%s\\n", name);\n}\n\ntraverse_result_free(r);\ntraverse_commit(tx);\ntraverse_close(db);' },
      { label: 'Python (ctypes)', code: 'import ctypes\nlib = ctypes.CDLL("./libtraverse_ffi.so")\n\nlib.traverse_open.restype = ctypes.c_void_p\nlib.traverse_query.restype = ctypes.c_void_p\n\ndb = lib.traverse_open(b"test.tvdb")\ntx = lib.traverse_begin(db)\nr = lib.traverse_query(tx,\n  b"MATCH (n) RETURN count(n)")\n# ... iterate results ...\nlib.traverse_commit(tx)\nlib.traverse_close(db)' },
    ],
  },

  {
    id: 'embed-python',
    name: 'Python (PyO3)',
    category: 'lib',
    summary: 'Embed Traverse in Python apps via PyO3.',
    syntax: 'pip install traverse-embedded\n# import traverse; or: maturin develop  (from crates/traverse-python)',
    description:
      'Python bindings built with PyO3: Traverse runs inside the Python process, with no server. Open a .tvdb file, run Cypher or GQL, close.' +
      '\n\nCore API:' +
      '\n  traverse.open(path) -> Database' +
      '\n  db.begin() -> Transaction' +
      '\n  tx.execute(query, params=None, dialect=None) -> QueryResult' +
      '\n      dialect: None (openCypher) or \"gql\" for ISO 39075 GQL' +
      '\n  tx.commit() / tx.abort()' +
      '\n  db.bulk_writer() -> BulkWriter' +
      '\n  db.flush() / db.close()' +
      '\n\nQueryResult: columns, row_count, stats properties. Iterable (yields rows). data() returns all rows.' +
      '\n\nBulkWriter: create_node(labels, properties), create_edge(src, tgt, type, properties), create_index(name, label, prop), create_edge_index(name, type, prop), drop_edge_index(name), commit(), abort().',
    examples: [
      { label: 'Open + query', code: 'import traverse\n\ndb = traverse.open("social.tvdb")\ntx = db.begin()\nresult = tx.execute(\n  "MATCH (p:Person) RETURN p.name LIMIT 10")\n\nfor row in result:\n    print(row[0])\ntx.commit()\ndb.close()' },
      { label: 'Parameterised query', code: 'tx = db.begin()\nresult = tx.execute(\n  "MATCH (p:Person {name: $name}) RETURN p",\n  {"name": "Alice"})\nprint(result.row_count)\ntx.commit()' },
      { label: 'Bulk load', code: 'bulk = db.bulk_writer()\nid1 = bulk.create_node(\n  ["Person"], {"name": "Alice", "age": 30})\nid2 = bulk.create_node(\n  ["Person"], {"name": "Bob", "age": 25})\nbulk.create_edge(id1, id2, "KNOWS")\nbulk.create_index("idx", "Person", "name")\nbulk.commit()' },
    ],
  },
  {
    id: 'embed-java',
    name: 'Java (JNI)',
    category: 'lib',
    summary: 'Embed Traverse in Java apps via JNI.',
    syntax: '<dependency>\n  <groupId>com.truespar</groupId>\n  <artifactId>traverse-java</artifactId>\n</dependency>\n// Java 25+; run with --enable-native-access=ALL-UNNAMED',
    description:
      'Java bindings over JNI: Traverse runs inside the JVM, with no server. The jar bundles the native library (Windows x64, Linux x64; elsewhere set TRAVERSE_JAVA_LIB to a library built from source).' +
      '\n\nCore API:' +
      '\n  Database.open(path) -> Database' +
      '\n  db.execute(query, params?, dialect?)   One statement as a transaction of its own' +
      '\n  db.begin() -> Transaction                The writer (waits up to getBusyTimeout())' +
      '\n  db.beginRead() -> Transaction            A snapshot reader: never waits' +
      '\n  tx.execute(query) / tx.execute(query, params) / tx.execute(query, params, dialect)' +
      '\n      dialect: null (openCypher) or \"gql\" for ISO 39075 GQL' +
      '\n  tx.commit() / tx.abort() / tx.cancel()' +
      '\n  db.bulkWriter() -> BulkWriter' +
      '\n  db.setStatementTimeout(duration) / db.flush() / db.close()' +
      '\n\nQueryResult: getColumns(), getRowCount(), getStats(), data(). Implements Iterable<Map<String, Object>> (one map per row, keyed by column).' +
      '\n\nBulkWriter: createNode(labels, properties), createEdge(src, tgt, type, properties), createIndex(name, label, prop), createEdgeIndex(name, type, prop), dropEdgeIndex(name), commit(), abort().' +
      '\n\nErrors are TraverseException (getGqlStatus()); a stopped statement throws QueryInterruptedException. Database, Transaction and BulkWriter implement AutoCloseable for try-with-resources.',
    examples: [
      { label: 'Open + query', code: 'import com.truespar.traverse.Database;\nimport com.truespar.traverse.Transaction;\n\ntry (Database db = Database.open("social.tvdb");\n     Transaction tx = db.beginRead()) {\n  for (var row : tx.execute(\n         "MATCH (p:Person) RETURN p.name AS name"))\n    System.out.println(row.get("name"));\n  tx.commit();\n}' },
      { label: 'Parameterised query', code: 'var params = Map.<String, Object>of("name", "Alice");\nvar r = tx.execute(\n  "MATCH (p {name: $name}) RETURN p",\n  params);\nSystem.out.println(r.getRowCount());\ntx.commit();' },
      { label: 'Bulk load', code: 'try (var bulk = db.bulkWriter()) {\n  long id1 = bulk.createNode(\n    new String[] {"Person"},\n    Map.of("name", "Alice"));\n  long id2 = bulk.createNode(\n    new String[] {"Person"},\n    Map.of("name", "Bob"));\n  bulk.createEdge(id1, id2, "KNOWS");\n  bulk.createIndex("idx", "Person", "name");\n  bulk.commit();\n}' },
    ],
  },
  {
    id: 'embed-node',
    name: 'Node.js (napi-rs)',
    category: 'lib',
    summary: 'Embed Traverse in Node.js apps via napi-rs.',
    syntax: 'npm install @truespar/traverse-node\n// Node.js 22+; prebuilt for Windows x64 and Linux x64',
    description:
      'Node.js bindings built with napi-rs, with TypeScript types: Traverse runs inside the Node.js process, with no server.' +
      '\n\nCore API:' +
      '\n  open(path) -> TraverseDatabase' +
      '\n  db.execute(query, params?, dialect?)        One statement as a transaction of its own' +
      '\n  db.executeAsync(query, params?, dialect?, signal?)   Off the event loop; an AbortSignal stops it' +
      '\n  db.begin() -> TraverseTransaction            The writer (waits up to db.busyTimeout ms)' +
      '\n  db.beginRead() -> TraverseTransaction        A snapshot reader: never waits' +
      '\n  tx.execute / tx.executeAsync                Same arguments; dialect \'gql\' runs ISO 39075 GQL' +
      '\n  tx.commit() / tx.abort() / tx.cancel()' +
      '\n  db.bulkWriter() -> TraverseBulkWriter' +
      '\n  db.statementTimeout / db.flush() / db.close()' +
      '\n\nQueryResult: { columns, rows, rowCount, stats }; each row is an object keyed by column.' +
      '\n\nBulkWriter: createNode(labels, properties), createEdge(src, tgt, type, properties), createIndex(name, label, prop), createEdgeIndex(name, type, prop), dropEdgeIndex(name), commit(), abort().' +
      '\n\nErrors carry gqlStatus, and code ERR_TRAVERSE_INTERRUPTED or ERR_TRAVERSE_TRANSACTION_FAILED where code branches on it.',
    examples: [
      { label: 'Open + query', code: 'import { open } from "@truespar/traverse-node";\n\nconst db = open("social.tvdb");\nconst { rows } = await db.executeAsync(\n  "MATCH (p:Person) RETURN p.name AS name LIMIT 10");\n\nfor (const row of rows)\n  console.log(row.name);\ndb.close();' },
      { label: 'Parameterised query', code: 'const tx = db.begin();\nconst result = tx.execute(\n  "MATCH (p {name: $name}) SET p.seen = true RETURN p",\n  { name: "Alice" });\nconsole.log(result.rowCount);\ntx.commit();' },
      { label: 'Bulk load', code: 'const bulk = db.bulkWriter();\nconst id1 = bulk.createNode(\n  ["Person"], { name: "Alice", age: 30 });\nconst id2 = bulk.createNode(\n  ["Person"], { name: "Bob", age: 25 });\nbulk.createEdge(id1, id2, "KNOWS");\nbulk.createIndex("idx", "Person", "name");\nbulk.commit();' },
    ],
  },
  {
    id: 'embed-go',
    name: 'Go (FFI)',
    category: 'lib',
    summary: 'Embed Traverse in Go apps via FFI, no CGo.',
    syntax: 'import traverse "github.com/truespar/traverse/go"\n// No CGo - loads the native library at run time',
    description:
      'Go bindings that load the native library at run time (LoadLibrary on Windows, dlopen on Linux and macOS), so no CGo or C toolchain is needed. Traverse runs inside the Go process, with no server. Set TRAVERSE_FFI_LIB, place the shared library next to the binary, or put it on the system library path.' +
      '\n\nCore API:' +
      '\n  traverse.Open(path) -> *Database, error' +
      '\n  db.Begin() -> *Transaction, error' +
      '\n  tx.Execute(query) -> *QueryResult, error' +
      '\n  tx.ExecuteGql(query) -> *QueryResult, error   (ISO 39075 GQL, same engine)' +
      '\n  tx.Commit() / tx.Abort()' +
      '\n  db.BulkWriter() -> *BulkWriter, error' +
      '\n  db.Flush() / db.Close()' +
      '\n\nQueryResult: Columns []string, Rows [][]interface{}, RowCount int.' +
      '\n\nBulkWriter: CreateNode(labels, props), CreateEdge(src, tgt, type, props), CreateIndex(name, label, prop), CreateEdgeIndex(name, type, prop), Commit(), Abort().' +
      '\n\nTraverseError: Code int, Message string. Error constants: ErrIO, ErrSyntax, ErrGraph, etc.',
    examples: [
      { label: 'Open + query', code: 'package main\n\nimport (\n  "fmt"\n  traverse "github.com/truespar/traverse/go"\n)\n\nfunc main() {\n  db, _ := traverse.Open("social.tvdb")\n  defer db.Close()\n  tx, _ := db.Begin()\n  r, _ := tx.Execute(\n    "MATCH (p:Person) RETURN p.name LIMIT 10")\n  for _, row := range r.Rows {\n    fmt.Println(row[0])\n  }\n  tx.Commit()\n}' },
      { label: 'Bulk load', code: 'bulk, _ := db.BulkWriter()\nid1, _ := bulk.CreateNode(\n  []string{"Person"},\n  map[string]interface{}{"name": "Alice"})\nid2, _ := bulk.CreateNode(\n  []string{"Person"},\n  map[string]interface{}{"name": "Bob"})\nbulk.CreateEdge(id1, id2, "KNOWS", nil)\nbulk.CreateIndex("idx", "Person", "name")\nbulk.Commit()' },
    ],
  },
  {
    id: 'http-python',
    name: 'Python HTTP',
    category: 'lib',
    summary: 'Python client for the Traverse HTTP API.',
    syntax: 'pip install traverse-http',
    description:
      'Python client for the Traverse server HTTP API, using httpx.' +
      '\n\nTraverseHttpClient(base_url, api_key=None, timeout=100, transfer_timeout=3600, allow_untrusted_certificate=False)' +
      '\n\nMethods:' +
      '\n  health() / ready()' +
      '\n  list_databases() / create_database(name) / drop_database(name)' +
      '\n  rename_database(old, new) / unload_database(name) / load_database(name, file)' +
      '\n  query(cypher, parameters=None, database=None, dialect=None)' +
      '\n  get_schema(database=None)' +
      '\n  import_cypher(cypher, database=None)' +
      '\n  import_csv(csv, label, columns, ...) / import_csv_edges(csv, type, source, target, ...)' +
      '\n  list_files() / upload_file(name, data, kind="database") / download_file(name, kind=...) / delete_file(name, kind=...)' +
      '\n  list_aliases() / create_alias(alias, db) / alter_alias(alias, db) / drop_alias(alias)' +
      '\n  list_users() / create_user(...) / update_user(...) / delete_user(name)' +
      '\n  save() / get_task(task_id) / close()' +
      '\n\nGQL and capability detection: pass dialect="gql" to query() to run ISO 39075 GQL; health().features reports server capabilities ({gql, grpc, tls, mcp, ml}) so you can check before use. File methods take kind="model" to target .tvmodel trained models; list_files() entries carry a kind field.',
    examples: [
      { label: 'Connect + query', code: 'from traverse_http import TraverseHttpClient\n\nclient = TraverseHttpClient(\n  "http://localhost:7691",\n  api_key="tvs_your_key")\n\nresult = client.query(\n  "MATCH (n) RETURN n.name LIMIT 10")\nfor row in result.rows:\n    print(row)\nclient.close()' },
      { label: 'GQL dialect', code: 'if client.health().features.get("gql"):\n    result = client.query(\n      "MATCH (n:Person) FILTER n.age > 30\\n"\n      "RETURN n.name",\n      dialect="gql")' },
      { label: 'Import CSV', code: 'from traverse_http import ColumnMapping\n\nresult = client.import_csv(\n  open("people.csv", "rb").read(),\n  label="Person",\n  columns=[\n    ColumnMapping("name", "string"),\n    ColumnMapping("age", "integer"),\n  ])' },
    ],
  },
  {
    id: 'http-node',
    name: 'Node.js HTTP',
    category: 'lib',
    summary: 'Node.js client for the Traverse HTTP API.',
    syntax: 'import { TraverseHttpClient } from "@truespar/traverse-http";\n// Requires Node.js >= 22 (native fetch)',
    description:
      'Node.js client for the Traverse server HTTP API, with no dependencies (it uses fetch and AbortSignal.timeout).' +
      '\n\nnew TraverseHttpClient(baseUrl, { apiKey, timeout, transferTimeout, allowUntrustedCertificate })' +
      '\n\nMethods:' +
      '\n  health() / ready()' +
      '\n  listDatabases() / createDatabase(name) / dropDatabase(name)' +
      '\n  renameDatabase(old, new) / unloadDatabase(name) / loadDatabase(name, file)' +
      '\n  query(cypher, parameters?, database?, dialect?)' +
      '\n  getSchema(database?)' +
      '\n  importCypher(cypher, database?)' +
      '\n  importCsv(csv, label, columns, ...) / importCsvEdges(csv, type, source, target, ...)' +
      '\n  listFiles() / uploadFile(name, data, kind?) / downloadFile(name, kind?) / deleteFile(name, kind?)' +
      '\n  listAliases() / createAlias(alias, db) / alterAlias(alias, db) / dropAlias(alias)' +
      '\n  listUsers() / createUser(...) / updateUser(...) / deleteUser(name)' +
      '\n  save() / getTask(taskId)' +
      '\n\nGQL and capability detection: pass dialect \'gql\' to query() to run ISO 39075 GQL; (await health()).features reports server capabilities ({gql, grpc, tls, mcp, ml}). File methods take kind \'model\' to target .tvmodel trained models; listFiles() entries carry a kind field.',
    examples: [
      { label: 'Connect + query', code: 'import { TraverseHttpClient } from "@truespar/traverse-http";\n\nconst client = new TraverseHttpClient(\n  "http://localhost:7691",\n  { apiKey: "tvs_your_key" });\n\nconst result = await client.query(\n  "MATCH (n) RETURN n.name LIMIT 10");\nconsole.log(result.rows);' },
      { label: 'Import Cypher', code: 'const result = await client.importCypher(\n  "CREATE (:Person {name: \'Alice\'})\\n" +\n  "CREATE (:Person {name: \'Bob\'})");\nconsole.log(result.nodesCreated);' },
    ],
  },
  {
    id: 'http-go',
    name: 'Go HTTP',
    category: 'lib',
    summary: 'Go client for the Traverse HTTP API.',
    syntax: 'import traversehttp "github.com/truespar/traverse/go/http"',
    description:
      'Go client for the Traverse server HTTP API, with no dependencies (net/http and encoding/json from the standard library).' +
      '\n\nNewClient(baseURL, &Options{ ApiKey, Timeout, TransferTimeout, AllowUntrustedCertificate })' +
      '\n\nMethods:' +
      '\n  Health() / Ready()' +
      '\n  ListDatabases() / CreateDatabase(name) / DropDatabase(name)' +
      '\n  RenameDatabase(old, new) / UnloadDatabase(name) / LoadDatabase(name, file)' +
      '\n  Query(cypher, params, database) / QueryDialect(query, params, database, dialect)' +
      '\n  GetSchema(database)' +
      '\n  ImportCypher(cypher, database)' +
      '\n  ImportCsv(...) / ImportCsvEdges(...)' +
      '\n  ListFiles() / UploadFile(name, data) / DownloadFile(name) / DeleteFile(name)' +
      '\n  UploadFileKind / DownloadFileKind / DeleteFileKind(name, ..., kind)' +
      '\n  ListAliases() / CreateAlias(alias, db) / AlterAlias(alias, db) / DropAlias(alias)' +
      '\n  ListUsers() / CreateUser(...) / UpdateUser(...) / DeleteUser(name)' +
      '\n  Save() / GetTask(taskId) / Close()' +
      '\n\nGQL and capability detection: QueryDialect(..., "gql") runs ISO 39075 GQL; Health().Features reports server capabilities ({gql, grpc, tls, mcp, ml}). The *Kind file variants take "model" to target .tvmodel trained models; ListFiles() entries carry a Kind field.',
    examples: [
      { label: 'Connect + query', code: 'package main\n\nimport (\n  "fmt"\n  traversehttp "github.com/truespar/traverse/go/http"\n)\n\nfunc main() {\n  client := traversehttp.NewClient(\n    "http://localhost:7691",\n    &traversehttp.Options{ApiKey: "tvs_your_key"})\n  defer client.Close()\n\n  result, _ := client.Query(\n    "MATCH (n) RETURN n.name LIMIT 10",\n    nil, "")\n  for _, row := range result.Rows {\n    fmt.Println(row)\n  }\n}' },
      { label: 'Create + query database', code: 'client.CreateDatabase("analytics")\nresult, _ := client.Query(\n  "CREATE (:Event {type: \'click\'})",\n  nil, "analytics")\nfmt.Println(result.Stats)' },
    ],
  },
  {
    id: 'http-java',
    name: 'Java HTTP',
    category: 'lib',
    summary: 'Java client for the Traverse HTTP API.',
    syntax: '<dependency>\n  <groupId>com.truespar</groupId>\n  <artifactId>traverse-http</artifactId>\n</dependency>',
    description:
      'Java client for the Traverse server HTTP API, using java.net.http.HttpClient (Java 25+) and Jackson 3 for JSON.' +
      '\n\nnew TraverseHttpClient(baseUrl, new TraverseHttpOptions(apiKey, timeout, fileTransferTimeout, allowUntrustedCertificate))' +
      '\n\nMethods:' +
      '\n  health() / ready()' +
      '\n  listDatabases() / createDatabase(name) / dropDatabase(name)' +
      '\n  renameDatabase(old, new) / unloadDatabase(name) / loadDatabase(name, file)' +
      '\n  query(cypher) / query(cypher, parameters, database) / query(cypher, parameters, database, dialect)' +
      '\n  getSchema(database)' +
      '\n  importCypher(cypher, database)' +
      '\n  importCsv(csv, label, columns, ...) / importCsvEdges(csv, type, source, target, ...)' +
      '\n  listFiles() / uploadFile(name, data, kind?) / downloadFile(name, kind?) / deleteFile(name, kind?)' +
      '\n  listAliases() / createAlias(alias, db) / alterAlias(alias, db) / dropAlias(alias)' +
      '\n  listUsers() / createUser(...) / updateUser(...) / deleteUser(name)' +
      '\n  save() / getTask(taskId) / close()' +
      '\n\nGQL and capability detection: query(..., "gql") runs ISO 39075 GQL; health().features() reports server capabilities ({gql, grpc, tls, mcp, ml}). File methods take kind "model" to target .tvmodel trained models; listFiles() entries carry a kind field.' +
      '\n\nAll methods throw TraverseHttpException on error (message + statusCode). Implements AutoCloseable.',
    examples: [
      { label: 'Connect + query', code: 'import com.truespar.traverse.http.*;\n\nvar opts = new TraverseHttpOptions("tvs_your_key");\ntry (var client = new TraverseHttpClient(\n       "http://localhost:7691", opts)) {\n  var result = client.query(\n    "MATCH (n) RETURN n.name LIMIT 10");\n  for (var row : result.rows())\n    System.out.println(row);\n}' },
      { label: 'Import Cypher', code: 'var result = client.importCypher(\n  "CREATE (:Person {name: \'Alice\'})\\n"\n  + "CREATE (:Person {name: \'Bob\'})",\n  null);\nSystem.out.println(\n  result.nodesCreated());' },
    ],
  },
  {
    id: 'http-dotnet',
    name: 'Traverse.Http',
    category: 'lib',
    summary: '.NET client for the Traverse HTTP API.',
    syntax: 'dotnet add package Traverse.Http',
    description:
      '.NET client for the Traverse server HTTP API: async/await throughout, IDisposable, CancellationToken support.' +
      '\n\nnew TraverseHttpClient(baseUrl, new TraverseHttpOptions { ApiKey, Timeout, FileTransferTimeout, AllowUntrustedCertificate })' +
      '\n\nAsync methods:' +
      '\n  HealthAsync() / ReadyAsync()' +
      '\n  ListDatabasesAsync() / CreateDatabaseAsync(name) / DropDatabaseAsync(name)' +
      '\n  RenameDatabaseAsync(old, new) / UnloadDatabaseAsync(name) / LoadDatabaseAsync(name, file)' +
      '\n  QueryAsync(cypher, parameters?, database?, dialect?)' +
      '\n  GetSchemaAsync(database?)' +
      '\n  ImportCypherAsync(cypher, database?)' +
      '\n  ImportCsvAsync(...) / ImportCsvEdgesAsync(...)' +
      '\n  ListFilesAsync() / UploadFileAsync(name, data, progress?, kind?) / DownloadFileAsync(name, dest, progress?, kind?) / DeleteFileAsync(name, kind?)' +
      '\n  ListAliasesAsync() / CreateAliasAsync(alias, db) / AlterAliasAsync(alias, db) / DropAliasAsync(alias)' +
      '\n  ListUsersAsync() / CreateUserAsync(...) / UpdateUserAsync(...) / DeleteUserAsync(name)' +
      '\n  SaveAsync() / GetTaskAsync(taskId) / PollTaskToCompletionAsync(taskId)' +
      '\n\nGQL and capability detection: QueryAsync(..., dialect: "gql") runs ISO 39075 GQL; HealthAsync().Features reports server capabilities ({gql, grpc, tls, mcp, ml}). File methods take kind "model" to target .tvmodel trained models; ListFilesAsync() entries carry a Kind field.' +
      '\n\nAccepts existing HttpClient via constructor overload for DI scenarios.',
    examples: [
      { label: 'Connect + query', code: 'using Traverse.Http;\n\nusing var client = new TraverseHttpClient(\n  "http://localhost:7691",\n  new TraverseHttpOptions {\n    ApiKey = "tvs_your_key" });\n\nvar result = await client.QueryAsync(\n  "MATCH (n) RETURN n.name LIMIT 10");\nforeach (var row in result.Rows)\n    Console.WriteLine(row[0]);' },
      { label: 'ASP.NET Core DI', code: '// Program.cs\nbuilder.Services.AddSingleton(\n  new TraverseHttpClient(\n    "http://localhost:7691",\n    new TraverseHttpOptions {\n      ApiKey = "tvs_your_key" }));\n\n// Controller\npublic class GraphController(\n    TraverseHttpClient client)\n{\n  [HttpGet("/nodes")]\n  public async Task<IActionResult> Nodes()\n  {\n    var r = await client.QueryAsync(\n      "MATCH (n) RETURN n LIMIT 10");\n    return Ok(r.Rows);\n  }\n}' },
    ],
  },

  // -- Benchmarking ----------------------------------------
  {
    id: 'bench',
    name: 'traverse-bench',
    category: 'bench',
    summary: 'Bolt benchmark tool and pipelined importer.',
    syntax: 'traverse-bench [OPTIONS]\ntraverse-bench --import <FILE> [OPTIONS]',
    description:
      'Performance benchmark suite for any Bolt-compatible server. Runs 26 parameterised Cypher queries across 7 groups, measures throughput (QPS) and latency percentiles (p50/p95/p99/max), and optionally profiles server resource usage (CPU%, RSS, threads).' +
      '\n\nBenchmark flags:' +
      '\n  --variant <small|medium|large>  Dataset size (default small)' +
      '\n  --duration <SECS>        Measurement time per query (default 10)' +
      '\n  --warmup <N>             Warmup iterations (default 3)' +
      '\n  --concurrency <N>        Worker threads (default 1)' +
      '\n  --groups <LIST>          Query groups or names, comma-separated' +
      '\n  --host <ADDR>            Bolt host (default 127.0.0.1)' +
      '\n  --port <PORT>            Bolt port (default 7690)' +
      '\n  --auth <USER:PASS>       Authentication' +
      '\n  --server-pid <PID>       Server PID for resource profiling (auto-detected if omitted)' +
      '\n  -o, --output <FILE>      JSON results file (stdout if omitted)' +
      '\n  --list-queries           List all queries and exit' +
      '\n\nQuery groups: read, expansion, scan, aggregate, shortest_path, update, write.' +
      '\n\nImport mode:' +
      '\n  --import <FILE>          Pipelined Bolt import (512 stmts/batch)' +
      '\n\nDataset download:' +
      '\n  --download <VARIANT>     Download pokec dataset (small, medium, large)' +
      '\n  --format <FMT>           Download format: cypher or csv (default cypher)' +
      '\n  --download-dir <DIR>     Output directory (default benchmarks/data/dataset_cache)' +
      '\n\nMuch faster than line-by-line import for bulk loading .cypher files into a running server.',
    examples: [
      { label: 'Run all benchmarks', code: 'traverse-bench --port 7690 --variant medium \\\n  --duration 10 --warmup 3 --concurrency 1 \\\n  --output results.json' },
      { label: 'Expansion queries only', code: 'traverse-bench --port 7690 --variant medium \\\n  --groups "expansion" --duration 5' },
      { label: 'Compare two servers', code: '# Traverse\ntraverse-bench --port 7690 --variant medium \\\n  -o traverse.json\n# Memgraph\ntraverse-bench --port 7689 --variant medium \\\n  -o memgraph.json' },
      { label: 'Pipelined import', code: 'traverse-bench --port 7690 \\\n  --import data.cypher' },
      { label: 'Download dataset', code: 'traverse-bench --download medium' },
    ],
  },
]

const CATEGORY_LABELS: Record<string, string> = {
  start: 'Getting Started',
  server: 'Server',
  api: 'HTTP API',
  shell: 'Cypher Shell',
  import: 'Data Import',
  lib: 'Libraries',
  bench: 'Benchmarking',
}

const CATEGORIES = ['start', 'server', 'api', 'shell', 'import', 'lib', 'bench'] as const

const filtered = computed(() => {
  if (!searchQuery.value.trim()) return docs
  const q = searchQuery.value.toLowerCase()
  return docs.filter(
    (d) =>
      d.name.toLowerCase().includes(q) ||
      d.summary.toLowerCase().includes(q) ||
      d.syntax.toLowerCase().includes(q) ||
      d.description.toLowerCase().includes(q),
  )
})

function entriesFor(cat: string) {
  return filtered.value.filter((d) => d.category === cat)
}

const expanded = ref<Set<string>>(new Set())

function toggleEntry(id: string) {
  if (expanded.value.has(id)) {
    expanded.value.delete(id)
  } else {
    expanded.value.add(id)
  }
}

function copyCode(code: string) {
  navigator.clipboard.writeText(code)
}
</script>

<template>
  <div class="docs-panel">
    <!-- Toolbar -->
    <div class="docs-panel__toolbar">
      <span class="docs-panel__title">Reference</span>
      <input
        v-model="searchQuery"
        class="docs-panel__search"
        placeholder="Search..."
      />
      <span class="docs-panel__count">{{ filtered.length }} entries</span>
      <a class="docs-panel__ext-link" href="https://truespar.com/traverse/docs" target="_blank" rel="noopener" title="Online documentation">
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
          <polyline points="15 3 21 3 21 9" />
          <line x1="10" y1="14" x2="21" y2="3" />
        </svg>
      </a>
    </div>

    <div class="docs-panel__body">
      <div class="docs-panel__content">
        <template v-for="cat in CATEGORIES" :key="cat">
          <div v-if="entriesFor(cat).length > 0" class="docs-panel__category">
            <div class="docs-panel__category-label">
              {{ CATEGORY_LABELS[cat] }}
              <span
                v-if="isServerOnly(cat)"
                class="docs-panel__server-only-badge"
                title="These features require running the bundled traverse-server. The in-browser WASM build doesn't expose them."
              >Server only</span>
            </div>

            <div
              v-for="entry in entriesFor(cat)"
              :key="entry.id"
              class="docs-panel__entry"
            >
              <button class="docs-panel__entry-header" @click="toggleEntry(entry.id)">
                <svg
                  class="docs-panel__chevron"
                  :class="{ 'docs-panel__chevron--open': expanded.has(entry.id) }"
                  width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"
                >
                  <path d="m9 18 6-6-6-6" />
                </svg>
                <span class="docs-panel__entry-name">{{ entry.name }}</span>
                <span class="docs-panel__entry-summary">{{ entry.summary }}</span>
              </button>

              <div v-if="expanded.has(entry.id)" class="docs-panel__entry-body">
                <div class="docs-panel__entry-columns">
                  <!-- Left: syntax + description -->
                  <div class="docs-panel__entry-main">
                    <div class="docs-panel__syntax">{{ entry.syntax }}</div>
                    <p class="docs-panel__desc">{{ entry.description }}</p>
                  </div>

                  <!-- Right: examples -->
                  <div v-if="entry.examples.length" class="docs-panel__entry-examples">
                    <div class="docs-panel__section-label">Examples</div>
                    <div v-for="ex in entry.examples" :key="ex.label" class="docs-panel__example">
                      <div class="docs-panel__example-header">
                        <span class="docs-panel__example-label">{{ ex.label }}</span>
                        <button class="docs-panel__copy-btn" title="Copy" @click="copyCode(ex.code)">
                          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                            <rect width="14" height="14" x="8" y="8" rx="2" ry="2" /><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" />
                          </svg>
                        </button>
                      </div>
                      <pre class="docs-panel__code">{{ ex.code }}</pre>
                    </div>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </template>

        <div v-if="filtered.length === 0" class="docs-panel__empty">
          No results for "{{ searchQuery }}"
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.docs-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
}

/* Toolbar */
.docs-panel__toolbar {
  display: flex;
  align-items: center;
  gap: 12px;
  height: 38px;
  padding: 0 16px;
  border-bottom: 1px solid var(--tv-border-default);
  flex-shrink: 0;
  background: var(--tv-bg-surface);
}

.docs-panel__title {
  font-size: 13px;
  font-weight: 600;
  color: var(--tv-text-primary);
  flex-shrink: 0;
}

.docs-panel__search {
  width: 220px;
  padding: 4px 8px;
  font-size: 12px;
  border: 1px solid var(--tv-border-default);
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-input);
  color: var(--tv-text-primary);
  outline: none;
}
.docs-panel__search:focus {
  border-color: var(--tv-accent);
}

.docs-panel__count {
  font-size: 11px;
  color: var(--tv-text-muted);
  flex-shrink: 0;
}

.docs-panel__ext-link {
  margin-left: auto;
  color: var(--tv-text-muted);
  display: flex;
  align-items: center;
  transition: color 0.15s;
}

.docs-panel__ext-link:hover {
  color: var(--tv-accent);
}

/* Body */
.docs-panel__body {
  flex: 1;
  overflow-y: auto;
  padding: 8px 0;
}

.docs-panel__content {
  padding: 0 16px;
}

.docs-panel__category {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.docs-panel__category-label {
  font-size: 11px;
  font-weight: 600;
  color: var(--tv-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
  padding: 12px 0 4px;
  display: flex;
  align-items: center;
  gap: 8px;
}

/* "Server only" pill - appears next to the category label in WASM
   mode for sections that require running traverse-server. Matches
   the visual weight of the header__wasm-badge so users associate
   the two. */
.docs-panel__server-only-badge {
  display: inline-flex;
  align-items: center;
  padding: 2px 6px;
  font-size: 10px;
  font-weight: 600;
  letter-spacing: 0.02em;
  text-transform: none;
  border-radius: var(--tv-radius-sm);
  background: var(--tv-bg-surface);
  color: var(--tv-text-secondary);
  border: 1px solid var(--tv-border-default);
}

.docs-panel__entry {
  border-bottom: 1px solid var(--tv-border-subtle, var(--tv-border-default));
}

.docs-panel__entry-header {
  display: flex;
  align-items: baseline;
  gap: 8px;
  width: 100%;
  padding: 8px 12px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: none;
  color: var(--tv-text-primary);
  cursor: pointer;
  text-align: left;
  font-family: var(--tv-font-sans);
  font-size: 13px;
  transition: background 0.1s;
}

.docs-panel__entry-header:hover {
  background: var(--tv-bg-hover);
}

.docs-panel__chevron {
  flex-shrink: 0;
  transition: transform 0.15s;
  color: var(--tv-text-muted);
  position: relative;
  top: 1px;
}

.docs-panel__chevron--open {
  transform: rotate(90deg);
}

.docs-panel__entry-name {
  font-family: var(--tv-font-mono);
  font-weight: 600;
  white-space: nowrap;
}

.docs-panel__entry-summary {
  color: var(--tv-text-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  font-size: 12px;
}

/* Expanded body */
.docs-panel__entry-body {
  padding: 4px 12px 16px;
}

.docs-panel__entry-columns {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 24px;
}

.docs-panel__entry-main {
  min-width: 0;
}

.docs-panel__entry-examples {
  min-width: 0;
}

.docs-panel__section-label {
  font-size: 10px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--tv-text-muted);
  margin-bottom: 6px;
}

.docs-panel__syntax {
  font-family: var(--tv-font-mono);
  font-size: 12px;
  color: var(--tv-accent);
  padding: 10px 12px;
  background: var(--tv-bg-elevated);
  border-radius: var(--tv-radius-sm);
  white-space: pre-wrap;
  word-break: break-all;
}

.docs-panel__desc {
  margin: 12px 0 0;
  font-family: var(--tv-font-mono);
  font-size: 12px;
  color: var(--tv-text-secondary);
  line-height: 1.6;
  white-space: pre-line;
}

.docs-panel__example {
  margin-bottom: 8px;
}

.docs-panel__example-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 4px;
}

.docs-panel__example-label {
  font-size: 12px;
  color: var(--tv-text-secondary);
  font-weight: 500;
}

.docs-panel__copy-btn {
  display: flex;
  align-items: center;
  padding: 3px;
  border: none;
  border-radius: var(--tv-radius-sm);
  background: none;
  color: var(--tv-text-muted);
  cursor: pointer;
  transition: color 0.15s;
}
.docs-panel__copy-btn:hover {
  color: var(--tv-text-primary);
}

.docs-panel__code {
  margin: 0;
  padding: 10px 12px;
  font-family: var(--tv-font-mono);
  font-size: 12px;
  line-height: 1.5;
  background: var(--tv-bg-elevated);
  border-radius: var(--tv-radius-sm);
  color: var(--tv-text-primary);
  white-space: pre-wrap;
  word-break: break-all;
  overflow-x: auto;
}

.docs-panel__empty {
  font-size: 13px;
  color: var(--tv-text-muted);
  text-align: center;
  padding: 48px 0;
}
</style>
