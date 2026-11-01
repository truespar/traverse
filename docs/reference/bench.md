# Traverse Bench Reference

`traverse-bench` is a benchmark and import tool for graph databases. It connects to any Bolt-compatible server (Traverse, Neo4j, Memgraph) and runs a suite of 26 Cypher queries against the Pokec social network dataset, measuring throughput, latency, and server resource usage. It also supports pipelined bulk import of `.cypher` files.

## Quick Start

```bash
# Download and import the medium dataset into Traverse (auto-downloads if not cached)
traverse-bench --import medium --port 7690

# Benchmark all queries against a local server
traverse-bench --port 7690 --variant medium --duration 10 --output results.json

# Benchmark specific queries
traverse-bench --port 7690 --variant medium --groups expansion_1,expansion_2

# List all available queries and groups
traverse-bench --list-queries

# Download CSV files for Neo4j native import
traverse-bench --download medium --format csv
```

## Command-Line Options

### General

| Flag | Description |
|------|-------------|
| `-V`, `--version` | Print version and exit |
| `--list-queries` | List all available queries and groups, then exit |

### Connection

| Flag | Description | Default |
|------|-------------|---------|
| `--host <HOST>` | Bolt server hostname | `127.0.0.1` |
| `--port <PORT>` | Bolt server port | `7690` |
| `--auth <USER:PASS>` | Authentication credentials | *(none)* |

### Benchmark Mode

Active when `--import` is not specified.

| Flag | Description | Default |
|------|-------------|---------|
| `--variant <VARIANT>` | Dataset variant: `small` (10k), `medium` (100k), `large` (1.6M vertices). Errors on unknown values. | `small` |
| `--duration <SECS>` | Measurement duration per query in seconds | `10.0` |
| `--warmup <N>` | Warmup iterations per query (serial, excluded from results) | `3` |
| `--concurrency <N>` | Number of parallel worker connections | `1` |
| `--groups <NAMES>` | Comma-separated query groups or individual query names (groups: `read`, `expansion`, `scan`, `aggregate`, `shortest_path`, `update`, `write`); omit for all queries | *(all)* |
| `--server-pid <PID>` | Server PID for resource profiling; auto-detected if omitted | *(auto)* |
| `-o`, `--output <FILE>` | Write JSON results to file | *(stdout)* |

### Import Mode

Active when `--import` is specified. Accepts a file path or a variant name (`small`, `medium`, `large`) - variant names auto-download the dataset if not cached.

| Flag | Description |
|------|-------------|
| `--import <FILE \| VARIANT>` | Path to `.cypher` file, or variant name to auto-download and import |

### Download Mode

| Flag | Description | Default |
|------|-------------|---------|
| `--download <VARIANT>` | Download the pokec dataset for the specified variant (`small`, `medium`, `large`) and exit | - |
| `--format <FORMAT>` | Download format: `cypher` (Bolt import) or `csv` (Neo4j native import) | `cypher` |
| `--download-dir <DIR>` | Output directory for downloaded datasets | `benchmarks/data/dataset_cache` |

Downloaded files are cached locally - subsequent runs reuse the cached files without re-downloading.

## Dataset Variants

The Pokec dataset is a real social network graph from Slovakia, available in three sizes:

| Variant | Vertices | Edges | Cypher (gz) | CSV (tar.gz) |
|---------|----------|-------|-------------|--------------|
| `small` | 10,000 | 122K | 810 KB | 653 KB |
| `medium` | 100,000 | 1.8M | 14 MB | 12 MB |
| `large` | 1,632,803 | 30.6M | 277 MB | 231 MB |

Two download formats are available:

- **cypher** - Cypher `CREATE` statements for Bolt-based import into Traverse or Memgraph
- **csv** - Node and edge CSV files for Neo4j native import via `neo4j-admin`

### Importing into Traverse or Memgraph

```bash
# Auto-download and import in one step
traverse-bench --import medium --port 7690

# Or download first, then import
traverse-bench --download medium
traverse-bench --import benchmarks/data/dataset_cache/pokec_medium_import.cypher --port 7690
```

### Importing into Neo4j

For large datasets use the CSV format with Neo4j's bulk importer, `neo4j-admin`, rather than an import over Bolt:

```bash
# Download CSV files
traverse-bench --download medium --format csv

# Import with neo4j-admin (stop Neo4j first)
neo4j-admin database import full \
  --nodes=User=benchmarks/data/dataset_cache/pokec_medium_nodes.csv \
  --relationships=Friend=benchmarks/data/dataset_cache/pokec_medium_edges.csv \
  neo4j

# Create indexes after import
cypher-shell -u neo4j -p password "CREATE INDEX FOR (n:User) ON (n.id)"
```

## Queries

26 benchmark queries organized into 7 groups. Use `--groups` to select by group name or individual query name.

### read - Point Lookups

| Query | Description |
|-------|-------------|
| `single_vertex_read` | Fetch a vertex by ID (`MATCH (n:User {id: $id}) RETURN n`) |
| `single_vertex_property` | Fetch specific properties (`RETURN n.age, n.name`) |
| `pattern_short` | 1-hop outgoing neighbor, single result |
| `pattern_cycle` | Detect cycle back to source vertex |
| `pattern_long` | 5-node path pattern with mixed directions |

### expansion - Multi-Hop Traversal

| Query | Description |
|-------|-------------|
| `expansion_1` | All 1-hop outgoing neighbors |
| `expansion_1_with_filter` | 1-hop with `WHERE n.age >= 18` |
| `expansion_2` | All 2-hop outgoing neighbors (distinct) |
| `expansion_2_with_filter` | 2-hop with age filter |
| `expansion_3` | 3-hop outgoing neighbors (distinct) |
| `expansion_3_with_filter` | 3-hop with age filter |
| `expansion_4` | 4-hop outgoing neighbors (distinct) |
| `expansion_4_with_filter` | 4-hop with age filter |
| `neighbours_2` | 1-2 hop via variable-length path `[*1..2]` |
| `neighbours_2_with_filter` | 1-2 hop with age filter |
| `neighbours_2_with_data` | 1-2 hop returning multiple properties |
| `neighbours_2_with_data_and_filter` | 1-2 hop with filter, returning full vertex |

### scan - Full Graph Scans

| Query | Description |
|-------|-------------|
| `scan_expand` | All edges with both endpoint IDs |
| `scan_expand_filter` | Edges filtered by target age > 50 |
| `scan_expand_2hop` | 2-hop path scan, distinct endpoints |
| `scan_expand_2hop_filter` | 2-hop scan with age filter |
| `scan_project` | All vertices, selective properties |
| `scan_agg_global` | 2-hop global count |
| `scan_agg_grouped` | 2-hop grouped by target age |

### aggregate - Aggregation

| Query | Description |
|-------|-------------|
| `count` | Count all vertices and non-null ages |
| `min_max_avg` | Min, max, avg of age |
| `group_by_age` | Group by age with count |
| `aggregate_with_filter` | Group by age with `WHERE age >= 18` |

### shortest_path - Graph Algorithms

| Query | Description |
|-------|-------------|
| `shortest_path` | Shortest path between two vertices (up to 15 hops) |
| `all_shortest_paths` | All shortest paths between two vertices (up to 2 hops) |

### update - Property Writes

| Query | Description |
|-------|-------------|
| `vertex_update` | Set a property on a vertex (`SET n.property = -1`) |

### write - Structural Writes

| Query | Description |
|-------|-------------|
| `vertex_create` | Create a new vertex with label `UserTemp` |
| `edge_create` | Create an edge between two existing vertices |

## Import Mode

Pipelined batch import of `.cypher` files over Bolt, faster than sending one statement at a time.

```bash
traverse-bench --host 127.0.0.1 --port 7689 \
  --import benchmarks/data/medium/dataset_cache/pokec_medium_import.cypher
```

Behavior:

1. Automatically creates an index on `User.id` (tries Traverse/Neo4j syntax first, falls back to Memgraph syntax).
2. Batches 512 statements per network flush (pipelined RUN+PULL pairs).
3. Automatically resets the connection on statement failures and continues.
4. Reports progress every ~10,000 statements with throughput (stmt/s).

## Execution Flow

1. **Connect** - Establishes Bolt connections (negotiates version 5.4 or 4.x).
2. **Warmup** - Runs `--warmup` iterations per query serially on a single connection. Not included in results.
3. **Benchmark** - Spawns `--concurrency` parallel workers. Each worker executes the query in a tight loop with random parameters until `--duration` seconds elapse.
4. **Collect** - Merges latencies from all workers. Computes throughput, percentiles, and resource statistics.
5. **Output** - Prints summary table to stderr, writes JSON to `--output` or stdout.

## Resource Profiling

On Linux, the tool automatically profiles the server process by sampling `/proc/<pid>/stat` once per second. It reports:

- **CPU %** - avg, min, max, p50, p95
- **RSS** - min, avg, max, final (MB)
- **Virtual memory** - max (MB)
- **Thread count** - min, max

The server PID is auto-detected via `ss -tlnp` from the listen port. Override with `--server-pid` if auto-detection fails.

## Output Format

JSON output (to file via `--output` or to stdout):

```json
{
  "benchmark": "pokec",
  "variant": "medium",
  "vertex_count": 100000,
  "duration_per_query": 10.0,
  "warmup_iterations": 3,
  "concurrency": 1,
  "results": [
    {
      "query_name": "expansion_1",
      "status": "ok",
      "iterations": 39960,
      "errors": 0,
      "duration_sec": 10.001,
      "throughput_qps": 3996.0,
      "concurrency": 1,
      "latency_ms": {
        "min": 0.215,
        "avg": 0.251,
        "p50": 0.240,
        "p95": 0.324,
        "p99": 0.399,
        "max": 1.234
      },
      "resources": {
        "samples": 11,
        "duration_sec": 10.1,
        "cpu_pct": { "avg": 45.2, "min": 30.1, "max": 95.8, "p50": 48.5, "p95": 92.1 },
        "rss_mb": { "min": 128.5, "avg": 150.3, "max": 175.2, "final": 172.5 },
        "vsize_mb": { "max": 512.0 },
        "threads": { "min": 4, "max": 16 }
      }
    }
  ],
  "resources": { }
}
```

## Examples

### Compare Traverse vs Memgraph

```bash
# Traverse
traverse-bench --port 7690 --variant medium --duration 10 \
  --output results_traverse.json

# Memgraph
traverse-bench --port 7689 --variant medium --duration 10 \
  --output results_memgraph.json
```

### Run only expansion queries with 4 concurrent workers

```bash
traverse-bench --port 7690 --variant medium \
  --groups expansion --concurrency 4 --duration 15
```

### Quick smoke test

```bash
traverse-bench --port 7690 --variant small --duration 2 --warmup 1
```
