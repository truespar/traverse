//! `apoc.periodic.*`: batched iteration and commits, background jobs.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::procedures::{ProcCall, run_inner_cypher};
use crate::types::CypherValue;

/// `apoc.periodic.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        // apoc.periodic.iterate(outer, inner, config) :: (batches, total, ...)
        "apoc.periodic.iterate" => {
            let mut out = Vec::new();
            for frame in frames {
                let outer = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    o => {
                        return Err(CypherError::type_error(format!(
                            "outer statement must be a STRING, got {}",
                            o.type_name()
                        )));
                    }
                };
                let inner = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    o => {
                        return Err(CypherError::type_error(format!(
                            "inner statement must be a STRING, got {}",
                            o.type_name()
                        )));
                    }
                };
                let batch_size = match args.and_then(|a| a.get(2)) {
                    Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                        CypherValue::Map(m) => match m.get("batchSize") {
                            Some(CypherValue::Integer(n)) if *n > 0 => *n as usize,
                            _ => 10_000,
                        },
                        _ => 10_000,
                    },
                    None => 10_000,
                };
                let t0 = web_time::Instant::now();
                let driving = run_inner_cypher(graph, &outer, Default::default())?;
                let total = driving.rows.len();
                // embedded: the whole run is one transaction; the inner statement runs per
                // driving row with its columns as parameters
                for row in &driving.rows {
                    let mut params = std::collections::HashMap::new();
                    for (i, col) in driving.columns.iter().enumerate() {
                        params.insert(
                            col.clone(),
                            row.get(i).cloned().unwrap_or(CypherValue::Null),
                        );
                    }
                    run_inner_cypher(graph, &inner, params)?;
                }
                let batches = total.div_ceil(batch_size.max(1));
                let mut nf = frame.clone();
                for (col, val) in [
                    ("batches", CypherValue::Integer(batches as i64)),
                    ("total", CypherValue::Integer(total as i64)),
                    (
                        "timeTaken",
                        CypherValue::Integer(t0.elapsed().as_secs() as i64),
                    ),
                    ("committedOperations", CypherValue::Integer(total as i64)),
                    ("failedOperations", CypherValue::Integer(0)),
                    ("failedBatches", CypherValue::Integer(0)),
                    ("retries", CypherValue::Integer(0)),
                    (
                        "errorMessages",
                        CypherValue::Map(std::collections::BTreeMap::new()),
                    ),
                    ("wasTerminated", CypherValue::Bool(false)),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        // apoc.periodic.commit(statement, params) :: (updates, executions)
        "apoc.periodic.commit" => {
            let mut out = Vec::new();
            for frame in frames {
                let stmt = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    o => {
                        return Err(CypherError::type_error(format!(
                            "statement must be a STRING, got {}",
                            o.type_name()
                        )));
                    }
                };
                let params: std::collections::HashMap<String, CypherValue> =
                    match args.and_then(|a| a.get(1)) {
                        Some(e) => match eval_expr(e, frame, &*graph, symtab)? {
                            CypherValue::Map(m) => m.into_iter().collect(),
                            _ => Default::default(),
                        },
                        None => Default::default(),
                    };
                let mut updates = 0i64;
                let mut executions = 0i64;
                // loop until the statement reports zero updates (its first column, per APOC: `...
                // RETURN count(*)`); hard cap for statements that never converge
                for _ in 0..1_000_000 {
                    crate::query::check_interrupt()?;
                    let result = run_inner_cypher(graph, &stmt, params.clone())?;
                    let n = result
                        .rows
                        .first()
                        .and_then(|r| r.first())
                        .and_then(|v| match v {
                            CypherValue::Integer(n) => Some(*n),
                            _ => None,
                        })
                        .unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    updates += n;
                    executions += 1;
                }
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("updates") {
                    nf[sl] = CypherValue::Integer(updates).into();
                }
                if let Some(sl) = cx.slot_for("executions") {
                    nf[sl] = CypherValue::Integer(executions).into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.periodic.truncate" => {
            for t in graph.relationship_type_names() {
                for eid in graph.edges_by_type(&t) {
                    let _ = graph.delete_edge(eid);
                    crate::executor::inc_stat(|s| s.relationships_deleted += 1);
                }
            }
            for nid in graph.nodes_vec() {
                let _ = graph.delete_node_cascading(nid);
                crate::executor::inc_stat(|s| s.nodes_deleted += 1);
            }
            Ok(frames.to_vec())
        }
        // -- apoc.periodic scheduler --------------------------
        "apoc.periodic.submit" | "apoc.periodic.repeat" | "apoc.periodic.countdown" => {
            let mut out = Vec::new();
            for frame in frames {
                let jname = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let statement = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                // Validate now - a clear error at submit beats a
                // background-thread log line.
                crate::parser::parse(&statement)?;
                let (delay, rate) = match name_lower.as_str() {
                    "apoc.periodic.repeat" => {
                        let r = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                            CypherValue::Integer(i) => i.max(1),
                            _ => 1,
                        };
                        (0, r)
                    }
                    "apoc.periodic.countdown" => {
                        let d = match eval_expr(cx.arg(2)?, frame, &*graph, symtab)? {
                            CypherValue::Integer(i) => i.max(0),
                            _ => 0,
                        };
                        (d, 0)
                    }
                    _ => (0, 0),
                };
                let job = crate::apoc_scheduler::register(&jname, delay, rate);
                if !crate::apoc_scheduler::has_executor() {
                    // Embedded fallback: run once inline against THIS
                    // graph borrow (documented divergence: no
                    // asynchrony / repetition without a server).
                    let r = run_inner_cypher(graph, &statement, Default::default())
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    crate::apoc_scheduler::set_inline_result(r);
                }
                crate::apoc_scheduler::spawn(job.clone(), statement);
                let mut nf = frame.clone();
                for (col, val) in [
                    ("name", CypherValue::String(job.name.clone())),
                    ("delay", CypherValue::Integer(job.delay)),
                    ("rate", CypherValue::Integer(job.rate)),
                    (
                        "done",
                        CypherValue::Bool(job.done.load(std::sync::atomic::Ordering::SeqCst)),
                    ),
                    (
                        "cancelled",
                        CypherValue::Bool(job.cancelled.load(std::sync::atomic::Ordering::SeqCst)),
                    ),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        "apoc.periodic.list" | "apoc.periodic.cancel" => {
            let mut out = Vec::new();
            for frame in frames {
                let jobs = if name_lower.ends_with("cancel") {
                    let jname = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                        CypherValue::String(s) => s,
                        _ => continue,
                    };
                    crate::apoc_scheduler::cancel(&jname).into_iter().collect()
                } else {
                    crate::apoc_scheduler::list()
                };
                for job in jobs {
                    let mut nf = frame.clone();
                    for (col, val) in [
                        ("name", CypherValue::String(job.name.clone())),
                        ("delay", CypherValue::Integer(job.delay)),
                        ("rate", CypherValue::Integer(job.rate)),
                        (
                            "done",
                            CypherValue::Bool(job.done.load(std::sync::atomic::Ordering::SeqCst)),
                        ),
                        (
                            "cancelled",
                            CypherValue::Bool(
                                job.cancelled.load(std::sync::atomic::Ordering::SeqCst),
                            ),
                        ),
                    ] {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.into();
                        }
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        other => Err(super::unknown(other)),
    }
}
