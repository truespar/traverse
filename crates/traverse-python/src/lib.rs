//! Python bindings for the Traverse embedded graph database: `traverse-core` and `traverse-cypher`
//! via PyO3, with an idiomatic API for opening databases, running Cypher and bulk writes.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};

use traverse_core::embedded::{Lease, SharedDatabase, Transaction};
use traverse_core::error::Error as CoreError;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId;
use traverse_core::storage::database::BulkLoadTx;
use traverse_core::tx::GraphView;
use traverse_cypher::embedded::EmbeddedTransaction;
use traverse_cypher::error::CypherError;
use traverse_cypher::executor::{CypherResult, QueryStats};
use traverse_cypher::plan_cache::PlanCache;
use traverse_cypher::status::GqlStatus;
use traverse_cypher::transactions::Parameters;
use traverse_cypher::types::CypherValue;

// mimalloc as the global allocator (as in the FFI crate)
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

pyo3::create_exception!(traverse, TraverseError, pyo3::exceptions::PyException);
pyo3::create_exception!(
    traverse,
    BusyError,
    TraverseError,
    "Another write transaction held the writer lock for the whole busy timeout. Retry."
);
pyo3::create_exception!(
    traverse,
    QueryInterruptedError,
    TraverseError,
    "The statement stopped before it finished: it ran past the statement timeout, it was \
     cancelled (Transaction.cancel), or its transaction was terminated (TERMINATE TRANSACTIONS)."
);

/// `err` with the GQLSTATUS (ISO/IEC 39075) as its `gql_status` attribute.
fn with_gql_status(err: PyErr, status: GqlStatus) -> PyErr {
    Python::attach(|py| {
        let _ = err.value(py).setattr("gql_status", status.code());
    });
    err
}

fn map_core_err(e: CoreError) -> PyErr {
    let status = GqlStatus::of_core(&e);
    let err = match e {
        CoreError::Busy(_) => BusyError::new_err(e.to_string()),
        // using a handle past its end is a usage error, not a database one
        CoreError::TransactionFinished | CoreError::DatabaseClosed => {
            PyRuntimeError::new_err(e.to_string())
        }
        _ => TraverseError::new_err(e.to_string()),
    };
    with_gql_status(err, status)
}

fn map_cypher_err(e: CypherError) -> PyErr {
    let status = e.status();
    let err = match e {
        CypherError::Interrupted { .. } => QueryInterruptedError::new_err(e.to_string()),
        // busy, closed, finished: the exceptions the direct calls raise
        CypherError::GraphError(core) => return map_core_err(core),
        _ => TraverseError::new_err(e.to_string()),
    };
    with_gql_status(err, status)
}

// ---------------------------------------------------------------------------
// CypherValue -> Python conversion
// ---------------------------------------------------------------------------

fn cypher_value_to_py(py: Python<'_>, val: &CypherValue) -> PyResult<Py<PyAny>> {
    let list = |items: &[CypherValue]| -> PyResult<Py<PyAny>> {
        let items = items
            .iter()
            .map(|v| cypher_value_to_py(py, v))
            .collect::<PyResult<Vec<_>>>()?;
        Ok(PyList::new(py, &items)?.into_any().unbind())
    };
    let text = |s: String| PyString::new(py, &s).into_any().unbind();
    Ok(match val {
        CypherValue::Null => py.None(),
        CypherValue::Bool(b) => PyBool::new(py, *b).to_owned().into_any().unbind(),
        CypherValue::Integer(i) => i.into_pyobject(py)?.into_any().unbind(),
        CypherValue::Float(f) => f.into_pyobject(py)?.into_any().unbind(),
        CypherValue::String(s) => PyString::new(py, s).into_any().unbind(),
        CypherValue::Bytes(b) => pyo3::types::PyBytes::new(py, b).into_any().unbind(),
        CypherValue::List(items) => list(items)?,
        CypherValue::Map(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, cypher_value_to_py(py, v)?)?;
            }
            dict.into_any().unbind()
        }
        CypherValue::Node {
            id,
            labels,
            properties,
        } => {
            let dict = PyDict::new(py);
            dict.set_item("_id", id.0 as i64)?;
            let py_labels: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
            dict.set_item("_labels", py_labels)?;
            for (k, v) in properties {
                dict.set_item(k, property_value_to_py(py, v)?)?;
            }
            dict.into_any().unbind()
        }
        CypherValue::Edge {
            id,
            edge_type,
            source,
            target,
            properties,
        } => {
            let dict = PyDict::new(py);
            dict.set_item("_id", id.0 as i64)?;
            dict.set_item("_type", edge_type.as_str())?;
            dict.set_item("_source", source.0 as i64)?;
            dict.set_item("_target", target.0 as i64)?;
            for (k, v) in properties {
                dict.set_item(k, property_value_to_py(py, v)?)?;
            }
            dict.into_any().unbind()
        }
        CypherValue::NodeRef(id) => {
            let dict = PyDict::new(py);
            dict.set_item("_id", id.0 as i64)?;
            dict.into_any().unbind()
        }
        CypherValue::EdgeRef {
            id, source, target, ..
        } => {
            let dict = PyDict::new(py);
            dict.set_item("_id", id.0 as i64)?;
            dict.set_item("_source", source.0 as i64)?;
            dict.set_item("_target", target.0 as i64)?;
            dict.into_any().unbind()
        }
        CypherValue::Path { nodes, edges } => {
            let dict = PyDict::new(py);
            dict.set_item("nodes", list(nodes)?)?;
            dict.set_item("edges", list(edges)?)?;
            dict.into_any().unbind()
        }
        CypherValue::Date(d) => text(d.to_string()),
        CypherValue::LocalTime(t) => text(t.to_string()),
        CypherValue::Time(t) => text(t.to_string()),
        CypherValue::LocalDateTime(dt) => text(dt.to_string()),
        CypherValue::DateTime(dt) => text(dt.to_string()),
        CypherValue::Duration(d) => text(d.to_string()),
    })
}

fn property_value_to_py(py: Python<'_>, val: &PropertyValue) -> PyResult<Py<PyAny>> {
    cypher_value_to_py(py, &CypherValue::from_property(val))
}

// ---------------------------------------------------------------------------
// Python -> CypherValue conversion (for query parameters)
// ---------------------------------------------------------------------------

fn py_to_cypher_value(obj: &Bound<'_, PyAny>) -> PyResult<CypherValue> {
    if obj.is_none() {
        return Ok(CypherValue::Null);
    }
    if let Ok(b) = obj.cast::<PyBool>() {
        return Ok(CypherValue::Bool(b.is_true()));
    }
    if let Ok(i) = obj.cast::<PyInt>() {
        return Ok(CypherValue::Integer(i.extract::<i64>()?));
    }
    if let Ok(f) = obj.cast::<PyFloat>() {
        return Ok(CypherValue::Float(f.extract::<f64>()?));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(CypherValue::String(s.to_str()?.to_string()));
    }
    if let Ok(list) = obj.cast::<PyList>() {
        let items: PyResult<Vec<CypherValue>> =
            list.iter().map(|item| py_to_cypher_value(&item)).collect();
        return Ok(CypherValue::List(items?));
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        let mut map = BTreeMap::new();
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            map.insert(key, py_to_cypher_value(&v)?);
        }
        return Ok(CypherValue::Map(map));
    }
    Err(PyTypeError::new_err(format!(
        "cannot convert {} to Cypher value",
        obj.get_type().name()?
    )))
}

// ---------------------------------------------------------------------------
// Python -> PropertyValue conversion (for bulk writer)
// ---------------------------------------------------------------------------

fn py_to_property_value(obj: &Bound<'_, PyAny>) -> PyResult<PropertyValue> {
    if obj.is_none() {
        return Ok(PropertyValue::Null);
    }
    if let Ok(b) = obj.cast::<PyBool>() {
        return Ok(PropertyValue::Bool(b.is_true()));
    }
    if let Ok(i) = obj.cast::<PyInt>() {
        return Ok(PropertyValue::Int(i.extract::<i64>()?));
    }
    if let Ok(f) = obj.cast::<PyFloat>() {
        return Ok(PropertyValue::Float(f.extract::<f64>()?));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(PropertyValue::String(s.to_str()?.to_string()));
    }
    if let Ok(list) = obj.cast::<PyList>() {
        let items: PyResult<Vec<PropertyValue>> = list
            .iter()
            .map(|item| py_to_property_value(&item))
            .collect();
        return Ok(PropertyValue::List(items?));
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        let mut map = BTreeMap::new();
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            map.insert(key, py_to_property_value(&v)?);
        }
        return Ok(PropertyValue::Map(map));
    }
    Err(PyTypeError::new_err(format!(
        "cannot convert {} to property value",
        obj.get_type().name()?
    )))
}

// ---------------------------------------------------------------------------
// QueryStats -> Python dict
// ---------------------------------------------------------------------------

fn stats_to_py(py: Python<'_>, stats: &QueryStats) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    for (key, n) in [
        ("nodes_created", stats.nodes_created),
        ("nodes_deleted", stats.nodes_deleted),
        ("relationships_created", stats.relationships_created),
        ("relationships_deleted", stats.relationships_deleted),
        ("properties_set", stats.properties_set),
        ("labels_added", stats.labels_added),
        ("labels_removed", stats.labels_removed),
        ("indexes_added", stats.indexes_added),
        ("indexes_removed", stats.indexes_removed),
        ("constraints_added", stats.constraints_added),
        ("constraints_removed", stats.constraints_removed),
    ] {
        if n != 0 {
            dict.set_item(key, n)?;
        }
    }
    Ok(dict.into_any().unbind())
}

// ---------------------------------------------------------------------------
// TvDatabase Python class
// ---------------------------------------------------------------------------

/// `seconds` as a busy timeout: `inf` waits as long as it takes.
fn busy_timeout_from_secs(seconds: f64) -> PyResult<Duration> {
    if seconds.is_nan() || seconds < 0.0 {
        return Err(PyValueError::new_err(
            "busy_timeout must be a non-negative number of seconds (or inf)",
        ));
    }
    Ok(Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX))
}

/// `seconds` as a statement timeout: `None`, 0 or `inf` is no limit.
fn statement_timeout_from_secs(seconds: Option<f64>) -> PyResult<Option<Duration>> {
    let Some(seconds) = seconds else {
        return Ok(None);
    };
    if seconds.is_nan() || seconds < 0.0 {
        return Err(PyValueError::new_err(
            "statement_timeout must be None or a non-negative number of seconds",
        ));
    }
    Ok((seconds > 0.0)
        .then(|| Duration::try_from_secs_f64(seconds).ok())
        .flatten()
        .map(|d| d.max(Duration::from_millis(1))))
}

/// An open Traverse database.
///
/// Open with :func:`traverse.open`. Close it with :meth:`close` or a ``with``
/// statement: closing rolls back transactions still open on it, saves, and
/// releases the file. Thread-safe.
#[pyclass(name = "Database")]
struct TvDatabase {
    db: SharedDatabase,
    /// Plans shared by every statement on the database.
    plans: Arc<PlanCache>,
}

/// Query parameters from a dict.
fn extract_params(params: Option<&Bound<'_, PyDict>>) -> PyResult<Parameters> {
    let mut map = HashMap::new();
    if let Some(dict) = params {
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            map.insert(key, py_to_cypher_value(&v)?);
        }
    }
    Ok(Arc::new(map))
}

/// Whether `dialect` is GQL; `None` and `"cypher"` are openCypher.
fn is_gql(dialect: Option<&str>) -> PyResult<bool> {
    match dialect {
        None | Some("cypher") => Ok(false),
        Some("gql") => Ok(true),
        Some(other) => Err(TraverseError::new_err(format!(
            "unknown dialect '{other}' - expected 'cypher' or 'gql'"
        ))),
    }
}

#[pymethods]
impl TvDatabase {
    /// Begin a write transaction.
    ///
    /// One write transaction is active at a time: this waits for the active
    /// one at most :attr:`busy_timeout` seconds, then raises :class:`BusyError`.
    ///
    /// Returns a :class:`Transaction` that can be used as a context manager.
    fn begin(&self, py: Python<'_>) -> PyResult<TvTransaction> {
        let lease = py.detach(|| self.db.begin_write()).map_err(map_core_err)?;
        Ok(TvTransaction::new(self, lease, false))
    }

    /// Begin a read transaction: a snapshot of the database as of now.
    ///
    /// Never waits and runs alongside the writer and other readers; writes
    /// raise :class:`TraverseError`. Returns a :class:`Transaction`.
    fn begin_read(&self) -> PyResult<TvTransaction> {
        let lease = self.db.begin_read().map_err(map_core_err)?;
        Ok(TvTransaction::new(self, lease, true))
    }

    /// Begin a bulk load for high-throughput loading.
    ///
    /// A write transaction that skips the redo log: :meth:`BulkWriter.commit`
    /// saves the database with the load in it, so the data is written once.
    /// Readers don't see the load until then; :meth:`BulkWriter.abort` rolls
    /// it back. Waits for an active writer like :meth:`begin`.
    ///
    /// Returns a :class:`BulkWriter`.
    fn bulk_writer(&self, py: Python<'_>) -> PyResult<TvBulkWriter> {
        let lease = py
            .detach(|| self.db.begin_bulk_load())
            .map_err(map_core_err)?;
        Ok(TvBulkWriter { lease })
    }

    /// Seconds :meth:`begin`, :meth:`bulk_writer` and :meth:`flush` wait for
    /// an active write transaction before raising :class:`BusyError`
    /// (``inf``: as long as it takes). Default 5.
    #[getter]
    fn get_busy_timeout(&self) -> f64 {
        match self.db.busy_timeout() {
            Duration::MAX => f64::INFINITY,
            d => d.as_secs_f64(),
        }
    }

    #[setter]
    fn set_busy_timeout(&self, seconds: f64) -> PyResult<()> {
        self.db.set_busy_timeout(busy_timeout_from_secs(seconds)?);
        Ok(())
    }

    /// Run a query as a transaction of its own (auto-commit).
    ///
    /// A read runs on a snapshot, so it never waits; a write runs in a write
    /// transaction that commits when the query succeeds and rolls back when it
    /// fails (waiting at most :attr:`busy_timeout` for the writer lock), in
    /// which ``CALL { ... } IN TRANSACTIONS`` commits each batch.
    ///
    /// Args:
    ///     query: Query string.
    ///     params: Optional dict of query parameters (``$name`` in query).
    ///     dialect: Optional query language: ``None`` (openCypher, the
    ///         default) or ``"gql"`` for ISO/IEC 39075 GQL.
    ///
    /// Returns a :class:`QueryResult`.
    #[pyo3(signature = (query, params=None, dialect=None))]
    fn execute(
        &self,
        py: Python<'_>,
        query: &str,
        params: Option<&Bound<'_, PyDict>>,
        dialect: Option<&str>,
    ) -> PyResult<TvQueryResult> {
        let params = extract_params(params)?;
        let gql = is_gql(dialect)?;
        // the query runs without the GIL: other Python threads keep going
        let result = py
            .detach(|| {
                if gql {
                    let shown = (!params.is_empty()).then(|| Arc::clone(&params));
                    traverse_cypher::embedded::auto_commit(
                        &self.db,
                        query,
                        shown,
                        None,
                        |_| traverse_gql::query_writes(query),
                        |view| traverse_gql::execute_gql_on(view, &self.db.name(), query, &params),
                    )
                } else {
                    traverse_cypher::embedded::execute_auto_commit(
                        &self.db,
                        &self.plans,
                        query,
                        &params,
                        None,
                    )
                }
            })
            .map_err(map_cypher_err)?;
        TvQueryResult::from_cypher_result(py, result)
    }

    /// Seconds a statement may run before it raises :class:`QueryInterruptedError`
    /// (``None``, the default: no limit; 0 means the same). Applies to transactions
    /// begun afterwards.
    #[getter]
    fn get_statement_timeout(&self) -> Option<f64> {
        self.db.statement_timeout().map(|d| d.as_secs_f64())
    }

    #[setter]
    fn set_statement_timeout(&self, seconds: Option<f64>) -> PyResult<()> {
        self.db
            .set_statement_timeout(statement_timeout_from_secs(seconds)?);
        Ok(())
    }

    /// Whether :meth:`close` ran.
    #[getter]
    fn closed(&self) -> bool {
        self.db.is_closed()
    }

    /// Flush dirty pages to disk (waits for an active writer like :meth:`begin`).
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.db.flush()).map_err(map_core_err)
    }

    /// Close the database: rolls back transactions and bulk loads still open
    /// on it (their later calls raise), saves, and releases the file.
    /// Closing twice is a no-op.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.db.close()).map_err(map_core_err)
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_val: &Bound<'_, PyAny>,
        _exc_tb: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false) // don't suppress exceptions
    }
}

// ---------------------------------------------------------------------------
// TvTransaction Python class
// ---------------------------------------------------------------------------

/// Ends a lease with `f`: finished earlier counts as done, a closed database raises.
fn finish_lease<T: Send + 'static>(
    py: Python<'_>,
    lease: &Lease<T>,
    f: impl FnOnce(T) -> traverse_core::error::Result<()> + Send,
) -> PyResult<()> {
    match py.detach(|| lease.finish(f)) {
        Ok(r) => r.map_err(map_core_err),
        Err(CoreError::TransactionFinished) => Ok(()),
        Err(e) => Err(map_core_err(e)),
    }
}

/// A transaction: the writer (:meth:`Database.begin`) or a snapshot reader
/// (:meth:`Database.begin_read`).
///
/// Use as a context manager: commits on a clean exit, rolls back on an
/// exception::
///
///     with db.begin() as tx:
///         tx.execute("CREATE (n:Person {name: 'Alice'})")
///
/// Dropping an uncommitted transaction rolls it back. Thread-safe: calls from
/// several threads take turns, except :meth:`cancel`, which stops the running
/// one. A statement that fails after changing data fails the transaction:
/// later statements and :meth:`commit` raise, and only a rollback ends it.
#[pyclass(name = "Transaction")]
struct TvTransaction {
    lease: Lease<Transaction>,
    /// Its place in the transaction registry and its statement timeout.
    embedded: EmbeddedTransaction,
    plans: Arc<PlanCache>,
    read_only: bool,
}

impl TvTransaction {
    fn new(db: &TvDatabase, lease: Lease<Transaction>, read_only: bool) -> Self {
        Self {
            lease,
            embedded: EmbeddedTransaction::on(&db.db),
            plans: Arc::clone(&db.plans),
            read_only,
        }
    }

    /// Terminated (TERMINATE TRANSACTIONS): rolled back and raised instead of a commit.
    fn check_live(&self, py: Python<'_>) -> PyResult<()> {
        match self.embedded.check_live() {
            Ok(()) => Ok(()),
            Err(e) => {
                self.abort(py)?;
                Err(map_cypher_err(e))
            }
        }
    }
}

#[pymethods]
impl TvTransaction {
    /// Execute a query.
    ///
    /// Args:
    ///     query: Query string.
    ///     params: Optional dict of query parameters (``$name`` in query).
    ///     dialect: Optional query language: ``None`` (openCypher, the
    ///         default) or ``"gql"`` for ISO/IEC 39075 GQL.
    ///
    /// Returns a :class:`QueryResult`.
    #[pyo3(signature = (query, params=None, dialect=None))]
    fn execute(
        &self,
        py: Python<'_>,
        query: &str,
        params: Option<&Bound<'_, PyDict>>,
        dialect: Option<&str>,
    ) -> PyResult<TvQueryResult> {
        let params = extract_params(params)?;
        let use_gql = is_gql(dialect)?;
        // the query runs without the GIL: other Python threads keep going
        let result = py
            .detach(|| {
                self.lease.with(|tx| {
                    tx.statement(|view| {
                        let shown = (!params.is_empty()).then(|| Arc::clone(&params));
                        self.embedded.run(query, shown, |_| {
                            if use_gql {
                                traverse_gql::execute_gql_on(
                                    view,
                                    self.embedded.database(),
                                    query,
                                    &params,
                                )
                            } else {
                                traverse_cypher::plan_cache::execute(
                                    &self.plans,
                                    view,
                                    query,
                                    &params,
                                )
                            }
                        })
                    })
                })
            })
            .map_err(map_core_err)?
            .map_err(map_cypher_err)?;
        TvQueryResult::from_cypher_result(py, result)
    }

    /// Stop the statement this transaction is running, from another thread:
    /// it raises :class:`QueryInterruptedError`. A statement that had already
    /// changed data fails the transaction. Does nothing when no statement runs
    /// or the transaction ended.
    fn cancel(&self) {
        traverse_cypher::embedded::cancel(&self.embedded.handle());
    }

    /// Whether this is a read transaction.
    #[getter]
    fn read_only(&self) -> bool {
        self.read_only
    }

    /// Commit the transaction (ends a read transaction).
    ///
    /// Raises :class:`RuntimeError` if it already finished or its database
    /// was closed (which rolled it back).
    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        self.check_live(py)?;
        py.detach(|| self.lease.finish(Transaction::commit))
            .and_then(|r| r)
            .map_err(map_core_err)
    }

    /// Abort the transaction, discarding all changes. A no-op once finished.
    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        match py.detach(|| self.lease.finish(Transaction::abort)) {
            Ok(()) | Err(CoreError::TransactionFinished | CoreError::DatabaseClosed) => Ok(()),
            Err(e) => Err(map_core_err(e)),
        }
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: &Bound<'_, PyAny>,
        _exc_val: &Bound<'_, PyAny>,
        _exc_tb: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        if exc_type.is_none() {
            // commits unless the block already finished it
            self.check_live(py)?;
            finish_lease(py, &self.lease, Transaction::commit)?;
        } else {
            self.abort(py)?;
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// TvQueryResult Python class
// ---------------------------------------------------------------------------

/// The result of a Cypher query.
///
/// Iterable - yields dicts mapping column names to values::
///
///     result = tx.execute("MATCH (n:Person) RETURN n.name, n.age")
///     for row in result:
///         print(row["n.name"], row["n.age"])
#[pyclass(name = "QueryResult")]
struct TvQueryResult {
    columns: Vec<String>,
    rows: Vec<Vec<Py<PyAny>>>,
    stats: Py<PyAny>,
    cursor: usize,
}

impl TvQueryResult {
    fn from_cypher_result(py: Python<'_>, result: CypherResult) -> PyResult<Self> {
        let rows = result
            .rows
            .iter()
            .map(|row| row.iter().map(|val| cypher_value_to_py(py, val)).collect())
            .collect::<PyResult<Vec<Vec<_>>>>()?;
        let stats = stats_to_py(py, &result.stats)?;
        Ok(TvQueryResult {
            columns: result.columns,
            rows,
            stats,
            cursor: 0,
        })
    }
}

#[pymethods]
impl TvQueryResult {
    /// Column names.
    #[getter]
    fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }

    /// Number of rows.
    #[getter]
    fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Mutation statistics (dict of non-zero counters).
    #[getter]
    fn stats(&self, py: Python<'_>) -> Py<PyAny> {
        self.stats.clone_ref(py)
    }

    /// Get all rows as a list of dicts.
    fn data(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let list = PyList::empty(py);
        for row in &self.rows {
            let dict = PyDict::new(py);
            for (col, val) in self.columns.iter().zip(row.iter()) {
                dict.set_item(col, val)?;
            }
            list.append(dict)?;
        }
        Ok(list.into_any().unbind())
    }

    fn __iter__(mut slf: PyRefMut<'_, Self>) -> PyRefMut<'_, Self> {
        slf.cursor = 0;
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if self.cursor >= self.rows.len() {
            return Ok(None);
        }
        let row = &self.rows[self.cursor];
        self.cursor += 1;
        let dict = PyDict::new(py);
        for (col, val) in self.columns.iter().zip(row.iter()) {
            dict.set_item(col, val)?;
        }
        Ok(Some(dict.into_any().unbind()))
    }

    fn __len__(&self) -> usize {
        self.rows.len()
    }

    fn __repr__(&self) -> String {
        format!(
            "<QueryResult columns={:?} rows={}>",
            self.columns,
            self.rows.len()
        )
    }
}

// ---------------------------------------------------------------------------
// TvBulkWriter Python class
// ---------------------------------------------------------------------------

/// Python dict -> property map (`None` values dropped).
fn py_props(properties: Option<&Bound<'_, PyDict>>) -> PyResult<BTreeMap<String, PropertyValue>> {
    let mut map = BTreeMap::new();
    if let Some(dict) = properties {
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            let pv = py_to_property_value(&v)?;
            if !matches!(pv, PropertyValue::Null) {
                map.insert(key, pv);
            }
        }
    }
    Ok(map)
}

/// Bulk load for high-throughput imports (see :meth:`Database.bulk_writer`).
///
///     with db.bulk_writer() as writer:
///         nid = writer.create_node(["Person"], {"name": "Alice", "age": 30})
///         writer.create_edge(nid, other_id, "KNOWS", {"since": 2020})
///
/// The ``with`` block commits on a clean exit and rolls back on an exception;
/// dropping an uncommitted writer rolls it back.
#[pyclass(name = "BulkWriter")]
struct TvBulkWriter {
    lease: Lease<BulkLoadTx>,
}

impl TvBulkWriter {
    fn with_load<R>(
        &self,
        f: impl FnOnce(&mut BulkLoadTx) -> traverse_core::error::Result<R>,
    ) -> PyResult<R> {
        self.lease.with(f).and_then(|r| r).map_err(map_core_err)
    }
}

#[pymethods]
impl TvBulkWriter {
    /// Create a node with the given labels and properties.
    ///
    /// Args:
    ///     labels: List of label strings.
    ///     properties: Dict of property name -> value.
    ///
    /// Returns the new node ID (int).
    #[pyo3(signature = (labels, properties=None))]
    fn create_node(
        &self,
        labels: Vec<String>,
        properties: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<i64> {
        let props = py_props(properties)?;
        let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        self.with_load(|tx| tx.create_node(&label_refs, props))
            .map(|id| id.0 as i64)
    }

    /// Create an edge between two existing nodes.
    ///
    /// Args:
    ///     source: Source node ID.
    ///     target: Target node ID.
    ///     edge_type: Relationship type string.
    ///     properties: Optional dict of property name -> value.
    ///
    /// Returns the new edge ID (int).
    #[pyo3(signature = (source, target, edge_type, properties=None))]
    fn create_edge(
        &self,
        source: i64,
        target: i64,
        edge_type: &str,
        properties: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<i64> {
        let props = py_props(properties)?;
        let (src, tgt) = (NodeId(source as u64), NodeId(target as u64));
        self.with_load(|tx| tx.create_edge(src, tgt, edge_type, props))
            .map(|id| id.0 as i64)
    }

    /// Create a property index on a node label.
    ///
    /// Args:
    ///     name: Index name.
    ///     label: Node label.
    ///     property: Property key to index.
    fn create_index(&self, name: &str, label: &str, property: &str) -> PyResult<()> {
        self.with_load(|tx| tx.create_index(name, label, property))
    }

    /// Create an edge index.
    ///
    /// Args:
    ///     name: Index name.
    ///     edge_type: Relationship type.
    ///     property: Optional property key (None for type-only index).
    #[pyo3(signature = (name, edge_type, property=None))]
    fn create_edge_index(
        &self,
        name: &str,
        edge_type: &str,
        property: Option<&str>,
    ) -> PyResult<()> {
        self.with_load(|tx| tx.create_edge_index(name, edge_type, property))
    }

    /// Drop an edge index by name.
    fn drop_edge_index(&self, name: &str) -> PyResult<()> {
        self.with_load(|tx| tx.drop_edge_index(name))
    }

    /// Commit the load: saves the database with it and makes it visible.
    /// Durable on return; on error nothing was published and the load is
    /// rolled back.
    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.lease.finish(|tx| tx.commit().map(drop)))
            .and_then(|r| r)
            .map_err(map_core_err)
    }

    /// Abort the load, rolling it back. A no-op once finished.
    fn abort(&self, py: Python<'_>) -> PyResult<()> {
        match py.detach(|| self.lease.finish(BulkLoadTx::abort)) {
            Ok(()) | Err(CoreError::TransactionFinished | CoreError::DatabaseClosed) => Ok(()),
            Err(e) => Err(map_core_err(e)),
        }
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: &Bound<'_, PyAny>,
        _exc_val: &Bound<'_, PyAny>,
        _exc_tb: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        if exc_type.is_none() {
            finish_lease(py, &self.lease, |tx| tx.commit().map(drop))?;
        } else {
            self.abort(py)?;
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Module-level functions
// ---------------------------------------------------------------------------

/// Open (or create) a database at the given path.
///
/// Args:
///     path: Path to ``.tvdb`` file.
///
/// Returns a :class:`Database`.
///
/// Example::
///
///     db = traverse.open("mydb.tvdb")
///     with db.begin() as tx:
///         tx.execute("CREATE (n:Person {name: 'Alice'})")
///     db.close()
#[pyfunction]
fn open(py: Python<'_>, path: &str) -> PyResult<TvDatabase> {
    let db = py
        .detach(|| SharedDatabase::open(path))
        .map_err(map_core_err)?;
    Ok(TvDatabase {
        db,
        plans: Arc::default(),
    })
}

// ---------------------------------------------------------------------------
// Module definition
// ---------------------------------------------------------------------------

/// Traverse embedded graph database - Python bindings.
#[pymodule]
fn traverse(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // every Traverse exception has `gql_status`; the engine sets it on the ones it raises
    m.py()
        .get_type::<TraverseError>()
        .setattr("gql_status", m.py().None())?;
    m.add("TraverseError", m.py().get_type::<TraverseError>())?;
    m.add("BusyError", m.py().get_type::<BusyError>())?;
    m.add(
        "QueryInterruptedError",
        m.py().get_type::<QueryInterruptedError>(),
    )?;
    m.add_class::<TvDatabase>()?;
    m.add_class::<TvTransaction>()?;
    m.add_class::<TvQueryResult>()?;
    m.add_class::<TvBulkWriter>()?;
    m.add_function(wrap_pyfunction!(open, m)?)?;
    Ok(())
}
