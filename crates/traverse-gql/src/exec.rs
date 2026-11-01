//! In-process GQL execution: the parse -> lower -> execute pipeline shared by
//! the server's `X-Query-Dialect: gql` route and the embedded bindings (PyO3 /
//! JNI / napi-rs / C FFI). Parse the ISO/IEC 39075 text, lower it to the Cypher
//! AST, install the section 14.5/section 14.6 path-mode and path-selector hooks, run it on
//! the shared engine. One implementation for every embedder, including the
//! easy-to-forget path-mode guard.
//!
//! Two statements have no Cypher equivalent and run here: the section 15.4 conditional
//! (`WHEN ... THEN ... [ELSE ...]`) and the transaction block (`START TRANSACTION ...
//! COMMIT | ROLLBACK`).
//!
//! Catalog statements (`CREATE GRAPH`, `CREATE SCHEMA`, ...) need a database
//! registry, which only the server has; here they surface as the lowerer's
//! "not supported" syntax error, the right answer for a single-graph embedded
//! session.

use std::collections::HashMap;
use std::time::Duration;

use traverse_core::tx::view::GraphView;
use traverse_cypher::ast as cy;
use traverse_cypher::error::CypherError;
use traverse_cypher::executor::{CypherResult, QueryStats, QueryType};
use traverse_cypher::status::GqlStatus;
use traverse_cypher::types::CypherValue;

use crate::ast::{Expression, Program, Statement, UseReference};
use crate::error::ParseError;
use crate::lower;

/// GQL parse/lower diagnostics -> one Cypher syntax error, all messages joined.
/// `CypherError::syntax` takes a byte offset; the first error's span start keeps editor markers
/// aligned.
pub fn errors_to_cypher_error(errs: Vec<ParseError>) -> CypherError {
    let first_span = errs.first().map(|e| e.span.start).unwrap_or(0);
    let msg = errs
        .iter()
        .map(|e| format!("{}..{}: {}", e.span.start, e.span.end, e.message))
        .collect::<Vec<_>>()
        .join("; ");
    CypherError::syntax(format!("GQL: {msg}"), first_span)
}

/// Whether `query` changes data, schema or the catalog, judged from its plans without running it:
/// an auto-commit caller runs a read on a snapshot and a write in a write transaction.
pub fn query_writes(query: &str) -> Result<bool, CypherError> {
    let program = crate::parse(query).map_err(errors_to_cypher_error)?;
    for stmt in &program.statements {
        if writes(stmt)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Runs a GQL query against a graph view. A `USE` in it is the host's routing: it was sent to the
/// graph it names (see [`use_target`]).
pub fn execute(
    graph: &mut dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<CypherResult, CypherError> {
    execute_timed(graph, query, params).map(|(result, ..)| result)
}

/// [`execute`] on the graph named `graph_name`, the only one a single-graph host (an embedded
/// database) has: a `USE` naming another graph fails.
pub fn execute_on(
    graph: &mut dyn GraphView,
    graph_name: &str,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<CypherResult, CypherError> {
    traverse_cypher::stack::with_stack_for_query(query, || {
        execute_timed_inner(graph, Some(graph_name), query, params)
    })
    .map(|(result, ..)| result)
}

/// Runs a GQL query, also returning `(parse+lower, planning, execution)` durations. The first
/// covers the whole GQL front end; the engine's own parse phase is zero for pre-lowered statements.
pub fn execute_timed(
    graph: &mut dyn GraphView,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<(CypherResult, Duration, Duration, Duration), CypherError> {
    traverse_cypher::stack::with_stack_for_query(query, || {
        execute_timed_inner(graph, None, query, params)
    })
}

/// The graph a query names with `USE` (ISO/IEC 39075 section 7.2), if any: a host with several graphs
/// runs the query on it. `USE name`, `USE a.b` and `USE graph.byName(<string or parameter>)` are
/// understood; every `USE` in one request must name the same graph (`25G04` otherwise).
pub fn use_target(
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<Option<String>, CypherError> {
    let program = crate::parse(query).map_err(errors_to_cypher_error)?;
    program_use_target(&program, query, params)
}

fn program_use_target(
    program: &Program,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<Option<String>, CypherError> {
    fn visit(
        stmt: &Statement,
        query: &str,
        params: &HashMap<String, CypherValue>,
        target: &mut Option<String>,
    ) -> Result<(), CypherError> {
        match stmt {
            Statement::Use {
                graph, reference, ..
            } => {
                let name = use_name(graph, reference, query, params)?;
                match target {
                    Some(t) if *t != name => {
                        return Err(CypherError::semantic(format!(
                            "a request can use one graph: `{t}` and `{name}`"
                        ))
                        .with_status(GqlStatus::MultipleGraphs));
                    }
                    _ => *target = Some(name),
                }
            }
            Statement::Transaction { statements, .. } => {
                for s in statements {
                    visit(s, query, params, target)?;
                }
            }
            Statement::When {
                then_stmt,
                else_stmt,
                ..
            } => {
                visit(then_stmt, query, params, target)?;
                if let Some(e) = else_stmt {
                    visit(e, query, params, target)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut target = None;
    for stmt in &program.statements {
        visit(stmt, query, params, &mut target)?;
    }
    Ok(target)
}

/// The graph name a `USE` reference names.
fn use_name(
    graph: &str,
    reference: &UseReference,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<String, CypherError> {
    match reference {
        UseReference::Simple => Ok(graph.to_string()),
        UseReference::Composite { segments } => Ok(segments.join(".")),
        UseReference::Dynamic {
            function,
            arg_spans,
        } => {
            let by_name = function.join(".").eq_ignore_ascii_case("graph.byName");
            if let ([span], true) = (arg_spans.as_slice(), by_name) {
                let raw = query.get(span.start..span.end).unwrap_or("").trim();
                if let Some(name) = raw.strip_prefix('$') {
                    return match params.get(name) {
                        Some(CypherValue::String(s)) => Ok(s.clone()),
                        _ => Err(CypherError::semantic(format!(
                            "USE graph.byName(${name}): expected a string parameter"
                        ))
                        .with_status(GqlStatus::InvalidParameter)),
                    };
                }
                let quoted = raw.len() >= 2
                    && ((raw.starts_with('\'') && raw.ends_with('\''))
                        || (raw.starts_with('"') && raw.ends_with('"')));
                if quoted {
                    return Ok(raw[1..raw.len() - 1].to_string());
                }
            }
            Err(CypherError::semantic(
                "USE takes a graph name or graph.byName(<string or parameter>)",
            )
            .with_status(GqlStatus::NotSupported))
        }
    }
}

/// `program` without its top-level `USE` statements (and their separators), which the host has
/// already acted on.
fn without_use(mut program: Program) -> Program {
    let mut i = 0;
    while i < program.statements.len() {
        if matches!(program.statements[i], Statement::Use { .. }) {
            program.statements.remove(i);
            if i < program.separators.len() {
                program.separators.remove(i);
            } else if i > 0 {
                program.separators.remove(i - 1);
            }
        } else {
            i += 1;
        }
    }
    program
}

/// Body of [`execute_timed`] and [`execute_on`]; runs on a query-sized stack for large texts (see
/// `traverse_cypher::stack`).
fn execute_timed_inner(
    graph: &mut dyn GraphView,
    graph_name: Option<&str>,
    query: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<(CypherResult, Duration, Duration, Duration), CypherError> {
    // web_time: std Instant natively, performance.now() shim on wasm32
    let t0 = web_time::Instant::now();
    let (program, parse_depth) = crate::parse_with_stats(query).map_err(errors_to_cypher_error)?;
    let target = program_use_target(&program, query, params)?;
    if let (Some(target), Some(name)) = (&target, graph_name)
        && target != name
    {
        return Err(CypherError::semantic(format!(
            "graph `{target}` not found: this database is `{name}`"
        ))
        .with_status(GqlStatus::GraphNotFound));
    }
    let program = without_use(program);
    let mut timing = Timing {
        front: t0.elapsed(),
        plan: Duration::ZERO,
        exec: Duration::ZERO,
    };
    if program.statements.is_empty() {
        // only `USE`: nothing to run
        return Ok((empty_result(), timing.front, timing.plan, timing.exec));
    }
    // lower, validate, plan and execute on a stack sized for the parser's measured AST depth (see
    // `traverse_cypher::stack`)
    let graph = &mut *graph;
    traverse_cypher::stack::with_stack_for_ast(&parse_depth, move || {
        let result = run_program(graph, &program, params, &mut timing)?;
        Ok((result, timing.front, timing.plan, timing.exec))
    })
}

/// Time in the GQL front end (parse + lower), planning and execution, summed over every statement
/// a conditional or transaction block runs.
struct Timing {
    front: Duration,
    plan: Duration,
    exec: Duration,
}

fn run_program(
    graph: &mut dyn GraphView,
    program: &Program,
    params: &HashMap<String, CypherValue>,
    timing: &mut Timing,
) -> Result<CypherResult, CypherError> {
    match program.statements.as_slice() {
        [
            Statement::When {
                condition,
                then_stmt,
                else_stmt,
                ..
            },
        ] => {
            let branch = if when_condition(graph, condition, params, timing)? {
                Some(then_stmt.as_ref())
            } else {
                else_stmt.as_deref()
            };
            match branch {
                Some(stmt) => run_program(graph, &single(stmt), params, timing),
                // false without ELSE: nothing runs, no rows
                None => Ok(empty_result()),
            }
        }
        [
            Statement::Transaction {
                read_only,
                rollback,
                statements,
                ..
            },
        ] => run_transaction(graph, *read_only, *rollback, statements, params, timing),
        _ => run_lowered(graph, program, params, timing),
    }
}

/// Lowers `program` to one Cypher statement and runs it, with the path-mode guard and ANY-selector
/// limit installed.
fn run_lowered(
    graph: &mut dyn GraphView,
    program: &Program,
    params: &HashMap<String, CypherValue>,
    timing: &mut Timing,
) -> Result<CypherResult, CypherError> {
    let t = web_time::Instant::now();
    let mut stmt = lower::lower(program).map_err(errors_to_cypher_error)?;
    // ISO section 14.5 path mode and section 14.6 path selector, read off the parsed program (the Cypher AST has
    // dropped them): a thread-local guard for the matcher's uniqueness rules, a top-level LIMIT
    // for ANY / ANY k
    let path_mode = lower::dominant_path_mode(program);
    if let Some(n) = lower::dominant_path_selector_limit(program) {
        lower::apply_path_selector_limit(&mut stmt, n);
    }
    let _path_mode_guard = traverse_cypher::session::set_path_mode(path_mode);
    timing.front += t.elapsed();
    let (result, _zero_parse, plan, exec) =
        traverse_cypher::execute_statement_with_params_timed(graph, stmt, params)?;
    timing.plan += plan;
    timing.exec += exec;
    Ok(result)
}

/// ISO section 15.4: the condition is evaluated once, with no bindings (literals, parameters,
/// `SESSION_USER` and functions of those; a variable is an error). True picks THEN; false or null
/// picks ELSE.
fn when_condition(
    graph: &mut dyn GraphView,
    condition: &Expression,
    params: &HashMap<String, CypherValue>,
    timing: &mut Timing,
) -> Result<bool, CypherError> {
    let t = web_time::Instant::now();
    let expr = lower::lower_expr(condition).map_err(|e| errors_to_cypher_error(vec![e]))?;
    timing.front += t.elapsed();
    // `RETURN <condition>` through the engine: validation, parameters and every function work as
    // in any other query
    let stmt = cy::Statement {
        mode: cy::QueryMode::Normal,
        query: cy::Query::Regular(cy::RegularQuery {
            single: cy::SingleQuery {
                parts: Vec::new(),
                final_part: cy::FinalPart {
                    clauses: Vec::new(),
                    return_clause: Some(cy::ReturnClause {
                        distinct: false,
                        items: cy::ProjectionItems::Explicit(vec![cy::ProjectionItem {
                            expr,
                            alias: Some("condition".into()),
                            source_text: None,
                        }]),
                        order_by: None,
                        skip: None,
                        limit: None,
                    }),
                },
            },
            unions: Vec::new(),
        }),
    };
    let (result, _zero_parse, plan, exec) =
        traverse_cypher::execute_statement_with_params_timed(graph, stmt, params)?;
    timing.plan += plan;
    timing.exec += exec;
    match result.rows.first().and_then(|row| row.first()) {
        Some(CypherValue::Bool(b)) => Ok(*b),
        Some(CypherValue::Null) | None => Ok(false),
        Some(other) => Err(CypherError::type_error(format!(
            "WHEN condition must be a BOOLEAN, got {}",
            other.type_name()
        ))),
    }
}

/// `START TRANSACTION [READ ONLY | READ WRITE] ... COMMIT | ROLLBACK`. The statements run in order on
/// the caller's transaction, so the block commits or fails as one; the result is the last
/// statement's, counting every statement's changes.
///
/// A READ ONLY block refuses statements that write, before any runs. A ROLLBACK block that writes
/// is refused the same way: changes made inside one call can't be discarded short of failing the
/// call, and an explicit transaction (Bolt, gRPC, the embedded API) is the way to roll back work.
/// A write the plan doesn't show (a writing procedure) fails the block when it happens, so the
/// caller aborts it.
fn run_transaction(
    graph: &mut dyn GraphView,
    read_only: bool,
    rollback: bool,
    statements: &[Statement],
    params: &HashMap<String, CypherValue>,
    timing: &mut Timing,
) -> Result<CypherResult, CypherError> {
    let refuse = |what: &str| {
        if read_only {
            CypherError::semantic(format!("a READ ONLY transaction can't run {what}"))
                .with_status(GqlStatus::ReadOnlyTransaction)
        } else {
            CypherError::semantic(format!(
                "a transaction block ending in ROLLBACK can't run {what}: its changes couldn't be \
                 discarded; use an explicit transaction to roll back"
            ))
            .with_status(GqlStatus::NotSupported)
        }
    };
    if read_only || rollback {
        for stmt in statements {
            if let Some(what) = writes(stmt)? {
                return Err(refuse(what));
            }
        }
    }
    let mut stats = QueryStats::default();
    let mut query_type = QueryType::Read;
    let mut last = empty_result();
    // a `USE` routed the request already
    for stmt in statements
        .iter()
        .filter(|s| !matches!(s, Statement::Use { .. }))
    {
        let result = run_program(graph, &single(stmt), params, timing)?;
        if (read_only || rollback) && !result.stats.is_empty() {
            return Err(refuse("a procedure that writes"));
        }
        stats += &result.stats;
        query_type = combine(query_type, result.query_type);
        last = result;
    }
    last.stats = stats;
    last.query_type = query_type;
    Ok(last)
}

/// What `stmt` changes, if its plan writes data or schema: `None` for reads.
fn writes(stmt: &Statement) -> Result<Option<&'static str>, CypherError> {
    Ok(match stmt {
        Statement::Query(_) => {
            let lowered = lower::lower(&single(stmt)).map_err(errors_to_cypher_error)?;
            let plan = traverse_cypher::planner::plan_statement_with_hints(
                &lowered,
                &traverse_cypher::planner::IndexHints::default(),
            )?;
            match traverse_cypher::executor::classify_plan(&plan) {
                QueryType::Read => None,
                QueryType::Schema => Some("schema changes"),
                QueryType::Write | QueryType::ReadWrite => Some("data changes"),
            }
        }
        Statement::CreateIndex { .. }
        | Statement::DropIndex { .. }
        | Statement::CreateConstraint { .. }
        | Statement::DropConstraint { .. } => Some("schema changes"),
        Statement::CreateGraph { .. }
        | Statement::DropGraph { .. }
        | Statement::CreateGraphType { .. }
        | Statement::DropGraphType { .. }
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. } => Some("catalog changes"),
        Statement::Use { .. } => None,
        Statement::When {
            then_stmt,
            else_stmt,
            ..
        } => match writes(then_stmt)? {
            Some(w) => Some(w),
            None => match else_stmt {
                Some(e) => writes(e)?,
                None => None,
            },
        },
        Statement::Transaction { statements, .. } => {
            let mut found = None;
            for s in statements {
                if let Some(w) = writes(s)? {
                    found = Some(w);
                    break;
                }
            }
            found
        }
    })
}

/// Query type of a block from its statements': any write makes it a write.
fn combine(a: QueryType, b: QueryType) -> QueryType {
    use QueryType::*;
    match (a, b) {
        (Read, x) | (x, Read) => x,
        (Schema, Schema) => Schema,
        (Write, Write) | (Write, Schema) | (Schema, Write) => Write,
        _ => ReadWrite,
    }
}

/// `stmt` alone as a program.
fn single(stmt: &Statement) -> Program {
    Program {
        statements: vec![stmt.clone()],
        separators: Vec::new(),
        span: stmt.span(),
    }
}

/// No columns, no rows, no changes.
fn empty_result() -> CypherResult {
    CypherResult {
        columns: Vec::new(),
        rows: Vec::new(),
        query_type: QueryType::Read,
        stats: QueryStats::default(),
        plan: None,
        bound: Vec::new(),
    }
}
