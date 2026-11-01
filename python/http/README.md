# traverse-http

Python client for the HTTP API of a [Traverse](https://github.com/truespar/traverse) server:
queries in openCypher or GQL, database and file management, aliases, schema, CSV and Cypher
import, and users. Python 3.10+, one dependency (`requests`).

To run Traverse inside your own process instead, use
[traverse-embedded](https://pypi.org/project/traverse-embedded/).

## Installation

```bash
pip install traverse-http
```

## Quick start

The server prints its API key at startup (`API key tvs_...`), or takes one from
`TRAVERSE_API_KEY`.

```python
from traverse_http import TraverseHttpClient

client = TraverseHttpClient("http://localhost:7691", api_key="tvs_your_key")

client.query("CREATE (:Person {name: 'Alice', age: 34}), (:Person {name: 'Bob', age: 28})")

result = client.query(
    "MATCH (p:Person) WHERE p.age > $age RETURN p.name AS name ORDER BY name",
    parameters={"age": 30},
)
print(result.columns, result.rows)  # ['name'] [['Alice']]

# ISO/IEC 39075 GQL instead of openCypher
result = client.query("MATCH (p:Person) FILTER p.age > 30 RETURN p.name AS name", dialect="gql")

for db in client.list_databases():
    print(db.name, db.aliases)

schema = client.get_schema()
print(schema.labels, schema.relationship_types)
```

Each call takes an optional `database=` to target a database other than the default one.

## Databases, files and aliases

```python
client.create_database("staging")
client.rename_database("staging", "registry")
client.drop_database("registry")  # unless the server runs with TRAVERSE_ALLOW_DROP_DATABASE=false

# upload a .tvdb file built offline (for example with traverse-embedded), load it and point an
# alias at it; queries against the alias move to the new data in one step
with open("registry_v2.tvdb", "rb") as f:
    client.upload_file("registry_v2", f.read())
client.load_database("registry_v2", "registry_v2.tvdb")
client.create_alias("registry", "registry_v2")  # later: client.alter_alias("registry", "registry_v3")
```

## CSV import

```python
from traverse_http import ColumnMapping, EdgeEndpoint

people = "id,name,age\n1,Alice,34\n2,Bob,28\n"
result = client.import_csv(
    people,
    "Person",
    [ColumnMapping("id", type="integer"), ColumnMapping("name"), ColumnMapping("age", type="integer")],
)
print(result.nodes_created)  # 2

knows = "from,to,since\n1,2,2019\n"
result = client.import_csv_edges(
    knows,
    "KNOWS",
    source=EdgeEndpoint(label="Person", column="from", property="id"),
    target=EdgeEndpoint(label="Person", column="to", property="id"),
    columns=[ColumnMapping("since", type="integer")],
)
print(result.edges_created)  # 1
```

Column types are `string` (the default), `integer`, `float` and `boolean`; `property=` stores a
column under another name.

## Errors

A failed request raises `TraverseHttpException`, with the HTTP status in `status_code`.

## License

Copyright 2026 The Intelligence Company AB (publ). Licensed under MIT or Apache-2.0, at your
option.
