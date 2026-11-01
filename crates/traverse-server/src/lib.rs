pub mod startup;
#[cfg(windows)]
pub(crate) mod win_service;

pub mod ai;
pub mod auth;
pub mod bolt_type;
pub mod config;
pub mod connection;
pub mod error;
pub mod framing;
#[cfg(feature = "grpc")]
pub mod grpc;
pub mod handshake;
pub mod http;
pub mod ip_filter;
pub mod message;
pub mod notification;
pub mod packstream;
pub mod registry;
pub mod server;
pub(crate) mod statement;
pub(crate) mod sync;

/// At most `max_len` bytes of `s`, cut at a character boundary (for logs and error reports).
pub(crate) fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Does the query modify data? Heuristic, word-boundary matching (no "PRESET" -> "SET" or "DELETED"
/// -> "DELETE" false positives), ASCII case-insensitive byte scanning so no uppercase copy is
/// allocated.
pub fn is_write_query(query: &str) -> bool {
    use crate::sync::CacheLock as _;
    // the classification is a pure function of the text and the scan makes ~10 passes (keywords +
    // APOC/GDS write prefixes), noticeable on dispatch-bound workloads, so it's memoized keyed by
    // the FULL text (a hash-only key could misroute a write on collision). Two levels: the
    // thread-local L1 makes the steady-state hit a lookup with no shared state; the global L2 means
    // a tokio task migrating to a cold worker pays one read-lock lookup, not a recompute.
    thread_local! {
        static MEMO: std::cell::RefCell<std::collections::HashMap<String, bool>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    static SHARED: std::sync::OnceLock<std::sync::RwLock<std::collections::HashMap<String, bool>>> =
        std::sync::OnceLock::new();
    if let Some(v) = MEMO.with(|m| m.borrow().get(query).copied()) {
        return v;
    }
    let shared = SHARED.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    let cached = shared.cache_read().get(query).copied();
    let v = match cached {
        Some(v) => v,
        None => {
            let v = classify_write_query(query);
            let mut m = shared.cache_write();
            if m.len() >= 2048 {
                m.clear();
            }
            m.insert(query.to_string(), v);
            v
        }
    };
    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= 2048 {
            m.clear();
        }
        m.insert(query.to_string(), v);
    });
    v
}

fn classify_write_query(query: &str) -> bool {
    // `INSERT` is GQL's write keyword (openCypher uses `CREATE`); listing it keeps GQL writes on
    // the begin_write() path (the executor never sees INSERT, the GQL parser lowers it to a Cypher
    // Create)
    const KEYWORDS: &[&[u8]] = &[
        b"CREATE", b"SET", b"DELETE", b"REMOVE", b"MERGE", b"DROP", b"INSERT",
    ];
    // write-capable APOC-compat namespaces: a bare `CALL apoc.refactor.rename.label(...)` has no
    // write keyword but mutates (apoc.cypher.run / apoc.when run dynamic strings; the keyword scan
    // over the whole text, string literals included, catches those, deliberately conservative)
    const WRITE_PREFIXES: &[&str] = &[
        "apoc.refactor.",
        "apoc.create.",
        "apoc.merge.",
        "apoc.periodic.",
        "apoc.schema.assert",
        "apoc.do.",
        "apoc.generate.",
        "apoc.import.",
        "apoc.example.",
    ];
    // cheap pre-gate: no prefix can match without "apoc.", so the hot read path skips a per-query
    // lowercase allocation
    if traverse_cypher::contains_ignore_ascii_case(query, b"apoc.")
        && WRITE_PREFIXES
            .iter()
            .any(|p| traverse_cypher::contains_ignore_ascii_case(query, p.as_bytes()))
    {
        return true;
    }
    let bytes = query.as_bytes();
    for kw in KEYWORDS {
        let kw_len = kw.len();
        if bytes.len() < kw_len {
            continue;
        }
        let mut i = 0;
        while i + kw_len <= bytes.len() {
            if bytes[i..i + kw_len].eq_ignore_ascii_case(kw) {
                let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
                let after_pos = i + kw_len;
                let after_ok =
                    after_pos >= bytes.len() || !bytes[after_pos].is_ascii_alphanumeric();
                if before_ok && after_ok {
                    return true;
                }
            }
            i += 1;
        }
    }
    // GDS write procedures: `CALL traverse.<algo>.write(...)` writes node properties via the
    // algorithm's bulk write_back, so it needs a WriteTx; the literal `.write(` after `traverse.`
    // identifies it without parsing
    is_gds_write_procedure(query)
}

/// Detects `CALL traverse.<...>.write(...)` at any depth (`traverse.pagerank.write(`,
/// `traverse.ml.pipeline.nodeClassification.predict.write(`), so `is_write_query` routes
/// algorithm-write calls through a WriteTx.
fn is_gds_write_procedure(query: &str) -> bool {
    // the native `traverse.<algo>.write(` form and the GDS compatibility forms `gds.<algo>.write(`
    // / `gds.graph.nodeProperties.write(`
    prefixed_write_call(query, b"traverse.") || prefixed_write_call(query, b"gds.")
}

fn prefixed_write_call(query: &str, prefix: &[u8]) -> bool {
    let bytes = query.as_bytes();
    let suffix = b".write(";
    if bytes.len() < prefix.len() + suffix.len() {
        return false;
    }
    // find `traverse.` and treat it as a write if `.write(` follows within that procedure call; the
    // search stops at the first space (other clauses are separate tokens)
    let mut i = 0;
    while i + prefix.len() <= bytes.len() {
        if bytes[i..i + prefix.len()].eq_ignore_ascii_case(prefix) {
            let start = i + prefix.len();
            let mut j = start;
            while j + suffix.len() <= bytes.len() {
                let c = bytes[j];
                // procedure names are dots and ASCII alphanumerics; stop at a parenthesis (end of
                // the name) or whitespace
                if c == b'(' || c.is_ascii_whitespace() {
                    break;
                }
                if bytes[j..j + suffix.len()].eq_ignore_ascii_case(suffix) {
                    return true;
                }
                j += 1;
            }
        }
        i += 1;
    }
    false
}

/// Should the query bypass the plan cache? See [`traverse_cypher::plan_cache::bypasses_cache`].
pub use traverse_cypher::plan_cache::bypasses_cache as is_cache_bypass;

/// Admin-only command (user/database management)?
pub fn is_admin_query(query: &str) -> bool {
    let upper = query.trim().to_uppercase();
    upper.starts_with("CREATE USER")
        || upper.starts_with("DROP USER")
        || upper.starts_with("ALTER USER")
        || upper.starts_with("SHOW USERS")
        || upper.starts_with("CREATE DATABASE")
        || upper.starts_with("DROP DATABASE")
        || upper.starts_with("LOAD DATABASE")
        || upper.starts_with("CREATE ALIAS")
        || upper.starts_with("DROP ALIAS")
        || upper.starts_with("ALTER ALIAS")
        || upper.starts_with("SHOW ALIASES")
}
