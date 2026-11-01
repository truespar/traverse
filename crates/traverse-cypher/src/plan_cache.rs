//! Plans shared across statements, one cache per database: keyed by the query text with its
//! literals stripped (`{id: 7}` and `{id: 8}` share a plan, the literals become parameters) and
//! dropped when the index catalog changes. The server and the embedded bindings both run through
//! it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use traverse_core::tx::view::GraphView;

use crate::PreparedQuery;
use crate::error::CypherError;
use crate::executor::CypherResult;
use crate::query::Params;

/// Plans a cache holds by default.
pub const DEFAULT_CAPACITY: usize = 1024;

/// A cached plan and the index generation it was planned for. Behind an `Arc`, so a hit bumps a
/// refcount instead of cloning the plan tree.
struct CachedPlan {
    prepared: Arc<PreparedQuery>,
    index_generation: u64,
    /// Approximate-LRU recency stamp, bumped on every hit; atomic so a hit stays under the read
    /// lock.
    last_used: AtomicU64,
}

/// Approximate-LRU plan cache. A hit is a read lock plus one atomic store, so concurrent
/// statements never serialize on a writer; eviction (only on a miss with a full cache) drops the
/// least recently used plan.
pub struct PlanCache {
    entries: RwLock<HashMap<String, CachedPlan>>,
    tick: AtomicU64,
    capacity: usize,
}

impl Default for PlanCache {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl std::fmt::Debug for PlanCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanCache")
            .field("len", &self.len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl PlanCache {
    /// An empty cache of [`DEFAULT_CAPACITY`] plans.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty cache of `capacity` plans (at least one).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            tick: AtomicU64::new(0),
            capacity: capacity.max(1),
        }
    }

    /// The plan cached for `stripped_query`, unless it's absent or was planned for another index
    /// generation.
    pub fn get(&self, stripped_query: &str, index_generation: u64) -> Option<Arc<PreparedQuery>> {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.get(stripped_query)?;
        if entry.index_generation != index_generation {
            return None;
        }
        let t = self.tick.fetch_add(1, Ordering::Relaxed);
        entry.last_used.store(t, Ordering::Relaxed);
        Some(Arc::clone(&entry.prepared))
    }

    /// Caches a plan, evicting the least recently used one when full.
    pub fn insert(
        &self,
        stripped_query: String,
        prepared: Arc<PreparedQuery>,
        index_generation: u64,
    ) {
        let t = self.tick.fetch_add(1, Ordering::Relaxed);
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= self.capacity
            && !entries.contains_key(&stripped_query)
            && let Some(victim) = entries
                .iter()
                .min_by_key(|(_, v)| v.last_used.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone())
        {
            entries.remove(&victim);
        }
        entries.insert(
            stripped_query,
            CachedPlan {
                prepared,
                index_generation,
                last_used: AtomicU64::new(t),
            },
        );
    }

    /// Drops every plan (after DDL, `db.clearQueryCaches()`).
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Plans cached.
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether no plan is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `query`'s plan on `graph` and the parameters to run it with: `params` plus the literals the
    /// cache key stripped (`__sp0`, ...). From the cache when it holds a plan for `graph`'s index
    /// catalog, planned and cached otherwise. DDL and most standalone `CALL`s
    /// ([`bypasses_cache`]) are planned from the text as written, every time.
    pub fn prepare(
        &self,
        graph: &dyn GraphView,
        query: &str,
        params: &Params,
    ) -> Result<Prepared, CypherError> {
        if bypasses_cache(query) {
            return Ok(Prepared {
                query: Arc::new(crate::prepare_query(graph, query, params)?),
                params: Arc::clone(params),
            });
        }
        // text the lexer rejects plans as written, so the error points into it
        let stripped = crate::strip_literals_cached(query)
            .unwrap_or_else(|_| Arc::new((query.to_string(), Vec::new())));
        let (text, literals) = (&stripped.0, &stripped.1);
        let params = if literals.is_empty() {
            Arc::clone(params)
        } else {
            let mut all = (**params).clone();
            for (i, value) in literals.iter().enumerate() {
                all.insert(format!("__sp{i}"), value.clone());
            }
            Arc::new(all)
        };
        let generation = graph.index_catalog().generation();
        if let Some(hit) = self.get(text, generation) {
            return Ok(Prepared { query: hit, params });
        }
        let prepared = Arc::new(crate::prepare_query(graph, text, &params)?);
        self.insert(text.clone(), Arc::clone(&prepared), generation);
        Ok(Prepared {
            query: prepared,
            params,
        })
    }
}

/// A statement ready to run: its plan and its parameters.
#[derive(Clone)]
pub struct Prepared {
    /// The plan.
    pub query: Arc<PreparedQuery>,
    /// The caller's parameters plus the stripped literals.
    pub params: Params,
}

impl Prepared {
    /// Whether running it changes data or schema (`EXPLAIN` runs nothing): an auto-commit caller
    /// runs it in a write transaction then.
    pub fn writes(&self) -> bool {
        self.query.mode != crate::ast::QueryMode::Explain
            && crate::executor::plan_is_mutating(&self.query.plan)
    }

    /// Runs it on `graph`.
    pub fn execute(&self, graph: &mut dyn GraphView) -> Result<CypherResult, CypherError> {
        crate::execute_prepared_shared(graph, &self.query, Arc::clone(&self.params))
    }
}

/// Runs `query` on `graph`, its plan through `cache`.
pub fn execute(
    cache: &PlanCache,
    graph: &mut dyn GraphView,
    query: &str,
    params: &Params,
) -> Result<CypherResult, CypherError> {
    cache.prepare(graph, query, params)?.execute(graph)
}

/// Parameters for a statement from a plain map.
pub fn params(map: HashMap<String, crate::types::CypherValue>) -> Params {
    Arc::new(map)
}

/// Standalone `CALL` procedures whose plans may be cached: read-only, no plan-time parameter
/// baking, stable output columns. The cache keys on literal-stripped text and binds parameters per
/// execution, so a cached plan reruns correctly with new arguments as long as the procedure doesn't
/// fold argument *values* into the plan. Anything not listed bypasses the cache; add procedures
/// once their output columns are verified fixed.
const CACHEABLE_PROCEDURES: &[&str] = &["db.index.vector.queryNodes"];

/// Should the query bypass the plan cache? DDL (`CREATE INDEX` / `DROP INDEX`), or a standalone
/// `CALL` to a procedure outside `CACHEABLE_PROCEDURES`. Only queries *starting* with `CALL`
/// count (an in-query `MATCH ... CALL ...` is cached). Runs on the raw pre-parse string (the cache
/// exists to skip parsing), so it's a cheap prefix/name test.
pub fn bypasses_cache(query: &str) -> bool {
    let trimmed = query.trim_start();
    let tb = trimmed.as_bytes();

    // standalone CALL: cacheable only for allowlisted read-only procedures
    if tb.len() >= 4 && tb[..4].eq_ignore_ascii_case(b"CALL") {
        return !is_cacheable_call(trimmed);
    }

    // DDL: any CREATE/DROP that isn't a data write (plain, VECTOR, FULLTEXT, TEXT, POINT, LOOKUP
    // indexes, plus constraints); CREATE VECTOR INDEX takes literal options the planner consumes at
    // prepare time, which literal-stripped cached plans would break
    const DDL_SECOND: &[&[u8]] = &[
        b"INDEX",
        b"VECTOR",
        b"FULLTEXT",
        b"TEXT",
        b"POINT",
        b"LOOKUP",
        b"CONSTRAINT",
    ];
    for lead in [&b"CREATE"[..], &b"DROP"[..]] {
        if tb.len() > lead.len() && tb[..lead.len()].eq_ignore_ascii_case(lead) {
            let rest = trimmed[lead.len()..].trim_start().as_bytes();
            // optional `OR REPLACE` between CREATE and the object kind
            let rest = if rest.len() > 2 && rest[..2].eq_ignore_ascii_case(b"OR") {
                let r2 = &trimmed[lead.len()..].trim_start()[2..];
                r2.trim_start()
                    .strip_prefix("REPLACE")
                    .or_else(|| r2.trim_start().strip_prefix("replace"))
                    .map(|r| r.trim_start().as_bytes())
                    .unwrap_or(rest)
            } else {
                rest
            };
            if DDL_SECOND
                .iter()
                .any(|kw| rest.len() >= kw.len() && rest[..kw.len()].eq_ignore_ascii_case(kw))
            {
                return true;
            }
        }
    }
    false
}

/// Does `query` (trimmed, starting with `CALL`) call an allowlisted procedure? The name is the
/// `[A-Za-z0-9_.]` run after `CALL`, matched case-insensitively.
fn is_cacheable_call(query: &str) -> bool {
    let rest = query[4..].trim_start();
    let name_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .unwrap_or(rest.len());
    let name = &rest[..name_end];
    CACHEABLE_PROCEDURES
        .iter()
        .any(|p| name.eq_ignore_ascii_case(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use traverse_core::graph::Graph;
    use traverse_core::tx::MvccStore;

    #[test]
    fn vector_query_is_cacheable() {
        // the hot path: standalone vector k-ANN with literal + param args
        assert!(!bypasses_cache(
            "CALL db.index.vector.queryNodes('idx', 10, $q) YIELD node, score RETURN node.id"
        ));
        // case-insensitive procedure name + leading whitespace
        assert!(!bypasses_cache(
            "  call DB.Index.Vector.QueryNodes('idx', 5, $q) YIELD node"
        ));
    }

    #[test]
    fn other_calls_still_bypass() {
        assert!(bypasses_cache("CALL db.labels()"));
        assert!(bypasses_cache("CALL traverse.pagerank.stream({})"));
        assert!(bypasses_cache("CALL { MATCH (n) RETURN n }"));
    }

    #[test]
    fn non_call_and_ddl() {
        assert!(!bypasses_cache("MATCH (n) RETURN n"));
        // an in-query CALL, not a standalone CALL prefix
        assert!(!bypasses_cache(
            "MATCH (x) CALL db.index.vector.queryNodes('i',1,$q) YIELD node RETURN node"
        ));
        assert!(bypasses_cache("CREATE INDEX foo FOR (n:L) ON (n.p)"));
        assert!(bypasses_cache("DROP INDEX foo"));
    }

    #[test]
    fn literals_share_a_plan_and_index_changes_replan() {
        let store = MvccStore::new(Graph::new());
        let cache = PlanCache::new();
        let none = params(HashMap::new());
        {
            let mut tx = store.begin_write();
            execute(&cache, &mut tx, "CREATE (:P {id: 1}), (:P {id: 2})", &none).unwrap();
            tx.commit().unwrap();
        }
        let read = |id: i64| {
            let mut view = store.begin_read().unwrap();
            let r = execute(
                &cache,
                &mut view,
                &format!("MATCH (n:P {{id: {id}}}) RETURN n.id AS id"),
                &none,
            )
            .unwrap();
            r.rows.len()
        };
        assert_eq!(read(1), 1);
        assert_eq!(read(2), 1);
        assert_eq!(read(3), 0);
        // the CREATE and the three reads: two plans
        assert_eq!(cache.len(), 2);
        {
            let mut tx = store.begin_write();
            execute(
                &cache,
                &mut tx,
                "CREATE INDEX p_id FOR (n:P) ON (n.id)",
                &none,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        // a new index generation: planned again, same answers
        assert_eq!(read(2), 1);
        assert_eq!(read(4), 0);
    }

    #[test]
    fn writes_follow_the_plan() {
        let store = MvccStore::new(Graph::new());
        let cache = PlanCache::new();
        let none = params(HashMap::new());
        let view = store.begin_read().unwrap();
        let writes = |q: &str| cache.prepare(&view, q, &none).unwrap().writes();
        assert!(!writes("MATCH (n) RETURN n"));
        assert!(writes("CREATE (a) WITH a MATCH (a)-->(b) RETURN b"));
        assert!(writes("FOREACH (i IN [1] | CREATE (:N))"));
        assert!(writes(
            "UNWIND [1] AS i CALL { WITH i CREATE (:N) } IN TRANSACTIONS"
        ));
        assert!(writes("CREATE INDEX i FOR (n:N) ON (n.p)"));
        assert!(!writes("EXPLAIN CREATE (:N)"));
        assert!(!writes("SHOW INDEXES"));
    }
}
