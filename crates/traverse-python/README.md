# traverse-embedded

The [Traverse](https://github.com/truespar/traverse) graph database inside your Python process,
with no server. Open or create a `.tvdb` file, query it with openCypher or ISO/IEC 39075 GQL, and
load data with the bulk writer. Python 3.10+ on Windows x64, Linux x64 and macOS on Apple silicon.

The import name is `traverse`. To talk to a Traverse server instead, use
[traverse-http](https://pypi.org/project/traverse-http/).

## Installation

```bash
pip install traverse-embedded
```

## Quick start

```python
import traverse

with traverse.open("people.tvdb") as db:
    db.execute("CREATE (:Person {name: 'Alice', age: 34}), (:Person {name: 'Bob', age: 28})")

    result = db.execute("MATCH (p:Person) WHERE p.age > $age RETURN p.name AS name", {"age": 30})
    print(result.columns, result.data())  # ['name'] [{'name': 'Alice'}]

    # ISO/IEC 39075 GQL instead of openCypher
    for row in db.execute("MATCH (p:Person) FILTER p.age < 30 RETURN p.name AS name", dialect="gql"):
        print(row["name"])  # Bob
```

`execute` on the database runs each query as a transaction of its own: a read runs on a snapshot,
a write commits when it succeeds and rolls back when it fails. Closing the database (the end of
the `with` block, or `db.close()`) saves it, and leaves the `.tvdb` as a single file.

## Transactions

```python
with traverse.open("people.tvdb") as db:
    # commits on a clean exit, rolls back on an exception
    with db.begin() as tx:
        tx.execute("CREATE (:Person {name: 'Carol', age: 41})")
        tx.execute("MATCH (a:Person {name: 'Alice'}), (c:Person {name: 'Carol'}) CREATE (a)-[:KNOWS]->(c)")

    # a snapshot of the database as of now; runs alongside the writer and other readers
    with db.begin_read() as snapshot:
        print(snapshot.execute("MATCH (p:Person) RETURN count(p) AS n").data())  # [{'n': 3}]
```

One write transaction is active at a time. `begin` waits for the active one for up to
`db.busy_timeout` seconds (default 5), then raises `traverse.BusyError`.

## Bulk loading

A bulk load is a write transaction that skips the redo log: committing it saves the database with
the load in it, so the data is written once. Readers don't see it until then.

```python
with traverse.open("registry.tvdb") as db:
    with db.bulk_writer() as bulk:
        acme = bulk.create_node(["Company"], {"name": "Acme", "founded": 1998})
        globex = bulk.create_node(["Company"], {"name": "Globex", "founded": 2004})
        bulk.create_edge(acme, globex, "OWNS", {"share": 0.6})
        bulk.create_index("company_name", "Company", "name")
```

A file built this way can be uploaded to a Traverse server and put behind an alias, so queries
move to the new data in one step (see the
[Traverse README](https://github.com/truespar/traverse#readme)).

## Timeouts, cancelling and errors

```python
with traverse.open("people.tvdb") as db:
    db.statement_timeout = 30  # seconds a statement may run (None: no limit)
    try:
        db.execute("MATCH (p:Person) RETURN nosuchfunction(p.name)")
    except traverse.TraverseError as e:
        print(e.gql_status)  # 42N48 (unknown function)
```

A statement that runs past `statement_timeout`, or that another thread stops with
`Transaction.cancel()`, raises `traverse.QueryInterruptedError`. Every error is a
`traverse.TraverseError`, with its GQLSTATUS code in `gql_status` (the ISO/IEC 39075 code where
the standard has one).

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your
option.
