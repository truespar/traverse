# Traverse for Java

Two Maven Central artifacts, both for Java 25 or later:

| Artifact | What it is |
|---|---|
| `com.truespar:traverse-java` | The database in your process, over JNI; the jar bundles the native library for Windows x64, Linux x64 and macOS on Apple silicon |
| `com.truespar:traverse-http` | Client for a Traverse server's HTTP management API (`java/http`) |

## Embedded

```xml
<dependency>
    <groupId>com.truespar</groupId>
    <artifactId>traverse-java</artifactId>
    <version>0.9.0</version>
</dependency>
```

```java
try (Database db = Database.open("graph.tvdb")) {
    // one statement as a transaction of its own
    db.execute("CREATE (:Person {name: $name})", Map.of("name", "Ada"));

    // the writer: one at a time, waits up to getBusyTimeout() for the active one
    try (Transaction tx = db.begin()) {
        tx.execute("MATCH (p:Person {name: 'Ada'}) SET p.born = 1815");
        tx.commit();
    }

    // a snapshot reader: never waits, runs alongside the writer
    try (Transaction tx = db.beginRead()) {
        for (Map<String, Object> row : tx.execute("MATCH (p:Person) RETURN p.name AS name")) {
            System.out.println(row.get("name"));
        }
    }
}
```

`execute(query, params, "gql")` runs ISO/IEC 39075 GQL instead of openCypher. A statement stopped
by `Transaction.cancel()` or `Database.setStatementTimeout` throws `QueryInterruptedException`;
every error is a `TraverseException` with its GQLSTATUS in `getGqlStatus()`. `Database.bulkWriter()`
loads nodes and edges in bulk.

Run with `--enable-native-access=ALL-UNNAMED` (or the module that uses Traverse) so the JVM loads
the library without a warning. The jar's library is extracted once per version into a per-user
cache. On other platforms (Intel macOS, Linux or Windows on ARM64) build it with `cargo build --release -p traverse-java`
and set `TRAVERSE_JAVA_LIB` (or the `traverse.library` system property) to the file; that also
overrides the bundled library.

## Build and test

```bash
cargo build -p traverse-java
TRAVERSE_JAVA_LIB=<cargo target dir>/debug/traverse_java.dll mvn test   # libtraverse_java.so on Linux
```

`../run_binding_tests.sh java` does both. The `java/http` tests need a running server on
`TRAVERSE_BASE_URL` (default `http://localhost:7691`, with `TRAVERSE_API_KEY` when it wants one).

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your option.
