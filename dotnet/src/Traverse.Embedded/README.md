# Traverse.Embedded

The .NET 10 SDK for embedded [Traverse](https://truespar.com/traverse/docs): the database runs in your process, with no server. Open or create a `.tvdb` file, query it with Cypher or GQL, load data with the bulk writer, and render results with Sigma.js.

## Installation

```bash
dotnet add package Traverse.Embedded
```

## Quick Start

```csharp
using Traverse;

// Open (or create) a database
using var db = Database.Open("my-graph.tvdb");

// Write data inside a transaction
using (var tx = db.BeginTransaction())
{
    tx.Query("CREATE (a:Person {name: 'Alice', age: 30})");
    tx.Query("CREATE (b:Person {name: 'Bob', age: 25})");
    tx.Query("""
        MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'})
        CREATE (a)-[:KNOWS {since: 2020}]->(b)
    """);
    tx.Commit();
}

// Read data
using (var tx = db.BeginTransaction())
{
    using var result = tx.Query("MATCH (p:Person) RETURN p.name, p.age ORDER BY p.age");

    foreach (var row in result)
    {
        Console.WriteLine($"{row.GetString("p.name")} is {row.GetInt64("p.age")} years old");
    }
    // Output:
    //   Bob is 25 years old
    //   Alice is 30 years old

    tx.Abort();
}
```

## API Overview

### Database

```csharp
// Open or create a database file
using var db = Database.Open("path/to/graph.tvdb");

// Flush dirty pages to disk
db.Flush();

// Run one query as a transaction of its own: a read on a snapshot, a write committed on
// success and rolled back on failure (CALL { ... } IN TRANSACTIONS commits each batch)
using var count = db.Query("MATCH (n) RETURN count(n)");
db.Query("CREATE (:Person {name: 'Ada'})").Dispose();

// Begin a write transaction (one at a time; waits up to BusyTimeout for the active one)
using var tx = db.BeginTransaction();

// Begin a read transaction: a snapshot as of now, runs alongside the writer
using var reader = db.BeginReadTransaction();

// How long writers wait for the active one before throwing TvErrorCode.Busy (default 5 s)
db.BusyTimeout = TimeSpan.FromSeconds(30);

// How long a statement may run before it throws TvErrorCode.Interrupted (default: no limit)
db.StatementTimeout = TimeSpan.FromSeconds(10);
```

Disposing the database rolls back transactions and bulk writes still open on it (their later
calls throw), saves, and releases the file. When that save fails `Dispose` throws a
`TraverseException`, like a `FileStream` whose last flush fails: what changed since the last save
is not on disk. Every object is safe to use from any thread, one call at a time; a transaction
that is never disposed is rolled back by the finalizer.

### Transaction

```csharp
// Execute a Cypher query
using var result = tx.Query("MATCH (n:Person) RETURN n.name");

// Commit changes
tx.Commit();

// Or abort (also happens automatically on Dispose if not committed)
tx.Abort();

// Stop a query from another thread, or through a CancellationToken
// (it then throws OperationCanceledException)
using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(2));
using var slow = tx.Query("MATCH (a), (b) RETURN count(*)", cts.Token);
tx.Cancel();
```

A statement that fails after changing data fails the transaction: later queries and `Commit`
throw `TvErrorCode.Transaction`, and only a rollback ends it. Running transactions are listed by
`SHOW TRANSACTIONS` (protocol `embedded`), and `TERMINATE TRANSACTIONS` ends them.

### QueryResult

```csharp
using var result = tx.Query("RETURN 1 AS x, 'hello' AS y");

// Metadata
int cols = result.ColumnCount;       // 2
long rows = result.RowCount;         // 1
string name = result.Columns[0].Name; // "x"

// Iterate rows (forward-only cursor)
foreach (var row in result)
{
    long x = row.GetInt64("x");
    string y = row.GetString("y");
}

// Or snapshot all rows into a list (safe to use after enumeration)
List<Row> allRows = result.ToList();
```

### Row

```csharp
// Typed accessors - by column index or name
string? s = row.GetString(0);
long i    = row.GetInt64("count");
double d  = row.GetDouble(2);
bool b    = row.GetBool("flag");
bool nil  = row.IsNull(0);
string t  = row.GetTypeName(0);  // "INTEGER", "STRING", "NODE", etc.
object? v = row.GetValue(0);     // Boxed typed value

// Snapshot: copy all values into managed memory
// (the live row is only valid during the current iteration)
Row snapshot = row.Snapshot();
```

### BulkWriter

A bulk load: a write transaction that skips the redo log.
`Commit` saves the database with the load in it, so the data is written once and is durable
when `Commit` returns. Readers don't see the load until then, and `Abort` (or disposing without
a commit) rolls it back. Like `BeginTransaction`, it holds the writer lock while open.

```csharp
using var bulk = db.BeginBulkWrite();

// Create nodes (property values are auto-inferred from strings)
var alice = bulk.CreateNode(["Person"], ["name", "age"], ["Alice", "30"]);
var bob = bulk.CreateNode(["Person"], ["name", "age"], ["Bob", "25"]);

// Create edges
bulk.CreateEdge(alice, bob, "KNOWS", ["since"], ["2020"]);

// Create indexes
bulk.CreateIndex("idx_person_name", "Person", "name");
bulk.CreateEdgeIndex("idx_knows_since", "KNOWS", "since");

bulk.Commit(); // or Abort(); rolls back on Dispose if not committed
```

### Memory Safety

Traverse holds the whole database in memory, so by default `Database.Open` rejects a file larger than 80% of physical RAM:

```csharp
// Adjust or disable the safety limit
Database.MaxFileSizeBytes = 16L * 1024 * 1024 * 1024; // 16 GB
Database.MaxFileSizeBytes = 0; // disable check
```

### GraphJsonSerializer - Sigma.js Integration

Serialize query results to [graphology](https://graphology.github.io/)-compatible JSON for use with [Sigma.js](https://www.sigmajs.org/):

```csharp
using (var tx = db.BeginTransaction())
{
    using var result = tx.Query("MATCH (a)-[e]->(b) RETURN a, e, b");
    string json = GraphJsonSerializer.Serialize(result);
    // Returns: {"nodes":[...],"edges":[...]}
    tx.Abort();
}
```

Output format:

```json
{
  "nodes": [
    {
      "key": "n0",
      "attributes": {
        "label": "Person",
        "name": "Alice",
        "labels": ["Person"]
      }
    }
  ],
  "edges": [
    {
      "key": "e0",
      "source": "n0",
      "target": "n1",
      "attributes": {
        "label": "KNOWS"
      }
    }
  ]
}
```

Load this directly into a Sigma.js graph:

```javascript
import Graph from "graphology";
import Sigma from "sigma";

const graph = new Graph();
const data = await fetch("/api/graph").then(r => r.json());
graph.import(data);
new Sigma(graph, document.getElementById("container"));
```

## Error Handling

All native errors are surfaced as `TraverseException` with a typed `ErrorCode`:

```csharp
try
{
    tx.Query("INVALID CYPHER");
}
catch (TraverseException ex) when (ex.ErrorCode == TvErrorCode.Syntax)
{
    Console.WriteLine($"Syntax error: {ex.Message}");
}
```

## Native Library

The NuGet package bundles the native Traverse library under `runtimes/<rid>/native/`:

| Runtime ID | Library |
|---|---|
| `win-x64` | `traverse_ffi.dll` |
| `linux-x64` | `libtraverse_ffi.so` |

On other platforms (macOS, or Linux and Windows on ARM64) build it from source with
`cargo build --release -p traverse-ffi` and set `TRAVERSE_FFI_LIB` to the library file
(`libtraverse_ffi.dylib` on macOS). The variable
also overrides the bundled library, for example with a build of your own.

## Documentation

Documentation: [truespar.com/traverse/docs](https://truespar.com/traverse/docs)

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your option.
