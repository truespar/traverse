# traverse-gql

ISO/IEC 39075 GQL for Traverse: a parser for the GQL dialect that lowers
to the same statement representation as `traverse-cypher`, so both
dialects run on one executor. The two parsers never touch each other.

```text
                   +------------------+
       Cypher  --> | traverse-cypher  | -+
                   +------------------+  v
                                       +------------------+    +----------+
                                       | shared AST       | -> | executor |
                                       | (cypher::Stmt)   |    +----------+
                                       +------------------+  ^
                   +------------------+  |
       GQL    --> | traverse-gql      | -+
                   +------------------+
```

GQL is on by default in `traverse-server` (`X-Query-Dialect: gql` on the
HTTP API) and is available in the embedded bindings and the WebAssembly
build.

```rust
let result = traverse_gql::execute_gql(&mut graph, "MATCH (n:Person) RETURN n.name", &params)?;
```

## Status

[`CONFORMANCE.md`](./CONFORMANCE.md) tracks support row by row against
the mandatory and optional feature tables Neo4j publishes for GQL, with
the known limitations. [`DESIGN.md`](./DESIGN.md) describes the
architecture and [`TESTING.md`](./TESTING.md) the test layers.

## Layout

```
src/
+-- lib.rs       public API
+-- lexer.rs     keyword table + tokenizer
+-- parser/      recursive-descent productions, by area
+-- ast.rs       GQL AST nodes
+-- lower.rs     GQL AST -> traverse_cypher::Statement
+-- exec.rs      parse -> lower -> execute, shared by the server and the bindings
+-- error.rs     ParseError
```

## Tests

```sh
cargo test -p traverse-gql
```

The openGQL TCK is a git submodule at `tests/opengql_tck/`
(`git submodule update --init`); without it the TCK harnesses skip.

## License

MIT OR Apache-2.0, like the rest of Traverse. The `opengql/tck`
submodule carries its upstream Apache-2.0 license.
