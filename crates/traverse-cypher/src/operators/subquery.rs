//! ISO 39075 section 9.4 inline procedure subquery (`CALL [(imports)] { body }`).
//!
//! Per input row, `body` runs with that row as initial bindings (narrowed to
//! `imports` when given, else transparent), and every inner row cross-joins onto
//! the outer record.
//!
//! The body runs in the `exec_subquery` record interpreter, which models a
//! **read-only** subset (MATCH / OPTIONAL MATCH / UNWIND / WITH / RETURN /
//! aggregation). Writes in the body (`CREATE` / `SET` / `MERGE` / `DELETE`) hit
//! its catch-all and raise a clear "not supported" error instead of silently
//! doing nothing; write bodies go through `CALL { ... } IN TRANSACTIONS`.

use traverse_core::tx::view::GraphView;

use crate::ast::OnErrorBehavior;
use crate::error::CypherError;
use crate::executor::{eval_expr, exec_subquery};
use crate::frame::{Frame, SymbolTable, frame_to_record, new_frame, record_to_frame};
use crate::memory::RowBuffer;
use crate::optimizer::rewrite::recurse_transform;
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Operator;

pub(crate) struct CallSubqueryOp<'p> {
    plan: &'p LogicalPlan,
    /// After the input materialised and the body ran per outer row, all output frames sit here so
    /// `next()` drains them in order.
    buffer: Option<RowBuffer>,
    pos: usize,
}

impl<'p> CallSubqueryOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for CallSubqueryOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::CallSubquery {
                input,
                imports,
                body,
                return_aliases: _,
            } = self.plan
            else {
                unreachable!();
            };

            // materialise outer frames once
            let outer_frames = super::exec_via_operators(input, graph, symtab)?;
            let mut output = RowBuffer::with_capacity(outer_frames.len());

            for outer in &outer_frames {
                // seed record for the body: only the `imports` when given, else every outer binding
                // (transparent scope); other outer bindings are reattached to the output rows
                // below, giving ISO section 9.4's cross-join
                let outer_record = frame_to_record(outer, symtab);
                let seed = match imports {
                    Some(names) => {
                        let mut r = std::collections::HashMap::with_capacity(names.len());
                        for n in names {
                            if let Some(v) = outer_record.get(n) {
                                r.insert(n.clone(), v.clone());
                            }
                        }
                        r
                    }
                    None => outer_record.clone(),
                };

                // run the body per row, seeded with `seed`
                let inner_records = exec_subquery(body, &seed, graph)?;

                if inner_records.is_empty() {
                    // an empty result contributes nothing ({outer} x {} = empty). Write-only bodies
                    // (FINISH / no RETURN) return one empty record from exec_subquery, so the outer
                    // row survives; this path only fires when the body truly produced no rows
                    continue;
                }

                for inner in inner_records {
                    // cross-join outer + inner bindings; inner names win on clashes (ISO section 9.4:
                    // RETURN scopes the names it introduces)
                    let mut merged = outer_record.clone();
                    for (k, v) in inner.into_iter() {
                        merged.insert(k, v);
                    }
                    output.push(record_to_frame(&merged, symtab))?;
                }
            }

            self.buffer = Some(output);
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

// -- CallInTransactionsOp -------------------------------------------------
//
// `CALL { body } IN TRANSACTIONS [OF n ROWS] [ON ERROR ...] [REPORT STATUS AS v]`:
// the body runs per input row through the **write-capable** operator pipeline
// (EmptyRow leaf swapped for a SeededRows leaf with that row), in batches of
// `n`: the write-subquery path the read-only exec_subquery can't offer.
//
// All writes hit one in-memory graph, so a batch can't roll back alone: ON
// ERROR CONTINUE/BREAK skip later rows, but writes a failing row already made
// stay. Most import errors (constraints, type errors) are caught before the
// write lands, so the failing row is skipped cleanly.

pub(crate) struct CallInTransactionsOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<RowBuffer>,
    pos: usize,
}

impl<'p> CallInTransactionsOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for CallInTransactionsOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::CallInTransactions {
                input,
                body,
                batch_size,
                on_error,
                report_status,
                return_aliases,
            } = self.plan
            else {
                unreachable!();
            };

            let outer_frames = super::exec_via_operators(input, graph, symtab)?;

            // each batch is a transaction of its own: committed when it succeeds, rolled back
            // when it fails. That needs an auto-commit statement (Neo4j: an implicit transaction);
            // inside a client's transaction the batches would commit the client's work
            if graph.is_explicit_transaction() {
                return Err(CypherError::semantic(
                    "CALL { ... } IN TRANSACTIONS runs only as an auto-commit statement, not \
                     inside an explicit transaction",
                )
                .with_status(crate::status::GqlStatus::ImplicitTransactionRequired));
            }
            let committing = graph.can_commit_batches();
            if committing && graph.write_count() > 0 {
                return Err(CypherError::semantic(
                    "CALL { ... } IN TRANSACTIONS can't follow writes in the same statement: its \
                     batches would commit them",
                ));
            }
            let outer_transaction = crate::query::with_current(|c| {
                c.and_then(|ctx| ctx.transaction())
                    .and_then(crate::transactions::display_id_of)
            });

            // batch size (constant expression; default 1000)
            let batch = match batch_size {
                Some(e) => {
                    let empty = new_frame(symtab);
                    match eval_expr(e, &empty, &*graph, symtab)? {
                        CypherValue::Integer(n) if n > 0 => n as usize,
                        _ => return Err(CypherError::semantic(
                            "CALL { ... } IN TRANSACTIONS OF requires a positive integer batch size"
                                .to_string(),
                        )),
                    }
                }
                None => 1000,
            };

            let has_return = !return_aliases.is_empty();
            let mut output = RowBuffer::new();

            'batches: for (number, chunk) in outer_frames.chunks(batch).enumerate() {
                crate::query::check()?;
                let mut batch_error: Option<String> = None;
                let mut batch_rows = RowBuffer::new();

                for f in chunk {
                    let seeded = seed_body(body, vec![f.clone()]);
                    match super::exec_via_operators(&seeded, graph, symtab) {
                        Ok(out) => {
                            if has_return {
                                batch_rows.extend(out)?;
                            }
                        }
                        // a stopped statement stops, whatever ON ERROR says
                        Err(e @ CypherError::Interrupted { .. }) => return Err(e),
                        Err(e) => match on_error {
                            // FAIL (default) aborts the statement; batches already committed stay
                            OnErrorBehavior::Fail => return Err(e),
                            // CONTINUE/BREAK record the error and end this batch
                            OnErrorBehavior::Continue | OnErrorBehavior::Break => {
                                batch_error = Some(e.to_string());
                                break;
                            }
                        },
                    }
                }

                // a failed batch leaves nothing behind: its writes roll back, its rows go
                if batch_error.is_some() {
                    if committing {
                        graph.rollback_batch()?;
                    }
                } else {
                    if committing {
                        graph.commit_batch()?;
                    }
                    output.extend(batch_rows)?;
                }
                if let Some(rs) = report_status {
                    let id = match &outer_transaction {
                        Some(outer) => format!("{outer}/batch-{}", number + 1),
                        None => format!("batch-{}", number + 1),
                    };
                    output.push(status_frame(rs, &batch_error, &id, symtab))?;
                }
                if batch_error.is_some() && *on_error == OnErrorBehavior::Break {
                    break 'batches;
                }
            }

            // write-only unit subquery (no RETURN, no status): outer rows pass through, cardinality
            // kept (Neo4j)
            if !has_return && report_status.is_none() {
                output = outer_frames;
            }

            self.buffer = Some(output);
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

/// Clones the body with its EmptyRow leaf swapped for SeededRows of the per-row seeds, giving
/// writes seeded input.
fn seed_body(body: &LogicalPlan, seeds: Vec<Frame>) -> LogicalPlan {
    recurse_transform(body.clone(), &|node| match node {
        LogicalPlan::EmptyRow => LogicalPlan::SeededRows(seeds.clone()),
        other => other,
    })
}

/// `REPORT STATUS AS var` row: a map of the batch outcome (Neo4j's keys).
fn status_frame(
    var: &str,
    batch_error: &Option<String>,
    transaction_id: &str,
    symtab: &SymbolTable,
) -> Frame {
    let mut status = std::collections::BTreeMap::new();
    status.insert("started".to_string(), CypherValue::Bool(true));
    status.insert(
        "committed".to_string(),
        CypherValue::Bool(batch_error.is_none()),
    );
    status.insert(
        "errorMessage".to_string(),
        match batch_error {
            Some(m) => CypherValue::String(m.clone()),
            None => CypherValue::Null,
        },
    );
    status.insert(
        "transactionId".to_string(),
        CypherValue::String(transaction_id.to_string()),
    );
    let mut frame = new_frame(symtab);
    if let Some(slot) = symtab.slot(var) {
        frame[slot] = CypherValue::Map(status).into();
    }
    frame
}
