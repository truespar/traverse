//! Traverse Cypher: openCypher parser, planner and executor over a
//! [`traverse_core::graph::Graph`].
//!
//! # Quick Start
//!
//! ```
//! use traverse_core::graph::Graph;
//! use traverse_cypher::execute;
//!
//! let mut g = Graph::new();
//! let result = execute(&mut g, "CREATE (n:Person {name: 'Alice'}) RETURN n.name").unwrap();
//! assert_eq!(result.columns, vec!["n.name"]);
//! assert_eq!(result.rows.len(), 1);
//! ```

#![warn(missing_docs)]

pub(crate) mod apoc;
pub(crate) mod apoc_export;
pub mod apoc_log;
pub mod apoc_scheduler;
pub(crate) mod apoc_trigger;
pub mod ast;
pub(crate) mod bytecode;
// the language bindings' file-bound databases, so not on wasm32
#[cfg(not(target_arch = "wasm32"))]
pub mod embedded;
pub mod error;
pub mod executor;
pub mod frame;
pub mod functions;
pub(crate) mod gds_catalog;
pub mod lexer;
pub mod memory;
pub(crate) mod operators;
pub mod optimizer;
pub mod parser;
pub mod plan_cache;
pub mod planner;
pub mod procedure;
pub mod query;
pub mod session;
pub mod settings;
pub mod stack;
pub mod statements;
pub mod status;
/// Temporal types (date, time, datetime, duration).
pub mod temporal;
pub mod transactions;
pub mod types;
pub mod validate;
pub(crate) mod xml;

pub mod algorithms;
pub mod ml;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
// `web_time::Instant` is `std::time::Instant` natively and a performance.now() shim on wasm32,
// where the std clock panics
use web_time::Instant;

use ast::QueryMode;
use error::CypherError;
use executor::CypherResult;
use planner::IndexHints;
use traverse_core::tx::view::GraphView;
use types::CypherValue;

/// Prepared (parsed, planned, optimized) query, ready to execute.
#[derive(Clone)]
pub struct PreparedQuery {
    /// Optimized logical plan.
    pub plan: planner::LogicalPlan,
    /// Output column names.
    pub columns: Vec<String>,
    /// Classified query type.
    pub query_type: executor::QueryType,
    /// Execution mode (Normal / Explain / Profile).
    pub mode: QueryMode,
    /// ISO section 14.5 dominant path mode of the statement, applied through a thread-local guard at
    /// execution (uniqueness rules live outside the plan, so cached plans carry it).
    pub path_mode: ast::PathMode,
}

/// Dominant ISO section 14.5 path mode of a statement: `ACYCLIC` beats `WALK` beats the `TRAIL` default
/// (program-level, like the GQL front-end).
fn dominant_path_mode(stmt: &ast::Statement) -> ast::PathMode {
    let mut mode = ast::PathMode::Trail;
    let mut visit_single = |sq: &ast::SingleQuery| {
        let clauses = sq
            .parts
            .iter()
            .flat_map(|p| p.clauses.iter())
            .chain(sq.final_part.clauses.iter());
        for c in clauses {
            if let ast::Clause::Match(mc) = c {
                for part in &mc.pattern.parts {
                    match part.path_mode {
                        ast::PathMode::Acyclic => mode = ast::PathMode::Acyclic,
                        ast::PathMode::Walk if mode == ast::PathMode::Trail => {
                            mode = ast::PathMode::Walk;
                        }
                        _ => {}
                    }
                }
            }
        }
    };
    if let ast::Query::Regular(ref rq) = stmt.query {
        visit_single(&rq.single);
        for u in &rq.unions {
            visit_single(&u.query);
        }
    }
    mode
}

/// ISO section 14.6 `ANY` / `ANY k`: the statement's smallest selector limit, injected as a top-level
/// LIMIT (like the GQL front-end's `apply_path_selector_limit`).
fn apply_dominant_selector_limit(stmt: &mut ast::Statement) {
    let mut limit: Option<usize> = None;
    let mut scan_single = |sq: &ast::SingleQuery| {
        let clauses = sq
            .parts
            .iter()
            .flat_map(|p| p.clauses.iter())
            .chain(sq.final_part.clauses.iter());
        for c in clauses {
            if let ast::Clause::Match(mc) = c {
                for part in &mc.pattern.parts {
                    if let Some(n) = part.path_selector_limit {
                        limit = Some(limit.map_or(n, |l: usize| l.min(n)));
                    }
                }
            }
        }
    };
    if let ast::Query::Regular(ref rq) = stmt.query {
        scan_single(&rq.single);
        for u in &rq.unions {
            scan_single(&u.query);
        }
    }
    let Some(n) = limit else { return };
    if let ast::Query::Regular(ref mut rq) = stmt.query {
        for sq in std::iter::once(&mut rq.single).chain(rq.unions.iter_mut().map(|u| &mut u.query))
        {
            if let Some(ref mut rc) = sq.final_part.return_clause
                && rc.limit.is_none()
            {
                rc.limit = Some(ast::Expr::Literal(ast::Literal::Integer(n as i64)));
            }
        }
    }
}

/// Parses and executes a Cypher query against `graph`; the engine's main entry point.
///
/// # Errors
///
/// [`CypherError`] on syntax, type or semantic errors and graph failures.
pub fn execute(graph: &mut dyn GraphView, query: &str) -> Result<CypherResult, CypherError> {
    stack::with_stack_for_query(query, || execute_inner(graph, query))
}

/// Body of the entry point above; see `stack::with_stack_for_query`.
fn execute_inner(graph: &mut dyn GraphView, query: &str) -> Result<CypherResult, CypherError> {
    executor::reset_query_stats();
    let _query = query::enter_statement(HashMap::new());
    let (mut stmt, parse_depth) = parser::parse_with_stats(query)?;
    // validate, plan, execute and drop on a stack sized for the AST depth the parser measured (see
    // `stack`)
    let graph = &mut *graph;
    stack::with_stack_for_ast(&parse_depth, move || {
        apply_dominant_selector_limit(&mut stmt);
        let pm = dominant_path_mode(&stmt);
        let _pm_guard = (pm != ast::PathMode::Trail).then(|| session::set_path_mode(pm));
        validate::validate(&stmt)?;
        let hints = build_index_hints(graph);
        let plan = planner::plan_statement_with_hints(&stmt, &hints)?;
        // deep plans (hundreds of clauses) optimize, execute and drop on a dedicated stack (see
        // `stack`)
        stack::run_deep_plan(plan, |plan| {
            let plan = optimizer::optimize(plan, graph, &hints);
            match stmt.mode {
                QueryMode::Explain => Ok(make_explain_result(&plan)),
                QueryMode::Profile => {
                    let plan_cv = executor::plan_to_cypher_value(&plan);
                    let mut result = execute_plan_with_triggers(&plan, graph)?;
                    result.plan = Some(plan_cv);
                    Ok(result)
                }
                QueryMode::Normal => execute_plan_with_triggers(&plan, graph),
            }
        })
    })
}

/// Parses and executes a Cypher query with parameters.
///
/// # Errors
///
/// [`CypherError`] on syntax, type or semantic errors and graph failures.
pub fn execute_with_params(
    graph: &mut dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<CypherResult, CypherError> {
    stack::with_stack_for_query(query, || execute_with_params_inner(graph, query, params))
}

/// Body of the entry point above; see `stack::with_stack_for_query`.
fn execute_with_params_inner(
    graph: &mut dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<CypherResult, CypherError> {
    executor::reset_query_stats();
    let _query = query::enter_statement(params.clone());
    let result = (|| {
        let (mut stmt, parse_depth) = parser::parse_with_stats(query)?;
        let graph = &mut *graph;
        stack::with_stack_for_ast(&parse_depth, move || {
            apply_dominant_selector_limit(&mut stmt);
            let pm = dominant_path_mode(&stmt);
            let _pm_guard = (pm != ast::PathMode::Trail).then(|| session::set_path_mode(pm));
            validate::validate(&stmt)?;
            let hints = build_index_hints(graph);
            let plan = planner::plan_statement_with_hints(&stmt, &hints)?;
            stack::run_deep_plan(plan, |plan| {
                let plan = optimizer::optimize(plan, graph, &hints);
                match stmt.mode {
                    QueryMode::Explain => Ok(make_explain_result(&plan)),
                    QueryMode::Profile => {
                        let plan_cv = executor::plan_to_cypher_value(&plan);
                        let mut result = execute_plan_with_triggers(&plan, graph)?;
                        result.plan = Some(plan_cv);
                        Ok(result)
                    }
                    QueryMode::Normal => execute_plan_with_triggers(&plan, graph),
                }
            })
        })
    })();
    executor::end_execution();
    result
}

/// Executes a pre-parsed AST with parameters, returning plan and execute timings
/// (parse is zero).
///
/// For alternative front-ends such as `traverse-gql`, which parses ISO/IEC 39075
/// and lowers it to a Cypher `Statement`. Skips the Cypher parser; validation,
/// planning and execution match [`execute_with_params_timed`]. Callers measuring
/// their own parse/lower cost add it to the zero `parse_duration`.
///
/// # Errors
///
/// [`CypherError`] on validation, planning or execution failures.
pub fn execute_statement_with_params_timed(
    graph: &mut dyn GraphView,
    stmt: ast::Statement,
    params: &HashMap<String, CypherValue>,
) -> Result<(CypherResult, Duration, Duration, Duration), CypherError> {
    executor::reset_query_stats();
    let _query = query::enter_statement(params.clone());
    let result = (|| {
        // phase 1: validate (the caller already has the AST)
        let t0 = Instant::now();
        validate::validate(&stmt)?;
        let parse_dur = t0.elapsed();

        // phase 2: plan + optimize
        let t1 = Instant::now();
        let hints = build_index_hints(graph);
        let plan = planner::plan_statement_with_hints(&stmt, &hints)?;
        stack::run_deep_plan(plan, |plan| {
            let plan = optimizer::optimize(plan, graph, &hints);
            let plan_dur = t1.elapsed();

            // phase 3: execute
            let t2 = Instant::now();
            let result = match stmt.mode {
                QueryMode::Explain => Ok(make_explain_result(&plan)),
                QueryMode::Profile => {
                    let plan_cv = executor::plan_to_cypher_value(&plan);
                    let mut result = execute_plan_with_triggers(&plan, graph)?;
                    result.plan = Some(plan_cv);
                    Ok(result)
                }
                QueryMode::Normal => execute_plan_with_triggers(&plan, graph),
            }?;
            let exec_dur = t2.elapsed();

            Ok((result, parse_dur, plan_dur, exec_dur))
        })
    })();
    executor::end_execution();
    result
}

/// [`execute_with_params`] with separate timings: `(result, parse_duration, plan_duration,
/// exec_duration)`.
pub fn execute_with_params_timed(
    graph: &mut dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<(CypherResult, Duration, Duration, Duration), CypherError> {
    stack::with_stack_for_query(query, || {
        execute_with_params_timed_inner(graph, query, params)
    })
}

/// Body of the entry point above; see `stack::with_stack_for_query`.
fn execute_with_params_timed_inner(
    graph: &mut dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<(CypherResult, Duration, Duration, Duration), CypherError> {
    executor::reset_query_stats();
    let _query = query::enter_statement(params.clone());
    let result = (|| {
        // phase 1: parse + validate
        let t0 = Instant::now();
        let (mut stmt, parse_depth) = parser::parse_with_stats(query)?;
        let graph = &mut *graph;
        stack::with_stack_for_ast(&parse_depth, move || {
            apply_dominant_selector_limit(&mut stmt);
            let pm = dominant_path_mode(&stmt);
            let _pm_guard = (pm != ast::PathMode::Trail).then(|| session::set_path_mode(pm));
            validate::validate(&stmt)?;
            let parse_dur = t0.elapsed();

            // phase 2: plan + optimize
            let t1 = Instant::now();
            let hints = build_index_hints(graph);
            let plan = planner::plan_statement_with_hints(&stmt, &hints)?;
            stack::run_deep_plan(plan, |plan| {
                let plan = optimizer::optimize(plan, graph, &hints);
                let plan_dur = t1.elapsed();

                // phase 3: execute
                let t2 = Instant::now();
                let result = match stmt.mode {
                    QueryMode::Explain => Ok(make_explain_result(&plan)),
                    QueryMode::Profile => {
                        let plan_cv = executor::plan_to_cypher_value(&plan);
                        let mut result = execute_plan_with_triggers(&plan, graph)?;
                        result.plan = Some(plan_cv);
                        Ok(result)
                    }
                    QueryMode::Normal => execute_plan_with_triggers(&plan, graph),
                }?;
                let exec_dur = t2.elapsed();

                Ok((result, parse_dur, plan_dur, exec_dur))
            })
        })
    })();
    executor::end_execution();
    result
}

/// Prepares a query without running it (parse, validate, plan, optimize); run it later with
/// [`execute_prepared`].
pub fn prepare_query(
    graph: &dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<PreparedQuery, CypherError> {
    stack::with_stack_for_query(query, || prepare_query_inner(graph, query, params))
}

/// Body of the entry point above; see `stack::with_stack_for_query`.
fn prepare_query_inner(
    graph: &dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<PreparedQuery, CypherError> {
    // planning reads parameters (implicit CALL arguments, dynamic DDL names)
    let _query = query::enter_statement(params.clone());
    let (mut stmt, parse_depth) = parser::parse_with_stats(query)?;
    stack::with_stack_for_ast(&parse_depth, move || {
        apply_dominant_selector_limit(&mut stmt);
        let pm = dominant_path_mode(&stmt);
        let _pm_guard = (pm != ast::PathMode::Trail).then(|| session::set_path_mode(pm));
        validate::validate(&stmt)?;
        let hints = build_index_hints(graph);
        let plan = planner::plan_statement_with_hints(&stmt, &hints)?;
        let plan = optimizer::optimize(plan, graph, &hints);
        let columns = executor::plan_columns(&plan);
        let query_type = executor::classify_plan(&plan);
        Ok(PreparedQuery {
            plan,
            columns,
            query_type,
            mode: stmt.mode,
            path_mode: dominant_path_mode(&stmt),
        })
    })
}

/// [`prepare_query`] with separate parse and plan durations.
pub fn prepare_query_timed(
    graph: &dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<(PreparedQuery, std::time::Duration, std::time::Duration), CypherError> {
    stack::with_stack_for_query(query, || prepare_query_timed_inner(graph, query, params))
}

/// Body of the entry point above; see `stack::with_stack_for_query`.
fn prepare_query_timed_inner(
    graph: &dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<(PreparedQuery, std::time::Duration, std::time::Duration), CypherError> {
    use web_time::Instant;
    // planning reads parameters (implicit CALL arguments, dynamic DDL names)
    let _query = query::enter_statement(params.clone());

    let parse_start = Instant::now();
    let (mut stmt, parse_depth) = parser::parse_with_stats(query)?;
    stack::with_stack_for_ast(&parse_depth, move || {
        apply_dominant_selector_limit(&mut stmt);
        let pm = dominant_path_mode(&stmt);
        let _pm_guard = (pm != ast::PathMode::Trail).then(|| session::set_path_mode(pm));
        validate::validate(&stmt)?;
        let parsing_time = parse_start.elapsed();

        let plan_start = Instant::now();
        let hints = build_index_hints(graph);
        let plan = planner::plan_statement_with_hints(&stmt, &hints)?;
        let plan = optimizer::optimize(plan, graph, &hints);
        let columns = executor::plan_columns(&plan);
        let query_type = executor::classify_plan(&plan);
        let planning_time = plan_start.elapsed();

        Ok((
            PreparedQuery {
                plan,
                columns,
                query_type,
                mode: stmt.mode,
                path_mode: dominant_path_mode(&stmt),
            },
            parsing_time,
            planning_time,
        ))
    })
}

impl Drop for PreparedQuery {
    /// Prepared plans live in caches and drop on whichever thread evicts them. Dropping a
    /// `LogicalPlan` recurses per level, so deep plans drop on a stack sized for them (see
    /// `stack`).
    fn drop(&mut self) {
        let plan = std::mem::replace(&mut self.plan, planner::LogicalPlan::EmptyRow);
        let depth = stack::plan_depth(&plan);
        stack::with_stack_for_depth(depth, move || drop(plan));
    }
}

/// Executes a prepared query.
pub fn execute_prepared(
    graph: &mut dyn GraphView,
    prepared: &PreparedQuery,
    params: &HashMap<String, CypherValue>,
) -> Result<CypherResult, CypherError> {
    execute_prepared_shared(graph, prepared, Arc::new(params.clone()))
}

/// [`execute_prepared`] with parameters the caller already shares (no copy).
pub(crate) fn execute_prepared_shared(
    graph: &mut dyn GraphView,
    prepared: &PreparedQuery,
    params: query::Params,
) -> Result<CypherResult, CypherError> {
    executor::reset_query_stats();
    let _query = query::enter_statement(params);
    let _pm_guard = (prepared.path_mode != ast::PathMode::Trail)
        .then(|| session::set_path_mode(prepared.path_mode));
    let result = match prepared.mode {
        QueryMode::Explain => Ok(make_explain_result(&prepared.plan)),
        QueryMode::Profile => {
            let plan_cv = executor::plan_to_cypher_value(&prepared.plan);
            let mut result = stack::run_borrowed_plan(&prepared.plan, || {
                execute_plan_with_triggers(&prepared.plan, graph)
            })?;
            result.plan = Some(plan_cv);
            Ok(result)
        }
        QueryMode::Normal => stack::run_borrowed_plan(&prepared.plan, || {
            execute_plan_with_triggers(&prepared.plan, graph)
        }),
    };
    executor::end_execution();
    result
}

/// Executes a prepared query, streaming when possible: qualifying `Normal`
/// queries return `Streaming` (rows pulled one at a time; the stream carries the
/// statement's context, so it can be pulled from any thread). EXPLAIN, PROFILE,
/// morsel-parallel and projection-less queries return `Materialized`.
pub fn execute_prepared_streaming<'a>(
    graph: &'a mut dyn GraphView,
    prepared: &'a PreparedQuery,
    params: &HashMap<String, CypherValue>,
) -> Result<executor::QueryExecution<'a>, CypherError> {
    executor::reset_query_stats();
    let _query = query::enter_statement(params.clone());
    match prepared.mode {
        QueryMode::Explain => {
            let result = make_explain_result(&prepared.plan);
            executor::end_execution();
            Ok(executor::QueryExecution::Materialized(result))
        }
        QueryMode::Profile => {
            let _pm_guard = (prepared.path_mode != ast::PathMode::Trail)
                .then(|| session::set_path_mode(prepared.path_mode));
            let plan_cv = executor::plan_to_cypher_value(&prepared.plan);
            let mut result = stack::run_borrowed_plan(&prepared.plan, || {
                executor::execute_plan(&prepared.plan, graph)
            })?;
            result.plan = Some(plan_cv);
            executor::end_execution();
            Ok(executor::QueryExecution::Materialized(result))
        }
        QueryMode::Normal => {
            // non-default path modes are read from a thread-local by the matcher; a stream would
            // outlive the guard, so materialize under it (rare)
            if prepared.path_mode != ast::PathMode::Trail {
                let _pm_guard = session::set_path_mode(prepared.path_mode);
                let result = stack::run_borrowed_plan(&prepared.plan, || {
                    executor::execute_plan(&prepared.plan, graph)
                })?;
                executor::end_execution();
                return Ok(executor::QueryExecution::Materialized(result));
            }
            let exec = executor::execute_plan_streaming(&prepared.plan, graph)?;
            if matches!(exec, executor::QueryExecution::Materialized(_)) {
                executor::end_execution();
            }
            Ok(exec)
        }
    }
}

/// Executes a prepared query, lazily when possible: `Normal` queries return `Lazy` (rows pulled via
/// [`executor::LazyResult::next_row()`]); EXPLAIN, PROFILE and errors return `Materialized`.
pub fn execute_prepared_lazy(
    graph: &mut dyn GraphView,
    prepared: &PreparedQuery,
    params: &HashMap<String, CypherValue>,
) -> Result<executor::QueryResultLazy, CypherError> {
    executor::reset_query_stats();
    let _query = query::enter_statement(params.clone());
    match prepared.mode {
        QueryMode::Explain => {
            let result = make_explain_result(&prepared.plan);
            executor::end_execution();
            Ok(executor::QueryResultLazy::Materialized(result))
        }
        QueryMode::Profile => {
            let _pm_guard = (prepared.path_mode != ast::PathMode::Trail)
                .then(|| session::set_path_mode(prepared.path_mode));
            let plan_cv = executor::plan_to_cypher_value(&prepared.plan);
            let mut result = stack::run_borrowed_plan(&prepared.plan, || {
                executor::execute_plan(&prepared.plan, graph)
            })?;
            result.plan = Some(plan_cv);
            executor::end_execution();
            Ok(executor::QueryResultLazy::Materialized(result))
        }
        QueryMode::Normal => {
            // the lazy result keeps the statement's context (parameters, clock, limits) and
            // enters it on whichever thread pulls rows later (Bolt PULL can resume elsewhere)
            //
            // non-default path modes: the guard can't outlive this function, so
            // materialize under it (rare)
            if prepared.path_mode != ast::PathMode::Trail {
                let _pm_guard = session::set_path_mode(prepared.path_mode);
                let result = stack::run_borrowed_plan(&prepared.plan, || {
                    executor::execute_plan(&prepared.plan, graph)
                })?;
                return Ok(executor::QueryResultLazy::Materialized(result));
            }
            executor::execute_plan_lazy(&prepared.plan, graph)
        }
    }
}

/// Executes a plan at a TOP-LEVEL query boundary: arms mutation-event capture if any
/// `apoc.trigger.*` is installed and fires the triggers after success (the embedded commit point).
/// Inner executions (subqueries, apoc.cypher.run, triggers) call `execute_plan` directly and share
/// the capture window without re-firing.
fn execute_plan_with_triggers(
    plan: &planner::LogicalPlan,
    graph: &mut dyn GraphView,
) -> Result<CypherResult, CypherError> {
    // pointer-keyed caches use AST addresses, which a freed previous query can alias: clear per
    // execution (the content-keyed caches refill them)
    executor::reset_pointer_caches();
    let armed = apoc_trigger::any_active() && !apoc_trigger::firing();
    if !armed {
        return executor::execute_plan(plan, graph);
    }
    traverse_core::graph::events::set_capture(true);
    let result = executor::execute_plan(plan, graph);
    traverse_core::graph::events::set_capture(false);
    let events = traverse_core::graph::events::drain();
    match result {
        Ok(res) => {
            apoc_trigger::fire(graph, events)?;
            Ok(res)
        }
        Err(e) => Err(e),
    }
}

/// EXPLAIN result without executing.
fn make_explain_result(plan: &planner::LogicalPlan) -> CypherResult {
    let plan_cv = executor::plan_to_cypher_value(plan);
    let query_type = executor::classify_plan(plan);
    CypherResult {
        columns: vec![],
        rows: vec![],
        query_type,
        stats: executor::QueryStats::default(),
        plan: Some(plan_cv),
        bound: Vec::new(),
    }
}

/// `build_index_hints` visible to `executor::optimize_subquery_plan`, so EXISTS/COUNT subqueries
/// get the full optimizer.
pub(crate) fn build_index_hints_for(graph: &dyn GraphView) -> IndexHints {
    build_index_hints(graph)
}

/// Index hints from the graph's index registry.
fn build_index_hints(graph: &dyn GraphView) -> IndexHints {
    graph
        .index_catalog()
        .descriptors()
        .iter()
        .filter_map(|d| {
            let lbl = graph.strings().try_resolve(d.label_id.0)?;
            let prop = graph.strings().try_resolve(d.property_key.0)?;
            Some((lbl.to_string(), prop.to_string()))
        })
        .collect()
}

/// Allocation-free case-insensitive substring search (`needle_lower` must be lowercase ASCII). Runs
/// on every query via `strip_literals`, so no `to_ascii_lowercase()` allocation here.
pub fn contains_ignore_ascii_case(haystack: &str, needle_lower: &[u8]) -> bool {
    let hay = haystack.as_bytes();
    if needle_lower.is_empty() || hay.len() < needle_lower.len() {
        return false;
    }
    let first = needle_lower[0];
    for start in 0..=(hay.len() - needle_lower.len()) {
        if hay[start].to_ascii_lowercase() != first {
            continue;
        }
        if hay[start..start + needle_lower.len()]
            .iter()
            .zip(needle_lower)
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
        {
            return true;
        }
    }
    false
}

/// Memoized [`strip_literals`]: a pure function of the text that runs on EVERY Bolt RUN for
/// plan-cache keys, a sizable share of point-read dispatch. Per worker thread, Arc-shared results,
/// capped.
pub fn strip_literals_cached(
    query: &str,
) -> Result<std::sync::Arc<(String, Vec<CypherValue>)>, CypherError> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;
    // L1 (thread-local) keeps steady-state hits free of shared state; L2 (global) turns a
    // cold-thread miss after tokio task migration into a read-locked lookup instead of a
    // re-tokenize
    thread_local! {
        static MEMO: RefCell<HashMap<String, Arc<(String, Vec<CypherValue>)>>> =
            RefCell::new(HashMap::new());
    }
    static SHARED: std::sync::OnceLock<
        std::sync::RwLock<HashMap<String, Arc<(String, Vec<CypherValue>)>>>,
    > = std::sync::OnceLock::new();
    if let Some(hit) = MEMO.with(|m| m.borrow().get(query).cloned()) {
        return Ok(hit);
    }
    let shared = SHARED.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    let v = match shared.read().ok().and_then(|m| m.get(query).cloned()) {
        Some(v) => v,
        None => {
            let v = Arc::new(strip_literals(query)?);
            if let Ok(mut m) = shared.write() {
                if m.len() >= 2048 {
                    m.clear();
                }
                m.insert(query.to_string(), v.clone());
            }
            v
        }
    };
    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= 2048 {
            m.clear();
        }
        m.insert(query.to_string(), v.clone());
    });
    Ok(v)
}

/// Normalises a query for plan caching: literals become `$__spN` placeholders, returned in order,
/// so queries differing only in literals share a plan (`MATCH (n) WHERE n.age > 30 RETURN n` -> `...
/// n.age > $__sp0 ...`). Positions where a placeholder wouldn't parse keep their literal.
pub fn strip_literals(query: &str) -> Result<(String, Vec<CypherValue>), CypherError> {
    // apoc.cypher.runFirstColumn* inlines its statement literal at parse time (COLLECT{} desugar),
    // where a placeholder is a semantic error: cache those queries by full text
    if query.len() < 16_384 && contains_ignore_ascii_case(query, b"runfirstcolumn") {
        return Ok((query.to_string(), Vec::new()));
    }
    let tokens = lexer::tokenize(query)?;
    let mut stripped = String::with_capacity(query.len());
    let mut params = Vec::new();
    let mut last_end = 0;

    // integers in QPP quantifiers (`-[:R]->{1,3}`, `((...)){2}`, `{,4}`, `{2,}`) are grammar, folded
    // into the relationship range at parse time, so a placeholder would be a syntax error. Detect
    // `{` right after `>` / `-` / `)` holding only integers and commas, and exempt those integers
    let mut qpp_exempt = vec![false; tokens.len()];
    for (i, token) in tokens.iter().enumerate() {
        if !matches!(token.kind, lexer::TokenKind::LBrace) {
            continue;
        }
        let after_quantifiable = i > 0
            && matches!(
                tokens[i - 1].kind,
                lexer::TokenKind::Gt | lexer::TokenKind::Minus | lexer::TokenKind::RParen
            );
        if !after_quantifiable {
            continue;
        }
        // lookahead `{ (Integer|Comma){1,3} }`; anything else (idents, colons, ...) is a subquery or
        // map
        let mut j = i + 1;
        let mut int_idxs = Vec::new();
        let mut ok = false;
        while j < tokens.len() && j <= i + 4 {
            match tokens[j].kind {
                lexer::TokenKind::Integer(_) => int_idxs.push(j),
                lexer::TokenKind::Comma => {}
                lexer::TokenKind::RBrace => {
                    ok = j > i + 1;
                    break;
                }
                _ => break,
            }
            j += 1;
        }
        if ok {
            for idx in int_idxs {
                qpp_exempt[idx] = true;
            }
        }
    }

    for (i, token) in tokens.iter().enumerate() {
        // range bounds next to `..` stay: `[*1..2]` -> Integer(1), DotDot, Integer(2)
        let is_range_bound = matches!(token.kind, lexer::TokenKind::Integer(_))
            && (matches!(
                tokens.get(i + 1).map(|t| &t.kind),
                Some(lexer::TokenKind::DotDot)
            ) || (i > 0 && matches!(tokens[i - 1].kind, lexer::TokenKind::DotDot)));

        // ISO section 14.6 selector counts are grammar (`SHORTEST 3 (...)`, `ANY 2 (...)`): a placeholder
        // would be a syntax error
        let is_selector_count = matches!(token.kind, lexer::TokenKind::Integer(_))
            && i > 0
            && matches!(
                &tokens[i - 1].kind,
                lexer::TokenKind::Ident(s)
                    if s.eq_ignore_ascii_case("SHORTEST") || s.eq_ignore_ascii_case("ANY")
            );

        let replacement = if is_range_bound || qpp_exempt[i] || is_selector_count {
            None
        } else {
            match &token.kind {
                lexer::TokenKind::Integer(n) => {
                    params.push(CypherValue::Integer(*n));
                    Some(format!("$__sp{}", params.len() - 1))
                }
                lexer::TokenKind::OverflowInt(_) => None, // rare, skip
                lexer::TokenKind::Float(f) => {
                    params.push(CypherValue::Float(*f));
                    Some(format!("$__sp{}", params.len() - 1))
                }
                lexer::TokenKind::StringLit(s) => {
                    params.push(CypherValue::String(s.clone()));
                    Some(format!("$__sp{}", params.len() - 1))
                }
                // not stripped: True, False, Null (keywords), parameters (already parameterized)
                _ => None,
            }
        };

        if let Some(placeholder) = replacement {
            // text between the previous token and this one
            stripped.push_str(&query[last_end..token.span.start]);
            stripped.push_str(&placeholder);
            last_end = token.span.end;
        }
    }
    // rest of the text
    stripped.push_str(&query[last_end..]);

    Ok((stripped, params))
}

/// Parses a Cypher query into an AST without executing it (validation, tooling).
///
/// # Errors
///
/// [`CypherError::SyntaxError`] for malformed queries.
pub fn parse(query: &str) -> Result<ast::Statement, CypherError> {
    parser::parse(query)
}

#[cfg(test)]
mod tests;
