//! `apoc.log.stream`: the in-process server log ring buffer. The server installs a tracing layer
//! that forwards every formatted log line here (capped). `apoc.log.stream(path)` streams a real
//! file when one exists at `path` (e.g. `--log-dir`'s traverse.log), else this buffer, so it works
//! with and without file logging (logging is stdout/TCP-first).

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

const CAP: usize = 10_000;

fn buffer() -> &'static Mutex<VecDeque<String>> {
    static BUF: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    BUF.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAP)))
}

/// Appends a log line (from the server's tracing layer).
pub fn push_line(line: &str) {
    let line = line.trim_end();
    if line.is_empty() {
        return;
    }
    let mut buf = buffer().lock().unwrap_or_else(|p| p.into_inner());
    if buf.len() == CAP {
        buf.pop_front();
    }
    buf.push_back(line.to_string());
}

/// Buffered lines, oldest first.
pub fn snapshot() -> Vec<String> {
    buffer()
        .lock()
        .map(|b| b.iter().cloned().collect())
        .unwrap_or_default()
}
