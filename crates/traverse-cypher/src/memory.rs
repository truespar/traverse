//! Per-query memory accounting.
//!
//! In-memory engine: a query buffering more rows than the machine holds doesn't
//! get slow, it kills the process (an un-indexed two-`MATCH` write can
//! materialize a huge cross product). So operators never buffer blindly: every
//! buffer growing with the data (materialized inputs, hash-join tables, sorts,
//! the result set) is *charged* here, and a query over budget fails with a clear
//! error while the process and other queries keep running. Same contract as
//! Neo4j's `db.memory.transaction.max` and Memgraph's `--query-memory-limit`.
//!
//! Two limits, checked on the same path:
//!
//! * **Per-query budget** ([`query_limit`]): estimated bytes of this query's
//!   buffers. Deterministic, attributable, and available everywhere the engine
//!   runs (embedded bindings, CLI, WASM, where there's no process limit). Set
//!   with `TRAVERSE_QUERY_MEMORY_LIMIT` (`8GB`, `512MB`, `0` = unlimited);
//!   defaults to a quarter of physical memory.
//! * **Process ceiling** (`traverse_core::memory`): RSS against
//!   `TRAVERSE_MEMORY_LIMIT` (default 90 % of physical memory). Coarse, but
//!   catches what the estimate can't see.
//!
//! Sizes are estimates (slot width plus the heap a value owns), not allocator
//! truth, the same trade-off as Neo4j's `HeapEstimator`: conservative enough to
//! stop a runaway query well before the machine is at risk, cheap enough per
//! buffered row (one pass over the chunk, one atomic add per 256 KiB).
//!
//! The tracker lives in the running query's [`QueryContext`], so it reaches
//! rayon workers and nested executions the same way the deadline does.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use crate::error::CypherError;
use crate::query::QueryContext;
use crate::types::CypherValue;

/// Charges batch locally up to this many bytes before touching the shared counter: one add per
/// thousands of frames, not an atomic per row.
pub const CHARGE_BATCH_BYTES: usize = 256 * 1024;

/// Per-query counter, part of the query's [`QueryContext`].
#[derive(Debug)]
pub struct QueryMemoryTracker {
    used: AtomicUsize,
    peak: AtomicUsize,
    /// 0 = unlimited.
    limit: usize,
}

impl QueryMemoryTracker {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            limit,
        }
    }

    /// Bytes charged to the query now.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// High-water mark of [`used`](Self::used).
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn add(&self, bytes: usize) -> Result<(), CypherError> {
        let now = self.used.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(now, Ordering::Relaxed);
        if self.limit != 0 && now > self.limit {
            return Err(memory_limit_error(now, self.limit));
        }
        // the process ceiling backstops what the estimate misses; it reads a cached RSS value,
        // cheap enough for the (already batched) charge path
        if crate::query::is_memory_exhausted() {
            return Err(process_limit_error());
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn sub(&self, bytes: usize) {
        // saturating: a release may transiently race a worker's add, and an underflow must never
        // wrap to "everything"
        let _ = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(bytes))
            });
    }
}

fn memory_limit_error(used: usize, limit: usize) -> CypherError {
    CypherError::runtime(format!(
        "query exceeded its memory limit: {} used, limit {} (TRAVERSE_QUERY_MEMORY_LIMIT). \
         The query buffers more intermediate rows than the budget allows - add a more \
         selective WHERE, a LIMIT, or process the rows in batches with \
         CALL {{ ... }} IN TRANSACTIONS; raise the limit if the query is intended to be this large",
        format_bytes(used),
        format_bytes(limit),
    ))
    .with_status(crate::status::GqlStatus::MemoryLimit)
}

pub(crate) fn process_limit_error() -> CypherError {
    CypherError::runtime(
        "query exceeded server memory limit (TRAVERSE_MEMORY_LIMIT) - try adding a more \
         selective WHERE clause, a LIMIT, or rewriting the query so the planner can avoid a \
         full scan",
    )
    .with_status(crate::status::GqlStatus::OutOfMemory)
}

/// `1234567` -> `1.2 MB`; enough precision for an error message.
pub fn format_bytes(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

// -- The running query's account ---------------------------------------

/// Bytes charged to the running query (0 if none).
pub fn used() -> usize {
    crate::query::with_current(|c| c.map_or(0, |ctx| ctx.memory().used()))
}

/// High-water mark of the running query (0 if none).
pub fn peak() -> usize {
    crate::query::with_current(|c| c.map_or(0, |ctx| ctx.memory().peak()))
}

/// Charges `bytes` to the running query; errors past the query budget or the process ceiling. No-op
/// without an active query.
#[inline]
pub fn charge(bytes: usize) -> Result<(), CypherError> {
    crate::query::with_current(|c| match c {
        Some(ctx) => ctx.memory().add(bytes),
        None => Ok(()),
    })
}

/// Releases bytes charged with [`charge`].
#[inline]
pub fn release(bytes: usize) {
    crate::query::with_current(|c| {
        if let Some(ctx) = c {
            ctx.memory().sub(bytes);
        }
    });
}

// -- Budget configuration ----------------------------------------------

static QUERY_LIMIT: OnceLock<AtomicUsize> = OnceLock::new();

/// Share of physical memory one query may buffer by default.
const DEFAULT_QUERY_FRACTION_PERCENT: usize = 25;

fn query_limit_cell() -> &'static AtomicUsize {
    QUERY_LIMIT.get_or_init(|| {
        let from_env = std::env::var("TRAVERSE_QUERY_MEMORY_LIMIT")
            .ok()
            .and_then(|v| parse_byte_size(&v));
        let limit = match from_env {
            Some(v) => v,
            None => default_query_limit(),
        };
        AtomicUsize::new(limit)
    })
}

/// A quarter of physical memory, or unlimited if undetectable (the process ceiling still applies
/// where RSS is readable).
pub fn default_query_limit() -> usize {
    match traverse_core::memory::total_memory() {
        Some(total) => total / 100 * DEFAULT_QUERY_FRACTION_PERCENT,
        None => 0,
    }
}

/// Per-query budget in bytes; 0 = unlimited.
pub fn query_limit() -> usize {
    query_limit_cell().load(Ordering::Relaxed)
}

/// Overrides the per-query budget (0 = unlimited) for queries started afterwards. Set by the
/// server's configuration; tests use it to hit the limit.
pub fn set_query_limit(bytes: usize) {
    query_limit_cell().store(bytes, Ordering::Relaxed);
}

/// Parses `8GB`, `512MB`, `64kb`, `1073741824`, `0`, `unlimited`, `none`; `None` otherwise.
pub fn parse_byte_size(s: &str) -> Option<usize> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let upper = s.to_ascii_uppercase();
    if upper == "UNLIMITED" || upper == "NONE" || upper == "OFF" {
        return Some(0);
    }
    // parse in u64, then check the address space: a usize TB multiplier overflows at compile time
    // on wasm32, even for non-TB inputs
    let (digits, mult): (&str, u64) = if let Some(n) = upper.strip_suffix("TB") {
        (n, 1 << 40)
    } else if let Some(n) = upper.strip_suffix("GB") {
        (n, 1 << 30)
    } else if let Some(n) = upper.strip_suffix("MB") {
        (n, 1 << 20)
    } else if let Some(n) = upper.strip_suffix("KB") {
        (n, 1 << 10)
    } else if let Some(n) = upper.strip_suffix('T') {
        (n, 1 << 40)
    } else if let Some(n) = upper.strip_suffix('G') {
        (n, 1 << 30)
    } else if let Some(n) = upper.strip_suffix('M') {
        (n, 1 << 20)
    } else if let Some(n) = upper.strip_suffix('K') {
        (n, 1 << 10)
    } else if let Some(n) = upper.strip_suffix('B') {
        (n, 1)
    } else {
        (upper.as_str(), 1)
    };
    digits
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .and_then(|bytes| usize::try_from(bytes).ok())
}

// -- Size estimation ---------------------------------------------------

/// Estimated bytes a value owns beyond its inline size: string/byte payloads, list and map elements
/// (recursively), materialized node/edge properties.
pub fn value_heap_bytes(v: &CypherValue) -> usize {
    const ELEM: usize = std::mem::size_of::<CypherValue>();
    // rough per-entry overhead of a `BTreeMap<String, _>` node
    const MAP_ENTRY: usize = 48;
    match v {
        CypherValue::Null
        | CypherValue::Bool(_)
        | CypherValue::Integer(_)
        | CypherValue::Float(_)
        | CypherValue::NodeRef(_)
        | CypherValue::EdgeRef { .. }
        | CypherValue::Date(_)
        | CypherValue::LocalTime(_)
        | CypherValue::Time(_)
        | CypherValue::LocalDateTime(_)
        | CypherValue::DateTime(_)
        | CypherValue::Duration(_) => 0,
        CypherValue::String(s) => s.len(),
        CypherValue::Bytes(b) => b.len(),
        CypherValue::List(items) => {
            items.len() * ELEM + items.iter().map(value_heap_bytes).sum::<usize>()
        }
        CypherValue::Map(m) => m
            .iter()
            .map(|(k, v)| MAP_ENTRY + k.len() + ELEM + value_heap_bytes(v))
            .sum(),
        CypherValue::Node {
            labels, properties, ..
        } => labels.iter().map(|l| l.len() + 24).sum::<usize>() + property_map_bytes(properties),
        CypherValue::Edge {
            edge_type,
            properties,
            ..
        } => edge_type.len() + property_map_bytes(properties),
        CypherValue::Path { nodes, edges } => {
            (nodes.len() + edges.len()) * ELEM
                + nodes.iter().map(value_heap_bytes).sum::<usize>()
                + edges.iter().map(value_heap_bytes).sum::<usize>()
        }
    }
}

fn property_map_bytes(m: &traverse_core::graph::property::PropertyMap) -> usize {
    use traverse_core::graph::property::PropertyValue;
    fn pv(v: &PropertyValue) -> usize {
        match v {
            PropertyValue::String(s) => s.len(),
            PropertyValue::Bytes(b) => b.len(),
            PropertyValue::List(items) => items.len() * 32 + items.iter().map(pv).sum::<usize>(),
            PropertyValue::Map(m) => m.iter().map(|(k, v)| 48 + k.len() + pv(v)).sum(),
            _ => 0,
        }
    }
    m.iter().map(|(k, v)| 48 + k.len() + 32 + pv(v)).sum()
}

// -- Charged buffers ---------------------------------------------------

/// Charge against the query current when opened. Released on drop, so a buffer's accounting ends
/// with the buffer, on whichever thread drops it.
#[derive(Debug)]
pub struct Charge {
    query: Option<Arc<QueryContext>>,
    charged: usize,
    pending: usize,
}

impl Charge {
    /// Empty charge against this thread's query.
    pub fn new() -> Self {
        Self {
            query: crate::query::current(),
            charged: 0,
            pending: 0,
        }
    }

    /// Adds `bytes`, flushing to the shared counter every [`CHARGE_BATCH_BYTES`].
    #[inline]
    pub fn add(&mut self, bytes: usize) -> Result<(), CypherError> {
        self.pending += bytes;
        if self.pending >= CHARGE_BATCH_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    /// Pushes batched bytes to the shared counter now.
    pub fn flush(&mut self) -> Result<(), CypherError> {
        let pending = std::mem::take(&mut self.pending);
        if pending == 0 {
            return Ok(());
        }
        // account before checking, so the release on drop stays balanced even when this charge
        // trips the limit
        self.charged += pending;
        match &self.query {
            Some(ctx) => ctx.memory().add(pending),
            None => Ok(()),
        }
    }

    /// Bytes held (including unflushed).
    pub fn bytes(&self) -> usize {
        self.charged + self.pending
    }
}

impl Default for Charge {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if self.charged > 0
            && let Some(ctx) = &self.query
        {
            ctx.memory().sub(self.charged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_byte_size("8GB"), Some(8 << 30));
        assert_eq!(parse_byte_size(" 512mb "), Some(512 << 20));
        assert_eq!(parse_byte_size("64K"), Some(64 << 10));
        assert_eq!(parse_byte_size("1073741824"), Some(1 << 30));
        assert_eq!(parse_byte_size("0"), Some(0));
        assert_eq!(parse_byte_size("unlimited"), Some(0));
        assert_eq!(parse_byte_size("lots"), None);
        assert_eq!(parse_byte_size(""), None);
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(3 << 30), "3.0 GB");
    }

    #[test]
    fn nested_scopes_share_one_account() {
        let _outer = crate::query::enter();
        charge(100).unwrap();
        {
            let _inner = crate::query::enter();
            charge(50).unwrap();
            assert_eq!(used(), 150);
        }
        assert_eq!(used(), 150, "the inner scope joined the outer query");
        release(150);
        assert_eq!(used(), 0);
    }

    #[test]
    fn charge_releases_on_drop() {
        let _scope = crate::query::enter();
        {
            let mut c = Charge::new();
            c.add(CHARGE_BATCH_BYTES + 1).unwrap();
            assert_eq!(used(), CHARGE_BATCH_BYTES + 1);
            assert_eq!(peak(), CHARGE_BATCH_BYTES + 1);
        }
        assert_eq!(used(), 0);
        assert_eq!(peak(), CHARGE_BATCH_BYTES + 1);
    }

    #[test]
    fn limit_trips_with_a_clear_error() {
        let _scope = crate::query::enter_with_memory_limit(1024);
        let err = charge(4096).unwrap_err().to_string();
        assert!(err.contains("memory limit"), "{err}");
        assert!(err.contains("TRAVERSE_QUERY_MEMORY_LIMIT"), "{err}");
    }

    #[test]
    fn value_sizes_count_owned_heap() {
        let s = CypherValue::String("x".repeat(100));
        assert_eq!(value_heap_bytes(&s), 100);
        let l = CypherValue::List(vec![s.clone(), s]);
        assert!(value_heap_bytes(&l) > 200);
    }
}
