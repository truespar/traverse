//! Java JNI bindings for Traverse embedded graph database.
//!
//! Wraps `traverse-core` and `traverse-cypher` directly via jni-rs 0.22,
//! exposing native methods for the Java `com.truespar.traverse` package.
//!
//! # Safety model
//!
//! Native methods receive the `long` handle their Java wrapper holds. A handle is freed only by
//! the wrapper's `Cleaner` (`nativeFree`), once the wrapper is unreachable, and every wrapper
//! method keeps itself reachable across its native calls, so a handle that reaches Rust points at
//! a live box from `to_handle`. Close/commit/abort only finish what the handle holds. Several
//! Java threads may use one handle at once, so it is only ever borrowed shared; the state behind
//! it synchronizes itself. Object arguments have the Java types the `native` declarations promise.
//! The `unsafe` blocks rely only on that, so they carry no individual `SAFETY:` notes.

#![allow(clippy::undocumented_unsafe_blocks)] // covered by the safety model above

use std::collections::{BTreeMap, HashMap};

use jni::objects::*;
use jni::strings::JNIString;
use jni::sys::*;
use jni::{Env, EnvUnowned, jni_sig, jni_str};

use std::sync::Arc;
use std::time::Duration;

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
use traverse_cypher::transactions::Parameters;
use traverse_cypher::types::CypherValue;

// Force mimalloc as global allocator (matches FFI and Python crates).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ---------------------------------------------------------------------------
// Custom error type for with_env closures
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct TraverseError {
    message: String,
    /// Thrown as `QueryInterruptedException`: a timeout, a cancel, a terminated transaction.
    interrupted: bool,
    /// The GQLSTATUS, when the error has one.
    gql_status: Option<&'static str>,
}

impl std::fmt::Display for TraverseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TraverseError {}

impl From<jni::errors::Error> for TraverseError {
    fn from(e: jni::errors::Error) -> Self {
        terr(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Error policy: throws com.truespar.traverse.TraverseException
// ---------------------------------------------------------------------------

struct ThrowTraverseExAndDefault;

impl<T: Default> jni::errors::ErrorPolicy<T, TraverseError> for ThrowTraverseExAndDefault {
    type Captures<'a: 'b, 'b> = ();

    fn on_error<'a: 'b, 'b>(
        env: &mut Env<'a>,
        _cap: &mut (),
        err: TraverseError,
    ) -> jni::errors::Result<T> {
        if env.exception_check() {
            return Ok(T::default());
        }
        let class = if err.interrupted {
            jni_str!("com/truespar/traverse/QueryInterruptedException")
        } else {
            jni_str!("com/truespar/traverse/TraverseException")
        };
        // (message, gqlStatus); the plain message when the exception can't be built
        let built = (|| -> jni::errors::Result<()> {
            let message = env.new_string(&err.message)?;
            let status = match err.gql_status {
                Some(code) => JObject::from(env.new_string(code)?),
                None => JObject::null(),
            };
            let thrown = env.new_object(
                class,
                &jni_sig!("(Ljava/lang/String;Ljava/lang/String;)V"),
                &[JValue::Object(&message), JValue::Object(&status)],
            )?;
            let thrown = env.cast_local::<JThrowable>(thrown)?;
            env.throw(thrown)
        })();
        if built.is_err() && !env.exception_check() {
            let _ = env.throw((class, JNIString::from(err.message)));
        }
        Ok(T::default())
    }

    fn on_panic<'a: 'b, 'b>(
        env: &mut Env<'a>,
        _cap: &mut (),
        payload: Box<dyn std::any::Any + Send + 'static>,
    ) -> jni::errors::Result<T> {
        let msg = match payload.downcast::<&'static str>() {
            Ok(s) => (*s).to_string(),
            Err(payload) => {
                if let Err(drop_panic) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
                {
                    std::mem::forget(drop_panic);
                }
                "non-string panic payload".to_string()
            }
        };
        let _ = env.throw((
            jni_str!("com/truespar/traverse/TraverseException"),
            JNIString::from(msg),
        ));
        Ok(T::default())
    }
}

// Shorthand for the Result type used in with_env closures.
type TResult<T> = Result<T, TraverseError>;

fn terr(msg: impl Into<String>) -> TraverseError {
    TraverseError {
        message: msg.into(),
        interrupted: false,
        gql_status: None,
    }
}

fn cypher_err(e: CypherError) -> TraverseError {
    TraverseError {
        interrupted: matches!(e, CypherError::Interrupted { .. }),
        gql_status: Some(e.status().code()),
        message: e.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Handle types
// ---------------------------------------------------------------------------

struct DatabaseHandle {
    db: SharedDatabase,
    /// Plans shared by every statement on the database.
    plans: Arc<PlanCache>,
}

struct TransactionHandle {
    lease: Lease<Transaction>,
    /// Its place in the transaction registry and its statement timeout.
    embedded: EmbeddedTransaction,
    plans: Arc<PlanCache>,
}

impl TransactionHandle {
    fn new(db: &DatabaseHandle, lease: Lease<Transaction>) -> Self {
        Self {
            lease,
            embedded: EmbeddedTransaction::on(&db.db),
            plans: Arc::clone(&db.plans),
        }
    }
}

struct BulkWriterHandle {
    lease: Lease<BulkLoadTx>,
}

fn core_err(e: CoreError) -> TraverseError {
    TraverseError {
        gql_status: Some(traverse_cypher::status::GqlStatus::of_core(&e).code()),
        ..terr(e.to_string())
    }
}

/// Finishes `lease` with an abort: finished earlier, or rolled back by a close, is fine.
fn abort_lease<T: Send + 'static>(lease: &Lease<T>, abort: impl FnOnce(T)) -> TResult<()> {
    match lease.finish(abort) {
        Ok(()) | Err(CoreError::TransactionFinished | CoreError::DatabaseClosed) => Ok(()),
        Err(e) => Err(core_err(e)),
    }
}

// ---------------------------------------------------------------------------
// Handle management
// ---------------------------------------------------------------------------

fn to_handle<T>(val: T) -> jlong {
    Box::into_raw(Box::new(val)) as jlong
}

/// # Safety
///
/// `h` is a live handle from `to_handle::<T>`, not yet passed to `handle_drop`.
unsafe fn handle_ref<T>(h: jlong) -> &'static T {
    unsafe { &*(h as *const T) }
}

/// # Safety
///
/// As for `handle_ref`; the handle is dead afterwards.
unsafe fn handle_drop<T>(h: jlong) {
    unsafe { drop(Box::from_raw(h as *mut T)) };
}

// ---------------------------------------------------------------------------
// Java object creation helpers
// ---------------------------------------------------------------------------

fn new_boxed_bool<'local>(env: &mut Env<'local>, v: bool) -> TResult<JObject<'local>> {
    Ok(env.new_object(
        jni_str!("java/lang/Boolean"),
        &jni_sig!("(Z)V"),
        &[JValue::Bool(v as jboolean)],
    )?)
}

fn new_boxed_long<'local>(env: &mut Env<'local>, v: i64) -> TResult<JObject<'local>> {
    Ok(env.new_object(
        jni_str!("java/lang/Long"),
        &jni_sig!("(J)V"),
        &[JValue::Long(v)],
    )?)
}

fn new_boxed_double<'local>(env: &mut Env<'local>, v: f64) -> TResult<JObject<'local>> {
    Ok(env.new_object(
        jni_str!("java/lang/Double"),
        &jni_sig!("(D)V"),
        &[JValue::Double(v)],
    )?)
}

fn new_arraylist<'local>(env: &mut Env<'local>, cap: i32) -> TResult<JObject<'local>> {
    Ok(env.new_object(
        jni_str!("java/util/ArrayList"),
        &jni_sig!("(I)V"),
        &[JValue::Int(cap)],
    )?)
}

fn new_hashmap<'local>(env: &mut Env<'local>) -> TResult<JObject<'local>> {
    Ok(env.new_object(jni_str!("java/util/HashMap"), &jni_sig!("()V"), &[])?)
}

fn list_add(env: &mut Env, list: &JObject, item: &JObject) -> TResult<()> {
    env.call_method(
        list,
        jni_str!("add"),
        &jni_sig!("(Ljava/lang/Object;)Z"),
        &[JValue::Object(item)],
    )?;
    Ok(())
}

fn map_put(env: &mut Env, map: &JObject, key: &JObject, val: &JObject) -> TResult<()> {
    env.call_method(
        map,
        jni_str!("put"),
        &jni_sig!("(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
        &[JValue::Object(key), JValue::Object(val)],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

fn get_rust_string(env: &mut Env, jstr: &JString) -> TResult<String> {
    Ok(jstr.mutf8_chars(env)?.to_string())
}

/// Reinterprets a `&JObject` as `&JString`.
///
/// # Safety
///
/// `obj` must be a `java.lang.String`.
unsafe fn obj_as_jstring<'a>(obj: &'a JObject<'_>) -> &'a JString<'a> {
    unsafe { &*(obj as *const JObject as *const JString) }
}

// ---------------------------------------------------------------------------
// CypherValue -> Java Object
// ---------------------------------------------------------------------------

fn cypher_value_to_java<'local>(
    env: &mut Env<'local>,
    val: &CypherValue,
) -> TResult<JObject<'local>> {
    match val {
        CypherValue::Null => Ok(JObject::null()),
        CypherValue::Bool(b) => new_boxed_bool(env, *b),
        CypherValue::Integer(i) => new_boxed_long(env, *i),
        CypherValue::Float(f) => new_boxed_double(env, *f),
        CypherValue::String(s) => Ok(env.new_string(s)?.into()),
        CypherValue::List(items) => {
            let list = new_arraylist(env, items.len() as i32)?;
            for item in items {
                let jv = cypher_value_to_java(env, item)?;
                list_add(env, &list, &jv)?;
            }
            Ok(list)
        }
        CypherValue::Map(map) => {
            let jmap = new_hashmap(env)?;
            for (k, v) in map {
                let jk: JObject = env.new_string(k)?.into();
                let jv = cypher_value_to_java(env, v)?;
                map_put(env, &jmap, &jk, &jv)?;
            }
            Ok(jmap)
        }
        CypherValue::Node {
            id,
            labels,
            properties,
        } => {
            let jmap = new_hashmap(env)?;
            let k: JObject = env.new_string("_id")?.into();
            let v = new_boxed_long(env, id.0 as i64)?;
            map_put(env, &jmap, &k, &v)?;
            let k: JObject = env.new_string("_labels")?.into();
            let label_list = new_arraylist(env, labels.len() as i32)?;
            for lbl in labels {
                let s: JObject = env.new_string(lbl)?.into();
                list_add(env, &label_list, &s)?;
            }
            map_put(env, &jmap, &k, &label_list)?;
            for (pk, pv) in properties {
                let jk: JObject = env.new_string(pk)?.into();
                let jv = property_value_to_java(env, pv)?;
                map_put(env, &jmap, &jk, &jv)?;
            }
            Ok(jmap)
        }
        CypherValue::Edge {
            id,
            edge_type,
            source,
            target,
            properties,
        } => {
            let jmap = new_hashmap(env)?;
            let k: JObject = env.new_string("_id")?.into();
            let v = new_boxed_long(env, id.0 as i64)?;
            map_put(env, &jmap, &k, &v)?;
            let k: JObject = env.new_string("_type")?.into();
            let v: JObject = env.new_string(edge_type.as_str())?.into();
            map_put(env, &jmap, &k, &v)?;
            let k: JObject = env.new_string("_source")?.into();
            let v = new_boxed_long(env, source.0 as i64)?;
            map_put(env, &jmap, &k, &v)?;
            let k: JObject = env.new_string("_target")?.into();
            let v = new_boxed_long(env, target.0 as i64)?;
            map_put(env, &jmap, &k, &v)?;
            for (pk, pv) in properties {
                let jk: JObject = env.new_string(pk)?.into();
                let jv = property_value_to_java(env, pv)?;
                map_put(env, &jmap, &jk, &jv)?;
            }
            Ok(jmap)
        }
        CypherValue::NodeRef(id) => {
            let jmap = new_hashmap(env)?;
            let k: JObject = env.new_string("_id")?.into();
            let v = new_boxed_long(env, id.0 as i64)?;
            map_put(env, &jmap, &k, &v)?;
            Ok(jmap)
        }
        CypherValue::EdgeRef {
            id, source, target, ..
        } => {
            let jmap = new_hashmap(env)?;
            let k: JObject = env.new_string("_id")?.into();
            let v = new_boxed_long(env, id.0 as i64)?;
            map_put(env, &jmap, &k, &v)?;
            let k: JObject = env.new_string("_source")?.into();
            let v = new_boxed_long(env, source.0 as i64)?;
            map_put(env, &jmap, &k, &v)?;
            let k: JObject = env.new_string("_target")?.into();
            let v = new_boxed_long(env, target.0 as i64)?;
            map_put(env, &jmap, &k, &v)?;
            Ok(jmap)
        }
        CypherValue::Path { nodes, edges } => {
            let jmap = new_hashmap(env)?;
            let node_list = new_arraylist(env, nodes.len() as i32)?;
            for n in nodes {
                let jn = cypher_value_to_java(env, n)?;
                list_add(env, &node_list, &jn)?;
            }
            let edge_list = new_arraylist(env, edges.len() as i32)?;
            for e in edges {
                let jedge = cypher_value_to_java(env, e)?;
                list_add(env, &edge_list, &jedge)?;
            }
            let k: JObject = env.new_string("nodes")?.into();
            map_put(env, &jmap, &k, &node_list)?;
            let k: JObject = env.new_string("edges")?.into();
            map_put(env, &jmap, &k, &edge_list)?;
            Ok(jmap)
        }
        CypherValue::Date(d) => Ok(env.new_string(d.to_string())?.into()),
        CypherValue::LocalTime(t) => Ok(env.new_string(t.to_string())?.into()),
        CypherValue::Time(t) => Ok(env.new_string(t.to_string())?.into()),
        CypherValue::LocalDateTime(dt) => Ok(env.new_string(dt.to_string())?.into()),
        CypherValue::DateTime(dt) => Ok(env.new_string(dt.to_string())?.into()),
        CypherValue::Duration(d) => Ok(env.new_string(d.to_string())?.into()),
        // BYTEARRAY -> java byte[] (mirrors the Python binding's bytes and
        // the Bolt driver's byte-array wire type).
        CypherValue::Bytes(b) => Ok(env.byte_array_from_slice(b)?.into()),
    }
}

fn property_value_to_java<'local>(
    env: &mut Env<'local>,
    val: &PropertyValue,
) -> TResult<JObject<'local>> {
    cypher_value_to_java(env, &CypherValue::from_property(val))
}

// ---------------------------------------------------------------------------
// Java Object -> CypherValue (for query parameters)
// ---------------------------------------------------------------------------

fn java_to_cypher_value(env: &mut Env, obj: &JObject) -> TResult<CypherValue> {
    if obj.is_null() {
        return Ok(CypherValue::Null);
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Boolean"))? {
        let v = env
            .call_method(obj, jni_str!("booleanValue"), &jni_sig!("()Z"), &[])?
            .z()?;
        return Ok(CypherValue::Bool(v));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Long"))? {
        let v = env
            .call_method(obj, jni_str!("longValue"), &jni_sig!("()J"), &[])?
            .j()?;
        return Ok(CypherValue::Integer(v));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Integer"))? {
        let v = env
            .call_method(obj, jni_str!("intValue"), &jni_sig!("()I"), &[])?
            .i()?;
        return Ok(CypherValue::Integer(v as i64));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Double"))? {
        let v = env
            .call_method(obj, jni_str!("doubleValue"), &jni_sig!("()D"), &[])?
            .d()?;
        return Ok(CypherValue::Float(v));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Float"))? {
        let v = env
            .call_method(obj, jni_str!("floatValue"), &jni_sig!("()F"), &[])?
            .f()?;
        return Ok(CypherValue::Float(v as f64));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/String"))? {
        let jstr = unsafe { obj_as_jstring(obj) };
        let s = get_rust_string(env, jstr)?;
        return Ok(CypherValue::String(s));
    }
    if env.is_instance_of(obj, jni_str!("java/util/List"))? {
        let size = env
            .call_method(obj, jni_str!("size"), &jni_sig!("()I"), &[])?
            .i()?;
        let mut items = Vec::with_capacity(size as usize);
        for i in 0..size {
            let item = env
                .call_method(
                    obj,
                    jni_str!("get"),
                    &jni_sig!("(I)Ljava/lang/Object;"),
                    &[JValue::Int(i)],
                )?
                .l()?;
            items.push(java_to_cypher_value(env, &item)?);
        }
        return Ok(CypherValue::List(items));
    }
    if env.is_instance_of(obj, jni_str!("java/util/Map"))? {
        return java_map_to_cypher_map(env, obj);
    }
    Err(terr("cannot convert Java object to Cypher value"))
}

fn java_map_to_cypher_map(env: &mut Env, map: &JObject) -> TResult<CypherValue> {
    let entry_set = env
        .call_method(
            map,
            jni_str!("entrySet"),
            &jni_sig!("()Ljava/util/Set;"),
            &[],
        )?
        .l()?;
    let iter = env
        .call_method(
            &entry_set,
            jni_str!("iterator"),
            &jni_sig!("()Ljava/util/Iterator;"),
            &[],
        )?
        .l()?;
    let mut result = BTreeMap::new();
    loop {
        let has_next = env
            .call_method(&iter, jni_str!("hasNext"), &jni_sig!("()Z"), &[])?
            .z()?;
        if !has_next {
            break;
        }
        let entry = env
            .call_method(
                &iter,
                jni_str!("next"),
                &jni_sig!("()Ljava/lang/Object;"),
                &[],
            )?
            .l()?;
        let key = env
            .call_method(
                &entry,
                jni_str!("getKey"),
                &jni_sig!("()Ljava/lang/Object;"),
                &[],
            )?
            .l()?;
        let val = env
            .call_method(
                &entry,
                jni_str!("getValue"),
                &jni_sig!("()Ljava/lang/Object;"),
                &[],
            )?
            .l()?;
        let jstr = unsafe { obj_as_jstring(&key) };
        let key_str = get_rust_string(env, jstr)?;
        result.insert(key_str, java_to_cypher_value(env, &val)?);
    }
    Ok(CypherValue::Map(result))
}

// ---------------------------------------------------------------------------
// Java Object -> PropertyValue (for bulk writer)
// ---------------------------------------------------------------------------

fn java_to_property_value(env: &mut Env, obj: &JObject) -> TResult<PropertyValue> {
    if obj.is_null() {
        return Ok(PropertyValue::Null);
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Boolean"))? {
        let v = env
            .call_method(obj, jni_str!("booleanValue"), &jni_sig!("()Z"), &[])?
            .z()?;
        return Ok(PropertyValue::Bool(v));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Long"))? {
        let v = env
            .call_method(obj, jni_str!("longValue"), &jni_sig!("()J"), &[])?
            .j()?;
        return Ok(PropertyValue::Int(v));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Integer"))? {
        let v = env
            .call_method(obj, jni_str!("intValue"), &jni_sig!("()I"), &[])?
            .i()?;
        return Ok(PropertyValue::Int(v as i64));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Double"))? {
        let v = env
            .call_method(obj, jni_str!("doubleValue"), &jni_sig!("()D"), &[])?
            .d()?;
        return Ok(PropertyValue::Float(v));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/Float"))? {
        let v = env
            .call_method(obj, jni_str!("floatValue"), &jni_sig!("()F"), &[])?
            .f()?;
        return Ok(PropertyValue::Float(v as f64));
    }
    if env.is_instance_of(obj, jni_str!("java/lang/String"))? {
        let jstr = unsafe { obj_as_jstring(obj) };
        let s = get_rust_string(env, jstr)?;
        return Ok(PropertyValue::String(s));
    }
    if env.is_instance_of(obj, jni_str!("java/util/List"))? {
        let size = env
            .call_method(obj, jni_str!("size"), &jni_sig!("()I"), &[])?
            .i()?;
        let mut items = Vec::with_capacity(size as usize);
        for i in 0..size {
            let item = env
                .call_method(
                    obj,
                    jni_str!("get"),
                    &jni_sig!("(I)Ljava/lang/Object;"),
                    &[JValue::Int(i)],
                )?
                .l()?;
            items.push(java_to_property_value(env, &item)?);
        }
        return Ok(PropertyValue::List(items));
    }
    Err(terr("cannot convert Java object to property value"))
}

// ---------------------------------------------------------------------------
// QueryResult construction
// ---------------------------------------------------------------------------

fn build_query_result<'local>(
    env: &mut Env<'local>,
    result: CypherResult,
) -> TResult<JObject<'local>> {
    let str_class = env.find_class(jni_str!("java/lang/String"))?;
    let col_count = result.columns.len();
    let columns_arr = env.new_object_array(col_count as i32, &str_class, JObject::null())?;
    for (i, col) in result.columns.iter().enumerate() {
        let s = env.new_string(col)?;
        columns_arr.set_element(env, i, &s)?;
    }

    let row_count = result.rows.len() as i32;
    let rows_list = new_arraylist(env, row_count)?;

    for row in &result.rows {
        let row_map = env.with_local_frame_returning_local::<_, JObject, TraverseError>(
            col_count + 4,
            |env| {
                let row_map = new_hashmap(env)?;
                for (col_name, val) in result.columns.iter().zip(row.iter()) {
                    let jk: JObject = env.new_string(col_name)?.into();
                    let jv = cypher_value_to_java(env, val)?;
                    map_put(env, &row_map, &jk, &jv)?;
                }
                Ok(row_map)
            },
        )?;
        list_add(env, &rows_list, &row_map)?;
    }

    let stats_map = build_stats_map(env, &result.stats)?;

    Ok(env.new_object(
        jni_str!("com/truespar/traverse/QueryResult"),
        &jni_sig!("([Ljava/lang/String;Ljava/util/List;Ljava/util/Map;)V"),
        &[
            JValue::Object(&columns_arr.into()),
            JValue::Object(&rows_list),
            JValue::Object(&stats_map),
        ],
    )?)
}

fn build_stats_map<'local>(env: &mut Env<'local>, stats: &QueryStats) -> TResult<JObject<'local>> {
    let map = new_hashmap(env)?;
    let mut add = |name: &str, val: i64| -> TResult<()> {
        if val != 0 {
            let k: JObject = env.new_string(name)?.into();
            let v = new_boxed_long(env, val)?;
            map_put(env, &map, &k, &v)?;
        }
        Ok(())
    };
    add("nodes_created", stats.nodes_created)?;
    add("nodes_deleted", stats.nodes_deleted)?;
    add("relationships_created", stats.relationships_created)?;
    add("relationships_deleted", stats.relationships_deleted)?;
    add("properties_set", stats.properties_set)?;
    add("labels_added", stats.labels_added)?;
    add("labels_removed", stats.labels_removed)?;
    add("indexes_added", stats.indexes_added)?;
    add("indexes_removed", stats.indexes_removed)?;
    add("constraints_added", stats.constraints_added)?;
    add("constraints_removed", stats.constraints_removed)?;
    Ok(map)
}

// ---------------------------------------------------------------------------
// Array extraction helpers
// ---------------------------------------------------------------------------

fn extract_string_array(env: &mut Env, arr: &JObject) -> TResult<Vec<String>> {
    if arr.is_null() {
        return Ok(Vec::new());
    }
    let jarr: &JObjectArray = unsafe { &*(arr as *const JObject as *const JObjectArray) };
    let len = jarr.len(env)?;
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let elem: JObject = jarr.get_element(env, i)?;
        if elem.is_null() {
            result.push(String::new());
        } else {
            let jstr = unsafe { obj_as_jstring(&elem) };
            result.push(get_rust_string(env, jstr)?);
        }
    }
    Ok(result)
}

fn extract_property_map(
    env: &mut Env,
    keys: &JObject,
    values: &JObject,
) -> TResult<BTreeMap<String, PropertyValue>> {
    if keys.is_null() || values.is_null() {
        return Ok(BTreeMap::new());
    }
    let key_strs = extract_string_array(env, keys)?;
    let val_arr: &JObjectArray = unsafe { &*(values as *const JObject as *const JObjectArray) };
    let val_len = val_arr.len(env)?;

    let mut map = BTreeMap::new();
    for (i, key) in key_strs.into_iter().enumerate() {
        if i >= val_len {
            break;
        }
        let val_obj: JObject = val_arr.get_element(env, i)?;
        let pv = java_to_property_value(env, &val_obj)?;
        if !matches!(pv, PropertyValue::Null) {
            map.insert(key, pv);
        }
    }
    Ok(map)
}

fn extract_params_map(
    env: &mut Env,
    keys: &JObject,
    values: &JObject,
) -> TResult<Option<HashMap<String, CypherValue>>> {
    if keys.is_null() || values.is_null() {
        return Ok(None);
    }
    let key_strs = extract_string_array(env, keys)?;
    let val_arr: &JObjectArray = unsafe { &*(values as *const JObject as *const JObjectArray) };
    let val_len = val_arr.len(env)?;

    let mut map = HashMap::new();
    for (i, key) in key_strs.into_iter().enumerate() {
        if i >= val_len {
            break;
        }
        let val_obj: JObject = val_arr.get_element(env, i)?;
        map.insert(key, java_to_cypher_value(env, &val_obj)?);
    }
    Ok(Some(map))
}

// ===========================================================================
// JNI EXPORTS - Database
// ===========================================================================

/// Exports `nativeFree` for a wrapper class whose handles box `$handle`.
macro_rules! native_free {
    ($name:ident, $handle:ty) => {
        #[unsafe(no_mangle)]
        pub extern "system" fn $name<'local>(
            mut unowned: EnvUnowned<'local>,
            _class: JClass<'local>,
            handle: jlong,
        ) {
            unowned
                .with_env(|_env| -> TResult<()> {
                    unsafe { handle_drop::<$handle>(handle) };
                    Ok(())
                })
                .resolve::<ThrowTraverseExAndDefault>()
        }
    };
}

native_free!(
    Java_com_truespar_traverse_Database_nativeFree,
    DatabaseHandle
);
native_free!(
    Java_com_truespar_traverse_Transaction_nativeFree,
    TransactionHandle
);
native_free!(
    Java_com_truespar_traverse_BulkWriter_nativeFree,
    BulkWriterHandle
);

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Database_nativeOpen<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    path: JString<'local>,
) -> jlong {
    unowned
        .with_env(|env| -> TResult<jlong> {
            let path_str = get_rust_string(env, &path)?;
            let db = SharedDatabase::open(&path_str).map_err(core_err)?;
            Ok(to_handle(DatabaseHandle {
                db,
                plans: Arc::default(),
            }))
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Database_nativeFlush<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    unowned
        .with_env(|_env| -> TResult<()> {
            let h = unsafe { handle_ref::<DatabaseHandle>(handle) };
            h.db.flush().map_err(core_err)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Database_nativeSetBusyTimeout<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    timeout_ms: jlong,
) {
    unowned
        .with_env(|_env| -> TResult<()> {
            let h = unsafe { handle_ref::<DatabaseHandle>(handle) };
            let timeout = u64::try_from(timeout_ms).map_or(Duration::MAX, Duration::from_millis);
            h.db.set_busy_timeout(timeout);
            Ok(())
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Database_nativeSetStatementTimeout<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    timeout_ms: jlong,
) {
    unowned
        .with_env(|_env| -> TResult<()> {
            let h = unsafe { handle_ref::<DatabaseHandle>(handle) };
            let timeout = u64::try_from(timeout_ms)
                .ok()
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis);
            h.db.set_statement_timeout(timeout);
            Ok(())
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Database_nativeClose<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    unowned
        .with_env(|_env| -> TResult<()> {
            let h = unsafe { handle_ref::<DatabaseHandle>(handle) };
            h.db.close().map_err(core_err)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Database_nativeBegin<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jlong {
    unowned
        .with_env(|_env| -> TResult<jlong> {
            let h = unsafe { handle_ref::<DatabaseHandle>(handle) };
            let lease = h.db.begin_write().map_err(core_err)?;
            Ok(to_handle(TransactionHandle::new(h, lease)))
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Database_nativeBeginRead<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jlong {
    unowned
        .with_env(|_env| -> TResult<jlong> {
            let h = unsafe { handle_ref::<DatabaseHandle>(handle) };
            let lease = h.db.begin_read().map_err(core_err)?;
            Ok(to_handle(TransactionHandle::new(h, lease)))
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Database_nativeBulkWriter<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jlong {
    unowned
        .with_env(|_env| -> TResult<jlong> {
            let h = unsafe { handle_ref::<DatabaseHandle>(handle) };
            let lease = h.db.begin_bulk_load().map_err(core_err)?;
            Ok(to_handle(BulkWriterHandle { lease }))
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

// ===========================================================================
// JNI EXPORTS - Transaction
// ===========================================================================

/// Runs a query on the transaction behind `handle`, then builds its Java result (after the
/// transaction is free for other threads again).
fn execute_on<'local>(
    env: &mut Env<'local>,
    handle: jlong,
    query: &JString<'local>,
    param_keys: &JObject<'local>,
    param_values: &JObject<'local>,
    gql: bool,
) -> TResult<JObject<'local>> {
    let query_str = get_rust_string(env, query)?;
    let params: Parameters =
        Arc::new(extract_params_map(env, param_keys, param_values)?.unwrap_or_default());
    let h = unsafe { handle_ref::<TransactionHandle>(handle) };
    let result = h
        .lease
        .with(|tx| {
            tx.statement(|view| {
                // listed, limited by the statement timeout, stoppable by `cancel()`
                let shown = (!params.is_empty()).then(|| Arc::clone(&params));
                h.embedded.run(&query_str, shown, |_| {
                    if gql {
                        traverse_gql::execute_gql_on(
                            view,
                            h.embedded.database(),
                            &query_str,
                            &params,
                        )
                    } else {
                        traverse_cypher::plan_cache::execute(&h.plans, view, &query_str, &params)
                    }
                })
            })
        })
        .map_err(core_err)?
        .map_err(cypher_err)?;
    build_query_result(env, result)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Database_nativeExecute<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    query: JString<'local>,
    param_keys: JObject<'local>,
    param_values: JObject<'local>,
    gql: jboolean,
) -> JObject<'local> {
    unowned
        .with_env(|env| -> TResult<JObject<'local>> {
            let query_str = get_rust_string(env, &query)?;
            let params: Parameters =
                Arc::new(extract_params_map(env, &param_keys, &param_values)?.unwrap_or_default());
            let h = unsafe { handle_ref::<DatabaseHandle>(handle) };
            let result = if gql {
                let shown = (!params.is_empty()).then(|| Arc::clone(&params));
                traverse_cypher::embedded::auto_commit(
                    &h.db,
                    &query_str,
                    shown,
                    None,
                    |_| traverse_gql::query_writes(&query_str),
                    |view| traverse_gql::execute_gql_on(view, &h.db.name(), &query_str, &params),
                )
            } else {
                traverse_cypher::embedded::execute_auto_commit(
                    &h.db, &h.plans, &query_str, &params, None,
                )
            };
            build_query_result(env, result.map_err(cypher_err)?)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Transaction_nativeExecute<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    query: JString<'local>,
    param_keys: JObject<'local>,
    param_values: JObject<'local>,
) -> JObject<'local> {
    unowned
        .with_env(|env| -> TResult<JObject<'local>> {
            execute_on(env, handle, &query, &param_keys, &param_values, false)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Transaction_nativeExecuteGql<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    query: JString<'local>,
    param_keys: JObject<'local>,
    param_values: JObject<'local>,
) -> JObject<'local> {
    unowned
        .with_env(|env| -> TResult<JObject<'local>> {
            execute_on(env, handle, &query, &param_keys, &param_values, true)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Transaction_nativeCommit<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    unowned
        .with_env(|_env| -> TResult<()> {
            let h = unsafe { handle_ref::<TransactionHandle>(handle) };
            // terminated (TERMINATE TRANSACTIONS): rolled back instead
            if let Err(e) = h.embedded.check_live() {
                abort_lease(&h.lease, Transaction::abort)?;
                return Err(cypher_err(e));
            }
            h.lease
                .finish(Transaction::commit)
                .and_then(|r| r)
                .map_err(core_err)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Transaction_nativeCancel<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    unowned
        .with_env(|_env| -> TResult<()> {
            let h = unsafe { handle_ref::<TransactionHandle>(handle) };
            traverse_cypher::embedded::cancel(&h.embedded.handle());
            Ok(())
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_Transaction_nativeAbort<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    unowned
        .with_env(|_env| -> TResult<()> {
            let h = unsafe { handle_ref::<TransactionHandle>(handle) };
            abort_lease(&h.lease, Transaction::abort)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

// ===========================================================================
// JNI EXPORTS - BulkWriter
// ===========================================================================

/// Runs `f` on the bulk load behind `handle`.
fn with_load<R>(
    handle: jlong,
    f: impl FnOnce(&mut BulkLoadTx) -> traverse_core::error::Result<R>,
) -> TResult<R> {
    let h = unsafe { handle_ref::<BulkWriterHandle>(handle) };
    h.lease.with(f).and_then(|r| r).map_err(core_err)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_BulkWriter_nativeCreateNode<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    labels: JObject<'local>,
    prop_keys: JObject<'local>,
    prop_values: JObject<'local>,
) -> jlong {
    unowned
        .with_env(|env| -> TResult<jlong> {
            let label_strs = extract_string_array(env, &labels)?;
            let props = extract_property_map(env, &prop_keys, &prop_values)?;
            let label_refs: Vec<&str> = label_strs.iter().map(|s| s.as_str()).collect();
            with_load(handle, |tx| tx.create_node(&label_refs, props)).map(|id| id.0 as i64)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_BulkWriter_nativeCreateEdge<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    source: jlong,
    target: jlong,
    edge_type: JString<'local>,
    prop_keys: JObject<'local>,
    prop_values: JObject<'local>,
) -> jlong {
    unowned
        .with_env(|env| -> TResult<jlong> {
            let edge_type_str = get_rust_string(env, &edge_type)?;
            let props = extract_property_map(env, &prop_keys, &prop_values)?;
            let (src, tgt) = (NodeId(source as u64), NodeId(target as u64));
            with_load(handle, |tx| tx.create_edge(src, tgt, &edge_type_str, props))
                .map(|id| id.0 as i64)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_BulkWriter_nativeCreateIndex<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    name: JString<'local>,
    label: JString<'local>,
    property: JString<'local>,
) {
    unowned
        .with_env(|env| -> TResult<()> {
            let name_str = get_rust_string(env, &name)?;
            let label_str = get_rust_string(env, &label)?;
            let prop_str = get_rust_string(env, &property)?;
            with_load(handle, |tx| {
                tx.create_index(&name_str, &label_str, &prop_str)
            })
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_BulkWriter_nativeCreateEdgeIndex<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    name: JString<'local>,
    edge_type: JString<'local>,
    property: JObject<'local>,
) {
    unowned
        .with_env(|env| -> TResult<()> {
            let name_str = get_rust_string(env, &name)?;
            let etype_str = get_rust_string(env, &edge_type)?;
            let prop_str = if property.is_null() {
                None
            } else {
                let jstr = unsafe { obj_as_jstring(&property) };
                Some(get_rust_string(env, jstr)?)
            };
            with_load(handle, |tx| {
                tx.create_edge_index(&name_str, &etype_str, prop_str.as_deref())
            })
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_BulkWriter_nativeDropEdgeIndex<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    name: JString<'local>,
) {
    unowned
        .with_env(|env| -> TResult<()> {
            let name_str = get_rust_string(env, &name)?;
            with_load(handle, |tx| tx.drop_edge_index(&name_str))
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_BulkWriter_nativeCommit<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    unowned
        .with_env(|_env| -> TResult<()> {
            let h = unsafe { handle_ref::<BulkWriterHandle>(handle) };
            h.lease
                .finish(|tx| tx.commit().map(drop))
                .and_then(|r| r)
                .map_err(core_err)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_truespar_traverse_BulkWriter_nativeAbort<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    unowned
        .with_env(|_env| -> TResult<()> {
            let h = unsafe { handle_ref::<BulkWriterHandle>(handle) };
            abort_lease(&h.lease, BulkLoadTx::abort)
        })
        .resolve::<ThrowTraverseExAndDefault>()
}
