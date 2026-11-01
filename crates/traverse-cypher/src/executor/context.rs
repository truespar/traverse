//! Per-thread execution state: mutation counters and compiled regexes. Parameters live in the
//! running query's [`crate::query::QueryContext`].

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use crate::error::CypherError;
use crate::types::CypherValue;

use super::comprehension::clear_comprehension_caches;
use super::plan_cache::clear_pointer_plans;

thread_local! {
    /// [`cached_regex`]: patterns as written, for searching.
    static REGEX_CACHE: RefCell<HashMap<String, regex::Regex>> = RefCell::new(HashMap::new());
    /// [`regex_full_match`]: the same patterns anchored at both ends. Kept apart so `=~` and
    /// `apoc.text.*` never get each other's compiled form for the same pattern text.
    static FULL_MATCH_REGEX_CACHE: RefCell<HashMap<String, regex::Regex>> =
        RefCell::new(HashMap::new());
}

/// Compiles a regex through the persistent content-keyed cache. `regex::Regex` clones share the
/// compiled program (Arc), so they're cheap; apoc.text.* call this per row.
pub(crate) fn cached_regex(pattern: &str) -> Result<regex::Regex, CypherError> {
    REGEX_CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        if let Some(re) = c.get(pattern) {
            return Ok(re.clone());
        }
        let re = regex::Regex::new(pattern)
            .map_err(|e| CypherError::semantic(format!("invalid regex: {e}")))?;
        c.insert(pattern.to_string(), re.clone());
        Ok(re)
    })
}

/// Cypher `=~`: does `pattern` match all of `s`? `Regex::is_match` finds substrings, so the
/// pattern runs as `^(?:pattern)$` (the group keeps alternations like `a|b` anchored at both
/// ends): `'hello world' =~ 'hello'` is false, as in Neo4j.
pub(super) fn regex_full_match(pattern: &str, s: &str) -> Result<bool, CypherError> {
    FULL_MATCH_REGEX_CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        if let Some(re) = c.get(pattern) {
            return Ok(re.is_match(s));
        }
        let re = regex::Regex::new(&format!("^(?:{pattern})$"))
            .map_err(|e| CypherError::type_error(format!("invalid regex: {e}")))?;
        let hit = re.is_match(s);
        c.insert(pattern.to_string(), re);
        Ok(hit)
    })
}

/// Any query parameters bound?
pub(super) fn has_query_params() -> bool {
    crate::query::with_current(|c| c.is_some_and(|ctx| !ctx.params().is_empty()))
}

/// Per-thread cleanup after an execution.
pub fn end_execution() {
    // the regex caches are content-keyed: kept across queries so hot patterns compile once,
    // size-capped against pattern churn
    for cache in [&REGEX_CACHE, &FULL_MATCH_REGEX_CACHE] {
        cache.with(|c| {
            let mut cache = c.borrow_mut();
            if cache.len() > 256 {
                cache.clear();
            }
        });
    }
    // pointer-keyed plans are only valid while this execution's AST lives
    clear_pointer_plans();
    crate::operators::load_csv::reset_csv_stack();
}

/// Invalidates POINTER-keyed per-execution caches. Must run at the start of every
/// top-level execution: keys are AST addresses, and a freed AST from the previous
/// query can alias the new one's nodes (a pattern comprehension once inherited a
/// stale cached plan that way). The content-keyed caches survive and refill these cheaply.
pub(crate) fn reset_pointer_caches() {
    clear_pointer_plans();
    clear_comprehension_caches();
}

/// Query parameter names (implicit CALL argument validation).
pub fn get_query_param_keys() -> HashSet<String> {
    crate::query::params().keys().cloned().collect()
}

/// One query parameter by name.
#[inline]
pub fn get_query_param(name: &str) -> Option<CypherValue> {
    crate::query::param(name)
}

// -- Query statistics tracking ------------------------------------------

/// Mutation counters collected during execution.
#[derive(Debug, Clone, Default)]
pub struct QueryStats {
    /// Number of nodes created.
    pub nodes_created: i64,
    /// Number of nodes deleted.
    pub nodes_deleted: i64,
    /// Number of relationships created.
    pub relationships_created: i64,
    /// Number of relationships deleted.
    pub relationships_deleted: i64,
    /// Number of properties set.
    pub properties_set: i64,
    /// Number of labels added.
    pub labels_added: i64,
    /// Number of labels removed.
    pub labels_removed: i64,
    /// Number of indexes added.
    pub indexes_added: i64,
    /// Number of indexes removed.
    pub indexes_removed: i64,
    /// Number of constraints added.
    pub constraints_added: i64,
    /// Number of constraints removed.
    pub constraints_removed: i64,
}

impl QueryStats {
    /// `true` if every counter is zero.
    pub fn is_empty(&self) -> bool {
        self.nodes_created == 0
            && self.nodes_deleted == 0
            && self.relationships_created == 0
            && self.relationships_deleted == 0
            && self.properties_set == 0
            && self.labels_added == 0
            && self.labels_removed == 0
            && self.indexes_added == 0
            && self.indexes_removed == 0
            && self.constraints_added == 0
            && self.constraints_removed == 0
    }
}

impl std::ops::AddAssign<&QueryStats> for QueryStats {
    /// Adds another statement's counters: totals for a block of statements.
    fn add_assign(&mut self, other: &QueryStats) {
        self.nodes_created += other.nodes_created;
        self.nodes_deleted += other.nodes_deleted;
        self.relationships_created += other.relationships_created;
        self.relationships_deleted += other.relationships_deleted;
        self.properties_set += other.properties_set;
        self.labels_added += other.labels_added;
        self.labels_removed += other.labels_removed;
        self.indexes_added += other.indexes_added;
        self.indexes_removed += other.indexes_removed;
        self.constraints_added += other.constraints_added;
        self.constraints_removed += other.constraints_removed;
    }
}

/// Query type for Bolt metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    /// Read-only query.
    Read,
    /// Write-only (no RETURN).
    Write,
    /// Read-write (mutation + RETURN).
    ReadWrite,
    /// Schema operation (CREATE INDEX, DROP INDEX, ...).
    Schema,
}

impl QueryType {
    /// Bolt protocol string.
    pub fn as_bolt_str(&self) -> &'static str {
        match self {
            QueryType::Read => "r",
            QueryType::Write => "w",
            QueryType::ReadWrite => "rw",
            QueryType::Schema => "s",
        }
    }
}

thread_local! {
    static QUERY_STATS: RefCell<QueryStats> = RefCell::new(QueryStats::default());
}

/// Resets query stats for a new execution.
pub fn reset_query_stats() {
    QUERY_STATS.with(|s| *s.borrow_mut() = QueryStats::default());
}

/// Stats accumulated by the last execution.
pub fn get_query_stats() -> QueryStats {
    QUERY_STATS.with(|s| s.borrow().clone())
}

pub(crate) fn inc_stat(f: impl FnOnce(&mut QueryStats)) {
    QUERY_STATS.with(|s| f(&mut s.borrow_mut()));
}
