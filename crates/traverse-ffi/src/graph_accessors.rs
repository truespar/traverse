//! FFI functions for structured access to Node, Edge, and Path values.
//!
//! These complement `traverse_result_get_string` by exposing individual
//! fields (id, labels, properties, type, source, target) so callers don't
//! have to parse the `Display` representation.

use std::ffi::CString;
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;

use traverse_cypher::types::CypherValue;

use crate::error::{self, TvError};
use crate::handles::{ResultInner, TvResult};
use crate::result::current_cell;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Safely dereference a *mutable* `TvResult` pointer.
unsafe fn deref_result_mut<'a>(r: *mut TvResult) -> Option<&'a mut ResultInner> {
    if r.is_null() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "result handle is null");
        return None;
    }
    Some(unsafe { &mut *(r as *mut ResultInner) })
}

/// Safely dereference a `TvResult` pointer (immutable).
unsafe fn deref_result<'a>(r: *const TvResult) -> Option<&'a ResultInner> {
    if r.is_null() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "result handle is null");
        return None;
    }
    Some(unsafe { &*(r as *const ResultInner) })
}

/// Format a `PropertyValue` as a Cypher-like string for FFI consumption.
fn property_value_to_string(val: &traverse_core::graph::property::PropertyValue) -> String {
    use traverse_core::graph::property::PropertyValue;
    match val {
        PropertyValue::Null => "null".to_string(),
        PropertyValue::Bool(b) => b.to_string(),
        PropertyValue::Int(i) => i.to_string(),
        PropertyValue::Float(f) => format!("{f}"),
        PropertyValue::String(s) => s.clone(),
        PropertyValue::Bytes(b) => format!("{b:?}"),
        PropertyValue::List(items) => {
            let inner: Vec<String> = items.iter().map(property_value_to_string).collect();
            format!("[{}]", inner.join(", "))
        }
        PropertyValue::Map(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}: {}", property_value_to_string(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        PropertyValue::DateTime(ms) => format!("datetime({ms})"),
        PropertyValue::Duration(ms) => format!("duration({ms})"),
        PropertyValue::Point2D { x, y, srid } => {
            format!("point({{x: {x}, y: {y}, srid: {srid}}})")
        }
        PropertyValue::Point3D { x, y, z, srid } => {
            format!("point({{x: {x}, y: {y}, z: {z}, srid: {srid}}})")
        }
        PropertyValue::Vector(v) => format!("{v:?}"),
    }
}

/// Cache a string in the graph_string_cache and return a pointer to it.
/// `col` and `sub_index` together form the cache key.
fn cache_string(
    inner: &mut ResultInner,
    col: usize,
    sub_index: usize,
    value: String,
) -> *const c_char {
    let key = (col, sub_index);
    let cstring = CString::new(value).unwrap_or_default();
    let ptr = cstring.as_ptr();
    inner.graph_string_cache.insert(key, cstring);
    ptr
}

// ---------------------------------------------------------------------------
// Node accessors
// ---------------------------------------------------------------------------

/// Return the internal ID of a Node value at `col`.
///
/// Returns -1 and sets an error if the value is not a Node.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_node_id(result: *const TvResult, col: i32) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return -1,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return -1,
            };
            match val {
                CypherValue::Node { id, .. } => id.0 as i64,
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected NODE");
                    -1
                }
            }
        }),
    )
}

/// Return the number of labels on a Node value at `col`.
///
/// Returns -1 and sets an error if the value is not a Node.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_node_label_count(
    result: *const TvResult,
    col: i32,
) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return -1,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return -1,
            };
            match val {
                CypherValue::Node { labels, .. } => labels.len() as i32,
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected NODE");
                    -1
                }
            }
        }),
    )
}

/// Return a label name from a Node value at `col` by label index.
///
/// The returned pointer is valid until the next `traverse_result_next` or
/// `traverse_result_free` call.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_node_label(
    result: *mut TvResult,
    col: i32,
    label_idx: i32,
) -> *const c_char {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null(),
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result_mut(result) } {
                Some(i) => i,
                None => return std::ptr::null(),
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return std::ptr::null(),
            };
            match val {
                CypherValue::Node { labels, .. } => {
                    if label_idx < 0 || label_idx as usize >= labels.len() {
                        error::set_last_error(
                            TvError::TV_ERROR_INVALID_ARG,
                            "label index out of bounds",
                        );
                        return std::ptr::null();
                    }
                    let label = labels[label_idx as usize].clone();
                    // sub_index: 1000 + label_idx to avoid colliding with prop keys
                    cache_string(inner, col as usize, 1000 + label_idx as usize, label)
                }
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected NODE");
                    std::ptr::null()
                }
            }
        }),
    )
}

/// Return the number of properties on a Node or Edge value at `col`.
///
/// Returns -1 and sets an error if the value is not a Node or Edge.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_node_prop_count(
    result: *const TvResult,
    col: i32,
) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return -1,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return -1,
            };
            match val {
                CypherValue::Node { properties, .. } | CypherValue::Edge { properties, .. } => {
                    properties.len() as i32
                }
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected NODE or RELATIONSHIP");
                    -1
                }
            }
        }),
    )
}

/// Return a property key from a Node or Edge value at `col` by property index.
///
/// Properties are iterated in sorted (BTreeMap) order. The returned pointer
/// is valid until the next `traverse_result_next` or `traverse_result_free`.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_node_prop_key(
    result: *mut TvResult,
    col: i32,
    prop_idx: i32,
) -> *const c_char {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null(),
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result_mut(result) } {
                Some(i) => i,
                None => return std::ptr::null(),
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return std::ptr::null(),
            };
            let props = match val {
                CypherValue::Node { properties, .. } | CypherValue::Edge { properties, .. } => {
                    properties
                }
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected NODE or RELATIONSHIP");
                    return std::ptr::null();
                }
            };
            if prop_idx < 0 || prop_idx as usize >= props.len() {
                error::set_last_error(
                    TvError::TV_ERROR_INVALID_ARG,
                    "property index out of bounds",
                );
                return std::ptr::null();
            }
            let key = props.keys().nth(prop_idx as usize).unwrap().clone();
            // sub_index: 2000 + prop_idx for keys
            cache_string(inner, col as usize, 2000 + prop_idx as usize, key)
        }),
    )
}

/// Return a property value (as a string) from a Node or Edge at `col` by
/// property index.
///
/// The returned pointer is valid until the next `traverse_result_next` or
/// `traverse_result_free`.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_node_prop_value(
    result: *mut TvResult,
    col: i32,
    prop_idx: i32,
) -> *const c_char {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null(),
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result_mut(result) } {
                Some(i) => i,
                None => return std::ptr::null(),
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return std::ptr::null(),
            };
            let props = match val {
                CypherValue::Node { properties, .. } | CypherValue::Edge { properties, .. } => {
                    properties
                }
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected NODE or RELATIONSHIP");
                    return std::ptr::null();
                }
            };
            if prop_idx < 0 || prop_idx as usize >= props.len() {
                error::set_last_error(
                    TvError::TV_ERROR_INVALID_ARG,
                    "property index out of bounds",
                );
                return std::ptr::null();
            }
            let value = props.values().nth(prop_idx as usize).unwrap();
            let s = property_value_to_string(value);
            // sub_index: 3000 + prop_idx for values
            cache_string(inner, col as usize, 3000 + prop_idx as usize, s)
        }),
    )
}

// ---------------------------------------------------------------------------
// Edge accessors
// ---------------------------------------------------------------------------

/// Return the internal ID of an Edge value at `col`.
///
/// Returns -1 and sets an error if the value is not an Edge.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_edge_id(result: *const TvResult, col: i32) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return -1,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return -1,
            };
            match val {
                CypherValue::Edge { id, .. } => id.0 as i64,
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected RELATIONSHIP");
                    -1
                }
            }
        }),
    )
}

/// Return the type name of an Edge value at `col`.
///
/// The returned pointer is valid until the next `traverse_result_next` or
/// `traverse_result_free`.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_edge_type_name(
    result: *mut TvResult,
    col: i32,
) -> *const c_char {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null(),
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result_mut(result) } {
                Some(i) => i,
                None => return std::ptr::null(),
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return std::ptr::null(),
            };
            match val {
                CypherValue::Edge { edge_type, .. } => {
                    // sub_index: 4000 for edge type name
                    cache_string(inner, col as usize, 4000, edge_type.clone())
                }
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected RELATIONSHIP");
                    std::ptr::null()
                }
            }
        }),
    )
}

/// Return the source node ID of an Edge value at `col`.
///
/// Returns -1 and sets an error if the value is not an Edge.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_edge_source(result: *const TvResult, col: i32) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return -1,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return -1,
            };
            match val {
                CypherValue::Edge { source, .. } => source.0 as i64,
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected RELATIONSHIP");
                    -1
                }
            }
        }),
    )
}

/// Return the target node ID of an Edge value at `col`.
///
/// Returns -1 and sets an error if the value is not an Edge.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_edge_target(result: *const TvResult, col: i32) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return -1,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return -1,
            };
            match val {
                CypherValue::Edge { target, .. } => target.0 as i64,
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected RELATIONSHIP");
                    -1
                }
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Path accessors
// ---------------------------------------------------------------------------

/// Return the length (number of edges) of a Path value at `col`.
///
/// Returns -1 and sets an error if the value is not a Path.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_path_length(result: *const TvResult, col: i32) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return -1,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return -1,
            };
            match val {
                CypherValue::Path { edges, .. } => edges.len() as i32,
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected PATH");
                    -1
                }
            }
        }),
    )
}

/// Return the node ID at position `idx` in a Path value at `col`.
///
/// A path of length N has N+1 nodes (indices 0..=N).
///
/// Returns -1 and sets an error if the value is not a Path or index is out
/// of bounds.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_path_node_id(
    result: *const TvResult,
    col: i32,
    idx: i32,
) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return -1,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return -1,
            };
            match val {
                CypherValue::Path { nodes, .. } => {
                    if idx < 0 || idx as usize >= nodes.len() {
                        error::set_last_error(
                            TvError::TV_ERROR_INVALID_ARG,
                            "path node index out of bounds",
                        );
                        return -1;
                    }
                    match &nodes[idx as usize] {
                        CypherValue::Node { id, .. } => id.0 as i64,
                        _ => {
                            error::set_last_error(
                                TvError::TV_ERROR_INTERNAL,
                                "path node is not a Node",
                            );
                            -1
                        }
                    }
                }
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected PATH");
                    -1
                }
            }
        }),
    )
}

/// Return the edge ID at position `idx` in a Path value at `col`.
///
/// A path of length N has N edges (indices 0..N).
///
/// Returns -1 and sets an error if the value is not a Path or index is out
/// of bounds.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_path_edge_id(
    result: *const TvResult,
    col: i32,
    idx: i32,
) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        -1,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return -1,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return -1,
            };
            match val {
                CypherValue::Path { edges, .. } => {
                    if idx < 0 || idx as usize >= edges.len() {
                        error::set_last_error(
                            TvError::TV_ERROR_INVALID_ARG,
                            "path edge index out of bounds",
                        );
                        return -1;
                    }
                    match &edges[idx as usize] {
                        CypherValue::Edge { id, .. } => id.0 as i64,
                        _ => {
                            error::set_last_error(
                                TvError::TV_ERROR_INTERNAL,
                                "path edge is not an Edge",
                            );
                            -1
                        }
                    }
                }
                _ => {
                    error::set_last_error(TvError::TV_ERROR_TYPE, "expected PATH");
                    -1
                }
            }
        }),
    )
}
