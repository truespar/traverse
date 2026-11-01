# Traverse.Bolt

A Bolt driver for .NET, for [Traverse](https://truespar.com/traverse/docs) and other Bolt servers.

## Installation

```bash
dotnet add package Traverse.Bolt
```

## Features

- **No dependencies** - .NET only, no third-party packages
- **Async** - all I/O is `async`/`await` with `ValueTask`
- **Connection pooling** - a pool with lifetime rotation, idle eviction, and liveness probing
- **Streaming** - `IAsyncEnumerable<BoltRecord>` cursor for large result sets
- **TLS** - optional encrypted connections with configurable certificate validation
- **Transactions** - explicit `BEGIN`/`COMMIT`/`ROLLBACK` with auto-rollback on dispose

## Bolt Protocol Support

| Version | Range | Notes |
|---------|-------|-------|
| Bolt 6+ | Manifest v1 | Forward-compatible manifest negotiation for Bolt 6+ servers |
| Bolt 6.0 | 6.0 | Classic handshake for Bolt 6.0 servers without manifest support |
| Bolt 5.x | 5.0-5.8 | Full PackStream v2 support including temporal types, spatial points, and paths |
| Bolt 4.x | 4.2-4.4 | Backwards compatibility with older servers |

## Quick Start

```csharp
await using var client = await BoltClient.ConnectAsync("localhost", 7690);

var result = await client.RunAsync(
    "MATCH (n:Person) WHERE n.age > $age RETURN n.name, n.age LIMIT 10",
    new() { ["age"] = 30L });

foreach (var record in result.Records)
    Console.WriteLine($"{record.GetString(0)}, age {record.GetInt64(1)}");
```

### Connect with authentication and TLS

```csharp
await using var client = await BoltClient.ConnectAsync("localhost", 7690,
    new BoltClientOptions
    {
        Username = "admin",
        Password = "password",
        Database = "mydb",
        UseTls = true
    });
```

### URI-based connection

```csharp
// bolt:// for plain, bolt+s:// for TLS, bolt+ssc:// for TLS with self-signed certs
await using var client = await BoltClient.ConnectAsync("bolt+s://localhost:7690",
    new BoltClientOptions { Username = "admin", Password = "password" });
```

## Connection Pooling

```csharp
var pool = new BoltPool("localhost", 7690,
    new BoltClientOptions { Username = "admin", Password = "password" },
    new BoltPoolOptions
    {
        MaxSize = 50,
        MaxConnectionLifetime = TimeSpan.FromHours(1),
        ConnectionIdleTimeout = TimeSpan.FromMinutes(5)
    });

await using var conn = await pool.AcquireAsync();
var result = await conn.RunAsync("MATCH (n) RETURN count(n) AS cnt");
Console.WriteLine(result.Records[0].GetInt64(0));
// Connection is returned to the pool on dispose
```

## Transactions

```csharp
await using var client = await BoltClient.ConnectAsync("localhost", 7690);

await using var tx = await client.BeginTransactionAsync();
await tx.RunAsync("CREATE (n:Person {name: $name})", new() { ["name"] = "Alice" });
await tx.RunAsync("CREATE (n:Person {name: $name})", new() { ["name"] = "Bob" });
await tx.CommitAsync();
// If not committed, dispose auto-rolls back
```

## Streaming

For large result sets, use `RunStreamAsync` to avoid materializing all records in memory:

```csharp
await using var client = await BoltClient.ConnectAsync("localhost", 7690,
    new BoltClientOptions { FetchSize = 1000 });

await using var cursor = await client.RunStreamAsync("MATCH (n:Person) RETURN n.name");

await foreach (var record in cursor)
    Console.WriteLine(record.GetString(0));
```

## Authentication

```csharp
// Basic (username/password) - used by default when options has Username/Password
var auth = BoltAuthToken.Basic("admin", "password");

// Bearer token
var auth = BoltAuthToken.Bearer("eyJhbGci...");

// Kerberos
var auth = BoltAuthToken.Kerberos("YIIJ...");

// No authentication
var auth = BoltAuthToken.None();

// Re-authenticate on a live connection (Bolt 5.1+)
await client.LogoffAsync();
await client.LogonAsync("newuser", "newpass");
```

## Query Options

```csharp
var result = await client.RunAsync(
    "MATCH (n) RETURN n",
    options: new QueryOptions
    {
        TxTimeout = TimeSpan.FromSeconds(10),
        Mode = AccessMode.Read,
        ImpersonatedUser = "alice",
        TxMetadata = new() { ["app"] = "etl-pipeline" }
    });
```

## Retry with Exponential Backoff

`BoltRetry` acquires a fresh connection from the pool on each attempt, with jitter and exponential backoff for transient errors:

```csharp
var pool = BoltPool.Create("localhost", 7690,
    new BoltClientOptions { Username = "admin", Password = "password" });

var result = await BoltRetry.ExecuteAsync(pool, async client =>
{
    return await client.RunAsync("MATCH (n) RETURN count(n) AS cnt");
});

// Custom retry options
await BoltRetry.ExecuteAsync(pool, async client =>
{
    await client.RunAsync("CREATE (:Event {ts: $ts})", new() { ["ts"] = DateTimeOffset.UtcNow });
}, new RetryOptions { InitialDelayMs = 500, MaxRetryTimeMs = 10_000 });
```

## Configuration

### BoltClientOptions

| Property | Default | Description |
|----------|---------|-------------|
| `Username` | `null` | Authentication username |
| `Password` | `null` | Authentication password |
| `Database` | `null` | Target database name |
| `UseTls` | `false` | Enable TLS encryption |
| `AllowUntrustedCertificate` | `false` | Accept self-signed certificates |
| `ConnectionTimeout` | 30s | TCP connection timeout |
| `FetchSize` | -1 (all) | Batch size for streaming PULL |
| `TelemetryDisabled` | `false` | Disable telemetry messages |

### BoltPoolOptions

| Property | Default | Description |
|----------|---------|-------------|
| `MaxSize` | 100 | Maximum connections in the pool |
| `ConnectionAcquisitionTimeout` | 1 min | Max wait time for a connection |
| `MaxConnectionLifetime` | 1 hour | Retire connections after this age |
| `ConnectionIdleTimeout` | `null` | Evict idle connections after this duration |
| `ConnectionLivenessThreshold` | `null` | Probe idle connections before reuse |

## Compatibility

Traverse.Bolt works with servers that implement the Bolt protocol, including Traverse, Neo4j 4.x
and 5.x, and Memgraph.

## Documentation

Documentation: [truespar.com/traverse/docs](https://truespar.com/traverse/docs)

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your option.
