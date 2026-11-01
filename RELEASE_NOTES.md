# Changelog

All notable changes to Traverse are documented in this file.

---

## 0.9.0 - unreleased

### Licensing removed

- **No licence key, no limits.** Traverse no longer asks for a licence key and no longer enforces plan limits. The 7-day trial, the activation flow, device registration, the database/node/edge/connection caps and the per-feature gates (user management, Graph Assistant) are gone; every feature is available to everyone. `--license-key` / `TRAVERSE_LICENSE_KEY` / `license_key`, the `/api/license/*` endpoints and the `license` block of `/api/health` have been removed, as have `Database.OpenLicensed` (.NET), `Database.openLicensed` (Java), `traverse.open_licensed` (Python) and `openLicensed` (Node.js) - use the plain `open`.
- **No version check.** The server, CLI (`:update`) and `traverse-admin update` no longer check online for new releases; the `update` block of `/api/health` and `/api/update/download` are gone.
- **No outside service.** Nothing in Traverse calls home any more. A shared query link now carries the query itself (in the part of the URL after `#`, which the browser never sends anywhere), so it works on any server and in the browser build; Studio's feedback form has been removed. `--api-base` / `TRAVERSE_API_BASE` / `api_base` and the `/api/feedback` and `/api/share-query` endpoints have been removed.

### Graph Assistant

- **Bring your own model.** Studio's Graph Assistant now talks to a model you configure: `ai_provider = "anthropic"` (the Claude API, `claude-opus-5-5` by default) or `"openai"` for any OpenAI-compatible endpoint - OpenAI, Azure OpenAI, or a model on your own machine through Ollama, LM Studio or vLLM. Set it with `--ai-provider`, `TRAVERSE_AI_PROVIDER` or `traverse.toml`, plus `ai_api_key`, `ai_model`, `ai_base_url`, `ai_max_tokens` and `ai_effort`. The key stays on the server, and nothing leaves your network unless the provider you pick is a cloud one. Without a provider the assistant is off and Studio hides it; `/api/health` reports `features.ai`, and the startup banner shows the model in use.
- **It knows the query language.** The assistant writes GQL when the editor is set to GQL, and its instructions now cover GQL, the index and constraint types, transactions, the graph algorithms and Graph ML.
- Studio now shows why an answer stopped (the provider's error, a refusal, the output limit) instead of an empty reply.

### Transactions

- **Explicit transactions no longer copy the database or lose updates.** `BEGIN ... COMMIT` over Bolt, gRPC transactions and the drivers' managed transactions now read a snapshot plus their own writes and apply everything at commit. Opening one is instant whatever the database size, an open transaction blocks no other reader or writer, and two transactions that change the same node or relationship no longer overwrite each other: the later commit fails with `Neo.TransientError.Transaction.Outdated` (gRPC `ABORTED`), which the drivers retry automatically. Concurrent `MERGE`s of the same unique key can no longer both create a node. In `wal` mode explicit-transaction commits are logged like any other write.
- **Read-only transactions.** `BEGIN` with `mode: "r"` (driver read sessions) refuses writes.
- **A failed statement fails its transaction.** In an explicit transaction over gRPC or in the embedded bindings, a statement that fails after it changed data (a constraint violation part way through, a stopped query) can't leave half its writes behind: later statements and the commit fail (gRPC `FAILED_PRECONDITION`, `TvErrorCode.Transaction`, `ErrTransaction`, `TV_ERROR_TRANSACTION`, `ERR_TRAVERSE_TRANSACTION_FAILED`) and only a rollback ends it, as over Bolt. A statement that fails before writing anything, such as a syntax error, leaves the transaction usable.
- **`CALL { ... } IN TRANSACTIONS` commits each batch.** As an auto-commit statement (Bolt, HTTP, WebSocket, MCP, gRPC without a transaction, the browser build) every batch commits on its own as it completes, so a large import holds one batch of uncommitted changes at a time. With `ON ERROR CONTINUE` or `BREAK` a failed batch is rolled back and its rows dropped while earlier batches stay committed; `REPORT STATUS` gives each batch's `started`, `committed`, `transactionId` and `errorMessage`. It's refused inside an explicit transaction and after writes in the same statement; in the embedded bindings run it with the auto-commit `execute` (below). Previously the batches ran in the statement's one transaction and a failed batch's writes could be committed.
- **Browser queries are atomic.** In the WebAssembly build each query is a transaction of its own: a write that fails or runs out of time leaves nothing behind, and the time limit covers the parallel phases on the browser's worker threads.
- **Idle transactions are rolled back** after `--tx-idle-timeout` / `TRAVERSE_TX_IDLE_TIMEOUT` / `tx_idle_timeout` seconds (default 60, `0` = no limit); the next message in the transaction fails with `Neo.TransientError.Transaction.Terminated`.
- **Consistent snapshots on every access path.** A read sees exactly the transactions committed before it started - through label and relationship-type scans, property, relationship, vector, full-text, point and text index lookups, traversals and counts - including nodes and relationships changed or deleted since. Previously some paths could show a writer's uncommitted changes or miss entities changed after the read began. Graph algorithms run on a consistent snapshot while writes continue, instead of failing with "uncommitted writes are present".
- Fixed: rolling back a transaction that changed indexed properties could leave stale index entries; rolling back schema changes didn't restore vector, full-text, point and text indexes or relationship and composite constraints; composite indexes created in `wal` mode weren't recovered after a crash; `r.prop IN [...]` over an indexed relationship property could return wrong rows when repeated with other values; vector indexes skipped embeddings stored as native vectors; a long-running server's memory grew with every write made while other queries were running.

### Durability

- **Group commit groups.** In `wal` mode concurrent commits now share one log write and one disk sync instead of syncing one after another, so write throughput scales with the number of concurrent writers.
- **Readers only see durable commits.** In `wal` mode a commit becomes visible to other queries once it is on disk, so nothing a client read can be lost in a crash.
- **Saves don't stop the database.** A save (`POST /api/save`, the periodic checkpoint, shutdown) writes the file from a snapshot of one moment: queries and writes carry on while it runs, and the graph is no longer copied in memory first, so saving a large database needs no room for a second copy of it. Unloading or renaming a database holds writes, not reads, until its file is written. Two saves of the same database run one after the other, so an older one can never replace a newer file. Studio's in-browser engine exports the same way.
- **The log stays small.** Every snapshot save (`POST /api/save`, the periodic checkpoint, shutdown) deletes the redo-log segments it covers; previously they accumulated in `wal` mode. Snapshot files are installed with a durable rename, so a crash can't bring back the previous snapshot.
- **A closed database is one file.** Closing a database, from an embedded binding or the CLI, deletes its redo log once the `.tvdb` holds everything in it, so a file built with an SDK or `traverse-admin` comes without a log segment beside it.
- In `wal` mode a transaction larger than a log segment (32 MiB, e.g. one statement creating a few hundred thousand nodes) failed to commit. It now gets a log segment sized to hold it, and recovery replays it like any other.
- Fixed: a database file opened without its redo log beside it (copied, uploaded or built on another machine) logged its new commits at positions the file already covered, so a crash lost them; the log now always continues past the file. A log left over from another copy of the database is no longer appended to.
- Fixed: a save running while writes were committing could record a commit as saved that wasn't in the file, so it was skipped on recovery; a commit landing during a save could be left out of the next save; the `wal-async` background sync stalled commits while it ran.

### Embedded bindings

- **Closing a database is always safe.** Closing (or disposing) a database rolls back the transactions and bulk writers still open on it, saves and releases the file; their later calls fail with a clear error instead of crashing the process. A transaction or bulk writer may also outlive its database object: the last handle to go saves and closes the file.
- **Use from any thread.** Transactions and bulk writers in .NET, Java, Go, Python and Node.js can be committed, aborted or queried from any thread, one call at a time, and one that is never closed is rolled back when the garbage collector finds it, releasing the writer lock.
- **Read transactions.** `BeginReadTransaction` (.NET), `beginRead` (Java, Node.js), `begin_read` (Python), `BeginRead` (Go) and `traverse_begin_read` (C) open a snapshot that runs alongside the writer and other readers.
- **Auto-commit queries.** `Database.Query` / `QueryGql` (.NET), `execute` (Java, Python, Node.js, with `executeAsync` in Node.js), `Execute` / `ExecuteGql` / `ExecuteContext` (Go) and `traverse_execute` / `traverse_execute_gql` (C) run a query as a transaction of its own: a read runs on a snapshot and never waits, a write commits when it succeeds and rolls back when it fails, and `CALL { ... } IN TRANSACTIONS` commits each batch. The query's plan decides read or write, procedures that write included. Cancel one with a `CancellationToken`, a `context.Context`, an `AbortSignal` or a canceller from `traverse_canceller_new`; waiting for the writer lock stops too.
- **Plans are cached.** Queries in the embedded bindings reuse plans across calls the way the server does: literals become parameters, so `{id: 7}` and `{id: 8}` share a plan, and a new or dropped index replans.
- **Busy timeout.** Waiting for the active write transaction is bounded: after 5 seconds by default, `begin`, the bulk writer and `flush` fail with a busy error (`TvErrorCode.Busy`, `traverse.BusyError`, `ErrBusy`, `TV_ERROR_BUSY`) instead of waiting forever, so a second `begin` on the thread holding the writer no longer hangs. Change it with `BusyTimeout` (.NET), `setBusyTimeout` (Java), `busy_timeout` (Python), `busyTimeout` (Node.js), `SetBusyTimeout` (Go) or `traverse_set_busy_timeout` (C).
- **Bulk writers are transactions.** A bulk load is invisible to readers until it commits, `abort` rolls it back, and `commit` saves the database with the load in it, so the data is written once and is durable when `commit` returns. Previously an aborted load stayed in memory and readers saw it while it was written.
- **Databases save themselves.** An embedded database saves in the background once its redo log has grown by 64 MiB, or when a commit has gone five minutes unsaved, so the log stays short and reopening after a crash replays little, without the application calling `flush`. A save that finds most of a file of 64 MiB or more reusable compacts it.
- **Saves don't hold up writes.** `flush` and `compact` work from a snapshot: the writer lock is held only for the moment it takes to start, so queries and commits carry on while the file is written, and what commits meanwhile is saved the next time.
- **Files stop growing under updates.** A save reuses the space of the records it deletes or replaces, and finds a page with room through a map instead of reading every page of the file for each record, so saving a few changes to a large database stays quick and a database updated over and over keeps its size.
- **Go modules live in the repository.** The Go packages are now `github.com/truespar/traverse/go` (embedded) and `github.com/truespar/traverse/go/http` (HTTP client), so `go get` fetches them straight from GitHub; change `truespar.com/traverse-go` imports to the new paths. The embedded package still loads the native library from `TRAVERSE_FFI_LIB` or next to the binary.
- **Go on Linux and macOS.** The embedded Go package only compiled on Windows; it now loads `libtraverse_ffi.so` / `.dylib` with `dlopen` there too, still without CGo. `Open` returns an error when the native library is missing or from another version, instead of panicking.
- **Python releases the GIL** while it waits for the writer lock, runs a query, commits, saves or closes, so other Python threads keep running.
- **Node.js package.** The embedded Node.js binding is published as `@truespar/traverse-node`, with prebuilt addons for Windows x64 and Linux x64 (glibc) and TypeScript types. Results are built straight into JavaScript values, so floats keep `NaN` and `+/-Infinity` (they came back as `null`), and a crash inside the engine fails the call with an error (GQLSTATUS `50N42`) instead of ending the Node.js process.
- **Bring your own native library.** `TRAVERSE_FFI_LIB` (.NET, as in Go) and `TRAVERSE_JAVA_LIB` or the `traverse.library` system property (Java) load the native library from a file, for a platform the packages don't bundle (macOS, ARM64) or a build of your own.
- **Java extracts its native library once.** The jar's library now goes into a per-user cache (`%LOCALAPPDATA%\traverse\jni` on Windows, `~/Library/Caches/traverse/jni` on macOS, `~/.cache/traverse/jni` on Linux), one copy per version, instead of a new temporary copy on every start, which Windows could never delete.
- **.NET reports a failed final save.** Disposing a `Database` throws a `TraverseException` when the save it makes on closing fails, like a `FileStream` whose last flush fails, instead of dropping what changed since the last save without a word.
- Fixed: on Linux the .NET, Java and Node.js embedded packages could fail to load their native library with "cannot allocate memory in static TLS block"; the Go bindings could report another call's error, or none, when a goroutine moved between threads; the Go bulk writer could pass the engine freed memory for a node's labels or the property keys and values of a node or relationship when a garbage collection ran at the wrong moment; Python raised `TraverseError` instead of `BusyError` when a query's wait for the writer lock timed out; an embedded database opened without durability after a crash in `wal` mode replayed the recovered log tail again after every save; a full save (`compact`) left the redo-log segments it covered on disk.

### Query engine

- **Per-query memory budget.** Every query now tracks the memory its own buffers hold - sorts, aggregations, hash joins, `Eager` barriers, variable-length expansions, result sets - against a budget. A query that exceeds it fails with a clear error (`query exceeded its memory limit ...`) while the server and every other query keep running. Set with `--query-memory-limit` / `TRAVERSE_QUERY_MEMORY_LIMIT` / `query_memory_limit` (default 25% of system RAM, `0` = unlimited); shown as `db.memory.transaction.max` in `SHOW SETTINGS`. The process-level budget (`TRAVERSE_MEMORY_LIMIT`) is now also enforced inside query execution and applies by default to the CLI and the embedded bindings.
- **Streaming writes.** `CREATE`, `SET`, `REMOVE` and `MERGE` apply one row at a time instead of buffering their whole input and output, so imports (`UNWIND $rows ... CREATE`, `LOAD CSV ... MERGE`, `MATCH (a {key}) MATCH (b {key}) CREATE (a)-[:R]->(b)`) and statements without `RETURN` run in memory proportional to a single row. The planner inserts an `Eager` operator - visible in `EXPLAIN` - only where a later clause must observe all of an earlier clause's writes.
- **Streaming joins and aggregation.** Cartesian products and hash joins emit rows as they are produced rather than materializing the joined result; `count`, `sum`, `avg`, `min`, `max` and `collect` over joins aggregate in one pass, holding only their groups.
- **Deep statements.** Generated scripts with thousands of chained clauses or deeply nested expressions no longer abort the process; the engine sizes its stack to the statement and rejects pathological nesting with a clear error.

### Seeing and stopping running work

- **`SHOW TRANSACTIONS` lists everything running**, across sessions and protocols (Bolt, HTTP, WebSocket, gRPC, MCP, embedded), with the standard columns: database, transaction and query ids, connection, client address, user, current query, start time, status and elapsed time by default; `YIELD *` adds metadata (Bolt `tx_metadata`), parameters, protocol, idle time and the memory the statement holds. Users see their own transactions; admins see everyone's.
- **`TERMINATE TRANSACTIONS` stops them**: by id, a list or a parameter, and chained after `SHOW TRANSACTIONS ... YIELD ... WHERE ...` to stop whatever matches. The running statement stops within milliseconds, its writes are rolled back, and the transaction's next message fails with `Neo.TransientError.Transaction.Terminated` (gRPC `ABORTED`).
- **A client going away stops its work**: a Bolt `RESET` or closed connection, an aborted or disconnected HTTP request, a cancelled gRPC call, and a new WebSocket `cancel` message. Studio's Run button turns into Stop while a query runs.
- **Time limits cover everything**: the statement's whole run including parallel algorithm phases, nested `apoc.cypher.*` calls and waiting for the writer lock. A Bolt `tx_timeout` now limits the transaction from `BEGIN` and reports `Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration`; gRPC requests honor `timeout_ms` with `DEADLINE_EXCEEDED`.
- **No more stalls behind a heavy query**: statements run off the server's async workers, so a long query on one connection can't hold up other connections or the cancels and timers above.
- **Embedded bindings stop statements too.** Each database has a statement timeout - `StatementTimeout` (.NET), `setStatementTimeout` (Java), `statement_timeout` (Python), `statementTimeout` (Node.js), `SetStatementTimeout` (Go), `traverse_set_statement_timeout` (C) - and a running statement can be cancelled from another thread: `Transaction.Cancel` or a `CancellationToken` (.NET), `cancel()` (Java, Python, Node.js), `Cancel` or a `context.Context` with `ExecuteContext` (Go), `traverse_canceller` + `traverse_cancel` (C). Node.js gets `executeAsync`, which runs off the event loop and takes an `AbortSignal`. A stopped statement fails with `TvErrorCode.Interrupted`, `QueryInterruptedException`, `traverse.QueryInterruptedError`, `ERR_TRAVERSE_INTERRUPTED`, `ErrInterrupted` or `TV_ERROR_INTERRUPTED`. Embedded transactions are listed by `SHOW TRANSACTIONS` (protocol `embedded`, database = the file name) and `TERMINATE TRANSACTIONS` ends them.
- Fixed: parallel phases of graph algorithms (all-pairs shortest paths, betweenness, closeness, harmonic centrality, similarity, random walks, node2vec training) ignored the query's time limit; `apoc.cypher.runTimeboxed` cleared the enclosing query's time limit and failed instead of returning the rows produced before its own limit (a timeboxed statement that writes still fails at its limit, so none of its writes are kept); APOC path expansion and `db.schema.*` sampling returned partial results when stopped instead of an error; parallel workers could see a stale statement clock (`datetime()` from an earlier query) and Bolt results streamed after the server switched threads could lose their parameters; the MCP query tool crashed on a failed commit and didn't mark its writes for saving; the import endpoint could panic on statements with non-ASCII text in error reports.

### GQL

- **`WHEN` and transaction blocks run everywhere.** `WHEN <condition> THEN <statement> [ELSE <statement>]` and `START TRANSACTION [READ ONLY | READ WRITE] ... COMMIT` now run on the server's GQL route and in every embedded binding; before, only the conformance test harness ran them. The condition is evaluated with no bindings (literals, parameters, `SESSION_USER`, functions). A transaction block runs its statements in order in the call's transaction, so over HTTP they all commit or none do, and returns the last statement's rows with every statement's counts; `READ ONLY` refuses statements that write before any runs. A block ending in `ROLLBACK` can't contain writes: use an explicit transaction over Bolt, gRPC or the embedded API to discard work.
- **A GQLSTATUS for every error.** Each error now names its condition with the ISO/IEC 39075 code where the standard has one (`22012` division by zero, `22003` numeric overflow, `22007` invalid datetime format, `25G03` a write in a `READ ONLY` block, `42001` invalid syntax, `G1001` deleting a node that still has relationships) and Neo4j's published code otherwise (`42N62` variable not defined, `42N48` unknown function, `42N08` unknown procedure, `42N51` missing parameter, `22N79`/`22N77`/`22N78` uniqueness, existence and type constraints, `25N17` `CALL { ... } IN TRANSACTIONS` inside a transaction, `51N73` memory limit, ...). Bolt 5.7+ FAILURE carries it in `gql_status` and `description` beside the matching Neo4j code (division by zero is now `Neo.ClientError.Statement.ArithmeticError`, an unknown procedure `Neo.ClientError.Procedure.ProcedureNotFound`, a busy writer lock the retryable `Neo.TransientError.Transaction.LockAcquisitionTimeout`); HTTP errors carry `gql_status` in either dialect; gRPC errors carry a `gql-status` metadata entry and a status code that follows it (`NOT_FOUND`, `FAILED_PRECONDITION`, `RESOURCE_EXHAUSTED`, `UNAVAILABLE`, ...); the embedded bindings expose it as `TraverseException.GqlStatus` (.NET), `getGqlStatus()` (Java), `gql_status` (Python), `err.gqlStatus` (Node.js), `TraverseError.GqlStatus` (Go) and `traverse_last_error_gql_status` (C).
- **Multi-character and explicit trim.** `btrim`, `ltrim` and `rtrim` take an optional second argument, the characters to strip (in Cypher too), and GQL has `TRIM([LEADING | TRAILING | BOTH] [char] FROM string)`. GQL trims strip spaces by default, as the standard says; Cypher keeps stripping any whitespace.
- **`USE <graph>` switches graphs.** `USE social MATCH (n) RETURN n` runs on the database `social` over the HTTP API, whatever the request's `database` field says; `USE graph.byName($name)` takes the name from a parameter. An embedded database runs a `USE` that names it and refuses any other graph. One request uses one graph: naming two is an error (`25G04`). `CONFORMANCE.md` now describes what the server and bindings run.
- Fixed: a block ending in `ROLLBACK` was treated like `COMMIT`; a statement that only writes (`INSERT ...` without `RETURN`) couldn't be followed by `COMMIT`, `ROLLBACK`, `ELSE`, `END` or `NEXT`.

### Graph ML

- **Node regression and link prediction pipelines** next to node classification: `traverse.ml.pipeline.nodeRegression.*` and `traverse.ml.pipeline.linkPrediction.*` take the same configuration procedures, `train`, `predict.stream` and `predict.write`. Link prediction scores the pairs that aren't connected yet and can write the top-ranked ones as new relationships.
- **R-GCN and GAT model candidates.** `addRGcn` (one weight matrix per relationship type) and `addGat` (multi-head attention) join `addGcn` and `addGraphSage` for all three tasks.
- **Model selection the GDS way.** Training holds out `testFraction` of the examples, scores every candidate with (stratified) k-fold cross-validation over `validationFolds`, retrains the winner on the whole training set and reports train, validation, outer-train and test metrics in `modelInfo`. Candidates accept ranges (`addGcn('p', {hidden: {range: [16, 128]}, learningRate: {range: [0.001, 0.1]}})`) sampled up to `maxTrials`; `configureAutoTuning('p', {strategy: 'successiveHalving'})` spends the epoch budget on the most promising trials. Pick the selection metric with `metrics` (`ACCURACY`, `F1_WEIGHTED`, `F1_MACRO`, `MEAN_SQUARED_ERROR`, `ROOT_MEAN_SQUARED_ERROR`, `MEAN_ABSOLUTE_ERROR`, `AUCPR`, `ROC_AUC`).
- **Node property steps.** `addNodeProperty` runs any node-property algorithm (PageRank, FastRP, Louvain, ...) or GraphMAE on the training graph before training and before every prediction, without writing to the database; select its `mutateProperty` as a feature. List and vector properties work as features too.
- **GraphMAE embeddings.** `traverse.ml.graphMae.stream` / `.write` learn self-supervised node embeddings from node properties and the graph's structure.
- **Explanations.** `traverse.ml.explain.pgExplainer`, `.counterfactual` and `.subgraphAudit` name the relationships behind a node classification: the most important ones, the fewest whose removal changes the prediction, and a small reproducible subgraph that keeps it.
- `predict.stream` returns class probabilities with `includePredictedProbabilities: true` and predicts the original class values; models predict on graphs that have grown since training; `train` accepts `nodeLabels`, `relationshipTypes`, `targetNodeLabels` and `wait: true`; `traverse.ml.trainStatus` reports trial progress.
- **GPU training and inference.** Servers built from source with the `ml-cuda` feature train, predict, embed and explain on NVIDIA GPUs; the published binaries run ML on the CPU. Every ML call takes a `device` setting (`'auto'`, `'cpu'`, `'gpu'` or `'gpu:<n>'`); calls without one use `--ml-device` / `TRAVERSE_ML_DEVICE` / `ml_device`, by default the first usable GPU, else the CPU. The GPU driver is loaded at runtime, so the same binary runs on machines without one. `traverse.ml.devices()` lists the CPU and GPUs, and why a GPU can't be used when it can't. A model trained on one device predicts on any other.
- **Graphs of any size.** When a full-graph training step wouldn't fit, training switches to mini-batches of sampled neighborhoods. Choose explicitly with `batching: 'fullGraph'` or `'miniBatch'`, `batchSize` (default 512) and `fanouts` (neighbors per hop, default `[25, 10]`, `-1` for all). Sampled neighbors are weighted so training matches whole-graph prediction. Evaluation and prediction stay exact and run a block of nodes at a time, in bounded memory. GraphMAE trains in mini-batches too.
- **Training jobs queue.** One training job runs at a time by default (`--ml-max-jobs` / `TRAVERSE_ML_MAX_JOBS` / `ml_max_jobs`) and the rest wait their turn. `traverse.ml.trainJobs()` lists them and `traverse.ml.trainCancel(jobId)` stops one. `train` returns at once: node property steps and GraphMAE now run inside the job.
- **Pipelines persist.** Pipelines are saved next to the database files and reload at startup; trained models are saved atomically.
- **Explanations scale.** Explanations work on the explained nodes' exact computation subgraph, so they cost the same on any graph size. GraphMAE, predictions and explanations stop when the query times out or runs out of memory.
- **Studio ML panel.** Predict a number or missing relationships as well as a category; pick GCN, GraphSAGE, GAT or R-GCN, or compare them and keep the best. Choose the device and training mode. Follow trial progress, cancel a run, and see runs still in progress after a reload. Each task shows its own test scores. Suggest and write new relationships, explain a prediction and show it on the graph, and see predicted numbers as node sizes.
- Fixed: training saw only part of the relationships, `configureSplit`, `configureAutoTuning` and node property steps had no effect, and `predict.write` bypassed property indexes and the write-ahead log. Naming the classes of a model in Studio made it predict class positions instead of the class values. A model that couldn't be saved to disk was reported as trained without a warning; `trainStatus` now says so in `error`.

### Vector indexes

- **Updates and deletes keep the index healthy.** Deleting a node or changing its embedding repairs the index in place and frees the slot for reuse, so search quality and memory stay steady under churn, with no rebuilds. Fixed: a query close to a deleted or updated vector could return no results.

### Storage

- **Database files are validated as they load.** Every `.tvdb` file - uploaded to the server, imported in Studio, opened by the CLI or an embedded binding - is checked while it loads: page layout, record encoding, ids and counters, relationships, labels and property data. A damaged or deliberately crafted file is refused with a clear error instead of crashing the process or loading an inconsistent graph; files written by earlier releases load unchanged.
- **No size limit on a value.** A node or relationship larger than a 16 KB page (a long text, a high-dimension embedding stored on a node, a large list or map) made every save of its database fail, so in the default snapshot mode the data written since the last save was lost at shutdown. Records, the schema catalog (names, index and constraint definitions, planner statistics) and the user catalog now continue on overflow pages when they need more than a page. A file that doesn't need them is written exactly as before, so earlier releases still open it; one that does says format version 7, which earlier releases refuse cleanly. In `wal` mode a single value larger than 16 MB now commits too.
- **Downloads are consistent.** Adding or changing a user no longer rewrites a page of the database file in place: the new user catalog goes after the last page and the header switches to it. A database download that's running meanwhile gets the file as it was when the download started, never a mix of old and new pages.
- **Checksummed `.tvmodel` files and user catalog.** Trained models and the user/role catalog are now stored in a framed container with a type tag, a format version and a CRC-32 checksum, so a truncated or corrupted file is reported as such instead of failing deep inside the parser - or loading with silently wrong contents. Files and databases written by earlier releases load unchanged and are rewritten in the new layout on their next save.

### Users and databases

- **User changes are saved as they're made.** `CREATE USER`, `ALTER USER` and `DROP USER` over Bolt and gRPC, and Studio's Users panel, write the change to the default database's file, crash-safely, before they return; a change that can't be saved fails and isn't made. Fixed: a periodic checkpoint or `POST /api/save` erased the users from the file, so the next start came up without them and with authentication off; with a data directory (`--data <folder>`) users were never loaded at startup, and Bolt and gRPC user changes weren't saved.
- **No silent open access.** If the stored users can't be read, the server refuses to start instead of starting without authentication; start it with `--auth user:pass` to replace them with that admin. It also refuses to start when the default database's file can't be opened, instead of starting empty and saving over it.
- **Faster, fairer logins.** gRPC checks a client's credentials once and remembers them until users change, instead of on every request. Password checks no longer hold up other logins or user changes, and an unknown user name takes as long to reject as a wrong password.
- **Safer database management.** `CREATE DATABASE` refuses a name whose `.tvdb` file is still in the data directory (an unloaded database, or one that failed to open) instead of overwriting it. The default database can't be unloaded, dropped or renamed. A file can be loaded as only one database at a time. Sessions still using a dropped or unloaded database get an error when they commit.
- Fixed: renaming a database in `wal` or `wal-async` mode left its redo log under the old name, so commits since the last save were lost on restart; dropping a database left its redo log behind, where it could replay into a later database of the same name. Aliases and Studio's saved queries and styles are written crash-safely, and concurrent Studio edits no longer lose one another.
- **The Windows service is the full server.** Running as a service now has gRPC, TLS, the periodic checkpoint, `wal-async` syncing, the configured durability mode, ML and `SHOW SETTINGS`, like the console server.
- A request that panics no longer makes every later request touching the database list, users, query plans or background tasks fail. A background task that fails unexpectedly is reported as failed instead of staying "running" and blocking `/api/quiesce`. Saves, loads, renames and user changes over HTTP no longer stall other requests while they write, and a download now fails if the database couldn't be saved first.
- Studio's AI chat, feedback and query-sharing proxies now verify the API server's certificate.

### Fixes

- A query whose writes sat under `OPTIONAL MATCH`, `FOREACH`, a relationship expansion or a few other operators could have them cut short: `LIMIT` stopped early, and a Bolt client that discarded the rest of the results left them half applied. Such queries now always run every write. Procedures that write (`apoc.create.*`, `apoc.merge.*`, `apoc.refactor.*`, `apoc.periodic.iterate`, algorithm `.write` modes, ...) count as writes the same way.
- `PROFILE` of a cached write skipped APOC triggers.
- `=~` and the `apoc.text.*` regex functions shared a cache of compiled patterns, so after one used a pattern the other could get the wrong form of it (`apoc.text.split('a,b', ',')` returned `['a,b']` after `'a,b' =~ ','` ran on the same thread).
- Virtual nodes and relationships from trigger parameters (`$deletedNodes`, `$deletedRelationships`) could share ids with ones made by `apoc.create.vNode` / `vRelationship` in the same query.
- Planner statistics too large for a file's catalog page (many labels and properties, collected with `ANALYZE GRAPH`) pushed the constraint and index definitions saved after them out of place, so opening the file lost or misread them. The catalog now continues on further pages when it needs to, and every entry stays where it belongs.
- A database whose latest commits came back from the redo log at startup counted as saved, so downloading it (or exporting it from Studio) before any other change returned a file without them, and the log segments holding them weren't trimmed.
- Embedded databases copied every node and relationship in memory on each save just to rewrite the file's catalog; a save now costs what changed since the last one.
- gRPC: a transaction begun read-only (`access_mode: "r"`, or on a read-only database) now rejects writes made through `Run` and the node/relationship/index RPCs.
- Algorithm write mode (`CALL traverse.pageRank.write(...)`, `gds.<algo>.write`) now updates property indexes on the written property and records the writes in the write-ahead log.
- .NET Bolt driver: once the pool's background maintenance had evicted an idle connection, `BoltPool` could open more connections than `MaxSize`, and returning a connection could throw `SemaphoreFullException`.
- .NET gRPC client: `TvValue.ToString()` formatted numbers in the current culture (`3,5` in Swedish); it now uses the invariant culture.

### Dependencies

- Removed the unmaintained `bincode` and `rustls-pemfile` crates. TLS certificate and key loading now goes through `rustls-pki-types`.
- Studio: `monaco-editor` 0.56 with `dompurify` 3.4.14, clearing every open `npm audit` advisory in the editor bundle; `pinia` 4, `markstream-vue` 2 (the Graph Assistant's markdown renderer).
- `rusqlite` 0.40 (bundled SQLite 3.53) behind `apoc.load.jdbc`'s SQLite dialect. A SQL string that contains more than one statement is now rejected with a clear error instead of silently running only the first.
- Current major versions across the board: `ureq` 3 (the HTTP client behind `LOAD CSV ... FROM 'http...'`, `apoc.load.json`/`jsonParams` and `apoc.spatial`, which now stream response bodies instead of buffering them), `arrow` 59, `ed25519-dalek` 3, `base64` 0.23, `lz4_flex` 0.14, `comfy-table` 8.
- `quick-xml` updated to 0.41, which fixes two denial-of-service issues (quadratic duplicate-attribute checking and unbounded namespace-declaration allocation) reachable through `apoc.load.xml`, `apoc.import.xml` and GraphML import.

---

## 0.8.2 - 2026-07-10

### Signed Windows binaries

Windows binaries (`traverse-server.exe`, `traverse-cli.exe`, `traverse-admin.exe`, `traverse-bench.exe`) are now Authenticode-signed - the publisher shows as **The Intelligence Company AB (publ)** in the Windows security prompts instead of "Unknown Publisher." Linux tarballs ship with a detached GPG signature and SHA-256 checksum.

### Query engine

- **Temporal types**: broader coverage and correctness fixes across dates, times, datetimes, and durations, including zoned and truncation edge cases.
- **GQL**: error semantics aligned with the ISO/IEC 39075 standard, and a range of correctness fixes across aggregation, path, and write operations.
- **Performance**: query-execution improvements, including a lower-overhead cartesian-product check and shared plan memoization.

### Fixes

- The "update available" notification now works - the version check previously queried with the wrong platform identifier and never found new releases. This also fixes `traverse-admin update --download` and the CLI self-update.

### Breaking changes

None. 0.8.2 is a maintenance and correctness release on top of 0.8.1.

---

## 0.8.1 - 2026-07-09

### Client compatibility

- **Configurable Bolt server agent.** Some client libraries only connect to a server that identifies as a genuine Neo4j instance. Set `--server-agent "Neo4j/5.26.0"` (or the `TRAVERSE_SERVER_AGENT` environment variable) to override the agent string advertised in the Bolt handshake, so those clients work against Traverse. The default remains `Traverse/<version>`.

### Breaking changes

None. 0.8.1 is a compatibility patch on top of 0.8.0.

---

## 0.8.0 - 2026-07-09

### APOC compatibility

Traverse now implements the APOC core catalog natively: **246 functions and 190 procedures**, with no plugin to install or allowlist.

- **Loaders**: `apoc.load.json` / `jsonParams` / `csv` / `xml` / `jdbc` (SQLite and PostgreSQL)
- **Export / import**: JSON, CSV, GraphML, Cypher script, and Arrow - in both directions
- **Triggers**: `apoc.trigger.*`, driven by graph change events
- **Background jobs**: `apoc.periodic.*` including the job scheduler, plus `apoc.refactor.*`, `apoc.schema.assert`, and conditional execution (`apoc.when` / `apoc.do.when`)
- **Aggregates & paths**: `apoc.agg.*`, the `apoc.path.expand` family, `apoc.meta.*`, `apoc.spatial.geocode`
- **Extended**: `apoc.generate.*` graph generators, `apoc.static.*` value store, `apoc.nodes.group`, `apoc.example.movies`
- New **BYTEARRAY** value type with `apoc.util.compress` / `decompress`

### GQL

- **ISO/IEC 39075 GQL**: every feature in the conformance tables, including quantified path patterns, path search prefixes, `NEXT` / `WHEN` composed queries, catalog statements, typed temporal literals, and the standard error envelope
- **GQL in the browser** - the WebAssembly build now includes the GQL dialect, so the Studio dialect picker works the same in-tab as against a server
- GQL entry points in all language bindings (Python, Java, Node.js, Go, .NET) and all HTTP clients

### Cypher

New Cypher features, including the Cypher 25 feature set:

- Quantified path patterns with per-iteration group variables; path selectors and match modes
- `SEARCH` clause - vector similarity search as a MATCH subclause
- `COLLECT { ... }` subqueries, `CALL { ... } IN TRANSACTIONS`, `FOREACH`
- Dynamic node labels, relationship types, and property names in both reads and writes
- `LOAD CSV` with dynamic columns and the `file()` / `linenumber()` helpers
- Composed queries (`NEXT`, `WHEN`), `FILTER` / `LET` / `FINISH` clauses, full label expressions, `||` concatenation
- `point()` and spatial predicates, `vector.similarity.*` functions, `IS ::` type predicates
- Composable `SHOW` commands: `SETTINGS`, `TRANSACTIONS`, `FUNCTIONS`, `PROCEDURES`, and typed `SHOW INDEXES` - all with `YIELD` / `WHERE` / `RETURN`

### Indexes & constraints

- **Vector index** - HNSW with SIMD-accelerated distance kernels and int8 quantization; `CREATE VECTOR INDEX` + `db.index.vector.queryNodes`
- **Full-text index** - BM25 ranking with phrase, prefix, and fuzzy query syntax
- **Text index** - accelerates `CONTAINS` and `ENDS WITH` filters
- **Point index** - spatial index with transparent query acceleration
- Relationship-property indexes, composite indexes, and `USING INDEX` / `USING SCAN` planner hints
- Composite `NODE KEY` / `UNIQUE` constraints, property type constraints, and relationship constraints (uniqueness / existence / type / key)

All index and constraint definitions persist in the database file and participate in crash recovery.

### Durability - opt-in write-ahead logging

Set `TRAVERSE_DURABILITY=wal` to enable per-commit durability: every committed write survives a hard crash and is replayed on the next start. Group commit keeps write throughput scaling under concurrency. The default remains snapshot-on-shutdown, unchanged from 0.7.0.

### ML pipelines

- `traverse.ml.pipeline.nodeClassification.*` procedures - configure, train, and predict entirely from Cypher, with asynchronous training jobs
- GNN model candidates: GCN and GraphSAGE pipelines, with R-GCN, GAT, and GraphMAE self-supervised pretraining in the ML engine
- Training on GPU (CUDA) or CPU; hyperparameter auto-tuning; stratified k-fold cross-validation with detailed metrics
- Explainability: PGExplainer, counterfactual explanations, and subgraph audit mode
- Model catalog with `.tvmodel` persistence; predictions can be written back as node properties
- **Studio ML panel** covering the full pipeline lifecycle, and **in-browser inference** via the new `@truespar/traverse-ml-wasm` package (CPU or WebGPU)

### GDS

- `gds.*` procedure compatibility: the graph catalog with named projections, algorithm execution on projections, mutate mode (algorithm results as projection property overlays), and link-prediction functions
- More of the algorithm library runs in parallel

### Performance

Faster query execution: plan caching on the write path, index seeks from runtime expressions, pattern-comprehension and `UNWIND` optimizations, and planner improvements including predicate push-down and `EXISTS` short-circuiting.

### Studio

- ML pipelines panel, GQL syntax highlighting and dialect picker, APOC & GDS reference entries in the docs panel
- Error toasts for failed queries

### Server & Bolt

- Improved driver interoperability: richer failure metadata, correct retry classification for timeouts, and graceful mid-stream error delivery
- Hardened wire-protocol limits and overload behavior

### Bug fixes

- `ORDER BY` results on parallel scans
- Floating-point `ORDER BY` comparison edge case
- Empty `CALL { }` subquery bodies under certain plan shapes
- `LIMIT` with parameters over Bolt
- Index staleness after `SET` on text-indexed properties

### Breaking changes

None. 0.8.0 is a feature release on top of 0.7.0 - existing Cypher queries, Bolt connections, embedded SDK calls, and HTTP API requests continue to work unchanged. Durability remains snapshot-only by default; write-ahead logging is strictly opt-in.


## 0.7.0 - 2026-05-15

### Graph Data Science library

Traverse now ships a built-in GDS library with **36+ algorithms** callable from Cypher via `CALL traverse.<algorithm>.<mode>(<config>) YIELD ...`. Every algorithm supports `stream` / `stats` / `write` modes where they apply; write mode stores the result as a node property.

- **Centrality**: PageRank, Article Rank, Eigenvector, Closeness, Harmonic Closeness, Betweenness, HITS, Degree, CELF
- **Community detection**: Louvain, Leiden, Label Propagation, WCC, SCC, K-Core, Triangle Count + LCC, Conductance, Modularity
- **Paths & traversal**: Dijkstra, A* (with ALT heuristic for non-geographic graphs), Bellman-Ford, Yen's K-Shortest, All-Pairs Shortest Paths, Delta-Stepping, BFS, DFS, Random Walk, Spanning Tree, Steiner Tree
- **DAG**: Topological Sort, Longest Path
- **Similarity**: Node Similarity (Jaccard), Cosine Similarity, kNN (structural / neighbor-overlap), kNN-Property (feature-vector / Euclidean, `gds.knn` semantics)
- **Link prediction**: Common Neighbors, Total Neighbors, Preferential Attachment, Adamic-Adar, Resource Allocation, Same Community
- **Embeddings**: FastRP, Node2Vec, HashGNN, GraphSAGE

**Studio integration**: the new GDS drawer in Studio exposes the catalog with a form-driven config builder, live result preview on the graph (node size / color gradient / community colors), and result tables for path / node-pair / entity-set / embedding outputs.

### Traverse in the browser (WebAssembly)

The Traverse engine now runs in a browser tab through WebAssembly, without a server. Try it at **https://traverse.truespar.com**.

- **The server's engine** - Cypher, GDS, the schema browser and the query editor work as against a server
- **Persistence via OPFS** - databases are stored in the browser's Origin Private File System and survive tab reloads
- **Multi-threaded** - uses all available browser threads
- **Pre-built sample databases** - Movies, OurAirports, OpenFlights, and Pokec (small + medium) downloadable directly into OPFS, each with starter queries
- **Shareable queries** - share links carry the full query + active database; no server round-trip required
- **AI chat + feedback** - included in the in-browser demo (rate-limited)
- **Embeddable** - also available as the `@truespar/traverse-wasm` npm package for integrators. See the [Embedding WASM docs](https://truespar.com/traverse/docs/v1/wasm-integration) for installation, hosting (Vercel / Netlify / Cloudflare / nginx / Apache / IIS), and TypeScript types.

### Studio

- **Layout for phones and tablets**: bottom tab bar, slide-over side panels, full-screen AI chat overlay, soft-keyboard handling, larger touch targets, no iOS auto-zoom in the editor
- **Add to Home Screen** support: with the manifest and Apple meta tags, Traverse launches full screen on iOS
- **Auto theme** - first visit follows the system light/dark preference; explicit toggle persists from then on
- **Pre-built sample databases panel** under Examples, with starter queries that install as saved queries on first load
- **AI chat knows the GDS catalog**: the assistant suggests `CALL` syntax with valid algorithm names, supported modes and required config
- **Result panel** - click the query-time chip to see a parse / plan / execute / round-trip breakdown; thousands separators in row counts; fixes for Table and JSON view OOMs on large results

### Performance

- **Weakly Connected Components** and **Random Walk** run faster
- **kNN-Property**: NN-Descent runs in parallel across cores
- **Label Propagation**: fewer iterations to convergence
- **2-hop scan + aggregate** queries on graphs without self-loops use a fast path again

### Bug fixes

- Planner crashes on `OPTIONAL MATCH` and multi-`MATCH` patterns with pre-bound endpoints
- Accessing an entity after `DELETE`, and counting self-loops, now follow openCypher
- `datetime.truncate` timezone offset for non-current dates
- `bellman_ford.stream` / `delta_stepping.stream` now populate `totalCost`
- Browser mode: rendering of node / edge / path values in result tables; map values now serialize as plain JS objects

### Breaking changes

None. 0.7.0 is a feature release on top of 0.6.7 - existing Cypher queries, Bolt connections, embedded SDK calls, and HTTP API requests continue to work unchanged.

---

## 0.6.7 - 2026-04-03

### Performance

- **Less memory during graph loading** - building the adjacency lists no longer makes temporary copies
- **Dirty-flag persistence** - only databases that have been modified are written to disk on shutdown, reducing save time for multi-database deployments
- **Raised default memory ceiling** from 70% to 90% of system RAM, allowing larger databases to load without manual tuning

### Server

- **`--log-dir` option** - write daily rotating log files to a directory (e.g. `--log-dir /var/log/traverse`)

### Bolt Protocol

- **Fix Bolt 6.0 wire format handling** - resolves connection failures with clients using Bolt protocol version 6.0

---

## 0.6.1 - 2026-03-11

### Server

- **Disk space protection** - file uploads are rejected with HTTP 507 when free disk space is critically low
- **Disk stats in health endpoint** - `/api/health` now includes `disk.free_bytes` and `disk.total_bytes`

---

## 0.6.0 - 2026-03-07

### Stability & Memory Fixes (Critical)

This release fixes memory leaks and out-of-memory crashes found in 0.5.0:

- **Fix main-heap memory leak on database unload** - databases were not being fully reclaimed after hot-swap cycles
- **Fix mimalloc RSS ratchet** - memory allocator was not returning pages to the OS after database unload, causing monotonically increasing RSS
- **Enforce memory ceiling during database load** - prevent OOM crashes by checking available memory before loading
- **Pre-flight memory check before database loading** - reject loads that would exceed available memory
- **Fix bulk-free incorrectly triggering for non-DB-allocated databases**
- **Fix server total memory on Windows** - use `estimated_memory` instead of working set for accurate reporting

### Language Bindings (Embedded)
- **Python** (PyO3): embedded bindings - `Database`, `Transaction`, `BulkWriter`, property indexes
- **Java** (JNI): embedded bindings with bundled native libraries (Windows x86_64, Linux x86_64)
- **Node.js** (napi-rs 3): embedded bindings, with an async API
- **Go** (FFI/syscall): embedded bindings - no CGo required, uses `syscall.LoadDLL`/`dlopen`
- **.NET**: `Traverse.Embedded` (FFI bindings), `Traverse.Grpc`, `Traverse.Bolt`, `Traverse.Http`

### HTTP Clients
- **Python** (`traverse-http`): HTTP management client - databases, queries, import, upload, tasks
- **Java** (`com.truespar:traverse-http`): HTTP management client with Jackson 3.1
- **Node.js** (`@truespar/traverse-http`): HTTP management client using native `fetch`
- **Go** (`go/http`): HTTP management client - standard library only, no external deps

### Studio (Web UI)
- Conditional node and edge styling with if/then/else rules based on property values (e.g. color nodes red when `status = 'critical'`)
- Library reference panel with docs for all language bindings and HTTP clients
- Fix files tab empty after login by deferring fetch until authenticated
- Fix memory popover to show all databases

### Server
- HTTP API + WebSocket enabled by default on port 7691
- Built-in MCP server for AI integration
- Bolt 6.0 protocol support
- Prevent concurrent database load/unload/drop via lifecycle semaphore
- Fix upload hanging on duplicate database name

### Packaging
- **Maven Central**: `com.truespar:traverse-java:0.6.2`, `com.truespar:traverse-http:0.6.0`
- **npm**: `@truespar/traverse-http@0.6.0`
- **NuGet**: `Traverse.Embedded`, `Traverse.Grpc`, `Traverse.Bolt`, `Traverse.Http`
- **Docker Hub**: `truespar/traverse:0.6.0`
- **Windows**: `traverse-0.6.0-x86_64-windows.zip`
- **Linux**: `traverse-0.6.0-x86_64-linux.tar.gz`

---

## 0.5.0 - 2026-03-05 (unstable)

> **Warning:** This was the first public release and contains known memory leaks that cause OOM crashes during database hot-swap cycles. Upgrade to 0.6.0 or later.

### Engine
- In-memory property graph with single-file `.tvdb` persistence
- Write-ahead log for crash-safe persistence
- Incremental dirty-page tracking and in-place flush
- MVCC with snapshot isolation
- B-tree property indexing with Cypher DDL and planner integration
- Edge indexes for fast relationship property lookups
- Cost-based query optimizer with DP join ordering
- `shortestPath()`, `allShortestPaths()`, and weighted shortest path (Dijkstra)
- Existence and unique constraints with eager validation
- Morsel-driven pipeline parallelism for read-only queries
- Aggregate pushdown, expand-aggregate fusion, typed i64 fast paths
- Arena/pool allocators for query execution
- LOAD CSV for bulk data import
- C FFI layer for cross-language bindings
- Memory budget enforcement with configurable `TRAVERSE_MEMORY_LIMIT`

### Cypher
- The openCypher query language
- CALL procedure support
- `reduce()`, map projections, list comprehensions
- SHOW INDEXES / SHOW CONSTRAINTS commands
- Global stripped-query plan cache

### Bolt Protocol
- Bolt 5.x and 6.0
- TLS support (`bolt+s://`)
- Authentication and RBAC with role-based access control
- Multi-database support with bookmarks
- NOOP keepalive, tx_timeout, query notifications, Vector type
- Streaming result materialization
- Pipelined batch imports (512 stmts/batch)

### Server
- Multi-database support with aliases and read-only databases
- gRPC API alongside Bolt protocol
- HTTP API with API key authentication and OpenAPI docs
- Database upload/download via HTTP
- Async task system for long-running operations
- IP allowlisting and query timeout
- Configuration system (TOML config, CLI flags, env vars)
- Version check and update notification
- Colored startup banner with connection URLs

### CLI
- Interactive Cypher REPL with tab completion
- Script execution, parameters, plan rendering
- Multi-database support with `:use` command
- TLS support for `bolt+s://` connections

### Studio (Web UI)
- Graph visualization with ForceAtlas2 and D3 layouts
- CSV import, database upload/download, file manager
- Schema panel with property nesting under labels
- Cypher editor with Monaco, per-database query drafts
- Style editor with color picker, shapes, conditional rules
- Graph Assistant: AI chat with SSE streaming
- Command palette (Ctrl+P), quick-save (Ctrl+S)
- Dashboard with benchmark charts
- Save & share queries
- License activation and device management
- Memory tooltip with per-database breakdown

### .NET
- `Traverse.Embedded`: Embedded graph database bindings
- `Traverse.Grpc`: gRPC client library
- `Traverse.Bolt`: Bolt client with no dependencies
- `Traverse.Http`: HTTP management client for the whole API

### Licensing
- Trial mode with check-in, device tracking, and plan enforcement
- Ed25519 response signing for API security

### Performance
- Parallel execution of scans, expansions and aggregations
- Smaller adjacency and property storage, with faster lookups during traversals
- Results stream to the client as they are produced
