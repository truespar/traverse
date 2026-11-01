package http

import (
	"crypto/rand"
	"errors"
	"fmt"
	"os"
	"strings"
	"testing"
)

var (
	testClient *TraverseHttpClient
	testDB     string
)

func uid() string {
	b := make([]byte, 4)
	rand.Read(b)
	return fmt.Sprintf("%x", b)
}

func TestMain(m *testing.M) {
	baseURL := os.Getenv("TRAVERSE_BASE_URL")
	if baseURL == "" {
		baseURL = "http://localhost:7691"
	}

	// Probe server health
	probe := NewClient(baseURL, &Options{Timeout: 3_000_000_000}) // 3s
	h, err := probe.Health()
	if err != nil || h.Status != "ok" {
		fmt.Fprintf(os.Stderr, "Traverse server not available at %s, skipping tests\n", baseURL)
		os.Exit(0)
	}
	probe.Close()

	var apiKey string
	if h.AuthRequired {
		apiKey = os.Getenv("TRAVERSE_API_KEY")
	}

	testClient = NewClient(baseURL, &Options{
		ApiKey:          apiKey,
		Timeout:         30_000_000_000,  // 30s
		TransferTimeout: 60_000_000_000,  // 60s
	})

	// Create test database
	testDB = fmt.Sprintf("_httptest_%s", uid())
	if err := testClient.CreateDatabase(testDB); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to create test database: %v\n", err)
		os.Exit(1)
	}

	code := m.Run()

	// Teardown
	_ = testClient.DropDatabase(testDB)
	testClient.Close()
	os.Exit(code)
}

// -- Health ----------------------------------------------------------

func TestHealth(t *testing.T) {
	t.Run("healthReturnsStatusAndVersion", func(t *testing.T) {
		h, err := testClient.Health()
		if err != nil {
			t.Fatalf("Health() error: %v", err)
		}
		if h.Status != "ok" {
			t.Errorf("expected status 'ok', got %q", h.Status)
		}
		if h.Version == "" {
			t.Error("expected non-empty version")
		}
	})

	t.Run("readyReturnsTrue", func(t *testing.T) {
		if !testClient.Ready() {
			t.Error("expected Ready() to return true")
		}
	})

	t.Run("healthReportsFeatures", func(t *testing.T) {
		h, err := testClient.Health()
		if err != nil {
			t.Fatalf("Health() error: %v", err)
		}
		// The features map is present on all current servers
		// (gql, grpc, tls, mcp, ml).
		if _, ok := h.Features["gql"]; !ok {
			t.Errorf("expected features map with a gql entry, got %v", h.Features)
		}
	})
}

// -- Query -----------------------------------------------------------

func TestQuery(t *testing.T) {
	t.Run("returnScalar", func(t *testing.T) {
		result, err := testClient.Query("RETURN 42 AS n", nil, testDB)
		if err != nil {
			t.Fatalf("Query() error: %v", err)
		}
		if len(result.Rows) != 1 {
			t.Fatalf("expected 1 row, got %d", len(result.Rows))
		}
		val, ok := result.Rows[0][0].(float64)
		if !ok || val != 42 {
			t.Errorf("expected 42, got %v", result.Rows[0][0])
		}
	})

	t.Run("gqlDialect", func(t *testing.T) {
		h, err := testClient.Health()
		if err != nil {
			t.Fatalf("Health() error: %v", err)
		}
		if !h.Features["gql"] {
			t.Skip("server built without the gql feature")
		}
		// `||` concat and `IS INTEGER` are GQL-only syntax - they
		// parse only when the dialect reaches the server.
		result, err := testClient.QueryDialect(
			"RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i", nil, testDB, "gql")
		if err != nil {
			t.Fatalf("QueryDialect() error: %v", err)
		}
		if s, _ := result.Rows[0][0].(string); s != "foobar" {
			t.Errorf("expected 'foobar', got %v", result.Rows[0][0])
		}
		if b, _ := result.Rows[0][1].(bool); !b {
			t.Errorf("expected true, got %v", result.Rows[0][1])
		}
	})

	t.Run("returnString", func(t *testing.T) {
		result, err := testClient.Query("RETURN 'hello' AS s", nil, testDB)
		if err != nil {
			t.Fatalf("Query() error: %v", err)
		}
		val, ok := result.Rows[0][0].(string)
		if !ok || val != "hello" {
			t.Errorf("expected 'hello', got %v", result.Rows[0][0])
		}
	})

	t.Run("withParameters", func(t *testing.T) {
		result, err := testClient.Query("RETURN $x AS val", map[string]any{"x": 99}, testDB)
		if err != nil {
			t.Fatalf("Query() error: %v", err)
		}
		val, ok := result.Rows[0][0].(float64)
		if !ok || val != 99 {
			t.Errorf("expected 99, got %v", result.Rows[0][0])
		}
	})

	t.Run("writeQueryStats", func(t *testing.T) {
		result, err := testClient.Query("CREATE (n:_HttpTest) RETURN n", nil, testDB)
		if err != nil {
			t.Fatalf("Query() error: %v", err)
		}
		defer testClient.Query("MATCH (n:_HttpTest) DELETE n", nil, testDB)
		if result.Stats.NodesCreated != 1 {
			t.Errorf("expected 1 node created, got %d", result.Stats.NodesCreated)
		}
	})

	t.Run("syntaxErrorThrows", func(t *testing.T) {
		_, err := testClient.Query("THIS IS NOT CYPHER", nil, testDB)
		if err == nil {
			t.Fatal("expected error for bad cypher")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
		if httpErr.StatusCode != 400 {
			t.Errorf("expected status 400, got %d", httpErr.StatusCode)
		}
	})
}

// -- Databases -------------------------------------------------------

func TestDatabases(t *testing.T) {
	t.Run("listDatabasesContainsTestDb", func(t *testing.T) {
		dbs, err := testClient.ListDatabases()
		if err != nil {
			t.Fatalf("ListDatabases() error: %v", err)
		}
		found := false
		for _, d := range dbs {
			if d.Name == testDB {
				found = true
				break
			}
		}
		if !found {
			t.Errorf("test database %q not found in list", testDB)
		}
	})

	t.Run("createAndDropRoundtrip", func(t *testing.T) {
		name := fmt.Sprintf("_httptest_%s", uid())
		if err := testClient.CreateDatabase(name); err != nil {
			t.Fatalf("CreateDatabase() error: %v", err)
		}
		dbs, _ := testClient.ListDatabases()
		if !containsDB(dbs, name) {
			t.Error("database not found after create")
		}
		if err := testClient.DropDatabase(name); err != nil {
			t.Fatalf("DropDatabase() error: %v", err)
		}
		dbs, _ = testClient.ListDatabases()
		if containsDB(dbs, name) {
			t.Error("database still found after drop")
		}
	})

	t.Run("renameRoundtrip", func(t *testing.T) {
		original := fmt.Sprintf("_httptest_%s", uid())
		renamed := fmt.Sprintf("_httptest_%s", uid())
		if err := testClient.CreateDatabase(original); err != nil {
			t.Fatalf("CreateDatabase() error: %v", err)
		}
		defer func() {
			testClient.DropDatabase(renamed)
			testClient.DropDatabase(original)
		}()
		if err := testClient.RenameDatabase(original, renamed); err != nil {
			t.Fatalf("RenameDatabase() error: %v", err)
		}
		dbs, _ := testClient.ListDatabases()
		if containsDB(dbs, original) {
			t.Error("original name still found after rename")
		}
		if !containsDB(dbs, renamed) {
			t.Error("renamed name not found after rename")
		}
	})

	t.Run("unloadAndLoadRoundtrip", func(t *testing.T) {
		name := fmt.Sprintf("_httptest_%s", uid())
		if err := testClient.CreateDatabase(name); err != nil {
			t.Fatalf("CreateDatabase() error: %v", err)
		}
		defer func() { testClient.DropDatabase(name) }()

		if err := testClient.UnloadDatabase(name); err != nil {
			t.Fatalf("UnloadDatabase() error: %v", err)
		}
		dbs, _ := testClient.ListDatabases()
		if containsDB(dbs, name) {
			t.Error("database still listed after unload")
		}
		files, err := testClient.ListFiles()
		if err != nil {
			t.Fatalf("ListFiles() error: %v", err)
		}
		var file string
		for _, f := range files {
			if f.Name == name {
				file = f.File
				break
			}
		}
		if file == "" {
			t.Fatal("unloaded file not found")
		}
		if err := testClient.LoadDatabase(name, file); err != nil {
			t.Fatalf("LoadDatabase() error: %v", err)
		}
		dbs, _ = testClient.ListDatabases()
		if !containsDB(dbs, name) {
			t.Error("database not found after reload")
		}
	})
}

// -- Schema ----------------------------------------------------------

func TestSchema(t *testing.T) {
	t.Run("getSchemaReturnsLabels", func(t *testing.T) {
		_, err := testClient.Query(
			"CREATE (a:_SchemaTestNode {name: 'a'})-[:_SCHEMA_TEST_REL]->(b:_SchemaTestNode {name: 'b'})",
			nil, testDB,
		)
		if err != nil {
			t.Fatalf("Query() error: %v", err)
		}
		defer testClient.Query("MATCH (n:_SchemaTestNode) DETACH DELETE n", nil, testDB)

		schema, err := testClient.GetSchema(testDB)
		if err != nil {
			t.Fatalf("GetSchema() error: %v", err)
		}
		if !containsStr(schema.Labels, "_SchemaTestNode") {
			t.Error("_SchemaTestNode not found in labels")
		}
		if !containsStr(schema.RelationshipTypes, "_SCHEMA_TEST_REL") {
			t.Error("_SCHEMA_TEST_REL not found in relationship_types")
		}
	})
}

// -- Aliases ---------------------------------------------------------

func TestAliases(t *testing.T) {
	t.Run("createAndDropRoundtrip", func(t *testing.T) {
		alias := fmt.Sprintf("_httptest_alias_%s", uid())
		if err := testClient.CreateAlias(alias, testDB); err != nil {
			t.Fatalf("CreateAlias() error: %v", err)
		}
		defer testClient.DropAlias(alias)

		aliases, _ := testClient.ListAliases()
		found := false
		for _, a := range aliases {
			if a.Alias == alias && a.Database == testDB {
				found = true
				break
			}
		}
		if !found {
			t.Error("alias not found after create")
		}
		if err := testClient.DropAlias(alias); err != nil {
			t.Fatalf("DropAlias() error: %v", err)
		}
		aliases, _ = testClient.ListAliases()
		for _, a := range aliases {
			if a.Alias == alias {
				t.Error("alias still found after drop")
				break
			}
		}
	})

	t.Run("alterChangesTarget", func(t *testing.T) {
		alias := fmt.Sprintf("_httptest_alias_%s", uid())
		db2 := fmt.Sprintf("_httptest_%s", uid())
		if err := testClient.CreateDatabase(db2); err != nil {
			t.Fatalf("CreateDatabase() error: %v", err)
		}
		defer func() { testClient.DropDatabase(db2) }()

		if err := testClient.CreateAlias(alias, testDB); err != nil {
			t.Fatalf("CreateAlias() error: %v", err)
		}
		defer testClient.DropAlias(alias)

		if err := testClient.AlterAlias(alias, db2); err != nil {
			t.Fatalf("AlterAlias() error: %v", err)
		}
		aliases, _ := testClient.ListAliases()
		found := false
		for _, a := range aliases {
			if a.Alias == alias && a.Database == db2 {
				found = true
				break
			}
		}
		if !found {
			t.Error("alias not pointing to new database after alter")
		}
	})
}

// -- Import ----------------------------------------------------------

func TestImport(t *testing.T) {
	t.Run("importCypherCreatesNodes", func(t *testing.T) {
		cypher := "CREATE (:_ImportTest {id: 1});\nCREATE (:_ImportTest {id: 2});\nCREATE (:_ImportTest {id: 3});"
		result, err := testClient.ImportCypher(cypher, testDB)
		if err != nil {
			t.Fatalf("ImportCypher() error: %v", err)
		}
		defer testClient.Query("MATCH (n:_ImportTest) DELETE n", nil, testDB)
		if result.Executed != 3 {
			t.Errorf("expected 3 executed, got %d", result.Executed)
		}
		if len(result.Errors) != 0 {
			t.Errorf("expected 0 errors, got %d", len(result.Errors))
		}
	})

	t.Run("importCsvCreatesNodes", func(t *testing.T) {
		csv := "name,age\nAlice,30\nBob,25"
		columns := []ColumnMapping{
			{Name: "name", Property: "name", Type: "string"},
			{Name: "age", Property: "age", Type: "integer"},
		}
		result, err := testClient.ImportCsv(csv, "_CsvTestNode", columns, &CsvImportOptions{Database: testDB})
		if err != nil {
			t.Fatalf("ImportCsv() error: %v", err)
		}
		defer testClient.Query("MATCH (n:_CsvTestNode) DELETE n", nil, testDB)
		if result.NodesCreated != 2 {
			t.Errorf("expected 2 nodes created, got %d", result.NodesCreated)
		}
		if len(result.Errors) != 0 {
			t.Errorf("expected 0 errors, got %d", len(result.Errors))
		}
	})

	t.Run("importCsvEdgesCreatesEdges", func(t *testing.T) {
		_, err := testClient.Query(
			"CREATE (:_CsvEdgeSrc {key: 'a'}), (:_CsvEdgeDst {key: 'b'})",
			nil, testDB,
		)
		if err != nil {
			t.Fatalf("Query() error: %v", err)
		}
		defer func() {
			testClient.Query("MATCH (n:_CsvEdgeSrc) DETACH DELETE n", nil, testDB)
			testClient.Query("MATCH (n:_CsvEdgeDst) DETACH DELETE n", nil, testDB)
		}()

		csv := "src,dst\na,b"
		source := EdgeEndpoint{Label: "_CsvEdgeSrc", Column: "src", Property: "key"}
		target := EdgeEndpoint{Label: "_CsvEdgeDst", Column: "dst", Property: "key"}
		result, err := testClient.ImportCsvEdges(csv, "_CSV_TEST_REL", source, target, &CsvEdgeImportOptions{Database: testDB})
		if err != nil {
			t.Fatalf("ImportCsvEdges() error: %v", err)
		}
		if result.EdgesCreated != 1 {
			t.Errorf("expected 1 edge created, got %d", result.EdgesCreated)
		}
		if len(result.Errors) != 0 {
			t.Errorf("expected 0 errors, got %d", len(result.Errors))
		}
	})
}

// -- Users -----------------------------------------------------------

func TestUsers(t *testing.T) {
	t.Run("createListDeleteRoundtrip", func(t *testing.T) {
		username := fmt.Sprintf("_httptest_%s", uid())
		if err := testClient.CreateUser(username, "testpass123", "reader"); err != nil {
			t.Fatalf("CreateUser() error: %v", err)
		}
		users, _ := testClient.ListUsers()
		if !containsUser(users, username) {
			t.Error("user not found after create")
		}
		if err := testClient.DeleteUser(username); err != nil {
			t.Fatalf("DeleteUser() error: %v", err)
		}
		users, _ = testClient.ListUsers()
		if containsUser(users, username) {
			t.Error("user still found after delete")
		}
	})

	t.Run("updateUserChangesRole", func(t *testing.T) {
		username := fmt.Sprintf("_httptest_%s", uid())
		if err := testClient.CreateUser(username, "testpass123", "reader"); err != nil {
			t.Fatalf("CreateUser() error: %v", err)
		}
		defer testClient.DeleteUser(username)

		role := "admin"
		if err := testClient.UpdateUser(username, nil, &role); err != nil {
			t.Fatalf("UpdateUser() error: %v", err)
		}
		users, _ := testClient.ListUsers()
		for _, u := range users {
			if u.Username == username {
				if u.Role != "admin" {
					t.Errorf("expected role 'admin', got %q", u.Role)
				}
				return
			}
		}
		t.Error("user not found after update")
	})
}

// -- Save ------------------------------------------------------------

func TestSave(t *testing.T) {
	t.Run("saveSucceeds", func(t *testing.T) {
		if err := testClient.Save(); err != nil {
			t.Fatalf("Save() error: %v", err)
		}
	})
}

// -- Files -----------------------------------------------------------

func TestFiles(t *testing.T) {
	t.Run("listFilesReturnsList", func(t *testing.T) {
		files, err := testClient.ListFiles()
		if err != nil {
			t.Fatalf("ListFiles() error: %v", err)
		}
		if files == nil {
			t.Error("expected non-nil list")
		}
	})

	t.Run("downloadFileStreamsBytes", func(t *testing.T) {
		data, err := testClient.DownloadFile(testDB)
		if err != nil {
			t.Fatalf("DownloadFile() error: %v", err)
		}
		if len(data) == 0 {
			t.Error("expected non-empty data")
		}
	})

	t.Run("uploadAndDeleteFileRoundtrip", func(t *testing.T) {
		name := fmt.Sprintf("_httptest_%s", uid())
		tempDB := fmt.Sprintf("_httptest_src_%s", uid())
		if err := testClient.CreateDatabase(tempDB); err != nil {
			t.Fatalf("CreateDatabase() error: %v", err)
		}

		tvdbBytes, err := testClient.DownloadFile(tempDB)
		if err != nil {
			testClient.DropDatabase(tempDB)
			t.Fatalf("DownloadFile() error: %v", err)
		}
		if err := testClient.DropDatabase(tempDB); err != nil {
			t.Fatalf("DropDatabase() error: %v", err)
		}

		upload, err := testClient.UploadFile(name, tvdbBytes)
		if err != nil {
			t.Fatalf("UploadFile() error: %v", err)
		}
		if upload.Name != name {
			t.Errorf("expected name %q, got %q", name, upload.Name)
		}

		files, _ := testClient.ListFiles()
		found := false
		for _, f := range files {
			if f.Name == name && !f.Loaded {
				found = true
				break
			}
		}
		if !found {
			t.Error("uploaded file not found (or is loaded)")
		}

		if err := testClient.DeleteFile(name); err != nil {
			t.Fatalf("DeleteFile() error: %v", err)
		}
		files, _ = testClient.ListFiles()
		for _, f := range files {
			if f.Name == name {
				t.Error("file still found after delete")
				break
			}
		}
	})
}

// -- Tasks -----------------------------------------------------------

func TestTasks(t *testing.T) {
	t.Run("asyncSaveSucceeds", func(t *testing.T) {
		if err := testClient.Save(); err != nil {
			t.Fatalf("Save() error: %v", err)
		}
	})

	t.Run("getTaskNotFoundThrows", func(t *testing.T) {
		_, err := testClient.GetTask("tsk_0000000000000000")
		if err == nil {
			t.Fatal("expected error for nonexistent task")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
		if httpErr.StatusCode != 404 {
			t.Errorf("expected status 404, got %d", httpErr.StatusCode)
		}
	})

	t.Run("asyncUnloadAndReloadRoundtrip", func(t *testing.T) {
		name := fmt.Sprintf("_httptest_%s", uid())
		if err := testClient.CreateDatabase(name); err != nil {
			t.Fatalf("CreateDatabase() error: %v", err)
		}
		defer func() { testClient.DropDatabase(name) }()

		if _, err := testClient.Query("CREATE (:_TaskMarker {v: 42})", nil, name); err != nil {
			t.Fatalf("Query() error: %v", err)
		}
		if err := testClient.UnloadDatabase(name); err != nil {
			t.Fatalf("UnloadDatabase() error: %v", err)
		}
		dbs, _ := testClient.ListDatabases()
		if containsDB(dbs, name) {
			t.Error("database still listed after unload")
		}
		files, _ := testClient.ListFiles()
		var file string
		for _, f := range files {
			if f.Name == name {
				file = f.File
				break
			}
		}
		if file == "" {
			t.Fatal("unloaded file not found")
		}
		if err := testClient.LoadDatabase(name, file); err != nil {
			t.Fatalf("LoadDatabase() error: %v", err)
		}
		result, err := testClient.Query("MATCH (n:_TaskMarker) RETURN n.v AS v", nil, name)
		if err != nil {
			t.Fatalf("Query() error: %v", err)
		}
		if len(result.Rows) != 1 {
			t.Fatalf("expected 1 row, got %d", len(result.Rows))
		}
		val, ok := result.Rows[0][0].(float64)
		if !ok || val != 42 {
			t.Errorf("expected 42, got %v", result.Rows[0][0])
		}
	})
}

// -- Errors ----------------------------------------------------------

func TestErrors(t *testing.T) {
	t.Run("invalidBaseUrlThrows", func(t *testing.T) {
		bad := NewClient("http://localhost:19999", &Options{Timeout: 2_000_000_000})
		_, err := bad.Health()
		if err == nil {
			t.Fatal("expected error for bad URL")
		}
		bad.Close()
	})

	t.Run("createDuplicateDatabaseThrows", func(t *testing.T) {
		err := testClient.CreateDatabase(testDB)
		if err == nil {
			t.Fatal("expected error for duplicate database")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
		if !strings.Contains(httpErr.Message, "already exists") {
			t.Errorf("expected 'already exists' in message, got %q", httpErr.Message)
		}
	})

	t.Run("dropNonexistentDatabaseThrows", func(t *testing.T) {
		err := testClient.DropDatabase("_httptest_does_not_exist_ever")
		if err == nil {
			t.Fatal("expected error for nonexistent database")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
	})

	t.Run("renameToExistingNameThrows", func(t *testing.T) {
		temp := fmt.Sprintf("_httptest_%s", uid())
		if err := testClient.CreateDatabase(temp); err != nil {
			t.Fatalf("CreateDatabase() error: %v", err)
		}
		defer func() { testClient.DropDatabase(temp) }()

		err := testClient.RenameDatabase(temp, testDB)
		if err == nil {
			t.Fatal("expected error for rename to existing name")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
		if !strings.Contains(httpErr.Message, "already exists") {
			t.Errorf("expected 'already exists' in message, got %q", httpErr.Message)
		}
	})

	t.Run("renameNonexistentDatabaseThrows", func(t *testing.T) {
		err := testClient.RenameDatabase("_httptest_does_not_exist_ever", "whatever")
		if err == nil {
			t.Fatal("expected error for nonexistent database")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
		if !strings.Contains(httpErr.Message, "does not exist") {
			t.Errorf("expected 'does not exist' in message, got %q", httpErr.Message)
		}
	})

	t.Run("createDuplicateAliasThrows", func(t *testing.T) {
		alias := fmt.Sprintf("_httptest_alias_%s", uid())
		if err := testClient.CreateAlias(alias, testDB); err != nil {
			t.Fatalf("CreateAlias() error: %v", err)
		}
		defer testClient.DropAlias(alias)

		err := testClient.CreateAlias(alias, testDB)
		if err == nil {
			t.Fatal("expected error for duplicate alias")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
		if !strings.Contains(httpErr.Message, "already exists") {
			t.Errorf("expected 'already exists' in message, got %q", httpErr.Message)
		}
	})

	t.Run("createAliasToNonexistentDbThrows", func(t *testing.T) {
		err := testClient.CreateAlias("_httptest_alias_bad", "_httptest_does_not_exist_ever")
		if err == nil {
			t.Fatal("expected error for nonexistent database")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
		if !strings.Contains(httpErr.Message, "does not exist") {
			t.Errorf("expected 'does not exist' in message, got %q", httpErr.Message)
		}
	})

	t.Run("dropNonexistentAliasThrows", func(t *testing.T) {
		err := testClient.DropAlias("_httptest_alias_does_not_exist")
		if err == nil {
			t.Fatal("expected error for nonexistent alias")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
	})

	t.Run("deleteNonexistentUserThrows", func(t *testing.T) {
		err := testClient.DeleteUser("_httptest_user_does_not_exist")
		if err == nil {
			t.Fatal("expected error for nonexistent user")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
		if !strings.Contains(httpErr.Message, "does not exist") {
			t.Errorf("expected 'does not exist' in message, got %q", httpErr.Message)
		}
	})

	t.Run("queryNonexistentDatabaseThrows", func(t *testing.T) {
		_, err := testClient.Query("RETURN 1", nil, "_httptest_does_not_exist_ever")
		if err == nil {
			t.Fatal("expected error for nonexistent database")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
	})

	t.Run("deleteLoadedFileThrows", func(t *testing.T) {
		err := testClient.DeleteFile(testDB)
		if err == nil {
			t.Fatal("expected error for loaded file")
		}
		var httpErr *TraverseHttpError
		if !errors.As(err, &httpErr) {
			t.Fatalf("expected TraverseHttpError, got %T", err)
		}
		if !strings.Contains(httpErr.Message, "loaded") {
			t.Errorf("expected 'loaded' in message, got %q", httpErr.Message)
		}
	})
}

// -- Helpers ---------------------------------------------------------

func containsDB(dbs []DatabaseInfo, name string) bool {
	for _, d := range dbs {
		if d.Name == name {
			return true
		}
	}
	return false
}

func containsStr(ss []string, s string) bool {
	for _, v := range ss {
		if v == s {
			return true
		}
	}
	return false
}

func containsUser(users []UserInfo, username string) bool {
	for _, u := range users {
		if u.Username == username {
			return true
		}
	}
	return false
}
