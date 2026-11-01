// Package traverse provides Go bindings for the Traverse embedded graph database.
//
// It loads the traverse_ffi native library at run time (LoadLibrary on Windows, dlopen on Linux
// and macOS), so no CGo is needed, and gives an idiomatic Go API for opening databases, running
// Cypher and GQL queries, and bulk loading data. The library is found through TRAVERSE_FFI_LIB,
// next to the executable, or on the system's library search path.
package traverse

import (
	"context"
	"fmt"
	"runtime"
	"sync"
	"time"
	"unsafe"
)

// --- Native functions --------------------------------------------------------

var (
	// Database
	procOpen                nativeFn
	procFlush               nativeFn
	procSetBusyTimeout      nativeFn
	procSetStatementTimeout nativeFn
	procClose               nativeFn

	// Transaction
	procBegin         nativeFn
	procBeginRead     nativeFn
	procCommit        nativeFn
	procAbort         nativeFn
	procCanceller     nativeFn
	procCancel        nativeFn
	procCancellerFree nativeFn
	procCancellerNew  nativeFn

	// Query
	procQuery      nativeFn
	procQueryGql   nativeFn
	procExecute    nativeFn
	procExecuteGql nativeFn

	// Result
	procResultColumnCount nativeFn
	procResultColumnName  nativeFn
	procResultRowCount    nativeFn
	procResultNext        nativeFn
	procResultFree        nativeFn
	procResultGetString   nativeFn
	procResultGetInt      nativeFn
	procResultGetBool     nativeFn
	procResultIsNull      nativeFn
	procResultGetType     nativeFn

	// Node accessors
	procResultGetNodeID         nativeFn
	procResultGetNodeLabelCount nativeFn
	procResultGetNodeLabel      nativeFn
	procResultGetNodePropCount  nativeFn
	procResultGetNodePropKey    nativeFn
	procResultGetNodePropValue  nativeFn

	// Edge accessors
	procResultGetEdgeID       nativeFn
	procResultGetEdgeTypeName nativeFn
	procResultGetEdgeSource   nativeFn
	procResultGetEdgeTarget   nativeFn

	// Path accessors
	procResultGetPathLength nativeFn
	procResultGetPathNodeID nativeFn
	procResultGetPathEdgeID nativeFn

	// Bulk writer
	procBulkBegin         nativeFn
	procBulkCreateNode    nativeFn
	procBulkCreateEdge    nativeFn
	procBulkCreateIndex   nativeFn
	procBulkCreateEdgeIdx nativeFn
	procBulkCommit        nativeFn
	procBulkAbort         nativeFn

	// Error
	procLastError          nativeFn
	procLastErrorCode      nativeFn
	procLastErrorGqlStatus nativeFn
)

// exports binds each function above to its symbol in the native library.
var exports = []struct {
	fn   *nativeFn
	name string
}{
	{&procOpen, "traverse_open"},
	{&procFlush, "traverse_flush"},
	{&procSetBusyTimeout, "traverse_set_busy_timeout"},
	{&procSetStatementTimeout, "traverse_set_statement_timeout"},
	{&procClose, "traverse_close"},
	{&procBegin, "traverse_begin"},
	{&procBeginRead, "traverse_begin_read"},
	{&procCommit, "traverse_commit"},
	{&procAbort, "traverse_abort"},
	{&procCanceller, "traverse_canceller"},
	{&procCancel, "traverse_cancel"},
	{&procCancellerFree, "traverse_canceller_free"},
	{&procCancellerNew, "traverse_canceller_new"},
	{&procQuery, "traverse_query"},
	{&procQueryGql, "traverse_query_gql"},
	{&procExecute, "traverse_execute"},
	{&procExecuteGql, "traverse_execute_gql"},
	{&procResultColumnCount, "traverse_result_column_count"},
	{&procResultColumnName, "traverse_result_column_name"},
	{&procResultRowCount, "traverse_result_row_count"},
	{&procResultNext, "traverse_result_next"},
	{&procResultFree, "traverse_result_free"},
	{&procResultGetString, "traverse_result_get_string"},
	{&procResultGetInt, "traverse_result_get_int"},
	{&procResultGetBool, "traverse_result_get_bool"},
	{&procResultIsNull, "traverse_result_is_null"},
	{&procResultGetType, "traverse_result_get_type"},
	{&procResultGetNodeID, "traverse_result_get_node_id"},
	{&procResultGetNodeLabelCount, "traverse_result_get_node_label_count"},
	{&procResultGetNodeLabel, "traverse_result_get_node_label"},
	{&procResultGetNodePropCount, "traverse_result_get_node_prop_count"},
	{&procResultGetNodePropKey, "traverse_result_get_node_prop_key"},
	{&procResultGetNodePropValue, "traverse_result_get_node_prop_value"},
	{&procResultGetEdgeID, "traverse_result_get_edge_id"},
	{&procResultGetEdgeTypeName, "traverse_result_get_edge_type_name"},
	{&procResultGetEdgeSource, "traverse_result_get_edge_source"},
	{&procResultGetEdgeTarget, "traverse_result_get_edge_target"},
	{&procResultGetPathLength, "traverse_result_get_path_length"},
	{&procResultGetPathNodeID, "traverse_result_get_path_node_id"},
	{&procResultGetPathEdgeID, "traverse_result_get_path_edge_id"},
	{&procBulkBegin, "traverse_bulk_begin"},
	{&procBulkCreateNode, "traverse_bulk_create_node"},
	{&procBulkCreateEdge, "traverse_bulk_create_edge"},
	{&procBulkCreateIndex, "traverse_bulk_create_index"},
	{&procBulkCreateEdgeIdx, "traverse_bulk_create_edge_index"},
	{&procBulkCommit, "traverse_bulk_commit"},
	{&procBulkAbort, "traverse_bulk_abort"},
	{&procLastError, "traverse_last_error"},
	{&procLastErrorCode, "traverse_last_error_code"},
	{&procLastErrorGqlStatus, "traverse_last_error_gql_status"},
}

// --- Helpers ----------------------------------------------------------------

// cStringPinned allocates a NUL-terminated byte slice and returns both
// the pointer (as uintptr) and the backing slice (to keep it alive).
func cStringPinned(s string) (uintptr, []byte) {
	b := append([]byte(s), 0)
	return uintptr(unsafe.Pointer(&b[0])), b
}

func goString(ptr uintptr) string {
	if ptr == 0 {
		return ""
	}
	// ptr addresses memory the native library owns, never the Go heap, so
	// the uintptr-to-pointer rule vet enforces (which guards against the GC
	// moving a Go object) does not apply. Reinterpret the integer's bits
	// through a *unsafe.Pointer rather than converting the variable, which
	// is the form `go vet` accepts for foreign memory.
	p := *(*unsafe.Pointer)(unsafe.Pointer(&ptr))
	// Find NUL terminator
	n := 0
	for *(*byte)(unsafe.Add(p, n)) != 0 {
		n++
	}
	return string(unsafe.Slice((*byte)(p), n))
}

// --- Error types ------------------------------------------------------------

// ErrorCode represents a Traverse FFI error code.
type ErrorCode int32

const (
	ErrOK                ErrorCode = 0
	ErrIO                ErrorCode = 1
	ErrCorrupt           ErrorCode = 2
	ErrLocked            ErrorCode = 3
	ErrSyntax            ErrorCode = 4
	ErrType              ErrorCode = 5
	ErrSemantic          ErrorCode = 6
	ErrGraph             ErrorCode = 7
	ErrTransaction       ErrorCode = 8
	ErrInvalidArg        ErrorCode = 9
	ErrInternal          ErrorCode = 10
	ErrResourceExhausted ErrorCode = 11
	// ErrBusy: another write transaction held the writer lock for the whole busy timeout. Retry.
	ErrBusy ErrorCode = 12
	// ErrInterrupted: the statement was stopped before it finished: it ran past the statement
	// timeout (SetStatementTimeout), it was cancelled (Transaction.Cancel), or its transaction was
	// terminated (TERMINATE TRANSACTIONS).
	ErrInterrupted ErrorCode = 13
)

// TraverseError is the error type returned by Traverse operations.
type TraverseError struct {
	Code    ErrorCode
	Message string
	// GqlStatus is the GQLSTATUS (ISO/IEC 39075): five characters naming the condition, such as
	// "22012" (division by zero) or "42N62" (variable not defined). Empty when the error has none.
	GqlStatus string
}

func (e *TraverseError) Error() string {
	return fmt.Sprintf("traverse error %d: %s", e.Code, e.Message)
}

func lastError() error {
	r := procLastErrorCode.Call()
	code := ErrorCode(int32(r))
	if code == ErrOK {
		return nil
	}
	r2 := procLastError.Call()
	msg := "unknown error"
	if r2 != 0 {
		msg = goString(r2)
	}
	status := ""
	if r3 := procLastErrorGqlStatus.Call(); r3 != 0 {
		status = goString(r3)
	}
	return &TraverseError{Code: code, Message: msg, GqlStatus: status}
}

func checkRC(r uintptr) error {
	if int32(r) != 0 {
		return lastError()
	}
	return nil
}

// --- Database ---------------------------------------------------------------

// errClosed is returned by calls on a closed database.
var errClosed = &TraverseError{Code: ErrInvalidArg, Message: "database is closed"}

// Database represents an open Traverse database. Safe for concurrent use.
type Database struct {
	mu       sync.Mutex
	ptr      uintptr
	inflight int  // native calls running on ptr
	closing  bool // Close ran while calls were in flight: the last one out closes ptr
}

// Open opens or creates a database at the given path. The first call loads the native library and
// fails if it can't be found or doesn't match this package.
func Open(path string) (*Database, error) {
	if err := loadLibrary(); err != nil {
		return nil, err
	}
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	cPath, pin := cStringPinned(path)
	r := procOpen.Call(cPath)
	runtime.KeepAlive(pin)
	if r == 0 {
		return nil, lastError()
	}
	db := &Database{ptr: r}
	runtime.SetFinalizer(db, func(d *Database) { d.Close() })
	return db, nil
}

// call runs fn with the native handle, which stays valid until fn returns, pinned to one OS
// thread: the library reports errors per thread, so a call and the error read after it must
// share one.
func (db *Database) call(fn func(ptr uintptr) error) error {
	db.mu.Lock()
	if db.ptr == 0 || db.closing {
		db.mu.Unlock()
		return errClosed
	}
	db.inflight++
	ptr := db.ptr
	db.mu.Unlock()

	defer func() {
		db.mu.Lock()
		db.inflight--
		closeNow := db.closing && db.inflight == 0
		if closeNow {
			db.ptr = 0
		}
		db.mu.Unlock()
		if closeNow {
			procClose.Call(ptr)
		}
	}()
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	return fn(ptr)
}

// Execute runs an openCypher query as a transaction of its own (auto-commit): a read on a
// snapshot, so it never waits; a write in a write transaction that commits when the query succeeds
// and rolls back when it fails (waiting at most the busy timeout for the writer lock), in which
// CALL { ... } IN TRANSACTIONS commits each batch.
func (db *Database) Execute(query string) (*QueryResult, error) {
	return db.execute(context.Background(), query, procExecute)
}

// ExecuteGql is Execute for an ISO/IEC 39075 GQL query.
func (db *Database) ExecuteGql(query string) (*QueryResult, error) {
	return db.execute(context.Background(), query, procExecuteGql)
}

// ExecuteContext is Execute, stopped when ctx is done (waiting for the writer lock included): it
// then returns ctx.Err().
func (db *Database) ExecuteContext(ctx context.Context, query string) (*QueryResult, error) {
	return db.execute(ctx, query, procExecute)
}

// ExecuteGqlContext is ExecuteGql, stopped when ctx is done: it then returns ctx.Err().
func (db *Database) ExecuteGqlContext(ctx context.Context, query string) (*QueryResult, error) {
	return db.execute(ctx, query, procExecuteGql)
}

func (db *Database) execute(ctx context.Context, query string, proc nativeFn) (*QueryResult, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	var canceller uintptr
	if ctx.Done() != nil {
		canceller = procCancellerNew.Call()
		defer procCancellerFree.Call(canceller)
		cancelled := make(chan struct{})
		stop := context.AfterFunc(ctx, func() {
			procCancel.Call(canceller)
			close(cancelled)
		})
		// runs before the free above
		defer func() {
			if !stop() {
				<-cancelled
			}
		}()
	}
	var result *QueryResult
	err := db.call(func(ptr uintptr) error {
		cQuery, pin := cStringPinned(query)
		r := proc.Call(ptr, cQuery, canceller)
		runtime.KeepAlive(pin)
		if r == 0 {
			return lastError()
		}
		defer procResultFree.Call(r)
		var err error
		result, err = readResult(r)
		return err
	})
	if errCode(err) == ErrInterrupted && ctx.Err() != nil {
		return nil, ctx.Err()
	}
	return result, err
}

// Flush writes dirty pages to disk. Waits for an active write transaction at most the busy
// timeout.
func (db *Database) Flush() error {
	return db.call(func(ptr uintptr) error {
		r := procFlush.Call(ptr)
		return checkRC(r)
	})
}

// SetBusyTimeout sets how long Begin, BulkWriter and Flush wait for an active write transaction
// before failing with ErrBusy; a negative timeout waits as long as it takes. The default is 5s.
func (db *Database) SetBusyTimeout(timeout time.Duration) error {
	ms := int64(-1)
	if timeout >= 0 {
		ms = timeout.Milliseconds()
	}
	return db.call(func(ptr uintptr) error {
		r := procSetBusyTimeout.Call(ptr, uintptr(ms))
		return checkRC(r)
	})
}

// SetStatementTimeout sets how long a statement may run before it fails with ErrInterrupted;
// zero or negative means no limit (the default). Applies to transactions begun afterwards.
func (db *Database) SetStatementTimeout(timeout time.Duration) error {
	ms := int64(0)
	if timeout > 0 {
		ms = max(timeout.Milliseconds(), 1)
	}
	return db.call(func(ptr uintptr) error {
		r := procSetStatementTimeout.Call(ptr, uintptr(ms))
		return checkRC(r)
	})
}

// Close rolls back the transactions and bulk writers still open on the database (their later
// calls fail), saves it, and releases the file. If other goroutines are inside calls on it, the
// last of them to return closes it instead. Safe to call more than once.
func (db *Database) Close() error {
	db.mu.Lock()
	if db.ptr == 0 || db.closing {
		db.mu.Unlock()
		return nil
	}
	db.closing = true
	if db.inflight > 0 {
		db.mu.Unlock()
		return nil
	}
	ptr := db.ptr
	db.ptr = 0
	db.mu.Unlock()
	runtime.SetFinalizer(db, nil)

	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	r := procClose.Call(ptr)
	return checkRC(r)
}

// --- Query types ------------------------------------------------------------

// QueryResult holds the result of a Cypher query.
type QueryResult struct {
	Columns  []string
	Rows     []map[string]interface{}
	RowCount int
}

// Node represents a graph node.
type Node struct {
	ID         int64
	Labels     []string
	Properties map[string]string
}

// Edge represents a graph relationship.
type Edge struct {
	ID         int64
	Type       string
	Source     int64
	Target     int64
	Properties map[string]string
}

// Path represents a graph path.
type Path struct {
	NodeIDs []int64
	EdgeIDs []int64
}

// --- Transaction ------------------------------------------------------------

// Transaction is a write transaction (Database.Begin) or a snapshot reader (Database.BeginRead).
// Safe for concurrent use: calls take turns, except Cancel, which stops the running one. A
// transaction dropped without Commit or Abort is rolled back once the garbage collector finds it;
// closing its database rolls it back at once. A statement that fails after changing data fails
// the transaction: later statements and Commit return ErrTransaction, and only Abort ends it.
type Transaction struct {
	mu       sync.Mutex
	ptr      uintptr
	db       *Database // keeps the database open (its finalizer would close it) while in use
	ReadOnly bool

	cancelMu  sync.Mutex // guards canceller apart from mu, which a running statement holds
	canceller uintptr
}

func (db *Database) begin(proc nativeFn, readOnly bool) (*Transaction, error) {
	var tx *Transaction
	err := db.call(func(ptr uintptr) error {
		r := proc.Call(ptr)
		if r == 0 {
			return lastError()
		}
		c := procCanceller.Call(r)
		if c == 0 {
			err := lastError()
			procAbort.Call(r)
			return err
		}
		tx = &Transaction{ptr: r, db: db, ReadOnly: readOnly, canceller: c}
		runtime.SetFinalizer(tx, func(t *Transaction) { t.Abort() })
		return nil
	})
	return tx, err
}

// Begin starts a write transaction. One is active at a time: this waits for the active one at
// most the busy timeout (see SetBusyTimeout), then fails with ErrBusy.
func (db *Database) Begin() (*Transaction, error) {
	return db.begin(procBegin, false)
}

// BeginRead starts a read transaction: a snapshot of the database as of now. It never waits, runs
// alongside the writer and other readers, and refuses writes. End it with Commit or Abort.
func (db *Database) BeginRead() (*Transaction, error) {
	return db.begin(procBeginRead, true)
}

// Execute runs an openCypher query within the transaction.
func (tx *Transaction) Execute(query string) (*QueryResult, error) {
	return tx.execute(query, procQuery)
}

// ExecuteGql runs an ISO/IEC 39075 GQL query within the transaction.
// The GQL front-end lowers into the same engine Execute uses -
// identical planning and execution, only the surface language differs.
func (tx *Transaction) ExecuteGql(query string) (*QueryResult, error) {
	return tx.execute(query, procQueryGql)
}

// ExecuteContext runs an openCypher query, stopped when ctx is done: it then returns ctx.Err().
func (tx *Transaction) ExecuteContext(ctx context.Context, query string) (*QueryResult, error) {
	return tx.executeContext(ctx, query, procQuery)
}

// ExecuteGqlContext runs a GQL query, stopped when ctx is done: it then returns ctx.Err().
func (tx *Transaction) ExecuteGqlContext(ctx context.Context, query string) (*QueryResult, error) {
	return tx.executeContext(ctx, query, procQueryGql)
}

func (tx *Transaction) executeContext(ctx context.Context, query string, proc nativeFn) (*QueryResult, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	cancelled := make(chan struct{})
	stop := context.AfterFunc(ctx, func() {
		tx.Cancel()
		close(cancelled)
	})
	result, err := tx.execute(query, proc)
	if !stop() {
		// a cancel already under way must land before the next statement starts
		<-cancelled
	}
	if errCode(err) == ErrInterrupted && ctx.Err() != nil {
		return nil, ctx.Err()
	}
	return result, err
}

// Cancel stops the statement the transaction is running, from any goroutine: it returns
// ErrInterrupted. A statement that had already changed data fails the transaction. Does nothing
// when no statement runs or the transaction ended.
func (tx *Transaction) Cancel() {
	tx.cancelMu.Lock()
	defer tx.cancelMu.Unlock()
	if tx.canceller != 0 {
		procCancel.Call(tx.canceller)
	}
}

func errCode(err error) ErrorCode {
	if te, ok := err.(*TraverseError); ok {
		return te.Code
	}
	return ErrOK
}

func (tx *Transaction) execute(query string, proc nativeFn) (*QueryResult, error) {
	tx.mu.Lock()
	defer tx.mu.Unlock()
	if tx.ptr == 0 {
		return nil, &TraverseError{Code: ErrTransaction, Message: "transaction already finished"}
	}
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	cQuery, pin := cStringPinned(query)
	r := proc.Call(tx.ptr, cQuery)
	runtime.KeepAlive(pin)
	if r == 0 {
		return nil, lastError()
	}
	defer procResultFree.Call(r)
	return readResult(r)
}

// finish ends the transaction with proc (commit or abort), which frees the native handle.
func (tx *Transaction) finish(proc nativeFn) error {
	tx.mu.Lock()
	defer tx.mu.Unlock()
	if tx.ptr == 0 {
		return &TraverseError{Code: ErrTransaction, Message: "transaction already finished"}
	}
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	r := proc.Call(tx.ptr)
	tx.ptr = 0
	err := checkRC(r)
	tx.cancelMu.Lock()
	procCancellerFree.Call(tx.canceller)
	tx.canceller = 0
	tx.cancelMu.Unlock()
	runtime.SetFinalizer(tx, nil)
	return err
}

// Commit commits the transaction (ends a read transaction).
func (tx *Transaction) Commit() error {
	return tx.finish(procCommit)
}

// Abort aborts the transaction, discarding all changes.
func (tx *Transaction) Abort() error {
	return tx.finish(procAbort)
}

// --- Result reading ---------------------------------------------------------

func readResult(resultPtr uintptr) (*QueryResult, error) {
	r := procResultColumnCount.Call(resultPtr)
	colCount := int(int32(r))

	columns := make([]string, colCount)
	for i := 0; i < colCount; i++ {
		r := procResultColumnName.Call(resultPtr, uintptr(i))
		if r != 0 {
			columns[i] = goString(r)
		}
	}

	r = procResultRowCount.Call(resultPtr)
	rowCount := int(int64(r))
	rows := make([]map[string]interface{}, 0, rowCount)

	for {
		r := procResultNext.Call(resultPtr)
		if r == 0 {
			break
		}
		row := make(map[string]interface{}, colCount)
		for i := 0; i < colCount; i++ {
			col := uintptr(i)
			r := procResultIsNull.Call(resultPtr, col)
			if r != 0 {
				row[columns[i]] = nil
				continue
			}
			r = procResultGetType.Call(resultPtr, col)
			typeName := ""
			if r != 0 {
				typeName = goString(r)
			}
			switch typeName {
			case "INTEGER":
				r = procResultGetInt.Call(resultPtr, col)
				row[columns[i]] = int64(r)
			case "FLOAT":
				// a float result comes back in a register Call doesn't read,
				// so floats are read as strings and parsed
				r = procResultGetString.Call(resultPtr, col)
				if r != 0 {
					s := goString(r)
					var f float64
					fmt.Sscanf(s, "%g", &f)
					row[columns[i]] = f
				}
			case "BOOLEAN":
				r = procResultGetBool.Call(resultPtr, col)
				row[columns[i]] = r != 0
			case "STRING":
				r = procResultGetString.Call(resultPtr, col)
				if r != 0 {
					row[columns[i]] = goString(r)
				} else {
					row[columns[i]] = ""
				}
			case "NODE":
				row[columns[i]] = readNode(resultPtr, col)
			case "RELATIONSHIP":
				row[columns[i]] = readEdge(resultPtr, col)
			case "PATH":
				row[columns[i]] = readPath(resultPtr, col)
			default:
				r = procResultGetString.Call(resultPtr, col)
				if r != 0 {
					row[columns[i]] = goString(r)
				}
			}
		}
		rows = append(rows, row)
	}

	return &QueryResult{
		Columns:  columns,
		Rows:     rows,
		RowCount: len(rows),
	}, nil
}

func readNode(resultPtr, col uintptr) Node {
	r := procResultGetNodeID.Call(resultPtr, col)
	node := Node{ID: int64(r)}

	r = procResultGetNodeLabelCount.Call(resultPtr, col)
	labelCount := int(int32(r))
	node.Labels = make([]string, labelCount)
	for i := 0; i < labelCount; i++ {
		r = procResultGetNodeLabel.Call(resultPtr, col, uintptr(i))
		if r != 0 {
			node.Labels[i] = goString(r)
		}
	}

	r = procResultGetNodePropCount.Call(resultPtr, col)
	propCount := int(int32(r))
	node.Properties = make(map[string]string, propCount)
	for i := 0; i < propCount; i++ {
		kr := procResultGetNodePropKey.Call(resultPtr, col, uintptr(i))
		vr := procResultGetNodePropValue.Call(resultPtr, col, uintptr(i))
		if kr != 0 && vr != 0 {
			node.Properties[goString(kr)] = goString(vr)
		}
	}
	return node
}

func readEdge(resultPtr, col uintptr) Edge {
	r := procResultGetEdgeID.Call(resultPtr, col)
	edge := Edge{ID: int64(r)}

	r = procResultGetEdgeTypeName.Call(resultPtr, col)
	if r != 0 {
		edge.Type = goString(r)
	}

	r = procResultGetEdgeSource.Call(resultPtr, col)
	edge.Source = int64(r)

	r = procResultGetEdgeTarget.Call(resultPtr, col)
	edge.Target = int64(r)

	r = procResultGetNodePropCount.Call(resultPtr, col)
	propCount := int(int32(r))
	edge.Properties = make(map[string]string, propCount)
	for i := 0; i < propCount; i++ {
		kr := procResultGetNodePropKey.Call(resultPtr, col, uintptr(i))
		vr := procResultGetNodePropValue.Call(resultPtr, col, uintptr(i))
		if kr != 0 && vr != 0 {
			edge.Properties[goString(kr)] = goString(vr)
		}
	}
	return edge
}

func readPath(resultPtr, col uintptr) Path {
	r := procResultGetPathLength.Call(resultPtr, col)
	length := int(int32(r))
	path := Path{
		NodeIDs: make([]int64, length+1),
		EdgeIDs: make([]int64, length),
	}
	for i := 0; i <= length; i++ {
		r = procResultGetPathNodeID.Call(resultPtr, col, uintptr(i))
		path.NodeIDs[i] = int64(r)
	}
	for i := 0; i < length; i++ {
		r = procResultGetPathEdgeID.Call(resultPtr, col, uintptr(i))
		path.EdgeIDs[i] = int64(r)
	}
	return path
}

// --- BulkWriter -------------------------------------------------------------

// BulkWriter loads data in bulk: a write transaction that skips the redo log. Commit saves the
// database with the load in it, so the data is written once; readers don't see the load until
// then, and Abort rolls it back. Safe for concurrent use: calls take turns. A writer dropped
// without Commit or Abort is rolled back once the garbage collector finds it.
type BulkWriter struct {
	mu  sync.Mutex
	ptr uintptr
	db  *Database // keeps the database open (its finalizer would close it) while in use
}

// BulkWriter begins a bulk load. Waits for an active write transaction at most the busy timeout,
// then fails with ErrBusy.
func (db *Database) BulkWriter() (*BulkWriter, error) {
	var bw *BulkWriter
	err := db.call(func(ptr uintptr) error {
		r := procBulkBegin.Call(ptr)
		if r == 0 {
			return lastError()
		}
		bw = &BulkWriter{ptr: r, db: db}
		runtime.SetFinalizer(bw, func(w *BulkWriter) { w.Abort() })
		return nil
	})
	return bw, err
}

// call runs fn with the open writer's native handle, pinned to one OS thread (see Database.call).
func (bw *BulkWriter) call(fn func(ptr uintptr) error) error {
	bw.mu.Lock()
	defer bw.mu.Unlock()
	if bw.ptr == 0 {
		return &TraverseError{Code: ErrInvalidArg, Message: "bulk writer already finished"}
	}
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	return fn(bw.ptr)
}

// CreateNode creates a node with the given labels and properties.
func (bw *BulkWriter) CreateNode(labels []string, properties map[string]string) (int64, error) {
	labelPtrs, labelPins := toCStringArray(labels)
	defer runtime.KeepAlive(labelPins)

	keys, values := flattenProps(properties)
	keyPtrs, keyPins := toCStringArray(keys)
	defer runtime.KeepAlive(keyPins)
	valPtrs, valPins := toCStringArray(values)
	defer runtime.KeepAlive(valPins)

	id := int64(-1)
	err := bw.call(func(ptr uintptr) error {
		r := procBulkCreateNode.Call(
			ptr,
			labelPtrs, uintptr(len(labels)),
			keyPtrs, valPtrs, uintptr(len(keys)),
		)
		if int64(r) == -1 {
			return lastError()
		}
		id = int64(r)
		return nil
	})
	return id, err
}

// CreateEdge creates an edge between two nodes.
func (bw *BulkWriter) CreateEdge(source, target int64, edgeType string, properties map[string]string) (int64, error) {
	cType, typePin := cStringPinned(edgeType)
	defer runtime.KeepAlive(typePin)

	keys, values := flattenProps(properties)
	keyPtrs, keyPins := toCStringArray(keys)
	defer runtime.KeepAlive(keyPins)
	valPtrs, valPins := toCStringArray(values)
	defer runtime.KeepAlive(valPins)

	id := int64(-1)
	err := bw.call(func(ptr uintptr) error {
		r := procBulkCreateEdge.Call(
			ptr,
			uintptr(source), uintptr(target),
			cType,
			keyPtrs, valPtrs, uintptr(len(keys)),
		)
		if int64(r) == -1 {
			return lastError()
		}
		id = int64(r)
		return nil
	})
	return id, err
}

// CreateIndex creates a node property index.
func (bw *BulkWriter) CreateIndex(name, label, property string) error {
	cName, pinN := cStringPinned(name)
	cLabel, pinL := cStringPinned(label)
	cProp, pinP := cStringPinned(property)
	defer runtime.KeepAlive(pinN)
	defer runtime.KeepAlive(pinL)
	defer runtime.KeepAlive(pinP)
	return bw.call(func(ptr uintptr) error {
		r := procBulkCreateIndex.Call(ptr, cName, cLabel, cProp)
		return checkRC(r)
	})
}

// CreateEdgeIndex creates an edge index. Pass empty property for a type-only index.
func (bw *BulkWriter) CreateEdgeIndex(name, edgeType, property string) error {
	cName, pinN := cStringPinned(name)
	cType, pinT := cStringPinned(edgeType)
	var cProp uintptr
	var pinP []byte
	if property != "" {
		cProp, pinP = cStringPinned(property)
	}
	defer runtime.KeepAlive(pinN)
	defer runtime.KeepAlive(pinT)
	defer runtime.KeepAlive(pinP)
	return bw.call(func(ptr uintptr) error {
		r := procBulkCreateEdgeIdx.Call(ptr, cName, cType, cProp)
		return checkRC(r)
	})
}

// finish ends the load with proc (commit or abort), which frees the native handle.
func (bw *BulkWriter) finish(proc nativeFn) error {
	return bw.call(func(ptr uintptr) error {
		r := proc.Call(ptr)
		bw.ptr = 0
		runtime.SetFinalizer(bw, nil)
		return checkRC(r)
	})
}

// Commit saves the database with the load in it and makes the load visible. Durable on return;
// on error nothing was published and the load is rolled back.
func (bw *BulkWriter) Commit() error {
	return bw.finish(procBulkCommit)
}

// Abort rolls the load back.
func (bw *BulkWriter) Abort() error {
	return bw.finish(procBulkAbort)
}

// --- Internal helpers -------------------------------------------------------

// cStringArray is a C array of NUL-terminated strings: the pointer array and the strings it points
// at, kept alive together (runtime.KeepAlive) until the call reading them returns.
type cStringArray struct {
	ptrs []uintptr
	bufs [][]byte
}

func toCStringArray(strs []string) (uintptr, *cStringArray) {
	if len(strs) == 0 {
		return 0, nil
	}
	a := &cStringArray{ptrs: make([]uintptr, len(strs)), bufs: make([][]byte, len(strs))}
	for i, s := range strs {
		b := append([]byte(s), 0)
		a.bufs[i] = b
		a.ptrs[i] = uintptr(unsafe.Pointer(&b[0]))
	}
	return uintptr(unsafe.Pointer(&a.ptrs[0])), a
}

func flattenProps(props map[string]string) ([]string, []string) {
	if len(props) == 0 {
		return nil, nil
	}
	keys := make([]string, 0, len(props))
	values := make([]string, 0, len(props))
	for k, v := range props {
		keys = append(keys, k)
		values = append(values, v)
	}
	return keys, values
}
