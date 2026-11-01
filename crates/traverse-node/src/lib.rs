//! Node.js N-API bindings for Traverse embedded graph database.
//!
//! Wraps `traverse-core` and `traverse-cypher` directly via napi-rs,
//! exposing an idiomatic JavaScript/TypeScript API.

use std::collections::{BTreeMap, HashMap};
use std::panic::AssertUnwindSafe;
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi::{JsValue, Task, check_status, sys};
use napi_derive::napi;
use serde_json::Value as JsonValue;

use traverse_core::embedded::{Lease, SharedDatabase, Transaction};
use traverse_core::error::Error as CoreError;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId;
use traverse_core::storage::database::BulkLoadTx;
use traverse_core::tx::GraphView;
use traverse_cypher::embedded::{EmbeddedTransaction, StatementCanceller};
use traverse_cypher::error::CypherError;
use traverse_cypher::executor::{CypherResult, QueryStats};
use traverse_cypher::plan_cache::PlanCache;
use traverse_cypher::status::GqlStatus;
use traverse_cypher::transactions::Parameters;
use traverse_cypher::types::CypherValue;

// Force mimalloc as global allocator (matches FFI, Python, Java crates).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn terr(msg: impl Into<String>) -> Error {
    Error::from_reason(msg.into())
}

fn map_core_err(e: CoreError) -> Error {
    terr(e.to_string())
}

/// `err.code` of a statement stopped before it finished: the statement timeout, `cancel()`, a
/// terminated transaction (`TERMINATE TRANSACTIONS`).
const ERR_INTERRUPTED: &str = "ERR_TRAVERSE_INTERRUPTED";
/// `err.code` once a statement failed after changing data: only `abort()` ends the transaction.
const ERR_TRANSACTION_FAILED: &str = "ERR_TRAVERSE_TRANSACTION_FAILED";

/// A JS error with `code` (and `name`) set.
fn js_error(
    env: &Env,
    message: &str,
    code: Option<&str>,
    name: Option<&str>,
    gql_status: Option<&str>,
) -> Error {
    let build = || -> Result<Error> {
        let mut error = env.create_error(Error::from_reason(message))?;
        if let Some(code) = code {
            error.set_named_property("code", code)?;
        }
        if let Some(name) = name {
            error.set_named_property("name", name)?;
        }
        if let Some(status) = gql_status {
            error.set_named_property("gqlStatus", status)?;
        }
        Ok(Error::from(error.to_unknown()))
    };
    build().unwrap_or_else(|e| e)
}

/// A failed statement or commit, before it becomes a JS error on the main thread: `code` for the
/// conditions Node.js code branches on, `gqlStatus` (ISO/IEC 39075) for every one.
pub struct Failure {
    message: String,
    code: Option<&'static str>,
    gql_status: &'static str,
}

impl Failure {
    fn core(e: CoreError) -> Self {
        Self {
            code: matches!(e, CoreError::TransactionFailed).then_some(ERR_TRANSACTION_FAILED),
            gql_status: GqlStatus::of_core(&e).code(),
            message: e.to_string(),
        }
    }

    fn cypher(e: CypherError) -> Self {
        let code = match &e {
            CypherError::Interrupted { .. } => Some(ERR_INTERRUPTED),
            CypherError::GraphError(CoreError::TransactionFailed) => Some(ERR_TRANSACTION_FAILED),
            _ => None,
        };
        Self {
            message: e.to_string(),
            code,
            gql_status: e.status().code(),
        }
    }

    /// A statement that panicked: an engine bug, reported like any failed statement.
    fn panic(payload: Box<dyn std::any::Any + Send>) -> Self {
        let detail = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        Self {
            message: format!("internal error: {detail}"),
            code: None,
            gql_status: GqlStatus::Unexpected.code(),
        }
    }

    fn into_error(self, env: &Env) -> Error {
        js_error(env, &self.message, self.code, None, Some(self.gql_status))
    }
}

// ---------------------------------------------------------------------------
// CypherValue -> JavaScript
// ---------------------------------------------------------------------------

/// `value` as a JS value.
fn js<T: ToNapiValue>(env: &Env, value: T) -> Result<sys::napi_value> {
    // SAFETY: an `Env` is the live env of the call it came with
    unsafe { T::to_napi_value(env.raw(), value) }
}

/// A JS array with one element per item.
fn js_array<T>(
    env: &Env,
    items: &[T],
    element: impl Fn(&T) -> Result<sys::napi_value>,
) -> Result<sys::napi_value> {
    let mut array = ptr::null_mut();
    // SAFETY: `env` is live (see `js`) and `array` a valid out-pointer
    check_status!(unsafe {
        sys::napi_create_array_with_length(env.raw(), items.len(), &mut array)
    })?;
    for (i, item) in items.iter().enumerate() {
        let value = element(item)?;
        // SAFETY: `array` and `value` were made on `env`, which is live
        check_status!(unsafe { sys::napi_set_element(env.raw(), array, i as u32, value) })?;
    }
    Ok(array)
}

/// A result value on its way to JavaScript, built straight into JS values: no JSON in between,
/// so floats keep NaN and +/-Infinity. Integers become numbers (exact up to 2^53).
struct Js<'a>(&'a CypherValue);

impl ToNapiValue for Js<'_> {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        to_js(&Env::from_raw(env), val.0)
    }
}

fn to_js(env: &Env, val: &CypherValue) -> Result<sys::napi_value> {
    match val {
        CypherValue::Null => js(env, Null),
        CypherValue::Bool(b) => js(env, *b),
        CypherValue::Integer(i) => js(env, *i),
        CypherValue::Float(f) => js(env, *f),
        CypherValue::String(s) => js(env, s.as_str()),
        // BYTEARRAY values arrive as base64 strings, like over HTTP and gRPC
        CypherValue::Bytes(b) => {
            use base64::Engine;
            js(env, base64::engine::general_purpose::STANDARD.encode(b))
        }
        CypherValue::List(items) => js_array(env, items, |item| to_js(env, item)),
        CypherValue::Map(map) => {
            let mut obj = Object::new(env)?;
            for (k, v) in map {
                obj.set(k, Js(v))?;
            }
            js(env, obj)
        }
        CypherValue::Node {
            id,
            labels,
            properties,
        } => {
            let mut obj = Object::new(env)?;
            obj.set("_id", id.0 as i64)?;
            obj.set("_labels", JsStrings(labels))?;
            for (k, v) in properties {
                obj.set(k, Js(&CypherValue::from_property(v)))?;
            }
            js(env, obj)
        }
        CypherValue::Edge {
            id,
            edge_type,
            source,
            target,
            properties,
        } => {
            let mut obj = Object::new(env)?;
            obj.set("_id", id.0 as i64)?;
            obj.set("_type", edge_type.as_str())?;
            obj.set("_source", source.0 as i64)?;
            obj.set("_target", target.0 as i64)?;
            for (k, v) in properties {
                obj.set(k, Js(&CypherValue::from_property(v)))?;
            }
            js(env, obj)
        }
        CypherValue::NodeRef(id) => {
            let mut obj = Object::new(env)?;
            obj.set("_id", id.0 as i64)?;
            js(env, obj)
        }
        CypherValue::EdgeRef {
            id, source, target, ..
        } => {
            let mut obj = Object::new(env)?;
            obj.set("_id", id.0 as i64)?;
            obj.set("_source", source.0 as i64)?;
            obj.set("_target", target.0 as i64)?;
            js(env, obj)
        }
        CypherValue::Path { nodes, edges } => {
            let mut obj = Object::new(env)?;
            obj.set("nodes", JsList(nodes))?;
            obj.set("edges", JsList(edges))?;
            js(env, obj)
        }
        CypherValue::Date(d) => js(env, d.to_string()),
        CypherValue::LocalTime(t) => js(env, t.to_string()),
        CypherValue::Time(t) => js(env, t.to_string()),
        CypherValue::LocalDateTime(dt) => js(env, dt.to_string()),
        CypherValue::DateTime(dt) => js(env, dt.to_string()),
        CypherValue::Duration(d) => js(env, d.to_string()),
    }
}

/// Values as a JS array.
struct JsList<'a>(&'a [CypherValue]);

impl ToNapiValue for JsList<'_> {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        let env = Env::from_raw(env);
        js_array(&env, val.0, |item| to_js(&env, item))
    }
}

/// Strings (a node's labels) as a JS array.
struct JsStrings<'a>(&'a [String]);

impl ToNapiValue for JsStrings<'_> {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        let env = Env::from_raw(env);
        js_array(&env, val.0, |s| js(&env, s.as_str()))
    }
}

// ---------------------------------------------------------------------------
// serde_json::Value -> CypherValue (for query parameters)
// ---------------------------------------------------------------------------

fn json_to_cypher_value(val: &JsonValue) -> CypherValue {
    match val {
        JsonValue::Null => CypherValue::Null,
        JsonValue::Bool(b) => CypherValue::Bool(*b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                CypherValue::Integer(i)
            } else {
                CypherValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        JsonValue::String(s) => CypherValue::String(s.clone()),
        JsonValue::Array(arr) => CypherValue::List(arr.iter().map(json_to_cypher_value).collect()),
        JsonValue::Object(map) => {
            let m: BTreeMap<String, CypherValue> = map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_cypher_value(v)))
                .collect();
            CypherValue::Map(m)
        }
    }
}

// ---------------------------------------------------------------------------
// serde_json::Value -> PropertyValue (for bulk writer)
// ---------------------------------------------------------------------------

fn json_to_property_value(val: &JsonValue) -> Result<PropertyValue> {
    match val {
        JsonValue::Null => Ok(PropertyValue::Null),
        JsonValue::Bool(b) => Ok(PropertyValue::Bool(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(PropertyValue::Int(i))
            } else {
                Ok(PropertyValue::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        JsonValue::String(s) => Ok(PropertyValue::String(s.clone())),
        JsonValue::Array(arr) => {
            let items: Result<Vec<PropertyValue>> =
                arr.iter().map(json_to_property_value).collect();
            Ok(PropertyValue::List(items?))
        }
        JsonValue::Object(_) => Err(terr("cannot convert object to property value")),
    }
}

fn extract_props(
    props: Option<HashMap<String, JsonValue>>,
) -> Result<BTreeMap<String, PropertyValue>> {
    let mut map = BTreeMap::new();
    if let Some(obj) = props {
        for (k, v) in obj {
            let pv = json_to_property_value(&v)?;
            if !matches!(pv, PropertyValue::Null) {
                map.insert(k, pv);
            }
        }
    }
    Ok(map)
}

fn extract_params(
    params: Option<HashMap<String, JsonValue>>,
) -> Option<HashMap<String, CypherValue>> {
    params.map(|obj: HashMap<String, JsonValue>| {
        obj.into_iter()
            .map(|(k, v)| (k, json_to_cypher_value(&v)))
            .collect()
    })
}

// ---------------------------------------------------------------------------
// QueryResult as napi object
// ---------------------------------------------------------------------------

#[napi(object, object_from_js = false)]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// One object per row, keyed by column.
    #[napi(ts_type = "Array<Record<string, any>>")]
    pub rows: Rows,
    pub row_count: u32,
    #[napi(ts_type = "QueryStats")]
    pub stats: QueryStatsJs,
}

/// `QueryResult.rows`, converted to JS when the result reaches JavaScript.
pub struct Rows {
    columns: Vec<String>,
    rows: Vec<Vec<CypherValue>>,
}

impl TypeName for Rows {
    fn type_name() -> &'static str {
        "Array<Record<string, any>>"
    }

    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for Rows {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        let env = Env::from_raw(env);
        js_array(&env, &val.rows, |row| {
            let mut obj = Object::new(&env)?;
            for (column, value) in val.columns.iter().zip(row) {
                obj.set(column, Js(value))?;
            }
            js(&env, obj)
        })
    }
}

#[napi(object, object_from_js = false, js_name = "QueryStats")]
pub struct QueryStatsJs {
    pub nodes_created: i64,
    pub nodes_deleted: i64,
    pub relationships_created: i64,
    pub relationships_deleted: i64,
    pub properties_set: i64,
    pub labels_added: i64,
    pub labels_removed: i64,
    pub indexes_added: i64,
    pub indexes_removed: i64,
    pub constraints_added: i64,
    pub constraints_removed: i64,
}

fn build_query_result(result: CypherResult) -> QueryResult {
    QueryResult {
        row_count: result.rows.len() as u32,
        columns: result.columns.clone(),
        stats: build_stats(&result.stats),
        rows: Rows {
            columns: result.columns,
            rows: result.rows,
        },
    }
}

fn build_stats(stats: &QueryStats) -> QueryStatsJs {
    QueryStatsJs {
        nodes_created: stats.nodes_created,
        nodes_deleted: stats.nodes_deleted,
        relationships_created: stats.relationships_created,
        relationships_deleted: stats.relationships_deleted,
        properties_set: stats.properties_set,
        labels_added: stats.labels_added,
        labels_removed: stats.labels_removed,
        indexes_added: stats.indexes_added,
        indexes_removed: stats.indexes_removed,
        constraints_added: stats.constraints_added,
        constraints_removed: stats.constraints_removed,
    }
}

// ---------------------------------------------------------------------------
// Database class
// ---------------------------------------------------------------------------

impl TraverseDatabase {
    fn target(&self) -> Target {
        Target::Database(self.db.clone(), Arc::clone(&self.plans))
    }
}

/// An open database. Close it with `close()`: that rolls back transactions and bulk writers still
/// open on it (their later calls throw), saves, and releases the file.
#[napi]
pub struct TraverseDatabase {
    db: SharedDatabase,
    /// Plans shared by every statement on the database.
    plans: Arc<PlanCache>,
}

#[napi]
impl TraverseDatabase {
    /// Begin a write transaction. One is active at a time: this waits for the active one at most
    /// `busyTimeout` milliseconds, then throws.
    #[napi(catch_unwind)]
    pub fn begin(&self) -> Result<TraverseTransaction> {
        let lease = self.db.begin_write().map_err(map_core_err)?;
        Ok(TraverseTransaction::new(self, lease, false))
    }

    /// Begin a read transaction: a snapshot of the database as of now. Never waits, runs
    /// alongside the writer and other readers, and refuses writes.
    #[napi(catch_unwind)]
    pub fn begin_read(&self) -> Result<TraverseTransaction> {
        let lease = self.db.begin_read().map_err(map_core_err)?;
        Ok(TraverseTransaction::new(self, lease, true))
    }

    /// Begin a bulk load: a write transaction that skips the redo log. `commit()` saves the
    /// database with the load in it, so the data is written once; readers don't see the load
    /// until then, and `abort()` rolls it back. Waits for an active writer like `begin()`.
    #[napi(catch_unwind)]
    pub fn bulk_writer(&self) -> Result<TraverseBulkWriter> {
        let lease = self.db.begin_bulk_load().map_err(map_core_err)?;
        Ok(TraverseBulkWriter { lease })
    }

    /// Runs a query as a transaction of its own (auto-commit): a read on a snapshot, so it never
    /// waits; a write in a write transaction that commits when the query succeeds and rolls back
    /// when it fails (waiting at most `busyTimeout` for the writer lock), in which
    /// `CALL { ... } IN TRANSACTIONS` commits each batch. `dialect`: undefined (openCypher, the
    /// default) or 'gql'. Blocks the event loop while it runs; see `executeAsync`.
    #[napi(catch_unwind)]
    pub fn execute(
        &self,
        env: Env,
        query: String,
        #[napi(ts_arg_type = "Record<string, unknown>")] params: Option<HashMap<String, JsonValue>>,
        #[napi(ts_arg_type = "'cypher' | 'gql'")] dialect: Option<String>,
    ) -> Result<QueryResult> {
        let gql = parse_dialect(dialect.as_deref())?;
        let params: Parameters = Arc::new(extract_params(params).unwrap_or_default());
        self.target()
            .execute(&query, &params, gql, None)
            .map(build_query_result)
            .map_err(|f| f.into_error(&env))
    }

    /// `execute` on the libuv thread pool: the event loop keeps running. Aborting `signal` stops
    /// the query and rejects with an `AbortError`.
    #[napi(ts_return_type = "Promise<QueryResult>", catch_unwind)]
    pub fn execute_async(
        &self,
        env: Env,
        query: String,
        #[napi(ts_arg_type = "Record<string, unknown>")] params: Option<HashMap<String, JsonValue>>,
        #[napi(ts_arg_type = "'cypher' | 'gql'")] dialect: Option<String>,
        #[napi(ts_arg_type = "AbortSignal")] signal: Option<Object>,
    ) -> Result<AsyncTask<ExecuteTask>> {
        self.target()
            .execute_async(&env, query, params, dialect, signal)
    }

    /// Milliseconds `begin()`, `bulkWriter()` and `flush()` wait for an active write transaction
    /// before throwing (`Infinity`: as long as it takes). Default 5000.
    #[napi(getter, js_name = "busyTimeout", catch_unwind)]
    pub fn get_busy_timeout(&self) -> f64 {
        match self.db.busy_timeout() {
            Duration::MAX => f64::INFINITY,
            d => d.as_secs_f64() * 1000.0,
        }
    }

    #[napi(setter, js_name = "busyTimeout", catch_unwind)]
    pub fn set_busy_timeout(&self, ms: f64) -> Result<()> {
        if ms.is_nan() || ms < 0.0 {
            return Err(terr(
                "busyTimeout must be a non-negative number of milliseconds (or Infinity)",
            ));
        }
        let timeout = Duration::try_from_secs_f64(ms / 1000.0).unwrap_or(Duration::MAX);
        self.db.set_busy_timeout(timeout);
        Ok(())
    }

    /// Milliseconds a statement may run before it throws with `code` `ERR_TRAVERSE_INTERRUPTED`
    /// (`Infinity`, the default: no limit; setting 0 means the same). Applies to transactions
    /// begun afterwards.
    #[napi(getter, js_name = "statementTimeout", catch_unwind)]
    pub fn get_statement_timeout(&self) -> f64 {
        self.db
            .statement_timeout()
            .map_or(f64::INFINITY, |d| d.as_secs_f64() * 1000.0)
    }

    #[napi(setter, js_name = "statementTimeout", catch_unwind)]
    pub fn set_statement_timeout(&self, ms: f64) -> Result<()> {
        if ms.is_nan() || ms < 0.0 {
            return Err(terr(
                "statementTimeout must be a non-negative number of milliseconds (or Infinity)",
            ));
        }
        let timeout = (ms > 0.0)
            .then(|| Duration::try_from_secs_f64(ms / 1000.0).ok())
            .flatten()
            .map(|d| d.max(Duration::from_millis(1)));
        self.db.set_statement_timeout(timeout);
        Ok(())
    }

    /// Whether `close()` ran.
    #[napi(getter, catch_unwind)]
    pub fn closed(&self) -> bool {
        self.db.is_closed()
    }

    /// Flush dirty pages to disk. Waits for an active writer like `begin()`.
    #[napi(catch_unwind)]
    pub fn flush(&self) -> Result<()> {
        self.db.flush().map_err(map_core_err)
    }

    /// Close the database. Closing twice is a no-op.
    #[napi(catch_unwind)]
    pub fn close(&self) -> Result<()> {
        self.db.close().map_err(map_core_err)
    }
}

// ---------------------------------------------------------------------------
// Transaction class
// ---------------------------------------------------------------------------

/// Ends `lease` with an abort: finished earlier, or rolled back by a close, is fine.
fn abort_lease<T: Send + 'static>(lease: &Lease<T>, abort: impl FnOnce(T)) -> Result<()> {
    match lease.finish(abort) {
        Ok(()) | Err(CoreError::TransactionFinished | CoreError::DatabaseClosed) => Ok(()),
        Err(e) => Err(map_core_err(e)),
    }
}

/// What a transaction shares with its running `executeAsync` tasks.
struct TxShared {
    lease: Lease<Transaction>,
    /// Its place in the transaction registry and its statement timeout.
    embedded: EmbeddedTransaction,
    plans: Arc<PlanCache>,
}

impl TxShared {
    /// Runs one statement; `canceller` stops it.
    fn execute(
        &self,
        query: &str,
        params: &Parameters,
        gql: bool,
        canceller: Option<&StatementCanceller>,
    ) -> std::result::Result<CypherResult, Failure> {
        self.lease
            .with(|tx| {
                tx.statement(|view| {
                    let shown = (!params.is_empty()).then(|| Arc::clone(params));
                    self.embedded.run(query, shown, |ctx| {
                        if let Some(canceller) = canceller {
                            canceller.attach(ctx);
                        }
                        if gql {
                            traverse_gql::execute_gql_on(
                                view,
                                self.embedded.database(),
                                query,
                                params,
                            )
                        } else {
                            traverse_cypher::plan_cache::execute(&self.plans, view, query, params)
                        }
                    })
                })
            })
            .map_err(Failure::core)?
            .map_err(Failure::cypher)
    }
}

/// Where a statement runs: in a transaction, or as a transaction of its own on the database.
#[derive(Clone)]
enum Target {
    Transaction(Arc<TxShared>),
    Database(SharedDatabase, Arc<PlanCache>),
}

impl Target {
    fn execute(
        &self,
        query: &str,
        params: &Parameters,
        gql: bool,
        canceller: Option<&StatementCanceller>,
    ) -> std::result::Result<CypherResult, Failure> {
        match self {
            Target::Transaction(tx) => tx.execute(query, params, gql, canceller),
            Target::Database(db, _) if gql => {
                let shown = (!params.is_empty()).then(|| Arc::clone(params));
                traverse_cypher::embedded::auto_commit(
                    db,
                    query,
                    shown,
                    canceller,
                    |_| traverse_gql::query_writes(query),
                    |view| traverse_gql::execute_gql_on(view, &db.name(), query, params),
                )
                .map_err(Failure::cypher)
            }
            Target::Database(db, plans) => {
                traverse_cypher::embedded::execute_auto_commit(db, plans, query, params, canceller)
                    .map_err(Failure::cypher)
            }
        }
    }

    /// `execute` off the event loop, stopped by `signal` (an `AbortSignal`).
    fn execute_async(
        self,
        env: &Env,
        query: String,
        params: Option<HashMap<String, JsonValue>>,
        dialect: Option<String>,
        signal: Option<Object>,
    ) -> Result<AsyncTask<ExecuteTask>> {
        let gql = parse_dialect(dialect.as_deref())?;
        let canceller = StatementCanceller::new();
        let signal = match signal {
            Some(object) => {
                if object
                    .get_named_property::<bool>("aborted")
                    .unwrap_or(false)
                {
                    canceller.cancel();
                }
                // SAFETY: `object` is a live value of this call's env
                let signal = unsafe { AbortSignal::from_napi_value(env.raw(), object.raw()) }?;
                let on_abort = canceller.clone();
                signal.on_abort(move || on_abort.cancel());
                Some(signal)
            }
            None => None,
        };
        let task = ExecuteTask {
            target: self,
            query,
            params: Arc::new(extract_params(params).unwrap_or_default()),
            gql,
            canceller,
        };
        Ok(AsyncTask::with_optional_signal(task, signal))
    }
}

fn parse_dialect(dialect: Option<&str>) -> Result<bool> {
    match dialect {
        None | Some("cypher") => Ok(false),
        Some("gql") => Ok(true),
        Some(other) => Err(terr(format!(
            "unknown dialect '{other}' - expected 'cypher' or 'gql'"
        ))),
    }
}

/// A transaction: the writer (`begin()`) or a snapshot reader (`beginRead()`). One that is never
/// committed or aborted rolls back when it is garbage collected. A statement that fails after
/// changing data fails the transaction: later statements and `commit()` throw with `code`
/// `ERR_TRAVERSE_TRANSACTION_FAILED`, and only `abort()` ends it.
#[napi]
pub struct TraverseTransaction {
    shared: Arc<TxShared>,
    read_only: bool,
}

#[napi]
impl TraverseTransaction {
    fn new(db: &TraverseDatabase, lease: Lease<Transaction>, read_only: bool) -> Self {
        Self {
            shared: Arc::new(TxShared {
                lease,
                embedded: EmbeddedTransaction::on(&db.db),
                plans: Arc::clone(&db.plans),
            }),
            read_only,
        }
    }

    /// Execute a query with optional parameters. `dialect` selects the
    /// query language: undefined (openCypher, the default) or 'gql'
    /// for ISO/IEC 39075 GQL. Blocks the event loop while it runs; see `executeAsync`.
    #[napi(catch_unwind)]
    pub fn execute(
        &self,
        env: Env,
        query: String,
        #[napi(ts_arg_type = "Record<string, unknown>")] params: Option<HashMap<String, JsonValue>>,
        #[napi(ts_arg_type = "'cypher' | 'gql'")] dialect: Option<String>,
    ) -> Result<QueryResult> {
        let gql = parse_dialect(dialect.as_deref())?;
        let params: Parameters = Arc::new(extract_params(params).unwrap_or_default());
        self.shared
            .execute(&query, &params, gql, None)
            .map(build_query_result)
            .map_err(|f| f.into_error(&env))
    }

    /// `execute` on the libuv thread pool: the event loop keeps running. Aborting `signal` stops
    /// the statement and rejects with an `AbortError`; `cancel()` stops it too. Statements on one
    /// transaction take turns.
    #[napi(ts_return_type = "Promise<QueryResult>", catch_unwind)]
    pub fn execute_async(
        &self,
        env: Env,
        query: String,
        #[napi(ts_arg_type = "Record<string, unknown>")] params: Option<HashMap<String, JsonValue>>,
        #[napi(ts_arg_type = "'cypher' | 'gql'")] dialect: Option<String>,
        #[napi(ts_arg_type = "AbortSignal")] signal: Option<Object>,
    ) -> Result<AsyncTask<ExecuteTask>> {
        Target::Transaction(Arc::clone(&self.shared))
            .execute_async(&env, query, params, dialect, signal)
    }

    /// Stops the statement this transaction is running (an `executeAsync`): it throws with `code`
    /// `ERR_TRAVERSE_INTERRUPTED`. A statement that had already changed data fails the
    /// transaction. Does nothing when no statement runs or the transaction ended.
    #[napi(catch_unwind)]
    pub fn cancel(&self) {
        traverse_cypher::embedded::cancel(&self.shared.embedded.handle());
    }

    /// Whether this is a read transaction.
    #[napi(getter, catch_unwind)]
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Commit the transaction (ends a read transaction).
    #[napi(catch_unwind)]
    pub fn commit(&self, env: Env) -> Result<()> {
        // terminated (TERMINATE TRANSACTIONS): rolled back instead
        if let Err(e) = self.shared.embedded.check_live() {
            abort_lease(&self.shared.lease, Transaction::abort)?;
            return Err(Failure::cypher(e).into_error(&env));
        }
        self.shared
            .lease
            .finish(Transaction::commit)
            .and_then(|r| r)
            .map_err(|e| Failure::core(e).into_error(&env))
    }

    /// Abort the transaction, discarding all changes. A no-op once finished.
    #[napi(catch_unwind)]
    pub fn abort(&self) -> Result<()> {
        abort_lease(&self.shared.lease, Transaction::abort)
    }
}

/// One `executeAsync` statement.
pub struct ExecuteTask {
    target: Target,
    query: String,
    params: Parameters,
    gql: bool,
    /// Stopped by the `AbortSignal`.
    canceller: StatementCanceller,
}

impl Task for ExecuteTask {
    type Output = std::result::Result<QueryResult, Failure>;
    type JsValue = QueryResult;

    fn compute(&mut self) -> Result<Self::Output> {
        if self.canceller.is_cancelled() {
            return Ok(Err(Failure {
                message: "aborted".into(),
                code: Some(ERR_INTERRUPTED),
                gql_status: GqlStatus::TransactionTerminated.code(),
            }));
        }
        // on the libuv pool: a panic unwinding out of here would abort Node
        let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.target
                .execute(&self.query, &self.params, self.gql, Some(&self.canceller))
        }));
        Ok(run
            .unwrap_or_else(|panic| Err(Failure::panic(panic)))
            .map(build_query_result))
    }

    fn resolve(&mut self, env: Env, output: Self::Output) -> Result<QueryResult> {
        match output {
            Ok(result) => Ok(result),
            Err(f) if f.code == Some(ERR_INTERRUPTED) && self.canceller.is_cancelled() => {
                Err(js_error(
                    &env,
                    "The operation was aborted",
                    Some("ABORT_ERR"),
                    Some("AbortError"),
                    None,
                ))
            }
            Err(f) => Err(f.into_error(&env)),
        }
    }
}

// ---------------------------------------------------------------------------
// BulkWriter class
// ---------------------------------------------------------------------------

/// A bulk load (see `bulkWriter()`). One that is never committed or aborted rolls back when it is
/// garbage collected.
#[napi]
pub struct TraverseBulkWriter {
    lease: Lease<BulkLoadTx>,
}

impl TraverseBulkWriter {
    fn with_load<R>(
        &self,
        f: impl FnOnce(&mut BulkLoadTx) -> traverse_core::error::Result<R>,
    ) -> Result<R> {
        self.lease.with(f).and_then(|r| r).map_err(map_core_err)
    }
}

#[napi]
impl TraverseBulkWriter {
    /// Create a node with the given labels and optional properties.
    #[napi(catch_unwind)]
    pub fn create_node(
        &self,
        labels: Vec<String>,
        #[napi(ts_arg_type = "Record<string, unknown>")] properties: Option<
            HashMap<String, JsonValue>,
        >,
    ) -> Result<i64> {
        let props = extract_props(properties)?;
        let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        self.with_load(|tx| tx.create_node(&label_refs, props))
            .map(|id| id.0 as i64)
    }

    /// Create an edge between two nodes.
    #[napi(catch_unwind)]
    pub fn create_edge(
        &self,
        source: i64,
        target: i64,
        edge_type: String,
        #[napi(ts_arg_type = "Record<string, unknown>")] properties: Option<
            HashMap<String, JsonValue>,
        >,
    ) -> Result<i64> {
        let props = extract_props(properties)?;
        let (src, tgt) = (NodeId(source as u64), NodeId(target as u64));
        self.with_load(|tx| tx.create_edge(src, tgt, &edge_type, props))
            .map(|id| id.0 as i64)
    }

    /// Create a property index on a node label.
    #[napi(catch_unwind)]
    pub fn create_index(&self, name: String, label: String, property: String) -> Result<()> {
        self.with_load(|tx| tx.create_index(&name, &label, &property))
    }

    /// Create an edge index (with optional property key).
    #[napi(catch_unwind)]
    pub fn create_edge_index(
        &self,
        name: String,
        edge_type: String,
        property: Option<String>,
    ) -> Result<()> {
        self.with_load(|tx| tx.create_edge_index(&name, &edge_type, property.as_deref()))
    }

    /// Drop an edge index by name.
    #[napi(catch_unwind)]
    pub fn drop_edge_index(&self, name: String) -> Result<()> {
        self.with_load(|tx| tx.drop_edge_index(&name))
    }

    /// Commit the load: saves the database with it and makes it visible. Durable on return; on
    /// error nothing was published and the load is rolled back.
    #[napi(catch_unwind)]
    pub fn commit(&self) -> Result<()> {
        self.lease
            .finish(|tx| tx.commit().map(drop))
            .and_then(|r| r)
            .map_err(map_core_err)
    }

    /// Abort the load, rolling it back. A no-op once finished.
    #[napi(catch_unwind)]
    pub fn abort(&self) -> Result<()> {
        abort_lease(&self.lease, BulkLoadTx::abort)
    }
}

// ---------------------------------------------------------------------------
// Module-level functions
// ---------------------------------------------------------------------------

/// Open (or create) a database at the given path.
#[napi(catch_unwind)]
pub fn open(path: String) -> Result<TraverseDatabase> {
    let db = SharedDatabase::open(&path).map_err(map_core_err)?;
    Ok(TraverseDatabase {
        db,
        plans: Arc::default(),
    })
}
