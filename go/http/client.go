package http

import (
	"bytes"
	"crypto/tls"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// Options configures the TraverseHttpClient.
type Options struct {
	ApiKey                    string
	Timeout                   time.Duration
	TransferTimeout           time.Duration
	AllowUntrustedCertificate bool
}

// TraverseHttpClient is an HTTP client for the Traverse server management API.
type TraverseHttpClient struct {
	baseURL         string
	apiKey          string
	timeout         time.Duration
	transferTimeout time.Duration
	client          *http.Client
}

// NewClient creates a new TraverseHttpClient. If opts is nil, defaults are used.
func NewClient(baseURL string, opts *Options) *TraverseHttpClient {
	timeout := 100 * time.Second
	transferTimeout := 1 * time.Hour
	apiKey := ""
	allowInsecure := false

	if opts != nil {
		if opts.Timeout > 0 {
			timeout = opts.Timeout
		}
		if opts.TransferTimeout > 0 {
			transferTimeout = opts.TransferTimeout
		}
		apiKey = opts.ApiKey
		allowInsecure = opts.AllowUntrustedCertificate
	}

	transport := &http.Transport{}
	if allowInsecure {
		transport.TLSClientConfig = &tls.Config{InsecureSkipVerify: true}
	}

	base := strings.TrimRight(baseURL, "/") + "/"

	return &TraverseHttpClient{
		baseURL:         base,
		apiKey:          apiKey,
		timeout:         timeout,
		transferTimeout: transferTimeout,
		client:          &http.Client{Transport: transport},
	}
}

// Close releases resources held by the client.
func (c *TraverseHttpClient) Close() {
	c.client.CloseIdleConnections()
}

// -- Health ----------------------------------------------------------

// Health returns the server health status.
func (c *TraverseHttpClient) Health() (*HealthResponse, error) {
	var result HealthResponse
	if err := c.doGet("api/health", &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// Ready returns true if the server is ready to accept requests.
func (c *TraverseHttpClient) Ready() bool {
	req, err := http.NewRequest("GET", c.buildURL("api/ready"), nil)
	if err != nil {
		return false
	}
	c.setHeaders(req)
	client := c.clientWithTimeout(c.timeout)
	resp, err := client.Do(req)
	if err != nil {
		return false
	}
	defer resp.Body.Close()
	io.ReadAll(resp.Body)
	return resp.StatusCode >= 200 && resp.StatusCode < 300
}

// -- Databases -------------------------------------------------------

// ListDatabases returns all databases on the server.
func (c *TraverseHttpClient) ListDatabases() ([]DatabaseInfo, error) {
	var result []DatabaseInfo
	if err := c.doGet("api/databases", &result); err != nil {
		return nil, err
	}
	return result, nil
}

// CreateDatabase creates a new empty database.
func (c *TraverseHttpClient) CreateDatabase(name string) error {
	return c.doPost("api/databases", map[string]string{"name": name}, nil)
}

// DropDatabase drops a database (async, polls to completion).
func (c *TraverseHttpClient) DropDatabase(name string) error {
	body, err := c.doDelete("api/databases/" + url.PathEscape(name) + "?async=true")
	if err != nil {
		return err
	}
	return c.pollTask(body)
}

// RenameDatabase renames a database.
func (c *TraverseHttpClient) RenameDatabase(oldName, newName string) error {
	return c.doPost("api/databases/"+url.PathEscape(oldName)+"/rename", map[string]string{"name": newName}, nil)
}

// UnloadDatabase unloads a database to disk (async, polls to completion).
func (c *TraverseHttpClient) UnloadDatabase(name string) error {
	body, err := c.doPostRaw("api/databases/"+url.PathEscape(name)+"/unload?async=true", nil, c.transferTimeout)
	if err != nil {
		return err
	}
	return c.pollTask(body)
}

// LoadDatabase loads a database from a file (async, polls to completion).
func (c *TraverseHttpClient) LoadDatabase(name, file string) error {
	body, err := c.doPostRaw("api/databases/load?async=true", map[string]string{"name": name, "file": file}, c.transferTimeout)
	if err != nil {
		return err
	}
	return c.pollTask(body)
}

// -- Files -----------------------------------------------------------

// ListFiles returns all .tvdb databases and .tvmodel models on the
// server; check TvdbFileInfo.Kind to tell them apart.
func (c *TraverseHttpClient) ListFiles() ([]TvdbFileInfo, error) {
	var result []TvdbFileInfo
	if err := c.doGet("api/files", &result); err != nil {
		return nil, err
	}
	return result, nil
}

// UploadFile uploads a .tvdb database file to the server.
func (c *TraverseHttpClient) UploadFile(name string, data []byte) (*UploadResult, error) {
	return c.UploadFileKind(name, data, "database")
}

// UploadFileKind uploads a .tvdb database (kind "database") or a
// .tvmodel trained model (kind "model" - requires a server built with
// the ml feature).
func (c *TraverseHttpClient) UploadFileKind(name string, data []byte, kind string) (*UploadResult, error) {
	reqURL := c.buildURL("api/files/" + url.PathEscape(name) + kindQuery(kind))
	req, err := http.NewRequest("PUT", reqURL, bytes.NewReader(data))
	if err != nil {
		return nil, err
	}
	c.setHeaders(req)
	req.Header.Set("Content-Type", "application/octet-stream")
	client := c.clientWithTimeout(c.transferTimeout)
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	respBody, _ := io.ReadAll(resp.Body)
	if err := ensureSuccessBytes(resp.StatusCode, respBody); err != nil {
		return nil, err
	}
	var result UploadResult
	if err := json.Unmarshal(respBody, &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// DownloadFile downloads a .tvdb database file from the server.
func (c *TraverseHttpClient) DownloadFile(name string) ([]byte, error) {
	return c.DownloadFileKind(name, "database")
}

// DownloadFileKind downloads a .tvdb database (kind "database") or a
// .tvmodel trained model (kind "model").
func (c *TraverseHttpClient) DownloadFileKind(name, kind string) ([]byte, error) {
	reqURL := c.buildURL("api/files/" + url.PathEscape(name) + kindQuery(kind))
	req, err := http.NewRequest("GET", reqURL, nil)
	if err != nil {
		return nil, err
	}
	c.setHeaders(req)
	client := c.clientWithTimeout(c.transferTimeout)
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	data, _ := io.ReadAll(resp.Body)
	if err := ensureSuccessBytes(resp.StatusCode, data); err != nil {
		return nil, err
	}
	return data, nil
}

// DeleteFile deletes a .tvdb database file from the server.
func (c *TraverseHttpClient) DeleteFile(name string) error {
	return c.DeleteFileKind(name, "database")
}

// DeleteFileKind deletes a .tvdb database (kind "database") or a
// .tvmodel trained model (kind "model").
func (c *TraverseHttpClient) DeleteFileKind(name, kind string) error {
	_, err := c.doDelete("api/files/" + url.PathEscape(name) + kindQuery(kind))
	return err
}

// kindQuery returns the query-string suffix for /api/files kind
// routing. The default "database" is omitted so requests stay
// byte-identical to pre-kind clients.
func kindQuery(kind string) string {
	if kind == "" || kind == "database" {
		return ""
	}
	return "?kind=" + url.QueryEscape(kind)
}

// -- Aliases ---------------------------------------------------------

// ListAliases returns all aliases.
func (c *TraverseHttpClient) ListAliases() ([]AliasInfo, error) {
	var result []AliasInfo
	if err := c.doGet("api/aliases", &result); err != nil {
		return nil, err
	}
	return result, nil
}

// CreateAlias creates an alias pointing to a database.
func (c *TraverseHttpClient) CreateAlias(alias, database string) error {
	return c.doPost("api/aliases", map[string]string{"alias": alias, "database": database}, nil)
}

// AlterAlias changes the database an alias points to.
func (c *TraverseHttpClient) AlterAlias(alias, database string) error {
	return c.doPut("api/aliases/"+url.PathEscape(alias), map[string]string{"database": database})
}

// DropAlias drops an alias.
func (c *TraverseHttpClient) DropAlias(alias string) error {
	_, err := c.doDelete("api/aliases/" + url.PathEscape(alias))
	return err
}

// -- Schema ----------------------------------------------------------

// GetSchema returns the schema of a database. Pass empty string for no filter.
func (c *TraverseHttpClient) GetSchema(database string) (*SchemaResponse, error) {
	path := "api/schema"
	if database != "" {
		path += "?database=" + url.PathEscape(database)
	}
	var result SchemaResponse
	if err := c.doGet(path, &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// -- Query -----------------------------------------------------------

// Query executes an openCypher query. Pass empty string for database to use the default.
func (c *TraverseHttpClient) Query(cypher string, parameters map[string]any, database string) (*QueryResponse, error) {
	return c.QueryDialect(cypher, parameters, database, "")
}

// QueryDialect executes a query in the given dialect: "" (openCypher,
// the default) or "gql" for ISO/IEC 39075 GQL - requires a server
// built with the gql feature (check HealthResponse.Features["gql"]).
func (c *TraverseHttpClient) QueryDialect(query string, parameters map[string]any, database, dialect string) (*QueryResponse, error) {
	body := map[string]any{"query": query}
	if parameters != nil {
		body["parameters"] = parameters
	}
	if database != "" {
		body["database"] = database
	}
	if dialect != "" {
		body["dialect"] = dialect
	}
	var result QueryResponse
	if err := c.doPost("api/query", body, &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// -- Import ----------------------------------------------------------

// ImportCypher imports Cypher statements. Pass empty string for database to use the default.
func (c *TraverseHttpClient) ImportCypher(cypher, database string) (*ImportResult, error) {
	body := map[string]any{"cypher": cypher}
	if database != "" {
		body["database"] = database
	}
	var result ImportResult
	if err := c.doPost("api/import", body, &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// ImportCsv imports nodes from CSV data. opts may be nil.
func (c *TraverseHttpClient) ImportCsv(csv, label string, columns []ColumnMapping, opts *CsvImportOptions) (*CsvImportResult, error) {
	body := map[string]any{
		"csv":     csv,
		"label":   label,
		"columns": columns,
	}
	if opts != nil {
		if opts.Database != "" {
			body["database"] = opts.Database
		}
		if opts.Delimiter != "" {
			body["delimiter"] = opts.Delimiter
		}
		if opts.SkipHeader != nil {
			body["skip_header"] = *opts.SkipHeader
		}
	}
	var result CsvImportResult
	if err := c.doPost("api/import/csv", body, &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// ImportCsvEdges imports edges from CSV data. opts may be nil.
func (c *TraverseHttpClient) ImportCsvEdges(csv, edgeType string, source, target EdgeEndpoint, opts *CsvEdgeImportOptions) (*CsvEdgeImportResult, error) {
	body := map[string]any{
		"csv":       csv,
		"edge_type": edgeType,
		"source":    source,
		"target":    target,
	}
	if opts != nil {
		if opts.Columns != nil {
			body["columns"] = opts.Columns
		}
		if opts.Database != "" {
			body["database"] = opts.Database
		}
		if opts.Delimiter != "" {
			body["delimiter"] = opts.Delimiter
		}
		if opts.SkipHeader != nil {
			body["skip_header"] = *opts.SkipHeader
		}
	}
	var result CsvEdgeImportResult
	if err := c.doPost("api/import/csv/edges", body, &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// -- Users -----------------------------------------------------------

// ListUsers returns all users.
func (c *TraverseHttpClient) ListUsers() ([]UserInfo, error) {
	var result []UserInfo
	if err := c.doGet("api/users", &result); err != nil {
		return nil, err
	}
	return result, nil
}

// CreateUser creates a new user.
func (c *TraverseHttpClient) CreateUser(username, password, role string) error {
	return c.doPost("api/users", map[string]string{
		"username": username,
		"password": password,
		"role":     role,
	}, nil)
}

// UpdateUser updates a user. Pass nil for fields that should not change.
func (c *TraverseHttpClient) UpdateUser(username string, password, role *string) error {
	body := map[string]string{}
	if password != nil {
		body["password"] = *password
	}
	if role != nil {
		body["role"] = *role
	}
	return c.doPut("api/users/"+url.PathEscape(username), body)
}

// DeleteUser deletes a user.
func (c *TraverseHttpClient) DeleteUser(username string) error {
	_, err := c.doDelete("api/users/" + url.PathEscape(username))
	return err
}

// -- Server operations -----------------------------------------------

// Save persists all databases to disk (async, polls to completion).
func (c *TraverseHttpClient) Save() error {
	body, err := c.doPostRaw("api/save?async=true", nil, c.transferTimeout)
	if err != nil {
		return err
	}
	return c.pollTask(body)
}

// -- Tasks -----------------------------------------------------------

// GetTask returns the status of an asynchronous task.
func (c *TraverseHttpClient) GetTask(taskId string) (*TaskInfo, error) {
	var result TaskInfo
	if err := c.doGet("api/tasks/"+url.PathEscape(taskId), &result); err != nil {
		return nil, err
	}
	return &result, nil
}

// -- Internal HTTP helpers -------------------------------------------

func (c *TraverseHttpClient) buildURL(path string) string {
	return c.baseURL + path
}

func (c *TraverseHttpClient) setHeaders(req *http.Request) {
	if c.apiKey != "" {
		req.Header.Set("Authorization", "Bearer "+c.apiKey)
	}
}

func (c *TraverseHttpClient) clientWithTimeout(timeout time.Duration) *http.Client {
	return &http.Client{
		Transport: c.client.Transport,
		Timeout:   timeout,
	}
}

func (c *TraverseHttpClient) doGet(path string, result any) error {
	req, err := http.NewRequest("GET", c.buildURL(path), nil)
	if err != nil {
		return err
	}
	c.setHeaders(req)
	client := c.clientWithTimeout(c.timeout)
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if err := ensureSuccessBytes(resp.StatusCode, body); err != nil {
		return err
	}
	return json.Unmarshal(body, result)
}

func (c *TraverseHttpClient) doPost(path string, reqBody any, result any) error {
	var bodyReader io.Reader
	if reqBody != nil {
		data, err := json.Marshal(reqBody)
		if err != nil {
			return err
		}
		bodyReader = bytes.NewReader(data)
	}
	req, err := http.NewRequest("POST", c.buildURL(path), bodyReader)
	if err != nil {
		return err
	}
	c.setHeaders(req)
	if reqBody != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	client := c.clientWithTimeout(c.timeout)
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if err := ensureSuccessBytes(resp.StatusCode, body); err != nil {
		return err
	}
	if result != nil && len(body) > 0 {
		return json.Unmarshal(body, result)
	}
	return nil
}

func (c *TraverseHttpClient) doPostRaw(path string, reqBody any, timeout time.Duration) (string, error) {
	var bodyReader io.Reader
	if reqBody != nil {
		data, err := json.Marshal(reqBody)
		if err != nil {
			return "", err
		}
		bodyReader = bytes.NewReader(data)
	}
	req, err := http.NewRequest("POST", c.buildURL(path), bodyReader)
	if err != nil {
		return "", err
	}
	c.setHeaders(req)
	if reqBody != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	client := c.clientWithTimeout(timeout)
	resp, err := client.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if err := ensureSuccessBytes(resp.StatusCode, body); err != nil {
		return "", err
	}
	return string(body), nil
}

func (c *TraverseHttpClient) doPut(path string, reqBody any) error {
	var bodyReader io.Reader
	if reqBody != nil {
		data, err := json.Marshal(reqBody)
		if err != nil {
			return err
		}
		bodyReader = bytes.NewReader(data)
	}
	req, err := http.NewRequest("PUT", c.buildURL(path), bodyReader)
	if err != nil {
		return err
	}
	c.setHeaders(req)
	if reqBody != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	client := c.clientWithTimeout(c.timeout)
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	return ensureSuccessBytes(resp.StatusCode, body)
}

func (c *TraverseHttpClient) doDelete(path string) (string, error) {
	req, err := http.NewRequest("DELETE", c.buildURL(path), nil)
	if err != nil {
		return "", err
	}
	c.setHeaders(req)
	client := c.clientWithTimeout(c.timeout)
	resp, err := client.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if err := ensureSuccessBytes(resp.StatusCode, body); err != nil {
		return "", err
	}
	return string(body), nil
}

func (c *TraverseHttpClient) pollTask(responseBody string) error {
	responseBody = strings.TrimSpace(responseBody)
	if responseBody == "" {
		return nil
	}
	var data map[string]any
	if err := json.Unmarshal([]byte(responseBody), &data); err != nil {
		return nil
	}
	taskID, ok := data["taskId"].(string)
	if !ok || taskID == "" {
		return nil
	}
	delay := 250 * time.Millisecond
	for {
		time.Sleep(delay)
		task, err := c.GetTask(taskID)
		if err != nil {
			return err
		}
		if task.Status == "completed" {
			return nil
		}
		if task.Status == "failed" {
			msg := task.Error
			if msg == "" {
				msg = "task failed"
			}
			return &TraverseHttpError{Message: msg}
		}
		delay *= 2
		if delay > 2*time.Second {
			delay = 2 * time.Second
		}
	}
}

func ensureSuccessBytes(statusCode int, body []byte) error {
	if statusCode >= 200 && statusCode < 300 {
		return nil
	}
	var errorMessage string
	var errBody struct {
		Error string `json:"error"`
	}
	if json.Unmarshal(body, &errBody) == nil && errBody.Error != "" {
		errorMessage = errBody.Error
	}
	if errorMessage == "" {
		errorMessage = fmt.Sprintf("HTTP %d", statusCode)
	}
	return &TraverseHttpError{
		Message:    errorMessage,
		StatusCode: statusCode,
	}
}
