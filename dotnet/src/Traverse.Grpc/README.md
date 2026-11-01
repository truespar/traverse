# Traverse.Grpc

gRPC client for the [Traverse](https://truespar.com/traverse/docs) graph database. Provides typed query results, transactions, batch operations, and schema inspection over gRPC.

## Features

- **Typed results** - nodes, edges, paths, and scalar values as strongly-typed objects
- **Transactions** - explicit `BEGIN`/`COMMIT`/`ROLLBACK` with auto-rollback on dispose
- **Batch operations** - create, update and delete in batches
- **Schema inspection** - list labels, relationship types, property keys, indexes, constraints
- **Connection pooling** - gRPC channel management with configurable options

## Quick Start

```csharp
using Traverse.Grpc;

var client = new TraverseGrpcClient("http://localhost:50051",
    new TraverseClientOptions
    {
        Credentials = new TraverseCredentials("admin", "password"),
        Database = "mydb"
    });

var result = await client.QueryAsync("MATCH (n:Person) RETURN n.name, n.age LIMIT 10");

foreach (var row in result)
    Console.WriteLine($"{row.GetString("n.name")}, age {row.GetInt64("n.age")}");
```

## Transactions

```csharp
await using var tx = await client.BeginTransactionAsync();
await tx.RunAsync("CREATE (:Person {name: $name})", new() { ["name"] = "Alice" });
await tx.RunAsync("CREATE (:Person {name: $name})", new() { ["name"] = "Bob" });
await tx.CommitAsync();
```

## Dependencies

- Google.Protobuf
- Grpc.Net.Client

## Documentation

Documentation: [truespar.com/traverse/docs](https://truespar.com/traverse/docs)

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your option.
