//! `traverse.<algo>.<mode>`: graph algorithms.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::types::CypherValue;

use super::ProcCall;

/// `traverse.<algo>.<mode>` through the algorithm registry: rows materialize once per call.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        name: procedure_name,
        args,
        yield_columns,
        frames: input_frames,
        symtab,
        ..
    } = *cx;
    let mut result_frames: Vec<Frame> = Vec::new();

    for frame in input_frames {
        // Algorithm procedures take a single optional MAP argument
        // (Neo4j GDS convention: `CALL traverse.pageRank.stream({...})`).
        let arg_map: std::collections::BTreeMap<String, CypherValue> = match args {
            Some(exprs) if !exprs.is_empty() => {
                let val = eval_expr(&exprs[0], frame, graph, symtab)?;
                match val {
                    CypherValue::Map(m) => m.into_iter().collect(),
                    CypherValue::Null => std::collections::BTreeMap::new(),
                    other => {
                        return Err(CypherError::type_error(format!(
                            "algorithm `{procedure_name}` expects a map argument, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            _ => std::collections::BTreeMap::new(),
        };

        // Pass YIELD column hints to the algorithm so it can skip
        // building per-row columns that won't be read. Big win for
        // heavy stream outputs (paths, embeddings).
        let yield_names: Vec<&str> = yield_columns.iter().map(|(out, _)| out.as_str()).collect();
        let yield_hint: Option<&[&str]> = if yield_columns.is_empty() {
            None
        } else {
            Some(yield_names.as_slice())
        };
        let dispatch =
            crate::algorithms::procedure::run_for_call(procedure_name, &arg_map, graph, yield_hint);
        let out = match dispatch {
            Some(Ok(out)) => out,
            Some(Err(e)) => {
                return Err(e.into_cypher(&format!("algorithm `{procedure_name}` failed")));
            }
            None => {
                return Err(CypherError::semantic(format!(
                    "ProcedureNotFound: algorithm {procedure_name} is not registered"
                ))
                .with_status(crate::status::GqlStatus::UnknownProcedure));
            }
        };

        // Parallel row binding for high-row-count algorithm outputs
        // (pageRank.stream, louvain.stream, fastRP.stream, ...). Each
        // row's frame is independent; we resolve symtab slots once
        // up front so workers don't redundantly hash strings.
        use rayon::prelude::*;
        if yield_columns.is_empty() {
            // No explicit YIELD: bind every (name, val) pair from
            // the row into its matching slot (if any).
            let new_rows: Vec<Frame> = out
                .rows
                .par_iter()
                .map(|row| {
                    let mut new_frame = frame.clone();
                    for (name, val) in row {
                        if let Some(slot) = symtab.slot(name.as_ref()) {
                            new_frame[slot] = val.clone().into();
                        }
                    }
                    new_frame
                })
                .collect();
            result_frames.extend(new_rows);
        } else {
            // Explicit YIELD: pre-resolve slot positions per yield-
            // column. For each row we only look up the names we want.
            let resolved: Vec<(String, Option<usize>)> = yield_columns
                .iter()
                .map(|(output_name, bound_var)| (output_name.clone(), symtab.slot(bound_var)))
                .collect();
            let resolved_ref: &[(String, Option<usize>)] = &resolved;
            let new_rows: Vec<Frame> = out
                .rows
                .par_iter()
                .map(|row| {
                    let mut new_frame = frame.clone();
                    for (output_name, slot) in resolved_ref {
                        let val = row
                            .iter()
                            .find(|(k, _)| k.as_ref() == output_name.as_str())
                            .map(|(_, v)| v.clone())
                            .unwrap_or(CypherValue::Null);
                        if let Some(s) = slot {
                            new_frame[*s] = val.into();
                        }
                    }
                    new_frame
                })
                .collect();
            result_frames.extend(new_rows);
        }
        let _ = &out.columns;
    }

    Ok(result_frames)
}
