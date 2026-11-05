//! `LOAD CSV`: rows of a file as list or map values, and the per-row context `file()` and
//! `linenumber()` read.
//!
//! A plain thread-local breaks under buffering between the loader and the operator that
//! evaluates `file()`: by then it holds the last row's value. So rows carry an index into a
//! per-thread stack of contexts in a hidden column; the functions look the entry up.

use std::cell::RefCell;

thread_local! {
    static LOAD_CSV_STACK: RefCell<Vec<LoadCsvContext>> = const { RefCell::new(Vec::new()) };
}

/// The loader's position when it emitted a row.
#[derive(Clone, Debug)]
pub struct LoadCsvContext {
    /// Resolved file path (no `file://` prefix).
    pub file: String,
    /// 1-based row number.
    pub line: i64,
}

/// Pushes a context entry, returns its index.
pub fn push_csv_context(ctx: LoadCsvContext) -> usize {
    LOAD_CSV_STACK.with(|s| {
        let mut v = s.borrow_mut();
        v.push(ctx);
        v.len() - 1
    })
}

/// Context by index; `None` if the stack was reset (new query) or the index is stale.
pub fn get_csv_context(idx: usize) -> Option<LoadCsvContext> {
    LOAD_CSV_STACK.with(|s| s.borrow().get(idx).cloned())
}

/// Clears the stack at the start of each query, so contexts don't leak across queries.
pub fn reset_csv_stack() {
    LOAD_CSV_STACK.with(|s| s.borrow_mut().clear());
}

/// Hidden variable holding the index into the context stack; the underscore prefix keeps it out
/// of result projections.
pub const CSV_CTX_SLOT: &str = "__csv_ctx_idx";

use std::collections::BTreeMap;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::{CompiledExpr, EvalScratch};
use super::super::plan::SeedRow;
use super::super::vector::{VECTOR_CAPACITY, Vector, VectorBuilder};
use super::{PhysOperator, install};

/// An open file being read: its records, the header row when there is one, and the position.
struct Reader {
    records: csv::StringRecordsIntoIter<Box<dyn std::io::Read>>,
    headers: Option<Vec<String>>,
    file: String,
    line: i64,
}

/// `LOAD CSV`: the rows of the file named by each (flat) input row, streamed in chunks.
pub struct LoadCsvOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    url: &'p CompiledExpr,
    with_headers: bool,
    row: DataPos,
    ctx: DataPos,
    out_group: GroupId,
    field_terminator: Option<char>,
    scratch: EvalScratch<'p>,
    reader: Option<Reader>,
}

impl<'p> LoadCsvOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        url: &'p CompiledExpr,
        with_headers: bool,
        row: DataPos,
        ctx: DataPos,
        out_group: GroupId,
        field_terminator: Option<char>,
    ) -> Self {
        Self {
            input,
            url,
            with_headers,
            row,
            ctx,
            out_group,
            field_terminator,
            scratch: EvalScratch::new(),
            reader: None,
        }
    }

    /// Opens the file (or URL) named by the current input row.
    fn open(&mut self, rs: &ResultSet, cx: &ExecCtx<'_>) -> Result<Reader, CypherError> {
        let path_val = self.url.eval_scalar(rs, cx.view(), &mut self.scratch)?;
        let path_str = match &path_val {
            CypherValue::String(s) => s.as_str(),
            _ => {
                return Err(CypherError::type_error(format!(
                    "LOAD CSV FROM expects a string path, got {}",
                    path_val.type_name()
                )));
            }
        };
        let file_path = path_str.strip_prefix("file://").unwrap_or(path_str);
        let delimiter = self.field_terminator.unwrap_or(',');
        // remote CSVs read over http(s) as Neo4j's LOAD CSV does; local paths stream from disk
        let source: Box<dyn std::io::Read> = if file_path.starts_with("http://")
            || file_path.starts_with("https://")
        {
            #[cfg(target_arch = "wasm32")]
            {
                return Err(CypherError::runtime(format!(
                    "LOAD CSV: cannot fetch '{file_path}': remote URLs are not available in WASM \
                     builds"
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
                        CypherError::runtime(format!("LOAD CSV: cannot fetch '{file_path}': {e}"))
                    })?;
                Box::new(resp.into_body().into_reader())
            }
        } else {
            Box::new(std::fs::File::open(file_path).map_err(|e| {
                CypherError::runtime(format!("LOAD CSV: cannot open '{file_path}': {e}"))
            })?)
        };
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(self.with_headers)
            .delimiter(delimiter as u8)
            // ragged rows are tolerated, as in Neo4j; short rows give nulls for missing columns
            .flexible(true)
            .from_reader(source);
        let headers = if self.with_headers {
            Some(
                rdr.headers()
                    .map_err(|e| {
                        CypherError::runtime(format!("LOAD CSV: error reading headers: {e}"))
                    })?
                    .iter()
                    .map(|h| h.to_string())
                    .collect(),
            )
        } else {
            None
        };
        Ok(Reader {
            records: rdr.into_records(),
            headers,
            file: file_path.to_string(),
            line: 0,
        })
    }
}

impl PhysOperator for LoadCsvOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(r) = &mut self.reader {
                let mut rows = VectorBuilder::new();
                let mut ctxs: Vec<i64> = Vec::new();
                while ctxs.len() < VECTOR_CAPACITY {
                    let Some(record) = r.records.next() else {
                        break;
                    };
                    let record = record.map_err(|e| {
                        CypherError::runtime(format!("LOAD CSV: error reading row: {e}"))
                    })?;
                    r.line += 1;
                    let value = match &r.headers {
                        Some(headers) => {
                            let mut map = BTreeMap::new();
                            for (i, field) in record.iter().enumerate() {
                                let key = headers.get(i).cloned().unwrap_or_else(|| i.to_string());
                                map.insert(key, CypherValue::String(field.to_string()));
                            }
                            CypherValue::Map(map)
                        }
                        None => CypherValue::List(
                            record
                                .iter()
                                .map(|f| CypherValue::String(f.to_string()))
                                .collect(),
                        ),
                    };
                    rows.push_any(value);
                    let idx = push_csv_context(LoadCsvContext {
                        file: r.file.clone(),
                        line: r.line,
                    });
                    ctxs.push(idx as i64);
                }
                if ctxs.is_empty() {
                    self.reader = None;
                    continue;
                }
                let n = ctxs.len();
                install(
                    rs,
                    self.out_group,
                    n,
                    vec![
                        (self.row.col, rows.finish()),
                        (self.ctx.col, Vector::ints(ctxs)),
                    ],
                );
                cx.tick()?;
                return Ok(true);
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            let r = self.open(rs, cx)?;
            self.reader = Some(r);
        }
    }

    fn reset(&mut self) {
        self.reader = None;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
