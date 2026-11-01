# traverse-http

Official Python HTTP client for [Traverse](https://truespar.com/traverse/docs) - Truespar's in-memory graph database.

- Execute Cypher queries with parameterized values
- Database management (create, drop, rename, load, unload)
- Schema introspection (labels, types, properties, indexes, constraints)
- CSV and Cypher import with async task polling
- File management (upload, download, delete `.tvdb` files)
- User and alias management
- Typed response models with dataclass return types
- Python 3.10+, MIT licensed

## Installation

```bash
pip install traverse-http
```

## Quick Start

```python
from traverse_http import TraverseHttpClient

client = TraverseHttpClient("http://localhost:7691", api_key="tvs_your_key")

# Execute a query
result = client.query("MATCH (n:Person) RETURN n.name, n.age LIMIT 10")
for row in result.rows:
    print(row)

# Run ISO/IEC 39075 GQL instead of openCypher (requires a server
# built with the gql feature - check client.health().features)
result = client.query("MATCH (n:Person) FILTER n.age > 30 RETURN n.name", dialect="gql")

# List databases
databases = client.list_databases()
for db in databases:
    print(f"{db.name}: {db.node_count} nodes, {db.edge_count} edges")

# Get schema
schema = client.schema()
print(schema.labels)
```

## Database Management

```python
client.create_database("mydb")
client.rename_database("mydb", "production")
client.drop_database("production")  # requires --allow-drop-database on server
```

## CSV Import

```python
result = client.import_csv(
    file_path="people.csv",
    label="Person",
    database="mydb",
)
print(f"Imported {result.nodes_created} nodes")
```

## Documentation

- [Traverse documentation](https://truespar.com/traverse/docs)
- [Python SDK reference](https://truespar.com/traverse/docs/v1/python-sdk)

## License

MIT - see [LICENSE](LICENSE).
