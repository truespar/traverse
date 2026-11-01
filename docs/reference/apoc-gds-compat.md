# APOC & GDS Compatibility

Traverse implements Neo4j's APOC and GDS libraries natively, so queries
written for them run unchanged. Behavior follows APOC core 2026.05 and
GDS 2.x.

## APOC

**Coverage: 100% of the APOC core catalog** - 246/246 functions and
190/190 procedures. Call them exactly as on Neo4j:

```cypher
RETURN apoc.text.levenshteinDistance('kitten', 'sitting');
CALL apoc.periodic.iterate('MATCH (n:Old) RETURN n', 'DELETE n', {batchSize: 1000});
CALL apoc.trigger.add('audit', 'UNWIND $createdNodes AS n SET n.created = timestamp()', {phase: 'after'});
CALL apoc.export.json.all('backup.jsonl', {});
CALL apoc.load.jdbc('jdbc:sqlite:/data/legacy.db', 'SELECT * FROM users');
```

Beyond the plain functions:

| Area | Support |
|------|---------|
| Triggers (`apoc.trigger.*`) | Full event system: `$createdNodes`, `$deletedNodes` (virtual snapshots), `$assignedLabels`, property old/new values |
| Background jobs (`apoc.periodic.*`) | Real scheduler: `submit`, `repeat`, `countdown`, `list`, `cancel` |
| Export/import | JSON, CSV, Cypher script, GraphML, Arrow - file or `{stream: true}`; local-files-only trust model |
| Dynamic Cypher | `apoc.cypher.run*`; `runFirstColumn*` compile to native subqueries |
| Virtual entities | `apoc.create.vNode` / `vRelationship`, virtual graph builders, `apoc.nodes.group` aggregation |
| External data | `apoc.load.json/jsonParams/xml/arrow`, `apoc.load.jdbc` (SQLite, PostgreSQL via native drivers), `apoc.spatial.geocode` (provider-configurable) |
| Static values & metadata | `apoc.static.*`, `apoc.meta.*`, `apoc.schema.*` |

Documented approximations (not bit-identical, semantics preserved):
phonetics (compact Metaphone), hashing fingerprint canonicalization,
`apoc.algo.dijkstra` treats missing weights as `1.0` (APOC: NaN).
Out of scope: JVM internals (`apoc.warmup.run` is a
no-op, `apoc.log.stream` streams Traverse's own log ring), remote Bolt
(`apoc.bolt.*`).

## GDS (Graph Data Science)

The `gds.*` procedures run on Traverse's own algorithm implementations,
with GDS's catalog model, call shapes and column names:

```cypher
CALL gds.graph.project('social', 'User', {FRIEND: {type: 'FRIEND', properties: 'weight'}});
CALL gds.pageRank.stream('social', {maxIterations: 20})
YIELD nodeId, score RETURN gds.util.asNode(nodeId).name, score;
CALL gds.fastRP.mutate('social', {mutateProperty: 'emb', embeddingDimension: 128});
CALL gds.knn.stream('social', {nodeProperties: ['emb'], topK: 10})
YIELD node1, node2, similarity RETURN *;
CALL gds.graph.nodeProperties.write('social', ['emb']);
CALL gds.graph.drop('social');
```

- **Materialized projections**: `gds.graph.project` builds a compact
  snapshot once (dense-id CSR); algorithm calls run against it with
  O(projection) cost and don't see concurrent writes, as in
  GDS.
- **32 algorithms, all modes** (`stream` / `stats` / `write` /
  `mutate`): pageRank, articleRank, eigenvector, betweenness, closeness
  (+harmonic), degree, hits, wcc, scc, louvain, leiden,
  labelPropagation, triangleCount, kcore, nodeSimilarity, knn, fastRP,
  hashgnn, node2vec, bfs, dfs, shortestPath.dijkstra/astar/yens,
  bellmanFord, allShortestPaths (+delta), randomWalk,
  dag.topologicalSort, dag.longestPath, influenceMaximization.celf.
- **Mutate chains**: `mutate` stores results as projection overlays;
  `gds.graph.nodeProperties.stream/write/drop` manage them, and
  algorithms consume them as input (e.g. knn over mutated embeddings).
- **Functions**: `gds.util.asNode/asNodes/NaN/infinity/isFinite`,
  `gds.similarity.*`, `gds.linkprediction.*` (values match GDS to 4
  decimals).

Known divergences: `closeness` uses standard closeness rather than
GDS's Wasserman-Faust normalization; `shortestPath.astar` requires the
live-graph path (geo heuristics); pageRank scores are rescaled to GDS's
unnormalized scale. Unmapped algorithms (e.g. kmeans) return an error
listing the supported set.

## Value types

`BYTEARRAY` values (from `apoc.util.compress`, Arrow streams, byte
properties) are a value type of their own: `bytes` over Bolt, base64 strings
over the HTTP API / gRPC / Node.js embedded binding, `bytes` in the
Python embedded binding, hex strings via the C FFI string accessor.

## How compatibility is tested

The engine's tests pin the behavior of each function and procedure
family: `crates/traverse-cypher/tests/apoc_compat.rs` (APOC),
`gds_compat.rs` (GDS) and `procedure_compat.rs` (the built-in `db.*` and
`dbms.*` procedures). Run them with
`cargo test -p traverse-cypher --test apoc_compat --test gds_compat --test procedure_compat`.
