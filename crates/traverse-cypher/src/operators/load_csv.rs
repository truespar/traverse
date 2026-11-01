//! LOAD CSV operator: reads CSV rows, binds each to a variable.

use std::cell::RefCell;
use std::collections::BTreeMap;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::{Frame, SymbolTable};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::Operator;

// -- LOAD CSV row context --------------------------------------------
//
// `file()` and `linenumber()` return the loader's current path and 1-based
// row number, NULL outside LOAD CSV. A plain thread-local breaks under
// buffering between LoadCsvOp.next() and the downstream operator that calls
// eval_expr: by eval time it holds the last row's value.
//
// So: a Vec stack indexed by the frame's hidden `__csv_ctx_idx` slot.
// LoadCsvOp pushes one entry per emitted row and writes its index into the
// slot; eval_expr looks it up. Survives any amount of buffering.

thread_local! {
    pub(crate) static LOAD_CSV_STACK: RefCell<Vec<LoadCsvContext>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug)]
pub struct LoadCsvContext {
    /// Resolved file path (no `file://` prefix).
    pub file: String,
    /// 1-based row number.
    pub line: i64,
}

/// Pushes a context entry, returns its index.
pub(crate) fn push_csv_context(ctx: LoadCsvContext) -> usize {
    LOAD_CSV_STACK.with(|s| {
        let mut v = s.borrow_mut();
        v.push(ctx);
        v.len() - 1
    })
}

/// Context by index; None if the stack was reset (new query) or the index is stale.
pub fn get_csv_context(idx: usize) -> Option<LoadCsvContext> {
    LOAD_CSV_STACK.with(|s| s.borrow().get(idx).cloned())
}

/// Clears the stack; called at the start of each query so contexts don't leak across queries.
pub fn reset_csv_stack() {
    LOAD_CSV_STACK.with(|s| s.borrow_mut().clear());
}

/// Hidden frame slot holding the index into `LOAD_CSV_STACK`; the underscore prefix keeps it out of
/// result projections.
pub const CSV_CTX_SLOT: &str = "__csv_ctx_idx";

pub(crate) struct LoadCsvOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<Vec<Frame>>,
    pos: usize,
}

impl<'p> LoadCsvOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for LoadCsvOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::LoadCsv {
                input,
                url,
                with_headers,
                row_alias,
                field_terminator,
            } = self.plan
            else {
                unreachable!();
            };

            let input_frames = super::exec_via_operators(input, graph, symtab)?;
            let alias_slot = symtab.slot(row_alias).unwrap();
            let ctx_slot = symtab.slot(CSV_CTX_SLOT);
            let mut result = Vec::new();

            for frame in &input_frames {
                let path_val = eval_expr(url, frame, graph, symtab)?;
                let path_str = match &path_val {
                    CypherValue::String(s) => s.as_str(),
                    _ => {
                        return Err(CypherError::type_error(format!(
                            "LOAD CSV FROM expects a string path, got {}",
                            path_val.type_name()
                        )));
                    }
                };

                // strip file:// prefix
                let file_path = path_str.strip_prefix("file://").unwrap_or(path_str);
                let ctx_file = file_path.to_string();

                let delimiter = field_terminator.unwrap_or(',');
                // remote CSVs: Neo4j's LOAD CSV reads http(s) URLs (tutorials commonly import from
                // raw.githubusercontent.com); local paths stream from disk, both feed the same
                // reader
                let source: Box<dyn std::io::Read> =
                    if file_path.starts_with("http://") || file_path.starts_with("https://") {
                        // no outbound HTTP on wasm32: local (OPFS-backed) paths only
                        #[cfg(target_arch = "wasm32")]
                        {
                            return Err(CypherError::runtime(format!(
                                "LOAD CSV: cannot fetch '{file_path}': remote URLs \
                             are not available in WASM builds"
                            )));
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let resp = ureq::config::Config::builder()
                                .timeout_global(Some(std::time::Duration::from_secs(60)))
                                .user_agent("traverse-load-csv")
                                .build()
                                .new_agent()
                                .get(file_path)
                                .call()
                                .map_err(|e| {
                                    CypherError::runtime(format!(
                                        "LOAD CSV: cannot fetch '{file_path}': {e}"
                                    ))
                                })?;
                            // stream the body into the reader, don't buffer the file
                            Box::new(resp.into_body().into_reader())
                        }
                    } else {
                        Box::new(std::fs::File::open(file_path).map_err(|e| {
                            CypherError::runtime(format!(
                                "LOAD CSV: cannot open '{}': {}",
                                file_path, e
                            ))
                        })?)
                    };
                let mut rdr = csv::ReaderBuilder::new()
                    .has_headers(*with_headers)
                    .delimiter(delimiter as u8)
                    // ragged rows are tolerated, as in Neo4j; short rows give nulls for the missing
                    // columns
                    .flexible(true)
                    .from_reader(source);

                let push_ctx_into = |new_frame: &mut Frame, line: i64| {
                    let idx = push_csv_context(LoadCsvContext {
                        file: ctx_file.clone(),
                        line,
                    });
                    if let Some(slot) = ctx_slot {
                        new_frame[slot] = CypherValue::Integer(idx as i64).into();
                    }
                };

                if *with_headers {
                    let headers: Vec<String> = rdr
                        .headers()
                        .map_err(|e| {
                            CypherError::runtime(format!("LOAD CSV: error reading headers: {}", e))
                        })?
                        .iter()
                        .map(|h| h.to_string())
                        .collect();

                    let mut line: i64 = 0;
                    for record in rdr.records() {
                        line += 1;
                        let record = record.map_err(|e| {
                            CypherError::runtime(format!("LOAD CSV: error reading row: {}", e))
                        })?;
                        let mut map = BTreeMap::new();
                        for (i, field) in record.iter().enumerate() {
                            let key = headers.get(i).cloned().unwrap_or_else(|| i.to_string());
                            map.insert(key, CypherValue::String(field.to_string()));
                        }
                        let mut new_frame = frame.clone();
                        new_frame[alias_slot] = CypherValue::Map(map).into();
                        push_ctx_into(&mut new_frame, line);
                        result.push(new_frame);
                    }
                } else {
                    let mut line: i64 = 0;
                    for record in rdr.records() {
                        line += 1;
                        let record = record.map_err(|e| {
                            CypherError::runtime(format!("LOAD CSV: error reading row: {}", e))
                        })?;
                        let items: Vec<CypherValue> = record
                            .iter()
                            .map(|f| CypherValue::String(f.to_string()))
                            .collect();
                        let mut new_frame = frame.clone();
                        new_frame[alias_slot] = CypherValue::List(items).into();
                        push_ctx_into(&mut new_frame, line);
                        result.push(new_frame);
                    }
                }
            }

            self.buffer = Some(result);
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
