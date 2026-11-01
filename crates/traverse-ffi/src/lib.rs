//! C FFI layer for Traverse embedded graph database.
//!
//! Provides opaque-pointer handles and `extern "C"` functions so any language
//! with a C-ABI foreign-function interface (C, C#, Python, ...) can use
//! Traverse.  Thread-local error reporting follows the SQLite/LMDB pattern.
//!
//! # Safety model
//!
//! Every exported function is `unsafe extern "C"` and states its pointer contract under
//! `# Safety`: handles come from this library and aren't used after their free/close call,
//! strings are NUL-terminated, arrays hold the stated count. The `unsafe` blocks inside rely only
//! on that contract plus the null checks in front of them, so they carry no individual `SAFETY:`
//! notes; blocks that need more (lifetime erasure) say so.

#![warn(missing_docs)]
#![allow(clippy::undocumented_unsafe_blocks)] // covered by the safety model above

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod bulk;
pub mod database;
pub mod error;
pub mod graph_accessors;
pub mod handles;
pub mod query;
pub mod result;
pub mod transaction;

use std::panic::UnwindSafe;

/// Catch any Rust panic and convert it to an FFI-safe error instead of
/// unwinding across the C ABI boundary (which is UB).
pub(crate) fn catch_panic<F, T>(default: T, f: F) -> T
where
    F: FnOnce() -> T + UnwindSafe,
{
    match std::panic::catch_unwind(f) {
        Ok(v) => v,
        Err(_) => {
            error::set_last_error(
                error::TvError::TV_ERROR_INTERNAL,
                "internal panic in traverse",
            );
            default
        }
    }
}
