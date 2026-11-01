# Traverse.Http

HTTP client for the [Traverse](https://truespar.com/traverse/docs) graph database management API. Manage databases, files, aliases, schema, and users over HTTP/HTTPS.

## Installation

```bash
dotnet add package Traverse.Http
```

## Features

- **No dependencies** - .NET only, no third-party packages
- **The whole HTTP API** - databases, files, aliases, schema, users, import, query
- **Async** - all methods are `async`/`await` with `CancellationToken` support
- **File transfers** - upload/download `.tvdb` files with progress reporting
- **TLS** - optional support for self-signed certificates

## Quick Start

```csharp
using Traverse.Http;

var client = new TraverseHttpClient("http://localhost:7691",
    new TraverseHttpOptions { ApiKey = "tvs_..." });

// List databases
var databases = await client.ListDatabasesAsync();
foreach (var db in databases)
    Console.WriteLine($"{db.Name}: {db.NodeCount} nodes, {db.EdgeCount} edges");

// Run a query
var result = await client.QueryAsync("MATCH (n) RETURN count(n) AS cnt");
Console.WriteLine(result.Rows[0]["cnt"]);

// Create a database
await client.CreateDatabaseAsync("mydb");

// Import Cypher statements
await client.ImportCypherAsync("CREATE (:Person {name: 'Alice'})");
```

## Database Management

```csharp
await client.CreateDatabaseAsync("analytics");
await client.RenameDatabaseAsync("analytics", "analytics-v2");
await client.DropDatabaseAsync("analytics-v2");

// Persist all databases to disk
await client.SaveAsync();
```

## File Management

```csharp
// List .tvdb files on the server
var files = await client.ListFilesAsync();

// Upload a database file
await using var stream = File.OpenRead("backup.tvdb");
await client.UploadFileAsync("backup.tvdb", stream);

// Download a database file
await client.DownloadFileAsync("backup.tvdb", "local-copy.tvdb");

// Delete a file from the server
await client.DeleteFileAsync("old-backup.tvdb");
```

## CSV Import

Import nodes and edges from CSV data:

```csharp
// Import nodes
var csv = "name,age\nAlice,30\nBob,25";
var result = await client.ImportCsvAsync(csv, "Person",
[
    new() { Name = "name" },
    new() { Name = "age", Type = "integer" }
]);
Console.WriteLine($"{result.NodesCreated} nodes created");

// Import edges
var edgeCsv = "from,to,since\nAlice,Bob,2020";
var edgeResult = await client.ImportCsvEdgesAsync(edgeCsv, "KNOWS",
    source: new() { Label = "Person", Column = "from", Property = "name" },
    target: new() { Label = "Person", Column = "to", Property = "name" },
    columns: [new() { Name = "since", Type = "integer" }]);
Console.WriteLine($"{edgeResult.EdgesCreated} edges created");
```

## Schema and Aliases

```csharp
// Get database schema
var schema = await client.GetSchemaAsync();

// Manage aliases
var aliases = await client.ListAliasesAsync();
await client.CreateAliasAsync("prod", "main-database");
await client.AlterAliasAsync("prod", "new-database");
await client.DropAliasAsync("prod");
```

## User Management

```csharp
var users = await client.ListUsersAsync();
await client.CreateUserAsync("alice", "secret", role: "admin");
await client.UpdateUserAsync("alice", role: "reader");
await client.DeleteUserAsync("alice");
```

## Health Checks

```csharp
var health = await client.HealthAsync();
Console.WriteLine($"Status: {health.Status}");

bool ready = await client.ReadyAsync();
```

## Replacing a Database

To replace a database while clients keep working, load the new file next to the old one and move
the alias the clients use:

```csharp
await using var stream = File.OpenRead("analytics-v2.tvdb");
await client.UploadFileAsync("analytics-v2.tvdb", stream);
await client.LoadDatabaseAsync("analytics-v2", "analytics-v2.tvdb");
await client.AlterAliasAsync("prod", "analytics-v2");
await client.UnloadDatabaseAsync("analytics-v1");
```

Replacing a database under the same name works without an alias, but between the unload and the
load the database doesn't exist and queries to it fail:

```csharp
var dbName = "analytics";
var newFile = "analytics-v2.tvdb";

// 1. Upload the new .tvdb file to the server's data directory
await using var stream = File.OpenRead(newFile);
await client.UploadFileAsync(newFile, stream);

// 2. Unload the old database (persists unsaved changes, frees memory)
await client.UnloadDatabaseAsync(dbName);

// 3. Load the new file under the same database name
await client.LoadDatabaseAsync(dbName, newFile);
```

Both `UnloadDatabaseAsync` and `LoadDatabaseAsync` run as async server tasks with optional progress reporting:

```csharp
var progress = new Progress<TaskInfo>(t =>
    Console.WriteLine($"{t.Status}: {t.ProgressPercent}%"));

await client.UnloadDatabaseAsync("analytics", progress);
await client.LoadDatabaseAsync("analytics", "analytics-v2.tvdb", progress);
```


## Configuration

| Property | Default | Description |
|----------|---------|-------------|
| `ApiKey` | `null` | API key for authentication |
| `AllowUntrustedCertificate` | `false` | Accept self-signed TLS certificates |
| `Timeout` | 100s | HTTP request timeout |
| `FileTransferTimeout` | infinite | Timeout for file upload/download |

## Documentation

Documentation: [truespar.com/traverse/docs](https://truespar.com/traverse/docs)

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your option.
