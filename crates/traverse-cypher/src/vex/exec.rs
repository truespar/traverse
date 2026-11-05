//! Running a compiled plan: the sink that turns the root's chunks into result rows, kept as
//! typed column chunks until a caller reads them (materializing references then), and a pull
//! stream for callers that take rows one at a time.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::memory::Charge;
use crate::types::CypherValue;

use super::chunk::{DataPos, GroupId, ResultSet};
use super::ctx::ExecCtx;
use super::expr::eval::materialize_entity;
use super::ops::{PhysOperator, build_operator};
use super::plan::PhysicalPlan;
use super::values::materialize_value;
use super::vector::{ValueRef, Vector};

/// Result rows of one chunk, one vector per result column.
#[derive(Debug)]
pub struct RowChunk {
    /// Columns in result order, each `len` lanes.
    pub cols: Vec<Vector>,
    /// Rows.
    pub len: usize,
}

impl RowChunk {
    /// The lane at `(row, col)`.
    #[inline]
    pub fn get(&self, row: usize, col: usize) -> ValueRef<'_> {
        self.cols[col].get(row)
    }

    /// Heap bytes the chunk holds.
    pub fn bytes(&self) -> usize {
        self.cols.iter().map(Vector::heap_bytes).sum::<usize>() + 32
    }
}

/// A result value with its references resolved against `graph`.
pub fn row_value(v: ValueRef<'_>, graph: &dyn GraphView) -> CypherValue {
    match v {
        ValueRef::Node(_) | ValueRef::Edge { .. } => materialize_entity(v, graph),
        ValueRef::Any(cv) => materialize_value(cv.clone(), graph),
        other => other.to_owned(),
    }
}

/// The result rows of the chunk in `rs`: every column over the product of the output groups.
pub fn chunk_rows(
    rs: &ResultSet,
    columns: &[(String, DataPos)],
    out_groups: &[GroupId],
) -> RowChunk {
    // the groups that vary across rows, in product order (the first varies slowest)
    let varying: Vec<(GroupId, usize)> = out_groups
        .iter()
        .copied()
        .filter(|&g| {
            let gr = rs.group(g);
            gr.active && gr.flat.is_none()
        })
        .map(|g| (g, rs.group(g).selected_len()))
        .collect();
    let rows: usize = varying
        .iter()
        .fold(1usize, |a, (_, n)| a.saturating_mul(*n));
    if rows == 0 {
        return RowChunk {
            cols: columns.iter().map(|_| Vector::empty()).collect(),
            len: 0,
        };
    }
    // lane index per row for each varying group, built on first use
    let mut indices: Vec<Option<Vec<u32>>> = vec![None; varying.len()];
    let lane_indices = |k: usize| -> Vec<u32> {
        let (g, n) = varying[k];
        let stride: usize = varying[k + 1..]
            .iter()
            .fold(1usize, |a, (_, m)| a.saturating_mul(*m));
        let group = rs.group(g);
        (0..rows)
            .map(|r| group.lane((r / stride) % n) as u32)
            .collect()
    };
    let cols: Vec<Vector> = columns
        .iter()
        .map(|(_, pos)| {
            if pos.group == u16::MAX || !rs.group(pos.group).active {
                return Vector::nulls(rows);
            }
            let g = rs.group(pos.group);
            let col = &g.cols[pos.col as usize];
            if let Some(lane) = g.current_lane() {
                return col.repeat(lane, rows);
            }
            let k = varying.iter().position(|(vg, _)| *vg == pos.group).unwrap();
            if varying.len() == 1 && g.sel.is_none() && col.len() == rows {
                return col.clone();
            }
            let idx = indices[k].get_or_insert_with(|| lane_indices(k));
            col.take(idx)
        })
        .collect();
    RowChunk { cols, len: rows }
}

/// Runs `plan` to completion and collects its result chunks, charged to the query's memory
/// budget for as long as the returned charge lives. A plan without result columns runs for its
/// effects and returns no chunks.
pub fn run_chunks(
    plan: &PhysicalPlan,
    graph: &mut dyn GraphView,
) -> Result<(Vec<RowChunk>, Charge), CypherError> {
    if let Some(out) = super::parallel::run_plan(plan, graph)? {
        return Ok(out);
    }
    let mut op = build_operator(&plan.root)?;
    let mut rs = ResultSet::new(&plan.widths);
    let mut cx = ExecCtx::new(graph);
    let mut out = Vec::new();
    let mut charge = Charge::new();
    let sink = !plan.columns.is_empty();
    while op.next(&mut rs, &mut cx)? {
        if !sink {
            continue;
        }
        let chunk = chunk_rows(&rs, &plan.columns, &plan.out_groups);
        if chunk.len == 0 {
            continue;
        }
        charge.add(chunk.bytes())?;
        out.push(chunk);
    }
    charge.flush()?;
    Ok((out, charge))
}

/// Rows of `chunks` as owned values, references resolved.
pub fn materialize_rows(
    chunks: Vec<RowChunk>,
    graph: &dyn GraphView,
) -> Result<Vec<Vec<CypherValue>>, CypherError> {
    let total: usize = chunks.iter().map(|c| c.len).sum();
    let mut rows = Vec::with_capacity(total);
    let mut n = 0u64;
    for chunk in chunks {
        for r in 0..chunk.len {
            rows.push(
                (0..chunk.cols.len())
                    .map(|c| row_value(chunk.get(r, c), graph))
                    .collect(),
            );
            n += 1;
            if n & 0xFFF == 0 {
                crate::query::check()?;
            }
        }
    }
    Ok(rows)
}

/// A pull stream over a plan: rows one at a time, the operators driven on demand.
pub struct Stream<'a> {
    op: Box<dyn PhysOperator + 'a>,
    rs: ResultSet,
    cx: ExecCtx<'a>,
    plan: &'a PhysicalPlan,
    pending: Option<RowChunk>,
    row: usize,
    done: bool,
}

impl<'a> Stream<'a> {
    /// Opens a stream over `plan`.
    pub fn new(plan: &'a PhysicalPlan, graph: &'a mut dyn GraphView) -> Result<Self, CypherError> {
        Ok(Self {
            op: build_operator(&plan.root)?,
            rs: ResultSet::new(&plan.widths),
            cx: ExecCtx::new(graph),
            plan,
            pending: None,
            row: 0,
            done: false,
        })
    }

    /// The graph the stream reads.
    pub fn graph(&self) -> &dyn GraphView {
        self.cx.view()
    }

    /// The next row with references resolved; `None` at the end.
    pub fn next_row(&mut self) -> Result<Option<Vec<CypherValue>>, CypherError> {
        loop {
            if let Some(chunk) = &self.pending {
                if self.row < chunk.len {
                    let r = self.row;
                    self.row += 1;
                    let graph = self.cx.view();
                    return Ok(Some(
                        (0..chunk.cols.len())
                            .map(|c| row_value(chunk.get(r, c), graph))
                            .collect(),
                    ));
                }
                self.pending = None;
            }
            if self.done {
                return Ok(None);
            }
            if !self.op.next(&mut self.rs, &mut self.cx)? {
                self.done = true;
                return Ok(None);
            }
            let chunk = chunk_rows(&self.rs, &self.plan.columns, &self.plan.out_groups);
            if chunk.len == 0 {
                continue;
            }
            self.pending = Some(chunk);
            self.row = 0;
        }
    }
}
