//! `traverse.ml.*`: ML pipeline procedures.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::types::CypherValue;

use super::ProcCall;

/// A `traverse.ml.*` procedure. These take positional arguments (GDS convention) and are stateful
/// across calls: they read and write a session-scoped pipeline catalog instead of computing a
/// result over the graph view. Rows bind like the algorithms' (`algorithm::call`).
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
        // Evaluate positional args in order.
        let arg_vec: Vec<CypherValue> = match args {
            Some(exprs) => exprs
                .iter()
                .map(|e| eval_expr(e, frame, graph, symtab))
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };

        let yield_names: Vec<&str> = yield_columns.iter().map(|(out, _)| out.as_str()).collect();
        let yield_hint: Option<&[&str]> = if yield_columns.is_empty() {
            None
        } else {
            Some(yield_names.as_slice())
        };
        let dispatch = crate::ml::run_for_call(procedure_name, &arg_vec, graph, yield_hint);
        let out = match dispatch {
            Some(Ok(out)) => out,
            Some(Err(e)) => return Err(e.into_cypher(procedure_name)),
            None => {
                return Err(CypherError::semantic(format!(
                    "ProcedureNotFound: ML procedure {procedure_name} is not registered"
                ))
                .with_status(crate::status::GqlStatus::UnknownProcedure));
            }
        };

        // Same row-binding pattern as the algorithm dispatcher.
        // Sequential here (not par_iter) - ML procedures emit
        // few rows (config snapshots, training summaries); the
        // parallel-rayon overhead wouldn't pay back.
        if yield_columns.is_empty() {
            for row in &out.rows {
                let mut new_frame = frame.clone();
                for (name, val) in row {
                    if let Some(slot) = symtab.slot(name.as_ref()) {
                        new_frame[slot] = val.clone().into();
                    }
                }
                result_frames.push(new_frame);
            }
        } else {
            let resolved: Vec<(String, Option<usize>)> = yield_columns
                .iter()
                .map(|(output_name, bound_var)| (output_name.clone(), symtab.slot(bound_var)))
                .collect();
            for row in &out.rows {
                let mut new_frame = frame.clone();
                for (output_name, slot) in &resolved {
                    let val = row
                        .iter()
                        .find(|(k, _)| k.as_ref() == output_name.as_str())
                        .map(|(_, v)| v.clone())
                        .unwrap_or(CypherValue::Null);
                    if let Some(s) = slot {
                        new_frame[*s] = val.into();
                    }
                }
                result_frames.push(new_frame);
            }
        }
        let _ = &out.columns;
    }

    Ok(result_frames)
}
