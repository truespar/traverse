//! Stack safety for deep recursion.
//!
//! Compilation and execution recurse over two user-controlled shapes:
//! **expression depth** (nesting and left-deep operator chains: parser,
//! validator, planner, optimizer and evaluator walk the `Expr` tree, as do the
//! derived `Clone` / `PartialEq` / `Drop`) and **plan depth** (every clause adds
//! a `LogicalPlan` level; optimizer passes, symbol-table builder, operator
//! builder and the pull chain recurse per level). The bundled Movies dataset (210
//! `CREATE` clauses in one statement) needs ~3 MiB of stack in debug builds, the
//! server's threads have 2 MiB, and a Rust stack overflow aborts the process.
//! So this is a robustness (and DoS) boundary.
//!
//! Layers, cheapest first:
//!
//! 1. **Measure, then size the stack once.** The parser records the deepest
//!    nesting and longest operator chain ([`ParseDepth`]), which bounds every
//!    later AST walk. An entry point runs validate -> plan -> optimize -> execute ->
//!    drop on one heap-allocated stack when the hint exceeds
//!    [`AST_DEPTH_THRESHOLD`] ([`with_stack_for_ast`]), the text exceeds
//!    [`BIG_QUERY_BYTES`] ([`with_stack_for_query`]), or the plan is deeper than
//!    [`DEEP_PLAN_DEPTH`] ([`run_deep_plan`]). Normal queries pay a few integer
//!    compares and a short plan walk; nothing per row or per expression node.
//! 2. `#[recursive::recursive]` (stacker growth) on the compile-time walkers
//!    (parser ladder, validator, planner, optimizer, physical lowering,
//!    expression compiler, plan formatting), the operator builder and the
//!    per-chunk expression evaluators: one pointer compare per call on
//!    once-per-query or once-per-chunk paths, never per row.
//! 3. Prefix chains (`NOT NOT ...`, `- - ...`, `!!...`) parse iteratively, using no
//!    parser stack.
//! 4. Hard limits ([`MAX_NESTING_DEPTH`] and [`MAX_CHAIN_LENGTH`] in both parsers,
//!    [`MAX_PLAN_DEPTH`] here) turn pathological input into a clean error
//!    instead of unbounded memory.
//!
//! Where `psm` can't switch stacks (`wasm32-unknown-unknown`) growth runs on the
//! current stack; the limits still apply.

use std::cell::Cell;

use crate::error::CypherError;
use crate::planner::LogicalPlan;

/// Max nesting of expressions, subqueries, label expressions and quantified path patterns in both
/// parsers. Real queries nest a few levels; 256 is generous and keeps the worst case cheap.
pub const MAX_NESTING_DEPTH: u32 = 256;

/// Max operands in one left-associative chain (`a OR b OR ...`, `x + y + ...`, `NOT NOT ...`). Chains
/// parse iteratively but build left-deep trees, so length is recursion depth for later walks. Far
/// beyond generated `OR` lists, which the planner turns into `IN [...]` anyway.
pub const MAX_CHAIN_LENGTH: usize = 4096;

/// AST depth hint above which an entry point uses a dedicated stack. Typical queries are well under
/// 16.
pub const AST_DEPTH_THRESHOLD: usize = 32;

/// Queries longer than this use a dedicated stack before the parser measured anything (text length
/// bounds every AST dimension). Typical queries are under 1 KiB; the Movies script is 28 KiB.
pub const BIG_QUERY_BYTES: usize = 4 * 1024;

/// Plans deeper than this optimize and execute on a dedicated stack. Far above hand-written
/// statements (20 clauses is already unusual), far below where 2 MiB gets tight in debug builds
/// (~300 levels).
pub const DEEP_PLAN_DEPTH: usize = 64;

/// Plans deeper than this are rejected (semantic error).
pub const MAX_PLAN_DEPTH: usize = 10_000;

/// Stack budget per AST level. Debug builds (MSVC `__chkstk` prologues) measured `eval_expr` 33
/// KiB, `exec_subquery` 45 KiB, `validate_expr` 9.5 KiB, `Expr::clone` 1.5 KiB per level; release
/// frames are ~10x smaller, so 16 KiB leaves wide margin (and the reservation is virtual).
const STACK_PER_AST_LEVEL: usize = if cfg!(debug_assertions) {
    64 * 1024
} else {
    16 * 1024
};

/// Stack budget per byte of query text. Densest shape is a `1+` chain, two bytes per tree level.
/// Only needs to carry the pipeline until the parser's measurement ([`with_stack_for_ast`]) takes
/// over with an exact budget, so it's sized for the compile-time walkers, not evaluation.
const STACK_PER_QUERY_BYTE: usize = if cfg!(debug_assertions) { 512 } else { 256 };

/// Stack budget per plan level on the dedicated stack. 416 levels of the debug symbol-table walk
/// overflowed 2 MiB and fit 3 MiB (~6-7 KiB/level); the pull chain and executor dispatch are
/// similar. 3x margin in debug; release frames are far smaller.
const STACK_PER_LEVEL: usize = if cfg!(debug_assertions) {
    24 * 1024
} else {
    8 * 1024
};

/// Fixed part of a dedicated stack: executor bookkeeping plus room for the `#[recursive]` red zone,
/// so compile-time walks inside don't immediately switch again.
const STACK_BASE: usize = 4 * 1024 * 1024;

/// Upper bound for any dedicated stack; past it the hard limits bound recursion.
const MAX_DEDICATED_STACK: usize = 512 * 1024 * 1024;

/// What the parser measured: deepest syntactic nesting and longest left-associative chain. Their
/// sum bounds every walk over the tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParseDepth {
    /// Deepest nesting of expressions / subqueries / label expressions / quantified path patterns.
    pub nesting: u32,
    /// Longest operator chain (operands), parsed into a tree that deep.
    pub chain: usize,
}

impl ParseDepth {
    /// Upper bound on any walk's depth over the tree.
    pub fn hint(&self) -> usize {
        (self.nesting as usize).saturating_add(self.chain)
    }
}

thread_local! {
    /// Size of the dedicated stack this thread runs on (0 = its own). Nested entry points
    /// (triggers, `apoc.cypher.run`, prepared-plan wrappers, the AST layer inside the query-length
    /// layer) reuse it if big enough, else grow again.
    static DEDICATED_STACK_BYTES: Cell<usize> = const { Cell::new(0) };
}

/// Depth of the deepest child-plan chain under `plan` (a leaf is 1). Iterative: the one plan walk
/// that must never recurse, since it decides whether the others may.
pub fn plan_depth(plan: &LogicalPlan) -> usize {
    let mut max = 0usize;
    let mut stack: Vec<(&LogicalPlan, usize)> = vec![(plan, 1)];
    while let Some((node, depth)) = stack.pop() {
        if depth > max {
            max = depth;
        }
        for child in children(node) {
            stack.push((child, depth + 1));
        }
    }
    max
}

/// Direct child plans of `plan`, any order. Keep in sync with `LogicalPlan`: a variant owning a
/// `Box<LogicalPlan>` missing here would make a deep chain behind it look shallow.
pub fn children(plan: &LogicalPlan) -> Vec<&LogicalPlan> {
    use LogicalPlan as P;
    match plan {
        P::ExpandOut { input, .. }
        | P::ExpandIn { input, .. }
        | P::ExpandBoth { input, .. }
        | P::DistinctExpand { input, .. }
        | P::Filter { input, .. }
        | P::Project { input, .. }
        | P::Sort { input, .. }
        | P::Limit { input, .. }
        | P::Skip { input, .. }
        | P::Aggregate { input, .. }
        | P::Unwind { input, .. }
        | P::LoadCsv { input, .. }
        | P::CreateNode { input, .. }
        | P::CreateRel { input, .. }
        | P::SetProperty { input, .. }
        | P::RemoveOp { input, .. }
        | P::Delete { input, .. }
        | P::Merge { input, .. }
        | P::BindPath { input, .. }
        | P::BindVarLists { input, .. }
        | P::BindQppGroups { input, .. }
        | P::BindPathEndpoints { input, .. }
        | P::BindEdgeEndpoints { input, .. }
        | P::CallProcedure { input, .. }
        | P::IndexLookupJoin { input, .. }
        | P::VectorSearchScan { input, .. }
        | P::Eager { input }
        | P::TransactionCommand { input, .. }
        | P::WeightedShortestPath { input, .. }
        | P::ShortestPath { input, .. } => vec![input],
        P::Union { left, right, .. }
        | P::CrossProduct { left, right, .. }
        | P::HashJoin { left, right, .. } => vec![left, right],
        P::ForEach { input, body, .. }
        | P::CallSubquery { input, body, .. }
        | P::CallInTransactions { input, body, .. } => vec![input, body],
        P::OptionalMatch {
            input, match_plan, ..
        } => vec![input, match_plan],
        _ => Vec::new(),
    }
}

/// Checks plan depth against [`MAX_PLAN_DEPTH`]; returns it.
pub fn check_plan_depth(plan: &LogicalPlan) -> Result<usize, CypherError> {
    let depth = plan_depth(plan);
    if depth > MAX_PLAN_DEPTH {
        return Err(CypherError::semantic(format!(
            "statement too complex: the query plan is {depth} levels deep (limit {MAX_PLAN_DEPTH}); \
             split it into several statements"
        )));
    }
    Ok(depth)
}

/// Runs `f` on a heap-allocated stack of at least `needed` bytes, in place if this thread's
/// dedicated stack is already that big.
fn grow<R>(needed: usize, f: impl FnOnce() -> R) -> R {
    let needed = needed.min(MAX_DEDICATED_STACK);
    let current = DEDICATED_STACK_BYTES.with(Cell::get);
    if current >= needed {
        return f();
    }
    // `maybe_grow` with red_zone == stack_size: switch unless the current stack still has the whole
    // budget
    stacker::maybe_grow(needed, needed, || {
        struct Reset(usize);
        impl Drop for Reset {
            fn drop(&mut self) {
                DEDICATED_STACK_BYTES.with(|c| c.set(self.0));
            }
        }
        let _reset = Reset(current);
        DEDICATED_STACK_BYTES.with(|c| c.set(needed));
        f()
    })
}

/// Runs `f` on a stack sized for the parser-measured AST depth; in place up to
/// [`AST_DEPTH_THRESHOLD`].
#[inline]
pub fn with_stack_for_ast<R>(depth: &ParseDepth, f: impl FnOnce() -> R) -> R {
    let hint = depth.hint();
    if hint <= AST_DEPTH_THRESHOLD {
        return f();
    }
    grow(
        STACK_BASE.saturating_add(hint.saturating_mul(STACK_PER_AST_LEVEL)),
        f,
    )
}

/// Runs the pipeline for `query` on a stack sized for its text length; in place up to
/// [`BIG_QUERY_BYTES`].
#[inline]
pub fn with_stack_for_query<R>(query: &str, f: impl FnOnce() -> R) -> R {
    if query.len() <= BIG_QUERY_BYTES {
        return f();
    }
    grow(
        STACK_BASE.saturating_add(query.len().saturating_mul(STACK_PER_QUERY_BYTE)),
        f,
    )
}

/// Runs `f` on a stack big enough for a plan of `depth`; in place up to [`DEEP_PLAN_DEPTH`].
#[inline]
pub fn with_stack_for_depth<R>(depth: usize, f: impl FnOnce() -> R) -> R {
    if depth <= DEEP_PLAN_DEPTH {
        return f();
    }
    grow(
        STACK_BASE.saturating_add(depth.saturating_mul(STACK_PER_LEVEL)),
        f,
    )
}

/// Optimizes + executes + drops an owned plan, on a dedicated stack if deep. By value because
/// dropping a deep `LogicalPlan` recurses per level too, so the drop must happen inside the grown
/// region: `f` consumes the plan entirely.
pub fn run_deep_plan<R>(
    plan: LogicalPlan,
    f: impl FnOnce(LogicalPlan) -> Result<R, CypherError>,
) -> Result<R, CypherError> {
    let depth = check_plan_depth(&plan)?;
    with_stack_for_depth(depth, move || f(plan))
}

/// Executes a borrowed (cached / prepared) plan, on a dedicated stack if deep.
pub fn run_borrowed_plan<R>(
    plan: &LogicalPlan,
    f: impl FnOnce() -> Result<R, CypherError>,
) -> Result<R, CypherError> {
    let depth = check_plan_depth(plan)?;
    with_stack_for_depth(depth, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_depth_is_one() {
        assert_eq!(plan_depth(&LogicalPlan::EmptyRow), 1);
    }

    #[test]
    fn spine_depth_counts_every_level() {
        let mut plan = LogicalPlan::EmptyRow;
        for _ in 0..300 {
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                count: crate::ast::Expr::Literal(crate::ast::Literal::Integer(1)),
            };
        }
        assert_eq!(plan_depth(&plan), 301);
        assert!(check_plan_depth(&plan).is_ok());
    }

    fn recurse(n: u32) -> u32 {
        let pad = [0u8; 4096];
        std::hint::black_box(&pad);
        if n == 0 { 0 } else { 1 + recurse(n - 1) }
    }

    fn on_tiny_thread(f: impl FnOnce() -> u32 + Send + 'static) -> u32 {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn deep_plans_grow_the_stack() {
        assert_eq!(
            on_tiny_thread(|| with_stack_for_depth(DEEP_PLAN_DEPTH + 1, || recurse(400))),
            400
        );
    }

    #[test]
    fn big_queries_grow_the_stack() {
        let query = "x".repeat(BIG_QUERY_BYTES + 1);
        assert_eq!(
            on_tiny_thread(move || with_stack_for_query(&query, || recurse(400))),
            400
        );
    }

    #[test]
    fn deep_asts_grow_the_stack() {
        let depth = ParseDepth {
            nesting: 1,
            chain: AST_DEPTH_THRESHOLD,
        };
        assert_eq!(
            on_tiny_thread(move || with_stack_for_ast(&depth, || recurse(400))),
            400
        );
    }

    #[test]
    fn nested_calls_reuse_a_big_enough_stack_and_grow_a_small_one() {
        with_stack_for_query(&"x".repeat(BIG_QUERY_BYTES + 1), || {
            let small = DEDICATED_STACK_BYTES.with(Cell::get);
            assert!(small > 0);
            // smaller or equal request: reused
            with_stack_for_depth(DEEP_PLAN_DEPTH + 1, || {
                assert_eq!(DEDICATED_STACK_BYTES.with(Cell::get), small);
            });
            // bigger request: larger stack, restored afterwards
            let depth = ParseDepth {
                nesting: 0,
                chain: MAX_CHAIN_LENGTH,
            };
            with_stack_for_ast(&depth, || {
                assert!(DEDICATED_STACK_BYTES.with(Cell::get) > small);
            });
            assert_eq!(DEDICATED_STACK_BYTES.with(Cell::get), small);
        });
        assert_eq!(DEDICATED_STACK_BYTES.with(Cell::get), 0);
    }
}
