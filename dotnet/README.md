# Traverse for .NET

Four NuGet packages in one solution (`Traverse.NET.slnx`), all on .NET 10:

| Package | What it is |
|---|---|
| [`Traverse.Embedded`](src/Traverse.Embedded/README.md) | The database in your process; bundles the native engine |
| [`Traverse.Bolt`](src/Traverse.Bolt/README.md) | Bolt driver for a Traverse server (or any other Bolt server) |
| [`Traverse.Grpc`](src/Traverse.Grpc/README.md) | gRPC client for a Traverse server |
| [`Traverse.Http`](src/Traverse.Http/README.md) | Client for the server's HTTP management API |

`src/Traverse.Bench` is the benchmark driver; it isn't packaged.

## Build and test

```bash
dotnet build Traverse.NET.slnx

# the embedded tests load the native library from TRAVERSE_FFI_LIB
cargo build -p traverse-ffi
TRAVERSE_FFI_LIB=<cargo target dir>/debug/traverse_ffi.dll dotnet test --solution Traverse.NET.slnx
```

`../run_binding_tests.sh dotnet` builds the library and runs the embedded tests with it.

The tests are xUnit.net v3 apps on Microsoft.Testing.Platform, which `global.json` selects for
`dotnet test`. The Bolt, gRPC and HTTP suites skip unless a server is listening: Bolt on
`localhost:7690` (TLS on `TRAVERSE_BOLT_TLS_URI`, default `bolt+ssc://localhost:7692`), gRPC on
`localhost:50051`, HTTP on `TRAVERSE_BASE_URL` (default `http://localhost:7691`, with
`TRAVERSE_API_KEY` when the server wants one).

Package versions live in `Directory.Packages.props`; the version and metadata the packages share
are in `Directory.Build.props`.

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your option.
