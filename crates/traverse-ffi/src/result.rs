//! FFI functions for reading query results.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;

use traverse_cypher::types::CypherValue;

use crate::error::{self, TvError};
use crate::handles::{ResultInner, TvResult};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Safely dereference an `TvResult` pointer, returning `None` (and setting an
/// error) when it is null.
unsafe fn deref_result<'a>(r: *const TvResult) -> Option<&'a ResultInner> {
    if r.is_null() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "result handle is null");
        return None;
    }
    Some(unsafe { &*(r as *const ResultInner) })
}

/// Safely dereference a *mutable* `TvResult` pointer.
unsafe fn deref_result_mut<'a>(r: *mut TvResult) -> Option<&'a mut ResultInner> {
    if r.is_null() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "result handle is null");
        return None;
    }
    Some(unsafe { &mut *(r as *mut ResultInner) })
}

/// Get the [`CypherValue`] at the current cursor row and the given column,
/// or `None` (with error set) if out of bounds.
pub(crate) fn current_cell(inner: &ResultInner, col: i32) -> Option<&CypherValue> {
    if inner.cursor < 0 || inner.cursor as usize >= inner.rows.len() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "cursor out of bounds");
        return None;
    }
    let row = &inner.rows[inner.cursor as usize];
    if col < 0 || col as usize >= row.len() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "column index out of bounds");
        return None;
    }
    Some(&row[col as usize])
}

/// Map a `CypherValue` to a NUL-terminated static C string for its type name.
fn type_name_cstr(val: &CypherValue) -> &'static CStr {
    macro_rules! cstr {
        ($s:literal) => {
            // SAFETY: all literals are NUL-terminated ASCII with no interior NULs.
            unsafe { CStr::from_bytes_with_nul_unchecked(concat!($s, "\0").as_bytes()) }
        };
    }
    match val {
        CypherValue::Null => cstr!("NULL"),
        CypherValue::Bool(_) => cstr!("BOOLEAN"),
        CypherValue::Integer(_) => cstr!("INTEGER"),
        CypherValue::Float(_) => cstr!("FLOAT"),
        CypherValue::String(_) => cstr!("STRING"),
        CypherValue::List(_) => cstr!("LIST"),
        CypherValue::Map(_) => cstr!("MAP"),
        CypherValue::Node { .. } => cstr!("NODE"),
        CypherValue::Edge { .. } => cstr!("RELATIONSHIP"),
        CypherValue::Path { .. } => cstr!("PATH"),
        CypherValue::Date(_) => cstr!("DATE"),
        CypherValue::LocalTime(_) => cstr!("LOCAL_TIME"),
        CypherValue::Time(_) => cstr!("TIME"),
        CypherValue::LocalDateTime(_) => cstr!("LOCAL_DATE_TIME"),
        CypherValue::DateTime(_) => cstr!("DATE_TIME"),
        CypherValue::Duration(_) => cstr!("DURATION"),
        // Crosses the C ABI as a hex string via the string accessor (no
        // new type tag - Go/.NET/Java wrappers stay source-compatible).
        CypherValue::Bytes(_) => cstr!("BYTEARRAY"),
        CypherValue::NodeRef(_) => cstr!("NODE"),
        CypherValue::EdgeRef { .. } => cstr!("RELATIONSHIP"),
    }
}

// ---------------------------------------------------------------------------
// FFI exports
// ---------------------------------------------------------------------------

/// Return the number of columns in the result set.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_column_count(result: *const TvResult) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        0,
        AssertUnwindSafe(|| match unsafe { deref_result(result) } {
            Some(inner) => inner.columns.len() as i32,
            None => 0,
        }),
    )
}

/// Return the name of column `col` (0-based).
///
/// The returned pointer is valid until [`traverse_result_free`] is called.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_column_name(
    result: *const TvResult,
    col: i32,
) -> *const c_char {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null(),
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return std::ptr::null(),
            };
            if col < 0 || col as usize >= inner.column_name_cache.len() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "column index out of bounds");
                return std::ptr::null();
            }
            inner.column_name_cache[col as usize].as_ptr()
        }),
    )
}

/// Return the total number of rows in the result set.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_row_count(result: *const TvResult) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        0,
        AssertUnwindSafe(|| match unsafe { deref_result(result) } {
            Some(inner) => inner.rows.len() as i64,
            None => 0,
        }),
    )
}

/// Advance the cursor to the next row.
///
/// Returns `true` if there is a row to read, `false` when past the end.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_next(result: *mut TvResult) -> bool {
    error::clear_last_error();
    crate::catch_panic(
        false,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result_mut(result) } {
                Some(i) => i,
                None => return false,
            };
            inner.cursor += 1;
            if (inner.cursor as usize) < inner.rows.len() {
                // Reset string caches for the new row.
                let col_count = inner.columns.len();
                inner.string_cache.clear();
                inner.string_cache.resize(col_count, None);
                inner.graph_string_cache.clear();
                true
            } else {
                false
            }
        }),
    )
}

/// Get the value at `col` in the current row as a NUL-terminated string.
///
/// For `String` values the raw string content is returned (no quotes).
/// For all other types the Cypher `Display` formatting is used.
///
/// The returned pointer is valid until the next call to
/// [`traverse_result_next`] or [`traverse_result_free`].
///
/// Returns `NULL` if the value is `Null` or on error.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle and the cursor must have been
/// advanced by [`traverse_result_next`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_string(
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
            if val.is_null() {
                return std::ptr::null();
            }

            let col_idx = col as usize;
            if inner.string_cache[col_idx].is_some() {
                return inner.string_cache[col_idx].as_ref().unwrap().as_ptr();
            }

            let s = match val {
                CypherValue::String(s) => s.clone(),
                other => other.to_string(),
            };
            let cstring = CString::new(s).unwrap_or_default();
            let ptr = cstring.as_ptr();
            inner.string_cache[col_idx] = Some(cstring);
            ptr
        }),
    )
}

/// Get the value at `col` in the current row as an `int64_t`.
///
/// Returns 0 and sets an error if the value is not an integer.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_int(result: *const TvResult, col: i32) -> i64 {
    error::clear_last_error();
    crate::catch_panic(
        0,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return 0,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return 0,
            };
            match val.as_integer() {
                Some(i) => i,
                None => {
                    error::set_last_error(
                        TvError::TV_ERROR_TYPE,
                        &format!("expected INTEGER, got {}", val.type_name()),
                    );
                    0
                }
            }
        }),
    )
}

/// Get the value at `col` in the current row as a `double`.
///
/// Returns 0.0 and sets an error if the value is not a float.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_double(result: *const TvResult, col: i32) -> f64 {
    error::clear_last_error();
    crate::catch_panic(
        0.0,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return 0.0,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return 0.0,
            };
            match val.as_float() {
                Some(f) => f,
                None => {
                    error::set_last_error(
                        TvError::TV_ERROR_TYPE,
                        &format!("expected FLOAT, got {}", val.type_name()),
                    );
                    0.0
                }
            }
        }),
    )
}

/// Get the value at `col` in the current row as a `bool`.
///
/// Returns `false` and sets an error if the value is not a boolean.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_bool(result: *const TvResult, col: i32) -> bool {
    error::clear_last_error();
    crate::catch_panic(
        false,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return false,
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return false,
            };
            match val.as_bool() {
                Some(b) => b,
                None => {
                    error::set_last_error(
                        TvError::TV_ERROR_TYPE,
                        &format!("expected BOOLEAN, got {}", val.type_name()),
                    );
                    false
                }
            }
        }),
    )
}

/// Check whether the value at `col` in the current row is `NULL`.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_is_null(result: *const TvResult, col: i32) -> bool {
    error::clear_last_error();
    crate::catch_panic(
        true,
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return true,
            };
            match current_cell(inner, col) {
                Some(v) => v.is_null(),
                None => true,
            }
        }),
    )
}

/// Return the type name of the value at `col` in the current row.
///
/// Returns one of: `"NULL"`, `"BOOLEAN"`, `"INTEGER"`, `"FLOAT"`,
/// `"STRING"`, `"LIST"`, `"MAP"`, `"NODE"`, `"RELATIONSHIP"`, `"PATH"`,
/// `"DATE"`, `"LOCAL_TIME"`, `"TIME"`, `"LOCAL_DATE_TIME"`, `"DATE_TIME"`,
/// `"DURATION"`.
///
/// The returned pointer is a static string (valid indefinitely).
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_get_type(
    result: *const TvResult,
    col: i32,
) -> *const c_char {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null(),
        AssertUnwindSafe(|| {
            let inner = match unsafe { deref_result(result) } {
                Some(i) => i,
                None => return std::ptr::null(),
            };
            let val = match current_cell(inner, col) {
                Some(v) => v,
                None => return std::ptr::null(),
            };
            // Return a NUL-terminated static C string for the type name.
            type_name_cstr(val).as_ptr()
        }),
    )
}

/// Free the result handle and all associated memory.
///
/// # Safety
///
/// `result` must be a valid [`TvResult`] handle or null (which is a no-op).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_result_free(result: *mut TvResult) {
    if result.is_null() {
        return;
    }
    // Reclaim the Box and drop it.
    let _ = unsafe { Box::from_raw(result as *mut ResultInner) };
}
