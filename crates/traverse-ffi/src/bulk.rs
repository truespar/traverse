//! FFI functions for bulk loading.
//!
//! A bulk writer is a write transaction that skips the redo log: its commit saves the database
//! with the load in it and returns once that save is durable, so the data is written once.
//! Readers don't see the load until commit; abort (or a failed commit) rolls it back. It holds
//! the writer lock while open, like any write transaction.
//!
//! Properties are passed as parallel arrays of C strings with automatic type
//! inference (same rules as traverse-admin CSV import).

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;

use traverse_core::error::Result as CoreResult;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId;
use traverse_core::storage::database::BulkLoadTx;
use traverse_core::tx::GraphView;

use crate::error::{self, TvError};
use crate::handles::{BulkWriterInner, DatabaseInner, TvBulkWriter, TvDatabase};

// ---------------------------------------------------------------------------
// Property type inference
// ---------------------------------------------------------------------------

/// Infer a [`PropertyValue`] from a string, using the same rules as
/// traverse-admin CSV import: empty -> Null, true/false -> Bool,
/// integer -> Int, decimal -> Float, otherwise String.
fn infer_value(s: &str) -> PropertyValue {
    if s.is_empty() {
        return PropertyValue::Null;
    }
    match s {
        "true" | "TRUE" | "True" => return PropertyValue::Bool(true),
        "false" | "FALSE" | "False" => return PropertyValue::Bool(false),
        _ => {}
    }
    if let Ok(i) = s.parse::<i64>() {
        return PropertyValue::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return PropertyValue::Float(f);
    }
    PropertyValue::String(s.to_string())
}

/// Parse parallel C-string arrays into a `BTreeMap<String, PropertyValue>`.
///
/// # Safety
///
/// `keys` and `values` must each point to at least `count` valid,
/// NUL-terminated UTF-8 strings (or be null when `count == 0`).
unsafe fn parse_properties(
    keys: *const *const c_char,
    values: *const *const c_char,
    count: i32,
) -> Result<BTreeMap<String, PropertyValue>, &'static str> {
    let mut props = BTreeMap::new();
    if count <= 0 {
        return Ok(props);
    }
    if keys.is_null() || values.is_null() {
        return Err("prop_keys or prop_values is null");
    }
    for i in 0..count as usize {
        let key_ptr = unsafe { *keys.add(i) };
        let val_ptr = unsafe { *values.add(i) };
        if key_ptr.is_null() || val_ptr.is_null() {
            return Err("null pointer in prop_keys or prop_values array");
        }
        let key = unsafe { CStr::from_ptr(key_ptr) }
            .to_str()
            .map_err(|_| "invalid UTF-8 in property key")?;
        let val = unsafe { CStr::from_ptr(val_ptr) }
            .to_str()
            .map_err(|_| "invalid UTF-8 in property value")?;
        let pv = infer_value(val);
        if !matches!(pv, PropertyValue::Null) {
            props.insert(key.to_string(), pv);
        }
    }
    Ok(props)
}

/// A NUL-terminated UTF-8 string argument named `what`.
///
/// # Safety
///
/// `ptr` is null or a valid NUL-terminated string.
unsafe fn arg_str<'a>(ptr: *const c_char, what: &str) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err(format!("{what} is null"));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| format!("invalid UTF-8 in {what}"))
}

/// `count` label strings.
///
/// # Safety
///
/// `labels` points to at least `count` valid NUL-terminated strings (or is null when `count <= 0`).
unsafe fn parse_labels<'a>(
    labels: *const *const c_char,
    count: i32,
) -> Result<Vec<&'a str>, String> {
    let count = count.max(0) as usize;
    if count > 0 && labels.is_null() {
        return Err("labels is null".into());
    }
    (0..count)
        .map(|i| unsafe { arg_str(*labels.add(i), "label") })
        .collect()
}

/// Runs `f` on the bulk load behind `writer`. On failure (null handle, finished load, closed
/// database, or `f`'s error) records the last error and returns its code.
///
/// # Safety
///
/// `writer` is null or a live handle from [`traverse_bulk_begin`].
unsafe fn with_load<R>(
    writer: *mut TvBulkWriter,
    f: impl FnOnce(&mut BulkLoadTx) -> CoreResult<R>,
) -> Result<R, i32> {
    if writer.is_null() {
        return Err(invalid("writer handle is null"));
    }
    let inner = unsafe { &*(writer as *const BulkWriterInner) };
    inner
        .lease
        .with(f)
        .and_then(|r| r)
        .map_err(|e| error::set_core_error(&e) as i32)
}

/// Records an argument error; returns its code.
fn invalid(msg: &str) -> i32 {
    error::set_last_error(TvError::TV_ERROR_INVALID_ARG, msg);
    TvError::TV_ERROR_INVALID_ARG as i32
}

/// `TV_OK` or the error code.
fn status(r: Result<(), i32>) -> i32 {
    r.map_or_else(|code| code, |()| TvError::TV_OK as i32)
}

// ---------------------------------------------------------------------------
// FFI exports
// ---------------------------------------------------------------------------

/// Begin a bulk load: a write transaction that skips the redo log. [`traverse_bulk_commit`] saves
/// the database with the load in it, so the data is written once; readers don't see the load until
/// then, and [`traverse_bulk_abort`] rolls it back. Waits for an active write transaction at most
/// the busy timeout (see `traverse_set_busy_timeout`), then fails with `TV_ERROR_BUSY`.
///
/// Returns a non-null handle on success, or `NULL` on error.
///
/// # Safety
///
/// `db` must be a valid, non-null [`TvDatabase`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_bulk_begin(db: *mut TvDatabase) -> *mut TvBulkWriter {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            if db.is_null() {
                invalid("db handle is null");
                return std::ptr::null_mut();
            }
            let db_inner = unsafe { &*(db as *const DatabaseInner) };
            match db_inner.db.begin_bulk_load() {
                Ok(lease) => {
                    Box::into_raw(Box::new(BulkWriterInner { lease })) as *mut TvBulkWriter
                }
                Err(e) => {
                    error::set_core_error(&e);
                    std::ptr::null_mut()
                }
            }
        }),
    )
}

/// Create a node via the bulk writer.
///
/// Returns the new node ID (>= 0) on success, or `-1` on error.
///
/// `labels` must point to `label_count` valid NUL-terminated UTF-8 strings.
/// `prop_keys` and `prop_values` are parallel arrays of `prop_count` strings;
/// values are auto-inferred (integer, float, bool, or string).
///
/// # Safety
///
/// `writer` must be a valid handle returned by [`traverse_bulk_begin`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_bulk_create_node(
    writer: *mut TvBulkWriter,
    labels: *const *const c_char,
    label_count: i32,
    prop_keys: *const *const c_char,
    prop_values: *const *const c_char,
    prop_count: i32,
) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let label_strs = match unsafe { parse_labels(labels, label_count) } {
                Ok(l) => l,
                Err(msg) => {
                    invalid(&msg);
                    return -1;
                }
            };
            let props = match unsafe { parse_properties(prop_keys, prop_values, prop_count) } {
                Ok(p) => p,
                Err(msg) => {
                    invalid(msg);
                    return -1;
                }
            };
            unsafe { with_load(writer, |tx| tx.create_node(&label_strs, props)) }
                .map_or(-1, |id| id.0 as i64)
        }),
    )
}

/// Create an edge via the bulk writer.
///
/// Returns the new edge ID (>= 0) on success, or `-1` on error (e.g. source
/// or target node not found).
///
/// # Safety
///
/// `writer` must be a valid handle returned by [`traverse_bulk_begin`].
/// `edge_type` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_bulk_create_edge(
    writer: *mut TvBulkWriter,
    source: i64,
    target: i64,
    edge_type: *const c_char,
    prop_keys: *const *const c_char,
    prop_values: *const *const c_char,
    prop_count: i32,
) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let et = match unsafe { arg_str(edge_type, "edge_type") } {
                Ok(s) => s,
                Err(msg) => {
                    invalid(&msg);
                    return -1;
                }
            };
            let props = match unsafe { parse_properties(prop_keys, prop_values, prop_count) } {
                Ok(p) => p,
                Err(msg) => {
                    invalid(msg);
                    return -1;
                }
            };
            let (src, tgt) = (NodeId(source as u64), NodeId(target as u64));
            unsafe { with_load(writer, |tx| tx.create_edge(src, tgt, et, props)) }
                .map_or(-1, |id| id.0 as i64)
        }),
    )
}

/// Create an index via the bulk writer.
///
/// Returns `TV_OK` (0) on success, or an error code on failure (e.g.
/// duplicate index name).
///
/// # Safety
///
/// `writer` must be a valid handle returned by [`traverse_bulk_begin`].
/// `name`, `label`, and `property` must be valid NUL-terminated UTF-8 strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_bulk_create_index(
    writer: *mut TvBulkWriter,
    name: *const c_char,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            let args = unsafe {
                arg_str(name, "index name")
                    .and_then(|n| Ok((n, arg_str(label, "label")?, arg_str(property, "property")?)))
            };
            let (name, label, property) = match args {
                Ok(a) => a,
                Err(msg) => return invalid(&msg),
            };
            status(unsafe { with_load(writer, |tx| tx.create_index(name, label, property)) })
        }),
    )
}

/// Create an edge index via the bulk writer.
///
/// For a **property index**, pass all three strings (`name`, `edge_type`,
/// `property`).  For a **type-only index**, pass `property` as `NULL`.
///
/// Returns `TV_OK` (0) on success, or an error code on failure.
///
/// # Safety
///
/// `writer` must be a valid handle returned by [`traverse_bulk_begin`].
/// `name` and `edge_type` must be valid NUL-terminated UTF-8 strings.
/// `property` may be `NULL` (type-only index) or a valid NUL-terminated
/// UTF-8 string (property index).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_bulk_create_edge_index(
    writer: *mut TvBulkWriter,
    name: *const c_char,
    edge_type: *const c_char,
    property: *const c_char,
) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            let args = unsafe {
                arg_str(name, "index name").and_then(|n| {
                    let property = if property.is_null() {
                        None
                    } else {
                        Some(arg_str(property, "property")?)
                    };
                    Ok((n, arg_str(edge_type, "edge_type")?, property))
                })
            };
            let (name, edge_type, property) = match args {
                Ok(a) => a,
                Err(msg) => return invalid(&msg),
            };
            status(unsafe {
                with_load(writer, |tx| tx.create_edge_index(name, edge_type, property))
            })
        }),
    )
}

/// Drop an edge index by name via the bulk writer.
///
/// Returns `TV_OK` (0) on success, or an error code on failure (e.g.
/// index not found).
///
/// # Safety
///
/// `writer` must be a valid handle returned by [`traverse_bulk_begin`].
/// `name` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_bulk_drop_edge_index(
    writer: *mut TvBulkWriter,
    name: *const c_char,
) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            let name = match unsafe { arg_str(name, "index name") } {
                Ok(s) => s,
                Err(msg) => return invalid(&msg),
            };
            status(unsafe { with_load(writer, |tx| tx.drop_edge_index(name)) })
        }),
    )
}

/// Commit the bulk load and free the handle: saves the database with the load in it, then makes
/// the load visible. Durable once this returns `TV_OK` (0); on error nothing was published and the
/// load is rolled back. After this call the handle is invalid.
///
/// # Safety
///
/// `writer` must be a valid handle returned by [`traverse_bulk_begin`]
/// that has not already been committed or aborted, and no other call may be using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_bulk_commit(writer: *mut TvBulkWriter) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            if writer.is_null() {
                return invalid("writer handle is null");
            }
            let inner = unsafe { Box::from_raw(writer as *mut BulkWriterInner) };
            match inner.lease.finish(BulkLoadTx::commit).and_then(|r| r) {
                Ok(_) => TvError::TV_OK as i32,
                Err(e) => error::set_core_error(&e) as i32,
            }
        }),
    )
}

/// Abort the bulk load, rolling it back, and free the handle.
///
/// Returns `TV_OK` (0).  After this call the handle is invalid.
///
/// # Safety
///
/// `writer` must be a valid handle returned by [`traverse_bulk_begin`]
/// that has not already been committed or aborted, and no other call may be using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_bulk_abort(writer: *mut TvBulkWriter) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            if writer.is_null() {
                return invalid("writer handle is null");
            }
            let inner = unsafe { Box::from_raw(writer as *mut BulkWriterInner) };
            // finished or rolled back by a close already: nothing left to undo
            let _ = inner.lease.finish(BulkLoadTx::abort);
            TvError::TV_OK as i32
        }),
    )
}
