//! `traverse-gql` parser fuzz target: libFuzzer bytes into `traverse_gql::parse`.
//! The parser must never panic; a panic is a finding, minimized and saved to
//! `fuzz/artifacts/parse/`.
//!
//! ```text
//! cargo +nightly fuzz run parse --max-total-time 600
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // the parser takes &str; lossy UTF-8 turns malformed bytes into `U+FFFD` so the parser, not the
    // I/O boundary, deals with them
    let input = String::from_utf8_lossy(data);
    let _ = traverse_gql::parse(&input);
});
