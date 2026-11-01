# Traverse Admin Reference

`traverse-admin` builds `.tvdb` database files offline, from CSV files or a Cypher script, with no server running. The files load into `traverse-server` (at startup, or uploaded and loaded while it runs), open in `traverse-cli --file` and in the embedded bindings.

## Subcommands

| Subcommand | Description |
|------------|-------------|
| `import` | Bulk import from CSV files |
| `cypher-import` | Import from a Cypher script file |

## Import

Creates a new `.tvdb` database from node and edge CSV files.

```bash
traverse-admin import \
  --nodes Person=persons.csv \
  --nodes Company=companies.csv \
  --edges WORKS_AT=employment.csv \
  --edges KNOWS=friendships.csv \
  --output mydb.tvdb
```

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--nodes <LABEL=FILE>` | Node CSV file (repeatable) | *(required)* |
| `--edges <TYPE=FILE>` | Edge CSV file (repeatable) | *(none)* |
| `-o`, `--output <FILE>` | Output `.tvdb` file path | *(required)* |
| `--id-type <TYPE>` | ID type hint | `string` |
| `--delimiter <CHAR>` | Field delimiter | `,` |
| `--skip-bad-rows` | Skip rows that fail to parse instead of aborting | `false` |

### CSV Format

**Node files** must have a header row. The special `:ID` column provides the unique node identifier used for edge resolution. All other columns become node properties. Columns starting with `:` (Neo4j conventions) are skipped.

```csv
:ID,name,age,active
alice,Alice,30,true
bob,Bob,25,false
```

**Edge files** must have `:START_ID` and `:END_ID` columns referencing node `:ID` values from the node import phase. All other non-`:` columns become edge properties.

```csv
:START_ID,:END_ID,since,weight
alice,bob,2020,0.8
bob,alice,2021,0.6
```

### Value Inference

Property values are automatically inferred from CSV text:

| Input | Inferred Type |
|-------|---------------|
| `true`, `TRUE`, `True` | Boolean (`true`) |
| `false`, `FALSE`, `False` | Boolean (`false`) |
| `42`, `-7` | Integer |
| `3.14`, `-0.5` | Float |
| *(empty)* | Null (property omitted) |
| *(anything else)* | String |

### Example

```bash
# Import a social network
traverse-admin import \
  --nodes User=users.csv \
  --edges FOLLOWS=follows.csv \
  --output social.tvdb \
  --skip-bad-rows

# Then start the server with the imported data
traverse-server --data social.tvdb
```

## Cypher Import

Executes a Cypher script file (one statement per line) to build a new `.tvdb` database.

```bash
traverse-admin cypher-import \
  --input dataset.cypher \
  --output mydb.tvdb
```

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `-i`, `--input <FILE>` | Input `.cypher` file path | *(required)* |
| `-o`, `--output <FILE>` | Output `.tvdb` file path | *(required)* |
| `--skip-errors` | Continue on statement errors | `false` |
| `--index <LABEL:PROP>` | Pre-create index before import (repeatable) | *(none)* |

### Auto-Indexing

When no `--index` flags are provided, the importer automatically detects the transition from `CREATE` statements to `MATCH` statements. At that point, it scans all existing data and creates indexes on every (label, property) pair found. This avoids O(N) full scans for `MATCH` lookups during the rest of the import.

If `--index` flags are provided, auto-indexing is skipped and only the specified indexes are created.

```bash
# Manual indexes
traverse-admin cypher-import \
  --input data.cypher \
  --output db.tvdb \
  --index Person:id \
  --index Company:name

# Auto-indexing (recommended for most imports)
traverse-admin cypher-import \
  --input data.cypher \
  --output db.tvdb
```

### Input Format

- One Cypher statement per line
- Lines starting with `//` are treated as comments and skipped
- Empty lines are skipped
- Trailing semicolons are stripped before execution

### Error Handling

Without `--skip-errors`, the first statement error aborts the import. With `--skip-errors`, errors are counted and the first 10 are printed; the import continues to completion and reports the total error count.

### Example

```bash
# Import a Cypher dump, skipping any errors
traverse-admin cypher-import \
  --input neo4j_export.cypher \
  --output imported.tvdb \
  --skip-errors

# Load the result
traverse-server --data imported.tvdb
```
