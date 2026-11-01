# Traverse CLI Reference

`traverse-cli` is an interactive Cypher shell for querying Traverse databases. It supports connecting to a running server via the Bolt protocol or opening a database file directly in embedded mode.

## Quick Start

```bash
# Connect to a local server
traverse-cli

# Connect to a specific server
traverse-cli --bolt bolt://192.168.1.10:7690

# Open a database file directly (no server needed)
traverse-cli --file mydb.tvdb

# Run a query and exit
traverse-cli "MATCH (n) RETURN count(n)"

# Run a script file
traverse-cli --script import.cypher
```

## Command-Line Options

| Flag | Description | Default |
|------|-------------|---------|
| `-b`, `--bolt <URI>` | Bolt server URI (`bolt://host:port` or `host:port`) | `bolt://localhost:7690` |
| `-d`, `--database <NAME>` | Select a database (Bolt mode only) | *(none)* |
| `-f`, `--file <PATH>` | Open `.tvdb` file directly (embedded mode) | *(none)* |
| `-s`, `--script <PATH>` | Run a `.cypher` script file and exit | *(none)* |
| `-u`, `--user <USER>` | Username for authentication (Bolt mode) | *(none)* |
| `-p`, `--password <PASS>` | Password for authentication (Bolt mode) | *(none)* |
| `--format <FMT>` | Output format: `table`, `json`, `csv` | `table` |
| `--no-highlight` | Disable syntax highlighting in REPL | `false` |
| `--tls-insecure` | Skip TLS certificate verification (for self-signed certs) | `false` |
| `[QUERY]` | Query to execute (non-interactive mode) | *(none)* |

## Connection Modes

### Bolt Mode (default)

Connects to a Traverse server, or another Bolt server, over the network, with authentication and transaction control.

```bash
traverse-cli --bolt bolt://localhost:7690 --user admin --password secret
```

### Embedded Mode

Opens a `.tvdb` database file directly without a running server. Useful for offline inspection, scripted imports, and quick queries.

```bash
traverse-cli --file mydb.tvdb
```

Note: Embedded mode does not support transaction control (`BEGIN`/`COMMIT`/`ROLLBACK`).

## Input Modes

- **Interactive REPL**: Default when running in a terminal. Supports line editing, history, tab completion, and syntax highlighting.
- **Script file** (`--script`): Reads and executes all statements from a file sequentially. Stops on the first error.
- **Positional query**: Execute a single query (or multiple semicolon-separated statements) and exit.
- **Piped stdin**: Reads entire stdin as a query when input is not a TTY.

## REPL Commands

Commands are prefixed with `:` and are only available in the interactive REPL.

| Command | Description |
|---------|-------------|
| `:help`, `:h` | Show available commands and usage examples |
| `:quit`, `:exit`, `:q` | Exit the shell |
| `:format <fmt>` | Set output format (`table`, `json`, `csv`). Shows current format if no argument given. |
| `:param <name> => <value>` | Set a query parameter |
| `:params` | Show all set parameters |
| `:params clear` | Clear all parameters |
| `:source <file>` | Execute a `.cypher` script file |
| `:use <database>` | Switch to a different database (Bolt mode). Shows current database if no argument given. |
| `:clear` | Clear the terminal screen |

### Query Parameters

Set parameters with `:param` and use them in queries with `$name`:

```
traverse> :param userId => 42
traverse> MATCH (u:User) WHERE u.id = $userId RETURN u;
```

Supported value types:

| Syntax | Type |
|--------|------|
| `42` | Integer |
| `3.14` | Float |
| `'hello'` or `"hello"` | String |
| `true` / `false` | Boolean |
| `null` | Null |

## Transaction Control

Available in Bolt mode only:

```cypher
BEGIN;
-- execute multiple statements within the transaction
MATCH (n:User) SET n.active = true;
CREATE (n:Log {action: 'bulk_update'});
COMMIT;
```

```cypher
BEGIN;
-- oops, undo everything
ROLLBACK;
```

## User Management

These commands are executed as regular Cypher statements:

```cypher
-- Create a user with a role
CREATE USER alice SET PASSWORD 'secret' SET ROLE Editor;

-- Change a password
ALTER USER alice SET PASSWORD 'newsecret';

-- Change a role
ALTER USER alice SET ROLE Admin;

-- Delete a user
DROP USER alice;

-- List all users
SHOW USERS;
```

Roles: `Admin`, `Editor`, `Reader`.

## Database Management

When running a multi-database Traverse server, use `--database` to select a database at startup or `:use` to switch at runtime:

```bash
# Connect to a specific database
traverse-cli --database mydb

# Or switch at runtime
traverse-cli
mydb> :use otherdb
Switched to database 'otherdb'.
otherdb>
```

```cypher
-- List all databases
SHOW DATABASES;

-- Create a new database
CREATE DATABASE analytics;

-- Drop a database
DROP DATABASE analytics;
```

The prompt shows the current database name.

## Data Import

```cypher
LOAD CSV WITH HEADERS FROM 'file:///path/to/data.csv' AS row
CREATE (n:Person {name: row.name, age: toInteger(row.age)});
```

## Output Formats

### Table (default)

```
+-------+-----+
| name  | age |
+-------+-----+
| Alice | 30  |
| Bob   | 25  |
+-------+-----+
2 rows
```

### JSON

```json
{"name": "Alice", "age": 30}
{"name": "Bob", "age": 25}
```

One JSON object per row (streaming format).

### CSV

```csv
name,age
Alice,30
Bob,25
```

RFC 4180 CSV with a header row.

### Switching Formats

```
traverse> :format json
Output format: json
traverse> MATCH (n:Person) RETURN n.name, n.age;
{"n.name": "Alice", "n.age": 30}
```

Or set at startup:

```bash
traverse-cli --format csv "MATCH (n) RETURN labels(n), count(*)"
```

## Features

- **Tab completion**: Completes Cypher keywords, node labels, relationship types, and property keys from the connected database.
- **Syntax highlighting**: Colorizes keywords, strings, numbers, and comments in the REPL.
- **Multi-line input**: Statements are buffered until a `;` terminator is entered.
- **Command history**: Stored in `~/.traverse_history` across sessions.
