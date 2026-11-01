package http

// HealthResponse is the response from GET /api/health.
type HealthResponse struct {
	Status            string           `json:"status"`
	Version           string           `json:"version"`
	UptimeSecs        int              `json:"uptime_secs"`
	MemoryBytes       int64            `json:"memory_bytes"`
	Databases         []HealthDatabase `json:"databases"`
	AllowDropDatabase bool             `json:"allow_drop_database"`
	AuthRequired      bool             `json:"auth_required"`
	// Features reports compile-time server capabilities, e.g.
	// {"gql": true, "ml": false}. Check Features["gql"] before
	// passing dialect "gql" to QueryDialect.
	Features map[string]bool `json:"features,omitempty"`
}

// HealthDatabase describes a single database entry in the health response.
type HealthDatabase struct {
	Name        string `json:"name"`
	Default     bool   `json:"default"`
	Nodes       int    `json:"nodes"`
	Edges       int    `json:"edges"`
	MemoryBytes int64  `json:"memory_bytes"`
}

// DatabaseInfo describes a database returned by GET /api/databases.
type DatabaseInfo struct {
	Name     string   `json:"name"`
	Default  bool     `json:"default"`
	ReadOnly bool     `json:"readOnly"`
	Aliases  []string `json:"aliases"`
}

// TvdbFileInfo describes a .tvdb database or .tvmodel trained model
// returned by GET /api/files.
type TvdbFileInfo struct {
	Name   string `json:"name"`
	File   string `json:"file"`
	Bytes  int64  `json:"bytes"`
	Loaded bool   `json:"loaded"`
	// Kind is "database" for .tvdb files, "model" for .tvmodel files.
	Kind string `json:"kind,omitempty"`
}

// UploadResult is the response from PUT /api/files/{name}.
type UploadResult struct {
	OK    bool   `json:"ok"`
	Name  string `json:"name"`
	File  string `json:"file"`
	Bytes int64  `json:"bytes"`
}

// AliasInfo describes an alias returned by GET /api/aliases.
type AliasInfo struct {
	Alias    string `json:"alias"`
	Database string `json:"database"`
}

// SchemaProperty describes a property in the schema.
type SchemaProperty struct {
	Name string `json:"name"`
	Type string `json:"type"`
}

// IndexInfo describes an index in the schema.
type IndexInfo struct {
	Name     string `json:"name"`
	Label    string `json:"label"`
	Property string `json:"property"`
	Entries  int    `json:"entries"`
}

// ConstraintInfo describes a constraint in the schema.
type ConstraintInfo struct {
	Name     string `json:"name"`
	Type     string `json:"type"`
	Label    string `json:"label"`
	Property string `json:"property"`
}

// LabelDetail describes a label or relationship type with its properties.
type LabelDetail struct {
	Name       string           `json:"name"`
	Properties []SchemaProperty `json:"properties"`
}

// SchemaResponse is the response from GET /api/schema.
type SchemaResponse struct {
	Labels                  []string         `json:"labels"`
	RelationshipTypes       []string         `json:"relationship_types"`
	PropertyKeys            []SchemaProperty `json:"property_keys"`
	Indexes                 []IndexInfo      `json:"indexes"`
	Constraints             []ConstraintInfo `json:"constraints"`
	LabelsDetail            []LabelDetail    `json:"labels_detail"`
	RelationshipTypesDetail []LabelDetail    `json:"relationship_types_detail"`
}

// QueryStats contains counters for a write query.
type QueryStats struct {
	NodesCreated         int `json:"nodes_created"`
	NodesDeleted         int `json:"nodes_deleted"`
	RelationshipsCreated int `json:"relationships_created"`
	RelationshipsDeleted int `json:"relationships_deleted"`
	PropertiesSet        int `json:"properties_set"`
	LabelsAdded          int `json:"labels_added"`
	LabelsRemoved        int `json:"labels_removed"`
}

// QueryResponse is the response from POST /api/query.
type QueryResponse struct {
	Columns         []string   `json:"columns"`
	Rows            [][]any    `json:"rows"`
	TotalRows       int        `json:"total_rows"`
	Truncated       bool       `json:"truncated"`
	QueryType       string     `json:"query_type"`
	Stats           QueryStats `json:"stats"`
	TimeMs          float64    `json:"time_ms"`
	ParsingTimeMs   float64    `json:"parsing_time_ms"`
	PlanningTimeMs  float64    `json:"planning_time_ms"`
	ExecutionTimeMs float64    `json:"execution_time_ms"`
}

// ImportError describes a single error during Cypher import.
type ImportError struct {
	Statement string `json:"statement"`
	Error     string `json:"error"`
}

// ImportResult is the response from POST /api/import.
type ImportResult struct {
	TotalStatements int           `json:"total_statements"`
	Executed        int           `json:"executed"`
	Errors          []ImportError `json:"errors"`
	Stats           QueryStats    `json:"stats"`
	TimeMs          float64       `json:"time_ms"`
}

// CsvImportError describes a single error during CSV import.
type CsvImportError struct {
	Row   int    `json:"row"`
	Error string `json:"error"`
}

// CsvImportResult is the response from POST /api/import/csv.
type CsvImportResult struct {
	NodesCreated  int              `json:"nodes_created"`
	PropertiesSet int              `json:"properties_set"`
	LabelsAdded   int              `json:"labels_added"`
	Errors        []CsvImportError `json:"errors"`
	TimeMs        float64          `json:"time_ms"`
}

// CsvEdgeImportResult is the response from POST /api/import/csv/edges.
type CsvEdgeImportResult struct {
	EdgesCreated  int              `json:"edges_created"`
	PropertiesSet int              `json:"properties_set"`
	Errors        []CsvImportError `json:"errors"`
	TimeMs        float64          `json:"time_ms"`
}

// ColumnMapping describes a column mapping for CSV import.
type ColumnMapping struct {
	Name     string `json:"name"`
	Property string `json:"property,omitempty"`
	Type     string `json:"type,omitempty"`
}

// EdgeEndpoint describes a source or target endpoint for CSV edge import.
type EdgeEndpoint struct {
	Label    string `json:"label"`
	Column   string `json:"column"`
	Property string `json:"property"`
}

// CsvImportOptions holds optional parameters for ImportCsv.
type CsvImportOptions struct {
	Database   string
	Delimiter  string
	SkipHeader *bool
}

// CsvEdgeImportOptions holds optional parameters for ImportCsvEdges.
type CsvEdgeImportOptions struct {
	Columns    []ColumnMapping
	Database   string
	Delimiter  string
	SkipHeader *bool
}

// TaskInfo describes an asynchronous task.
type TaskInfo struct {
	ID         string `json:"id"`
	Operation  string `json:"operation"`
	Status     string `json:"status"`
	Result     any    `json:"result,omitempty"`
	Error      string `json:"error,omitempty"`
	CreatedAt  string `json:"createdAt"`
	FinishedAt string `json:"finishedAt,omitempty"`
}

// UserInfo describes a user returned by GET /api/users.
type UserInfo struct {
	Username string `json:"username"`
	Role     string `json:"role"`
}
