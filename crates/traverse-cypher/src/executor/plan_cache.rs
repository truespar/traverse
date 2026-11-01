//! Plans for subqueries and pattern comprehensions, which run per outer row: cached by AST
//! address within an execution and by content across executions.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::error::CypherError;
use crate::planner::LogicalPlan;

thread_local! {
    /// Subquery plans keyed by the AST node's address. EXISTS / COUNT subqueries and pattern
    /// comprehensions run per outer row; this keeps them from being planned per row. Cleared per
    /// top-level query; sound because the prepared query owns the AST for the whole execution.
    static SUBQUERY_PLAN_CACHE: RefCell<HashMap<usize, LogicalPlan>> = RefCell::new(HashMap::new());
    /// Content-keyed plans that survive across queries (the pointer cache above is per
    /// execution). Key: hash of the sub-AST's Debug repr + a salt per AST kind. Hit once per
    /// distinct subquery per execution (the pointer cache serves rows), so the Debug cost
    /// amortizes.
    static SUBQUERY_PLAN_CACHE_PERSISTENT: RefCell<HashMap<u64, LogicalPlan>> =
        RefCell::new(HashMap::new());
}

/// Content fingerprint for the persistent subquery plan cache.
pub(super) fn subquery_fingerprint(debug_repr: &str, salt: u8) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    salt.hash(&mut h);
    debug_repr.hash(&mut h);
    h.finish()
}

/// Plan for the AST node at address `key`: from the pointer cache, else the content cache
/// (`fingerprint`), else `build` (stored in both).
pub(super) fn cached_plan(
    key: usize,
    fingerprint: impl FnOnce() -> u64,
    build: impl FnOnce() -> Result<LogicalPlan, CypherError>,
) -> Result<LogicalPlan, CypherError> {
    if let Some(plan) = SUBQUERY_PLAN_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Ok(plan);
    }
    let fp = fingerprint();
    let plan = match SUBQUERY_PLAN_CACHE_PERSISTENT.with(|c| c.borrow().get(&fp).cloned()) {
        Some(plan) => plan,
        None => {
            let plan = build()?;
            SUBQUERY_PLAN_CACHE_PERSISTENT.with(|c| {
                // bounded: plans are cheap to rebuild once, unbounded growth over many distinct
                // subqueries isn't
                let mut cache = c.borrow_mut();
                if cache.len() >= 512 {
                    cache.clear();
                }
                cache.insert(fp, plan.clone());
            });
            plan
        }
    };
    SUBQUERY_PLAN_CACHE.with(|c| c.borrow_mut().insert(key, plan.clone()));
    Ok(plan)
}

/// Drops the pointer-keyed plans; the content-keyed ones survive.
pub(super) fn clear_pointer_plans() {
    SUBQUERY_PLAN_CACHE.with(|c| c.borrow_mut().clear());
}
