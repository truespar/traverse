package traverse

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"
)

func tempDB(t *testing.T) (*Database, string) {
	t.Helper()
	dir := t.TempDir()
	path := filepath.Join(dir, "test.tvdb")
	db, err := Open(path)
	if err != nil {
		t.Fatalf("Open(%s): %v", path, err)
	}
	return db, path
}

// --- TestOpen ---------------------------------------------------------------

func TestOpen(t *testing.T) {
	db, _ := tempDB(t)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestOpenCreatesFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "new.tvdb")
	db, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	db.Flush()
	defer db.Close()
	if _, err := os.Stat(path); os.IsNotExist(err) {
		t.Fatal("database file not created")
	}
}

func TestReopen(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "reopen.tvdb")

	db, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	tx, _ := db.Begin()
	tx.Execute("CREATE (:Marker {v: 42})")
	tx.Commit()
	db.Flush()
	db.Close()

	db2, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer db2.Close()
	tx2, _ := db2.Begin()
	r, _ := tx2.Execute("MATCH (n:Marker) RETURN n.v")
	tx2.Commit()
	if r.RowCount != 1 {
		t.Fatalf("expected 1 row after reopen, got %d", r.RowCount)
	}
	if r.Rows[0]["n.v"].(int64) != 42 {
		t.Fatal("expected v=42")
	}
}

func TestOpenEmptyPathFails(t *testing.T) {
	_, err := Open("")
	if err == nil {
		t.Fatal("expected error for empty path")
	}
}

func TestCloseIdempotent(t *testing.T) {
	db, _ := tempDB(t)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal("second close should be no-op")
	}
}

// --- TestTransaction --------------------------------------------------------

func TestTransactionCommit(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, err := db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	_, err = tx.Execute("CREATE (:Person {name: 'Alice'})")
	if err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}

	tx2, _ := db.Begin()
	r, _ := tx2.Execute("MATCH (p:Person) RETURN p.name")
	tx2.Commit()
	if r.RowCount != 1 || r.Rows[0]["p.name"] != "Alice" {
		t.Fatalf("expected Alice, got %v", r.Rows)
	}
}

// GQL-only syntax (|| concat, IS INTEGER, INSERT/FILTER) - the
// openCypher parser rejects these, so passing proves ExecuteGql
// reached the ISO/IEC 39075 front-end.
func TestExecuteGql(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, err := db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	r, err := tx.ExecuteGql("RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i")
	if err != nil {
		t.Fatal(err)
	}
	if r.Rows[0]["s"] != "foobar" || r.Rows[0]["i"] != true {
		t.Fatalf("unexpected row: %v", r.Rows[0])
	}
	if _, err := tx.ExecuteGql("INSERT (:Person {name: 'Ada', age: 36})"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}

	tx2, _ := db.Begin()
	r2, err := tx2.ExecuteGql("MATCH (p:Person) FILTER p.age > 10 RETURN p.name AS name")
	tx2.Commit()
	if err != nil {
		t.Fatal(err)
	}
	if r2.RowCount != 1 || r2.Rows[0]["name"] != "Ada" {
		t.Fatalf("expected Ada, got %v", r2.Rows)
	}
}

// Plain Execute must keep rejecting GQL-only syntax - the dialects
// stay separate.
func TestCypherRejectsGqlSyntax(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	defer tx.Abort()
	if _, err := tx.Execute("RETURN 1 IS INTEGER AS i"); err == nil {
		t.Fatal("expected syntax error from the openCypher parser")
	}
}

func TestTransactionAbort(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:Ghost)")
	tx.Abort()

	tx2, _ := db.Begin()
	r, _ := tx2.Execute("MATCH (n:Ghost) RETURN count(n) AS c")
	tx2.Commit()
	if r.Rows[0]["c"].(int64) != 0 {
		t.Fatal("expected 0 after abort")
	}
}

func TestTransactionReturnTypes(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	r, _ := tx.Execute("RETURN 42 AS i, 3.14 AS f, true AS b, 'hello' AS s, null AS n")
	tx.Commit()

	row := r.Rows[0]
	if row["i"].(int64) != 42 {
		t.Fatal("int")
	}
	if row["f"].(float64) != 3.14 {
		t.Fatal("float")
	}
	if row["b"].(bool) != true {
		t.Fatal("bool")
	}
	if row["s"].(string) != "hello" {
		t.Fatal("string")
	}
	if row["n"] != nil {
		t.Fatal("null")
	}
}

func TestTransactionNodeReturn(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:Animal {species: 'Cat', legs: 4})")
	r, _ := tx.Execute("MATCH (a:Animal) RETURN a")
	tx.Commit()

	node := r.Rows[0]["a"].(Node)
	if len(node.Labels) != 1 || node.Labels[0] != "Animal" {
		t.Fatalf("expected [Animal], got %v", node.Labels)
	}
	if node.Properties["species"] != "Cat" {
		t.Fatal("expected species=Cat")
	}
}

func TestTransactionEdgeReturn(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:A)-[:LINKS {w: 5}]->(:B)")
	r, _ := tx.Execute("MATCH (:A)-[r:LINKS]->(:B) RETURN r")
	tx.Commit()

	edge := r.Rows[0]["r"].(Edge)
	if edge.Type != "LINKS" {
		t.Fatalf("expected LINKS, got %s", edge.Type)
	}
	if edge.Properties["w"] != "5" {
		t.Fatal("expected w=5")
	}
}

func TestTransactionMultipleQueries(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:X)")
	tx.Execute("CREATE (:Y)")
	r, _ := tx.Execute("MATCH (n) RETURN count(n) AS c")
	tx.Commit()
	if r.Rows[0]["c"].(int64) != 2 {
		t.Fatal("expected 2")
	}
}

func TestTransactionSyntaxError(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	_, err := tx.Execute("NOT VALID CYPHER")
	tx.Abort()
	if err == nil {
		t.Fatal("expected syntax error")
	}
	tvErr, ok := err.(*TraverseError)
	if !ok {
		t.Fatal("expected TraverseError")
	}
	if tvErr.Code != ErrSyntax {
		t.Fatalf("expected ErrSyntax, got %d", tvErr.Code)
	}
}

func TestTransactionUnknownFunction(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	_, err := tx.Execute("RETURN noSuchFunction()")
	tx.Abort()
	if err == nil {
		t.Fatal("expected error for unknown function")
	}
}

func TestCommitAfterCommit(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:X)")
	tx.Commit()
	err := tx.Commit()
	if err == nil {
		t.Fatal("expected error on double commit")
	}
}

func TestExecuteAfterCommit(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Commit()
	_, err := tx.Execute("RETURN 1")
	if err == nil {
		t.Fatal("expected error on execute after commit")
	}
}

// --- TestBulkWriter ---------------------------------------------------------

func TestBulkWriterCreateNode(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, err := db.BulkWriter()
	if err != nil {
		t.Fatal(err)
	}
	id, err := bw.CreateNode([]string{"Person"}, map[string]string{"name": "Bob"})
	if err != nil {
		t.Fatal(err)
	}
	if id < 0 {
		t.Fatal("expected non-negative ID")
	}
	if err := bw.Commit(); err != nil {
		t.Fatal(err)
	}

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (p:Person) RETURN p.name")
	tx.Commit()
	if r.Rows[0]["p.name"] != "Bob" {
		t.Fatal("expected Bob")
	}
}

func TestBulkWriterCreateEdge(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	a, _ := bw.CreateNode([]string{"A"}, nil)
	b, _ := bw.CreateNode([]string{"B"}, nil)
	eid, err := bw.CreateEdge(a, b, "KNOWS", map[string]string{"since": "2024"})
	if err != nil {
		t.Fatal(err)
	}
	if eid < 0 {
		t.Fatal("expected non-negative edge ID")
	}
	bw.Commit()

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (:A)-[r:KNOWS]->(:B) RETURN r.since")
	tx.Commit()
	// Bulk writer infers "2024" as integer
	if r.Rows[0]["r.since"].(int64) != 2024 {
		t.Fatal("expected since=2024")
	}
}

func TestBulkWriterMultipleLabels(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	bw.CreateNode([]string{"Person", "Employee"}, nil)
	bw.Commit()

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (n:Person:Employee) RETURN count(n) AS c")
	tx.Commit()
	if r.Rows[0]["c"].(int64) != 1 {
		t.Fatal("expected 1 node with both labels")
	}
}

func TestBulkWriterEdgeNoProps(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	a, _ := bw.CreateNode([]string{"X"}, nil)
	b, _ := bw.CreateNode([]string{"Y"}, nil)
	_, err := bw.CreateEdge(a, b, "LINKS", nil)
	if err != nil {
		t.Fatal(err)
	}
	bw.Commit()
}

func TestBulkWriterEdgeIndex(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	err := bw.CreateEdgeIndex("idx_likes", "LIKES", "")
	if err != nil {
		t.Fatal(err)
	}
	bw.Commit()
}

func TestBulkWriterAbort(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	bw.CreateNode([]string{"Ghost"}, nil)
	bw.Abort()
	if n := countNodes(t, db, "Ghost"); n != 0 {
		t.Fatalf("expected the aborted load rolled back, got %d", n)
	}

	// After abort, should be able to start a new writer
	bw2, err := db.BulkWriter()
	if err != nil {
		t.Fatal("expected to start new bulk writer after abort")
	}
	bw2.Abort()
}

func TestBulkWriterCommitAfterCommit(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	bw.CreateNode([]string{"X"}, nil)
	bw.Commit()
	err := bw.Commit()
	if err == nil {
		t.Fatal("expected error on double commit")
	}
}

func TestBulkWriterCreateNodeAfterCommit(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	bw.Commit()
	_, err := bw.CreateNode([]string{"X"}, nil)
	if err == nil {
		t.Fatal("expected error on create after commit")
	}
}

func TestBulkWriterInvalidSource(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	a, _ := bw.CreateNode([]string{"X"}, nil)
	_, err := bw.CreateEdge(a, 999999, "BAD", nil)
	if err == nil {
		t.Fatal("expected error for invalid target")
	}
	bw.Abort()
}

func TestBulkWriterInsert1000(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	for i := 0; i < 1000; i++ {
		_, err := bw.CreateNode([]string{"N"}, map[string]string{"i": fmt.Sprintf("%d", i)})
		if err != nil {
			t.Fatal(err)
		}
	}
	bw.Commit()

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (n:N) RETURN count(n) AS c")
	tx.Commit()
	if r.Rows[0]["c"].(int64) != 1000 {
		t.Fatal("expected 1000 nodes")
	}
}

func TestBulkWriterGraphStructure(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	a, _ := bw.CreateNode([]string{"Person"}, map[string]string{"name": "Alice"})
	b, _ := bw.CreateNode([]string{"Person"}, map[string]string{"name": "Bob"})
	c, _ := bw.CreateNode([]string{"Person"}, map[string]string{"name": "Charlie"})
	bw.CreateEdge(a, b, "KNOWS", nil)
	bw.CreateEdge(b, c, "KNOWS", nil)
	bw.CreateEdge(a, c, "KNOWS", nil)
	bw.Commit()

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (p:Person) RETURN count(p) AS np")
	r2, _ := tx.Execute("MATCH ()-[r:KNOWS]->() RETURN count(r) AS nr")
	tx.Commit()

	if r.Rows[0]["np"].(int64) != 3 {
		t.Fatal("expected 3 persons")
	}
	if r2.Rows[0]["nr"].(int64) != 3 {
		t.Fatal("expected 3 edges")
	}
}

// --- TestIndex --------------------------------------------------------------

func TestCreateNodeIndex(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	err := bw.CreateIndex("idx_name", "Person", "name")
	if err != nil {
		t.Fatal(err)
	}
	bw.Commit()
}

func TestCreateEdgeIndex(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	err := bw.CreateEdgeIndex("idx_since", "KNOWS", "since")
	if err != nil {
		t.Fatal(err)
	}
	bw.Commit()
}

func TestCreateEdgeTypeOnlyIndex(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	err := bw.CreateEdgeIndex("idx_knows", "KNOWS", "")
	if err != nil {
		t.Fatal(err)
	}
	bw.Commit()
}

func TestDuplicateIndexFails(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	bw.CreateIndex("idx_dup", "Person", "name")
	err := bw.CreateIndex("idx_dup", "Person", "name")
	if err == nil {
		t.Fatal("expected error for duplicate index")
	}
	bw.Abort()
}

// --- TestFlush --------------------------------------------------------------

func TestFlush(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	if err := db.Flush(); err != nil {
		t.Fatal(err)
	}
}

func TestFlushPersists(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "flush.tvdb")

	db, _ := Open(path)
	tx, _ := db.Begin()
	tx.Execute("CREATE (:Durable {x: 99})")
	tx.Commit()
	db.Flush()
	db.Close()

	db2, _ := Open(path)
	defer db2.Close()
	tx2, _ := db2.Begin()
	r, _ := tx2.Execute("MATCH (d:Durable) RETURN d.x")
	tx2.Commit()
	if r.RowCount != 1 || r.Rows[0]["d.x"].(int64) != 99 {
		t.Fatal("data not persisted after flush")
	}
}

// --- TestPropertyEdgeCases --------------------------------------------------

func TestPropertyEmptyString(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:T {s: ''})")
	r, _ := tx.Execute("MATCH (t:T) RETURN t.s")
	tx.Commit()
	if r.Rows[0]["t.s"] != "" {
		t.Fatal("expected empty string")
	}
}

func TestPropertyLargeString(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	big := ""
	for i := 0; i < 10000; i++ {
		big += "x"
	}
	tx.Execute(fmt.Sprintf("CREATE (:T {s: '%s'})", big))
	r, _ := tx.Execute("MATCH (t:T) RETURN t.s")
	tx.Commit()
	if len(r.Rows[0]["t.s"].(string)) != 10000 {
		t.Fatal("expected 10000 char string")
	}
}

func TestPropertyNegativeInt(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:T {v: -42})")
	r, _ := tx.Execute("MATCH (t:T) RETURN t.v")
	tx.Commit()
	if r.Rows[0]["t.v"].(int64) != -42 {
		t.Fatal("expected -42")
	}
}

func TestPropertyZero(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:T {v: 0})")
	r, _ := tx.Execute("MATCH (t:T) RETURN t.v")
	tx.Commit()
	if r.Rows[0]["t.v"].(int64) != 0 {
		t.Fatal("expected 0")
	}
}

func TestPropertyLargeInt(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:T {v: 9223372036854775807})")
	r, _ := tx.Execute("MATCH (t:T) RETURN t.v")
	tx.Commit()
	if r.Rows[0]["t.v"].(int64) != 9223372036854775807 {
		t.Fatal("expected max int64")
	}
}

func TestPropertyNegativeFloat(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:T {v: -1.5})")
	r, _ := tx.Execute("MATCH (t:T) RETURN t.v")
	tx.Commit()
	if r.Rows[0]["t.v"].(float64) != -1.5 {
		t.Fatal("expected -1.5")
	}
}

func TestPropertyBoolTrue(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:T {v: true})")
	r, _ := tx.Execute("MATCH (t:T) RETURN t.v")
	tx.Commit()
	if r.Rows[0]["t.v"].(bool) != true {
		t.Fatal("expected true")
	}
}

func TestPropertyBoolFalse(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:T {v: false})")
	r, _ := tx.Execute("MATCH (t:T) RETURN t.v")
	tx.Commit()
	if r.Rows[0]["t.v"].(bool) != false {
		t.Fatal("expected false")
	}
}

func TestPropertyNull(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	r, _ := tx.Execute("RETURN null AS v")
	tx.Commit()
	if r.Rows[0]["v"] != nil {
		t.Fatal("expected nil")
	}
}

func TestPropertyMultipleTypes(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:Multi {name: 'test', age: 30, active: true, score: 9.5})")
	r, _ := tx.Execute("MATCH (m:Multi) RETURN m.name, m.age, m.active, m.score")
	tx.Commit()

	row := r.Rows[0]
	if row["m.name"] != "test" {
		t.Fatal("name")
	}
	if row["m.age"].(int64) != 30 {
		t.Fatal("age")
	}
	if row["m.active"].(bool) != true {
		t.Fatal("active")
	}
	if row["m.score"].(float64) != 9.5 {
		t.Fatal("score")
	}
}

func TestPropertyUnicode(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:T {v: '日本語テスト'})")
	r, _ := tx.Execute("MATCH (t:T) RETURN t.v")
	tx.Commit()
	if r.Rows[0]["t.v"] != "日本語テスト" {
		t.Fatal("expected unicode string")
	}
}

// --- TestConcurrency --------------------------------------------------------

func TestConcurrentReads(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	for i := 0; i < 100; i++ {
		tx.Execute(fmt.Sprintf("CREATE (:N {i: %d})", i))
	}
	tx.Commit()

	var wg sync.WaitGroup
	errors := make(chan error, 10)
	for g := 0; g < 10; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			tx, err := db.BeginRead()
			if err != nil {
				errors <- err
				return
			}
			r, err := tx.Execute("MATCH (n:N) RETURN count(n) AS c")
			if err != nil {
				tx.Abort()
				errors <- err
				return
			}
			tx.Commit()
			if r.Rows[0]["c"].(int64) != 100 {
				errors <- fmt.Errorf("expected 100, got %v", r.Rows[0]["c"])
			}
		}()
	}
	wg.Wait()
	close(errors)
	for err := range errors {
		t.Fatal(err)
	}
}

func TestSequentialTransactions(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	for i := 0; i < 10; i++ {
		tx, _ := db.Begin()
		tx.Execute(fmt.Sprintf("CREATE (:S {i: %d})", i))
		tx.Commit()
	}

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (s:S) RETURN count(s) AS c")
	tx.Commit()
	if r.Rows[0]["c"].(int64) != 10 {
		t.Fatal("expected 10")
	}
}

func TestSequentialBulkWriters(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	for i := 0; i < 5; i++ {
		bw, _ := db.BulkWriter()
		bw.CreateNode([]string{"B"}, map[string]string{"i": fmt.Sprintf("%d", i)})
		bw.Commit()
	}

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (b:B) RETURN count(b) AS c")
	tx.Commit()
	if r.Rows[0]["c"].(int64) != 5 {
		t.Fatal("expected 5")
	}
}

// --- TestScale --------------------------------------------------------------

func TestBulkInsert10000(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	for i := 0; i < 10000; i++ {
		bw.CreateNode([]string{"N"}, map[string]string{"i": fmt.Sprintf("%d", i)})
	}
	bw.Commit()

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (n:N) RETURN count(n) AS c")
	tx.Commit()
	if r.Rows[0]["c"].(int64) != 10000 {
		t.Fatal("expected 10000")
	}
}

func TestBulkInsertWithEdges(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	ids := make([]int64, 100)
	for i := 0; i < 100; i++ {
		id, _ := bw.CreateNode([]string{"N"}, nil)
		ids[i] = id
	}
	for i := 0; i < 99; i++ {
		bw.CreateEdge(ids[i], ids[i+1], "NEXT", nil)
	}
	bw.Commit()

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH ()-[r:NEXT]->() RETURN count(r) AS c")
	tx.Commit()
	if r.Rows[0]["c"].(int64) != 99 {
		t.Fatal("expected 99 edges")
	}
}

func TestLargeQuery(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	for i := 0; i < 1000; i++ {
		bw.CreateNode([]string{"Item"}, map[string]string{"val": fmt.Sprintf("%d", i)})
	}
	bw.Commit()

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (n:Item) RETURN n.val ORDER BY n.val")
	tx.Commit()
	if r.RowCount != 1000 {
		t.Fatalf("expected 1000 rows, got %d", r.RowCount)
	}
}

func TestMultipleColumns(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	r, _ := tx.Execute("RETURN 1 AS a, 2 AS b, 3 AS c")
	tx.Commit()

	if len(r.Columns) != 3 || r.Columns[0] != "a" || r.Columns[1] != "b" || r.Columns[2] != "c" {
		t.Fatalf("expected columns [a,b,c], got %v", r.Columns)
	}
	if r.Rows[0]["a"].(int64) != 1 || r.Rows[0]["b"].(int64) != 2 || r.Rows[0]["c"].(int64) != 3 {
		t.Fatal("expected 1,2,3")
	}
}

func TestEmptyResult(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	r, _ := tx.Execute("MATCH (n:NoExist) RETURN n")
	tx.Commit()
	if r.RowCount != 0 {
		t.Fatal("expected 0 rows")
	}
}

// --- Handle lifetimes, read transactions, busy timeout ---------------------

func countNodes(t *testing.T, db *Database, label string) int64 {
	t.Helper()
	tx, err := db.BeginRead()
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Commit()
	r, err := tx.Execute(fmt.Sprintf("MATCH (n:%s) RETURN count(n) AS c", label))
	if err != nil {
		t.Fatal(err)
	}
	return r.Rows[0]["c"].(int64)
}

func TestCloseRollsBackOpenTransactions(t *testing.T) {
	db, path := tempDB(t)
	tx, _ := db.Begin()
	if _, err := tx.Execute("CREATE (:Lost)"); err != nil {
		t.Fatal(err)
	}
	reader, _ := db.BeginRead()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	_, err := tx.Execute("RETURN 1")
	if errCode(err) != ErrTransaction || !strings.Contains(err.Error(), "closed") {
		t.Fatalf("expected a closed-database error, got %v", err)
	}
	if err := tx.Commit(); errCode(err) != ErrTransaction {
		t.Fatalf("commit after close: %v", err)
	}
	if err := reader.Abort(); err != nil {
		t.Fatal(err)
	}

	db2, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer db2.Close()
	if n := countNodes(t, db2, "Lost"); n != 0 {
		t.Fatalf("expected the open transaction rolled back, found %d", n)
	}
}

func TestReadTransactionSeesItsSnapshot(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	writer, _ := db.Begin()
	writer.Execute("CREATE (:N)")
	reader, err := db.BeginRead()
	if err != nil {
		t.Fatal(err)
	}
	if !reader.ReadOnly {
		t.Fatal("expected a read-only transaction")
	}
	if _, err := reader.Execute("CREATE (:M)"); err == nil {
		t.Fatal("expected a write in a read transaction to fail")
	}
	if err := writer.Commit(); err != nil {
		t.Fatal(err)
	}
	r, _ := reader.Execute("MATCH (n:N) RETURN count(n) AS c")
	if r.Rows[0]["c"].(int64) != 0 {
		t.Fatal("a snapshot must not see a later commit")
	}
	reader.Commit()
	if n := countNodes(t, db, "N"); n != 1 {
		t.Fatalf("expected 1, got %d", n)
	}
}

func TestBusyTimeout(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	if err := db.SetBusyTimeout(20 * time.Millisecond); err != nil {
		t.Fatal(err)
	}

	writer, _ := db.Begin()
	if _, err := db.Begin(); errCode(err) != ErrBusy {
		t.Fatalf("second writer: expected ErrBusy, got %v", err)
	}
	if _, err := db.BulkWriter(); errCode(err) != ErrBusy {
		t.Fatalf("bulk writer: expected ErrBusy, got %v", err)
	}
	if err := db.Flush(); errCode(err) != ErrBusy {
		t.Fatalf("flush: expected ErrBusy, got %v", err)
	}
	writer.Abort()
	next, err := db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	next.Commit()
}

func TestTransactionFinishesOnAnotherGoroutine(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	tx, _ := db.Begin()
	tx.Execute("CREATE (:N)")
	done := make(chan error)
	go func() { done <- tx.Commit() }()
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	if n := countNodes(t, db, "N"); n != 1 {
		t.Fatalf("expected 1, got %d", n)
	}
}

//go:noinline
func abandonWriter(db *Database) {
	tx, _ := db.Begin()
	tx.Execute("CREATE (:Abandoned)")
}

func TestAbandonedTransactionIsRolledBack(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	db.SetBusyTimeout(10 * time.Millisecond)
	abandonWriter(db)

	// the finalizer, on its own goroutine, rolls the writer back and releases the lock
	deadline := time.Now().Add(10 * time.Second)
	for {
		runtime.GC()
		tx, err := db.Begin()
		if err == nil {
			r, _ := tx.Execute("MATCH (n:Abandoned) RETURN count(n) AS c")
			tx.Abort()
			if r.Rows[0]["c"].(int64) != 0 {
				t.Fatal("expected the abandoned writes rolled back")
			}
			return
		}
		if errCode(err) != ErrBusy || time.Now().After(deadline) {
			t.Fatalf("writer lock never released: %v", err)
		}
	}
}

func TestCloseWhileGoroutinesWork(t *testing.T) {
	db, _ := tempDB(t)
	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			for i := 0; i < 200; i++ {
				var tx *Transaction
				var err error
				if g%2 == 0 {
					tx, err = db.Begin()
				} else {
					tx, err = db.BeginRead()
				}
				if err != nil {
					if errCode(err) == ErrOK {
						t.Errorf("unexpected error type: %v", err)
					}
					continue
				}
				if g%2 == 0 {
					tx.Execute("CREATE (:N)")
				} else {
					tx.Execute("MATCH (n) RETURN count(n)")
				}
				tx.Commit()
			}
		}(g)
	}
	time.Sleep(20 * time.Millisecond)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	wg.Wait()
	if _, err := db.Begin(); err == nil {
		t.Fatal("expected a closed database to refuse new transactions")
	}
}

func TestBulkLoadInvisibleUntilCommit(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()

	bw, _ := db.BulkWriter()
	for i := 0; i < 100; i++ {
		if _, err := bw.CreateNode([]string{"Item"}, map[string]string{"i": fmt.Sprint(i)}); err != nil {
			t.Fatal(err)
		}
	}
	if n := countNodes(t, db, "Item"); n != 0 {
		t.Fatalf("expected the load invisible before commit, got %d", n)
	}
	if err := bw.Commit(); err != nil {
		t.Fatal(err)
	}
	if n := countNodes(t, db, "Item"); n != 100 {
		t.Fatalf("expected 100, got %d", n)
	}
}

// --- Stopping statements ----------------------------------------------------

// a three-way cross product over 2,000 nodes: minutes of work unless stopped
const longQuery = "MATCH (a:Big), (b:Big), (c:Big) RETURN count(*)"

func seedBig(t *testing.T, db *Database) {
	t.Helper()
	tx, err := db.Begin()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Execute("UNWIND range(1, 2000) AS i CREATE (:Big {i: i})"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
}

func TestContextStopsQuery(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	seedBig(t, db)
	tx, _ := db.BeginRead()
	defer tx.Abort()

	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	start := time.Now()
	if _, err := tx.ExecuteContext(ctx, longQuery); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected context.DeadlineExceeded, got %v", err)
	}
	if d := time.Since(start); d > 20*time.Second {
		t.Fatalf("took %v", d)
	}
	// the transaction carries on: a read changed nothing
	r, err := tx.ExecuteContext(context.Background(), "RETURN 1 AS n")
	if err != nil || r.Rows[0]["n"].(int64) != 1 {
		t.Fatalf("expected the transaction usable, got %v", err)
	}
	// a done context never starts the statement
	if _, err := tx.ExecuteContext(ctx, "RETURN 1"); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected context.DeadlineExceeded, got %v", err)
	}
}

func TestCancelFromAnotherGoroutine(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	seedBig(t, db)
	tx, _ := db.BeginRead()
	defer tx.Abort()

	go func() {
		time.Sleep(200 * time.Millisecond)
		tx.Cancel()
	}()
	if _, err := tx.Execute(longQuery); errCode(err) != ErrInterrupted {
		t.Fatalf("expected ErrInterrupted, got %v", err)
	}
	// nothing runs: a cancel does nothing
	tx.Cancel()
	if _, err := tx.Execute("RETURN 1"); err != nil {
		t.Fatal(err)
	}
}

func TestStatementTimeout(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	seedBig(t, db)
	if err := db.SetStatementTimeout(200 * time.Millisecond); err != nil {
		t.Fatal(err)
	}
	tx, _ := db.BeginRead()
	defer tx.Abort()
	_, err := tx.Execute(longQuery)
	if errCode(err) != ErrInterrupted || !strings.Contains(err.Error(), "timed out") {
		t.Fatalf("expected a timeout, got %v", err)
	}
}

func TestTerminateTransactions(t *testing.T) {
	// a name of its own: SHOW TRANSACTIONS lists a database by its file name
	name := fmt.Sprintf("terminate_%d", time.Now().UnixNano())
	db, err := Open(filepath.Join(t.TempDir(), name+".tvdb"))
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	victim, _ := db.Begin()
	if _, err := victim.Execute("CREATE (:Pending)"); err != nil {
		t.Fatal(err)
	}
	admin, _ := db.BeginRead()
	r, err := admin.Execute("SHOW TRANSACTIONS YIELD transactionId AS id, database, protocol, currentQuery " +
		"WHERE database = '" + name + "' AND protocol = 'embedded' AND currentQuery = '' " +
		"TERMINATE TRANSACTIONS id YIELD message RETURN message")
	admin.Commit()
	if err != nil {
		t.Fatal(err)
	}
	if len(r.Rows) != 1 || r.Rows[0]["message"] != "Transaction terminated." {
		t.Fatalf("expected one terminated transaction, got %v", r.Rows)
	}
	if _, err := victim.Execute("RETURN 1"); errCode(err) != ErrInterrupted {
		t.Fatalf("expected ErrInterrupted, got %v", err)
	}
	if err := victim.Commit(); err == nil {
		t.Fatal("expected a terminated transaction to refuse commit")
	}
	if n := countNodes(t, db, "Pending"); n != 0 {
		t.Fatalf("expected the terminated writes rolled back, got %d", n)
	}
}

func TestFailedStatementFailsTransaction(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	tx, _ := db.Begin()

	// fails before writing: the transaction carries on
	if _, err := tx.Execute("CREATE (:Kept"); err == nil {
		t.Fatal("expected a syntax error")
	}
	if _, err := tx.Execute("CREATE (:Kept)"); err != nil {
		t.Fatal(err)
	}
	// writes, then fails
	if _, err := tx.Execute("CREATE (:Partial) WITH 1 AS one RETURN 10 / 0"); err == nil {
		t.Fatal("expected division by zero")
	}
	if _, err := tx.Execute("RETURN 1"); errCode(err) != ErrTransaction {
		t.Fatalf("expected ErrTransaction, got %v", err)
	}
	if err := tx.Commit(); errCode(err) != ErrTransaction {
		t.Fatalf("expected commit to fail with ErrTransaction, got %v", err)
	}
	if n := countNodes(t, db, "Kept"); n != 0 {
		t.Fatalf("expected nothing committed, got %d", n)
	}
}

// --- Auto-commit ------------------------------------------------------------

func autoCount(t *testing.T, db *Database, label string) int64 {
	t.Helper()
	r, err := db.Execute(fmt.Sprintf("MATCH (n:%s) RETURN count(n) AS c", label))
	if err != nil {
		t.Fatal(err)
	}
	return r.Rows[0]["c"].(int64)
}

func TestAutoCommit(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	if _, err := db.Execute("CREATE (:A {v: 1}), (:A {v: 2})"); err != nil {
		t.Fatal(err)
	}
	if n := autoCount(t, db, "A"); n != 2 {
		t.Fatalf("expected 2, got %d", n)
	}
	if _, err := db.ExecuteGql("INSERT (:G {v: 1})"); err != nil {
		t.Fatal(err)
	}
	if r, err := db.ExecuteGql("MATCH (n:G) RETURN n.v AS v"); err != nil || r.Rows[0]["v"].(int64) != 1 {
		t.Fatalf("expected the GQL insert, got %v %v", r, err)
	}
	// a failed write leaves nothing behind
	if _, err := db.Execute("CREATE (:B) WITH 1 AS one RETURN 10 / 0"); err == nil {
		t.Fatal("expected division by zero")
	}
	if n := autoCount(t, db, "B"); n != 0 {
		t.Fatalf("expected the failed write rolled back, got %d", n)
	}
	// a procedure that writes runs as a write
	if _, err := db.Execute("CALL apoc.create.node(['P'], {v: 1}) YIELD node RETURN node"); err != nil {
		t.Fatal(err)
	}
	if n := autoCount(t, db, "P"); n != 1 {
		t.Fatalf("expected 1, got %d", n)
	}
}

func TestErrorsCarryTheirGqlStatus(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	for query, want := range map[string]string{"RETURN 1 / 0": "22012", "RETURN x": "42N62"} {
		_, err := db.Execute(query)
		te, ok := err.(*TraverseError)
		if !ok || te.GqlStatus != want {
			t.Fatalf("%s: expected GQLSTATUS %s, got %v", query, want, err)
		}
	}
}

func TestAutoCommitCallInTransactions(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	if _, err := db.Execute("UNWIND [1, 0, 2] AS i CALL { WITH i CREATE (:C {v: 10 / i}) } " +
		"IN TRANSACTIONS OF 1 ROW ON ERROR CONTINUE"); err != nil {
		t.Fatal(err)
	}
	if n := autoCount(t, db, "C"); n != 2 {
		t.Fatalf("expected the two good batches committed, got %d", n)
	}
}

func TestAutoCommitReadsNeverWait(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	db.SetBusyTimeout(50 * time.Millisecond)
	writer, _ := db.Begin()
	defer writer.Abort()
	if _, err := db.Execute("RETURN 1"); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Execute("CREATE (:D)"); errCode(err) != ErrBusy {
		t.Fatalf("expected ErrBusy, got %v", err)
	}
}

func TestAutoCommitContext(t *testing.T) {
	db, _ := tempDB(t)
	defer db.Close()
	seedBig(t, db)
	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	if _, err := db.ExecuteContext(ctx, longQuery); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected context.DeadlineExceeded, got %v", err)
	}
	// a write waiting for the writer lock stops too
	db.SetBusyTimeout(-1)
	writer, _ := db.Begin()
	defer writer.Abort()
	ctx2, cancel2 := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel2()
	if _, err := db.ExecuteContext(ctx2, "CREATE (:W)"); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected context.DeadlineExceeded, got %v", err)
	}
}
