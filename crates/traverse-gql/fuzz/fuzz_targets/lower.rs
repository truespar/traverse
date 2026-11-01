//! `traverse-gql` parser + lowerer fuzz target: inputs that parse go through
//! `traverse_gql::lower::lower`, catching lowering panics the parse-only target
//! misses. Parse failures are dropped (the `parse` target covers them).
//!
//! ```text
//! cargo +nightly fuzz run lower --max-total-time 600
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    if let Ok(program) = traverse_gql::parse(&input) {
        let _ = traverse_gql::lower::lower(&program);
    }
});
