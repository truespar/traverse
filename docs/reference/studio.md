# Traverse Studio Reference

Traverse Studio is the built-in web UI for managing and querying Traverse databases. It is embedded in the `traverse-server` binary and served on the HTTP port.

## Accessing Studio

The HTTP API and Studio are enabled by default on port 7691:

```bash
traverse-server --data ./databases/
```

Open [http://localhost:7691](http://localhost:7691) in your browser. If the server has an API key configured, Studio will prompt you to enter it (shown in the server startup output).

## Layout

Studio uses a three-panel layout:

- **Left sidebar** - Activity bar with panel navigation (resizable, 180-500px)
- **Center** - Query editor (top) and results (bottom), with a draggable divider
- **Right sidebar** - AI Graph Assistant (optional, resizable, 280-600px)

Panel sizes are persisted in the browser's localStorage.

## Query Editor

The query editor uses Monaco Editor with Cypher syntax highlighting and schema-aware autocomplete.

### Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl+Enter` | Execute query |
| `Ctrl+Shift+Enter` | Explain query (prepends `EXPLAIN`) |
| `Ctrl+S` | Save query |
| `Ctrl+Shift+S` | Share query |
| `Ctrl+P` | Open saved queries palette |

### Features

- **Autocomplete** - Suggests Cypher keywords, node labels, relationship types, and property keys from the current database schema
- **Multi-line input** - Statements are terminated by `;`
- **Query history** - Last 100 queries, persisted across sessions
- **Draft saving** - Current query text is auto-saved per database

## Result Views

Query results are displayed in three switchable views:

### Graph View

Graph rendering with Sigma.js (WebGL).

- **Layout** - Force-directed (ForceAtlas2) with overlap prevention
- **Interaction** - Click nodes/edges to inspect properties, drag to reposition, scroll to zoom
- **Styling** - Node colors and sizes are configurable per label (via Schema Browser)
- **Limits** - Renders up to 3,500 nodes and 8,500 edges by default (configurable in Settings)

### Table View

The results as a table.

- Node and edge properties are flattened into columns
- Special formatting for nulls, booleans, and numbers
- Copy the table, or export it as CSV

### JSON View

Raw query result as formatted JSON.

### Result Metadata

Displayed below results:

- Row count (with truncation indicator for large results)
- Execution statistics (nodes/edges created/deleted, properties set)
- Timing breakdown (roundtrip, parsing, planning, execution)

## Activity Bar

The left sidebar provides access to all panels:

| Panel | Description |
|-------|-------------|
| **Dashboard** | Benchmark comparison charts and quick links |
| **Schema** | Browse labels, relationships, properties, indexes, constraints |
| **Files** | Manage `.tvdb` files on the server |
| **Users** | User management |
| **Import** | Import data from Cypher, CSV nodes, or CSV edges |
| **Examples** | Load pre-built example graphs |
| **Cypher Reference** | Built-in Cypher language reference with runnable examples |
| **Docs** | Link to full documentation |
| **Settings** | Editor, graph rendering, and theme preferences |
| **Graph Assistant** | Writes and explains queries, answered by the model the server is configured with (`ai_provider`); hidden when none is set |

## Schema Browser

Displays the structure of the current database:

- **Labels** - Node labels with their properties and types
- **Relationships** - Relationship types with their properties and types
- **Property Keys** - All property keys in the database
- **Indexes** - Name, label, property, and entry count
- **Constraints** - Uniqueness and existence constraints

Additional actions:

- **Color picker** - Set node/edge colors per label/type (persisted as visualization styles)
- **Rename database** - Change the current database name
- **Drop database** - Delete the current database (requires confirmation and `allow_drop_database`)

## Database Management

The header bar displays the current database and provides a dropdown to:

- **Switch databases** - Select from all loaded databases
- **Create database** - Create a new empty database inline
- **View stats** - Node count, edge count, memory usage per database
- **Aliases** - View and manage database aliases (shorthand names)

Badges indicate database status: `default`, `read-only`, `alias`.

## Data Import

Three import modes are available from the Import panel:

### Cypher Import

Paste or upload Cypher `CREATE` statements. Statements are executed in batches with progress reporting.

### CSV Node Import

Upload a CSV file to create nodes:

1. Select the CSV file and delimiter
2. Map columns to properties
3. Assign a node label
4. Preview data before importing

### CSV Edge Import

Upload a CSV file to create relationships between existing nodes:

1. Select source and target node labels
2. Map CSV columns to source/target lookup properties
3. Specify the relationship type

**Limits:** 50 MB warning, 512 MB hard limit per import.

After import, Studio automatically refreshes the schema and saves to disk.

## Example Graphs

Pre-built datasets organized by category (Entertainment, Business, Social, Science, Technology). Click to load into a new database. Studio warns before overwriting existing data.

## File Manager

Browse and manage `.tvdb` files on the server:

- List all files in the data directory with size and loaded status
- Upload new `.tvdb` files
- Download existing files
- Load/unload databases from files

## User Management

Manage database users:

- List all users with roles
- Create new users with username, password, and role
- Edit user passwords and roles
- Delete users

Roles: `Admin`, `Editor`, `Reader`.

## Graph Assistant (AI Chat)

A chat in the right sidebar, answered by the model the server is configured with (`ai_provider`; hidden when none is set):

- Sees the current database's schema
- Writes Cypher or GQL queries from a question
- Click "Use" on suggested queries to populate the editor
- Streaming responses with markdown formatting
- Conversation history within the session

## Settings

| Setting | Default | Description |
|---------|---------|-------------|
| Max nodes to render | 3,500 | Graph view node limit |
| Max edges to render | 8,500 | Graph view edge limit |
| Editor font size | 14px | Monaco editor font size (8-32px) |
| Theme | System | Light or dark mode |

All settings are persisted in localStorage.

## Saved Queries

Save frequently used queries with a custom name:

- **Save** (`Ctrl+S`) - Save current query with a name (auto-named with timestamp if blank)
- **Open** (`Ctrl+P`) - Open the query palette to browse and load saved queries
- **Share** (`Ctrl+Shift+S`) - Generate a link that opens Studio with the query in the editor; the query travels in the link itself
- **Delete** - Remove a saved query from the list

Saved queries are stored per-database on the server.

## Themes

Studio supports light and dark themes. Toggle via the moon/sun icon in the header. The theme affects the editor, graph visualization, and all UI panels.

## Connection Status

The header shows a connection indicator:

- **Green** - Connected to the server
- **Red** - Disconnected (server unreachable or auth failure)

Studio also displays:

- Server version
- Total server memory usage with per-database breakdown tooltip

## API Reference

Studio connects to the server via the HTTP API. The interactive API reference (Scalar) is at `/api/docs`. The OpenAPI 3.1 specification is served at `/api/openapi.json`.
