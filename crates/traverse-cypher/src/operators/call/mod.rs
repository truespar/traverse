//! CALL: the procedure operator and its dispatch to each procedure family: APOC, graph
//! algorithms, ML pipelines, GDS compatibility, vector and full-text search, schema
//! introspection, and the data-driven registry for everything else.

mod algorithm;
mod apoc;
mod gds;
mod ml;
mod schema;
mod vector;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::{eval_expr, get_query_param, proc_values_equal};
use crate::frame::{Frame, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Operator;

pub(super) use vector::vector_from_list;

/// Does the procedure change the graph? Its access mode as Neo4j declares it (`WRITE` or
/// `SCHEMA`): a caller choosing between a snapshot and a write transaction asks this, and a plan
/// calling one is a writing plan (it runs to completion; `LIMIT` can't cut it short). Dynamic
/// Cypher follows Neo4j: `apoc.cypher.run`, `apoc.when` and `apoc.case` read, their `doIt`,
/// `runWrite`, `runMany` and `apoc.do.*` forms write.
pub(crate) fn procedure_writes(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    // `traverse.<algo>.write`, `gds.<algo>.write`, `gds.graph.nodeProperties.write`, ML
    // `....predict.write`: results written back as properties
    if name.ends_with(".write") {
        return true;
    }
    const NAMESPACES: &[&str] = &[
        "apoc.merge.",
        "apoc.refactor.",
        "apoc.import.",
        "apoc.generate.",
        "apoc.atomic.",
    ];
    const PROCEDURES: &[&str] = &[
        "apoc.create.node",
        "apoc.create.nodes",
        "apoc.create.relationship",
        "apoc.create.addlabels",
        "apoc.create.removelabels",
        "apoc.create.setlabels",
        "apoc.create.setproperty",
        "apoc.create.setproperties",
        "apoc.create.removeproperties",
        "apoc.create.setrelproperty",
        "apoc.create.setrelproperties",
        "apoc.create.removerelproperties",
        "apoc.nodes.delete",
        "apoc.nodes.link",
        "apoc.periodic.iterate",
        "apoc.periodic.commit",
        "apoc.periodic.truncate",
        "apoc.cypher.doit",
        "apoc.cypher.runwrite",
        "apoc.cypher.runmany",
        "apoc.cypher.runschema",
        "apoc.do.when",
        "apoc.do.case",
        "apoc.convert.setjsonproperty",
        "apoc.example.movies",
        "apoc.schema.assert",
        "apoc.lock.all",
        "apoc.lock.nodes",
        "apoc.lock.rels",
        "db.create.setnodevectorproperty",
        "db.create.setrelationshipvectorproperty",
        "db.create.setvectorproperty",
        "db.index.vector.createnodeindex",
    ];
    NAMESPACES.iter().any(|p| name.starts_with(p)) || PROCEDURES.contains(&name.as_str())
}

/// `CALL proc(...) YIELD ...`: runs the procedure once over all its input rows, then streams the
/// output.
pub(crate) struct CallProcedureOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> CallProcedureOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for CallProcedureOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::CallProcedure {
                input,
                procedure_name,
                args,
                yield_columns,
            } = self.plan
            else {
                unreachable!();
            };
            // the procedure runs once over all input rows, then its output streams
            let frames = super::exec_via_operators(input, graph, symtab)?;
            let cx = ProcCall {
                name: procedure_name,
                name_lower: procedure_name.to_ascii_lowercase(),
                args: args.as_ref(),
                yield_columns,
                frames: &frames,
                symtab,
            };
            self.buffer = Some(dispatch(&cx, graph)?);
        }
        let buf = self.buffer.as_mut().unwrap();
        if self.pos < buf.len() {
            let frame = std::mem::take(&mut buf[self.pos]);
            self.pos += 1;
            Ok(Some(frame))
        } else {
            Ok(None)
        }
    }
}

/// One procedure call: what was called, its arguments, how YIELD maps output columns to slots,
/// and the input rows it runs for.
pub(super) struct ProcCall<'a> {
    /// As written.
    pub(super) name: &'a str,
    /// Lowercased, for families matched case-insensitively.
    pub(super) name_lower: String,
    pub(super) args: Option<&'a Vec<crate::ast::Expr>>,
    pub(super) yield_columns: &'a [(String, String)],
    pub(super) frames: &'a [Frame],
    pub(super) symtab: &'a SymbolTable,
}

impl<'a> ProcCall<'a> {
    /// Slot of output column `col`: its YIELD alias if renamed, else the column itself.
    pub(super) fn slot_for(&self, col: &str) -> Option<usize> {
        let target = self
            .yield_columns
            .iter()
            .find(|(o, _)| o == col)
            .map(|(_, b)| b.as_str())
            .unwrap_or(col);
        self.symtab.slot(target)
    }

    /// Argument `i`, or an error naming the procedure.
    pub(super) fn arg(&self, i: usize) -> Result<&'a crate::ast::Expr, CypherError> {
        self.args.and_then(|a| a.get(i)).ok_or_else(|| {
            CypherError::semantic(format!("{} is missing argument {}", self.name, i + 1))
        })
    }

    /// Argument `i` evaluated on `frame`; `None` if it's absent or fails to evaluate.
    pub(super) fn opt_arg(
        &self,
        i: usize,
        frame: &Frame,
        graph: &dyn GraphView,
    ) -> Option<CypherValue> {
        self.args
            .and_then(|a| a.get(i))
            .and_then(|e| eval_expr(e, frame, graph, self.symtab).ok())
    }

    /// One output row per `(column, value)` row that `rows_per_frame` yields for an input frame.
    pub(super) fn emit_rows(
        &self,
        rows_per_frame: &dyn Fn(
            &Frame,
        )
            -> Result<Vec<Vec<(&'static str, CypherValue)>>, CypherError>,
    ) -> Result<Vec<Frame>, CypherError> {
        let mut out = Vec::new();
        for frame in self.frames {
            for row in rows_per_frame(frame)? {
                let mut nf = frame.clone();
                for (col, val) in row {
                    if let Some(sl) = self.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
        }
        Ok(out)
    }

    /// Every input frame with `row` bound.
    pub(super) fn emit_one(&self, row: Vec<(&'static str, CypherValue)>) -> Vec<Frame> {
        let mut out = Vec::new();
        for frame in self.frames {
            let mut nf = frame.clone();
            for (col, val) in &row {
                if let Some(sl) = self.slot_for(col) {
                    nf[sl] = val.clone().into();
                }
            }
            out.push(nf);
        }
        out
    }
}

/// Runs the call in its procedure family. Order matters where names overlap: `traverse.ml.*` before
/// the `traverse.<algo>.<mode>` algorithms, and the built-ins before the data-driven registry.
fn dispatch(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let name = cx.name;
    if name.starts_with("traverse.ml.") {
        // multi-step and stateful with GDS's column set, unlike the `.stream / .stats / .write`
        // algorithms
        return ml::call(cx, graph);
    }
    if name.starts_with("gds.") {
        return gds::call(cx, graph);
    }
    if name.starts_with("traverse.") {
        return algorithm::call(cx, graph);
    }
    match name {
        "db.index.vector.queryNodes" => vector::query_nodes(cx, graph),
        "db.index.fulltext.queryNodes" => vector::fulltext_query_nodes(cx, graph),
        "db.index.vector.createNodeIndex"
        | "db.create.setNodeVectorProperty"
        | "db.create.setRelationshipVectorProperty"
        | "db.create.setVectorProperty" => vector::write(cx, graph),
        _ if name.starts_with("apoc.") => apoc::call(cx, graph),
        _ => match crate::procedure::builtin_schema_proc_columns(name) {
            Some(cols) => schema::call(cx, cols, graph),
            None => registered(cx, graph),
        },
    }
}

/// A procedure from the data-driven registry (`crate::procedure`): its rows whose input columns
/// match the arguments.
fn registered(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        name: procedure_name,
        args,
        yield_columns,
        frames,
        symtab,
        ..
    } = *cx;
    let proc = crate::procedure::get_procedure(procedure_name).ok_or_else(|| {
        CypherError::semantic(format!(
            "ProcedureNotFound: procedure {procedure_name} not found"
        ))
        .with_status(crate::status::GqlStatus::UnknownProcedure)
    })?;
    let mut result = Vec::new();

    for frame in frames {
        // arguments: explicit, or implicit from query params
        let arg_values: Vec<CypherValue> = if let Some(arg_exprs) = args {
            arg_exprs
                .iter()
                .map(|e| eval_expr(e, frame, graph, symtab))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            // implicit: query params in parameter order
            proc.params
                .iter()
                .map(|p| get_query_param(&p.name).unwrap_or(CypherValue::Null))
                .collect()
        };

        // no outputs: pass the input record through
        if proc.outputs.is_empty() {
            result.push(frame.clone());
            continue;
        }

        // rows whose input columns match the arguments
        for proc_row in &proc.rows {
            let matches = proc
                .params
                .iter()
                .zip(arg_values.iter())
                .all(|(param, arg)| {
                    if let Some(row_val) = proc_row.get(&param.name) {
                        // FLOAT? params with integer args: promote for comparison
                        if param.typ == crate::procedure::ProcType::Float {
                            let promoted_arg = match arg {
                                CypherValue::Integer(i) => CypherValue::Float(*i as f64),
                                other => other.clone(),
                            };
                            let promoted_row = match row_val {
                                CypherValue::Integer(i) => CypherValue::Float(*i as f64),
                                other => other.clone(),
                            };
                            proc_values_equal(&promoted_arg, &promoted_row)
                        } else {
                            proc_values_equal(arg, row_val)
                        }
                    } else {
                        // param column missing from the row = null match
                        matches!(arg, CypherValue::Null)
                    }
                });

            if matches {
                let mut new_frame = frame.clone();
                if yield_columns.is_empty() {
                    // no explicit YIELD: all output columns
                    for out in &proc.outputs {
                        let val = proc_row
                            .get(&out.name)
                            .cloned()
                            .unwrap_or(CypherValue::Null);
                        if let Some(slot) = symtab.slot(&out.name) {
                            new_frame[slot] = val.into();
                        }
                    }
                } else {
                    for (output_name, bound_var) in yield_columns {
                        let val = proc_row
                            .get(output_name)
                            .cloned()
                            .unwrap_or(CypherValue::Null);
                        if let Some(slot) = symtab.slot(bound_var) {
                            new_frame[slot] = val.into();
                        }
                    }
                }
                result.push(new_frame);
            }
        }
    }
    Ok(result)
}

/// Runs a dynamic Cypher statement on the SAME graph view inside the current
/// query (apoc.cypher.run / apoc.periodic.iterate). Query parameters are saved
/// and restored around it; statistics add to the outer query's counters
/// (execute_plan doesn't reset them).
fn run_inner_cypher(
    graph: &mut dyn GraphView,
    query: &str,
    params: std::collections::HashMap<String, CypherValue>,
) -> Result<crate::CypherResult, CypherError> {
    crate::stack::with_stack_for_query(query, || run_inner_cypher_inner(graph, query, params))
}

/// Body of the entry point above; see `stack::with_stack_for_query`.
fn run_inner_cypher_inner(
    graph: &mut dyn GraphView,
    query: &str,
    params: std::collections::HashMap<String, CypherValue>,
) -> Result<crate::CypherResult, CypherError> {
    // part of the calling statement: its limits, budget and clock, these parameters
    let _query = crate::query::enter_nested(params);
    let (stmt, parse_depth) = crate::parser::parse_with_stats(query)?;
    let graph = &mut *graph;
    crate::stack::with_stack_for_ast(&parse_depth, move || {
        crate::validate::validate(&stmt)?;
        let hints = crate::build_index_hints_for(&*graph);
        let plan = crate::planner::plan_statement_with_hints(&stmt, &hints)?;
        crate::stack::run_deep_plan(plan, |plan| {
            let plan = crate::optimizer::optimize(plan, &*graph, &hints);
            crate::executor::execute_plan(&plan, graph)
        })
    })
}

/// [`run_inner_cypher`] under a time limit of its own (`apoc.cypher.runTimeboxed`): at the limit
/// the statement stops and the rows it produced so far are its result, as in APOC. The enclosing
/// query stopping still fails it.
fn run_inner_cypher_timeboxed(
    graph: &mut dyn GraphView,
    query: &str,
    params: std::collections::HashMap<String, CypherValue>,
    timeout: std::time::Duration,
) -> Result<crate::CypherResult, CypherError> {
    // the enclosing statement's budget and clock, a limit and parameters of its own
    let options = |timeout| crate::query::ChildOptions {
        timeout,
        params: Some(std::sync::Arc::new(params)),
        shared_clock: true,
        ..Default::default()
    };
    let ctx = match crate::query::current() {
        Some(outer) => outer.child(options(Some(timeout))),
        None => crate::query::QueryContext::new(crate::query::QueryOptions {
            timeout: Some(timeout),
            ..Default::default()
        })
        .child(options(None)),
    };
    let _scope = ctx.enter();
    crate::stack::with_stack_for_query(query, || {
        let (stmt, parse_depth) = crate::parser::parse_with_stats(query)?;
        let graph = &mut *graph;
        crate::stack::with_stack_for_ast(&parse_depth, move || {
            crate::validate::validate(&stmt)?;
            let hints = crate::build_index_hints_for(&*graph);
            let plan = crate::planner::plan_statement_with_hints(&stmt, &hints)?;
            crate::stack::run_deep_plan(plan, |plan| {
                let plan = crate::optimizer::optimize(plan, &*graph, &hints);
                collect_until_own_deadline(&plan, graph, &ctx)
            })
        })
    })
}

/// Rows of `plan` until it ends or `ctx`'s own deadline passes (its parent stopping is an error).
/// A statement that writes can't stop half way inside the enclosing one, so for it the own
/// deadline is an error too.
fn collect_until_own_deadline(
    plan: &LogicalPlan,
    graph: &mut dyn GraphView,
    ctx: &crate::query::QueryContext,
) -> Result<crate::CypherResult, CypherError> {
    let writes = crate::executor::plan_is_mutating(plan);
    let own_deadline = |e: &CypherError| {
        !writes
            && matches!(
                e,
                CypherError::Interrupted {
                    reason: crate::query::Interrupt::TimedOut,
                    ..
                }
            )
            && ctx.parent().is_none_or(|p| p.interrupt().is_none())
    };
    let stopped = |rows| crate::CypherResult {
        columns: crate::executor::plan_columns(plan),
        rows,
        query_type: crate::executor::classify_plan(plan),
        stats: crate::executor::get_query_stats(),
        plan: None,
        bound: Vec::new(),
    };
    match crate::executor::execute_plan_streaming(plan, graph) {
        Ok(crate::executor::QueryExecution::Materialized(result)) => Ok(result),
        Ok(crate::executor::QueryExecution::Streaming(mut stream)) => {
            let columns = stream.columns().to_vec();
            let mut rows = Vec::new();
            loop {
                match stream.next_row() {
                    Ok(Some(row)) => rows.push(row),
                    Ok(None) => break,
                    Err(e) if own_deadline(&e) => break,
                    Err(e) => return Err(e),
                }
            }
            let footer = stream.finish();
            Ok(crate::CypherResult {
                columns,
                rows,
                query_type: footer.query_type,
                stats: footer.stats,
                plan: None,
                bound: Vec::new(),
            })
        }
        // a materializing plan (writes, morsel-parallel) stopped before producing rows
        Err(e) if own_deadline(&e) => Ok(stopped(Vec::new())),
        Err(e) => Err(e),
    }
}
