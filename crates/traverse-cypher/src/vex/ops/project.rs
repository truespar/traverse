//! Projection with expressions, DISTINCT and `WITH ... WHERE`: the rows of the input groups
//! become one flat group of the projected columns.

use rustc_hash::FxHashSet;

use crate::error::CypherError;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::eval::EvalCtx;
use super::super::expr::{CompiledExpr, EvalScratch, kernels};
use super::super::plan::SeedRow;
use super::super::values::{JoinKey, join_key_ref};
use super::super::vector::{VECTOR_CAPACITY, Vector, VectorBuilder};
use super::{PhysOperator, install, move_cols};

/// The projection operator.
pub struct ProjectOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    items: &'p [(CompiledExpr, DataPos)],
    moves: Vec<(DataPos, u16)>,
    in_groups: &'p [GroupId],
    out_group: GroupId,
    distinct: bool,
    where_: Option<&'p CompiledExpr>,
    scratch: EvalScratch<'p>,
    /// Rows of the current input chunk's product already emitted (row-wise path).
    row_cursor: usize,
    have_chunk: bool,
    seen: FxHashSet<Vec<JoinKey>>,
    width: usize,
}

impl<'p> ProjectOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        items: &'p [(CompiledExpr, DataPos)],
        moves: &'p [(DataPos, DataPos)],
        in_groups: &'p [GroupId],
        out_group: GroupId,
        distinct: bool,
        where_: Option<&'p CompiledExpr>,
    ) -> Self {
        Self {
            input,
            items,
            moves: move_cols(moves),
            in_groups,
            out_group,
            distinct,
            where_,
            scratch: EvalScratch::new(),
            row_cursor: 0,
            have_chunk: false,
            seen: FxHashSet::default(),
            width: 0,
        }
    }

    /// Projects the current chunk vectorized when its rows vary along at most one group.
    fn eval_group(&self, rs: &ResultSet) -> Option<Option<GroupId>> {
        let unflat: Vec<GroupId> = self
            .in_groups
            .iter()
            .copied()
            .filter(|&g| rs.group(g).active && rs.group(g).flat.is_none())
            .collect();
        match unflat.len() {
            0 => Some(None),
            1 => Some(Some(unflat[0])),
            _ => None,
        }
    }

    /// Applies DISTINCT and WHERE to freshly built output columns; returns the surviving lanes.
    fn finish_chunk(
        &mut self,
        rs: &mut ResultSet,
        cx: &ExecCtx<'_>,
        n: usize,
        cols: Vec<(u16, Vector)>,
        row_lanes: Option<&[Vec<usize>]>,
    ) -> Result<bool, CypherError> {
        install(rs, self.out_group, n, cols);
        let mut keep: Vec<u32> = (0..n as u32).collect();
        if let Some(w) = self.where_ {
            // the WHERE sees old bindings (input groups) and new ones (output group): evaluate
            // per output row with the input lanes it came from
            let mut kept = Vec::with_capacity(n);
            for (i, &lane) in keep.iter().enumerate() {
                let v = match row_lanes {
                    Some(rl) => {
                        let mut lanes = rl[i].clone();
                        lanes[self.out_group as usize] = lane as usize;
                        w.eval_row(rs, cx.view(), &lanes, &mut self.scratch)?
                    }
                    None => {
                        // vectorized path: output lane i came from selected position i of the
                        // one unflat input group (groups outside the input may be stale)
                        let mut lanes = vec![0usize; rs.groups.len()];
                        for (g, group) in rs.groups.iter().enumerate() {
                            if let Some(l) = group.current_lane() {
                                lanes[g] = l;
                            }
                        }
                        for &g in self.in_groups {
                            let group = rs.group(g);
                            if group.active && group.flat.is_none() {
                                lanes[g as usize] = group.lane(lane as usize);
                            }
                        }
                        lanes[self.out_group as usize] = lane as usize;
                        w.eval_row(rs, cx.view(), &lanes, &mut self.scratch)?
                    }
                };
                if kernels::lane_bool3(super::super::vector::ValueRef::of(&v)) == Some(true) {
                    kept.push(lane);
                }
            }
            keep = kept;
        }
        if self.distinct {
            let mut kept = Vec::with_capacity(keep.len());
            for &lane in &keep {
                let key: Vec<JoinKey> = (0..self.width)
                    .map(|c| join_key_ref(rs.group(self.out_group).cols[c].get(lane as usize)))
                    .collect();
                if self.seen.insert(key) {
                    kept.push(lane);
                }
            }
            keep = kept;
        }
        if keep.len() == n {
            return Ok(n > 0);
        }
        if keep.is_empty() {
            return Ok(false);
        }
        rs.group_mut(self.out_group).narrow(keep);
        Ok(true)
    }
}

impl PhysOperator for ProjectOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.width == 0 {
            self.width = rs.group(self.out_group).cols.len();
        }
        loop {
            // row-wise continuation of a large product
            if self.have_chunk {
                let total = rs.row_count(self.in_groups);
                if self.row_cursor < total {
                    let end = (self.row_cursor + VECTOR_CAPACITY).min(total);
                    let mut builders: Vec<VectorBuilder> =
                        (0..self.width).map(|_| VectorBuilder::new()).collect();
                    let mut lanes = Vec::new();
                    let mut row_lanes: Vec<Vec<usize>> = Vec::with_capacity(end - self.row_cursor);
                    for row in self.row_cursor..end {
                        rs.row_lanes(self.in_groups, row, &mut lanes);
                        for (expr, out) in self.items {
                            let v = expr.eval_row(rs, cx.view(), &lanes, &mut self.scratch)?;
                            builders[out.col as usize].push_owned(v);
                        }
                        for (src, c) in &self.moves {
                            builders[*c as usize].push(rs.value_at(*src, &lanes));
                        }
                        row_lanes.push(lanes.clone());
                    }
                    let n = end - self.row_cursor;
                    self.row_cursor = end;
                    let cols: Vec<(u16, Vector)> = builders
                        .into_iter()
                        .enumerate()
                        .map(|(c, b)| (c as u16, b.finish()))
                        .collect();
                    cx.tick()?;
                    if self.finish_chunk(rs, cx, n, cols, Some(&row_lanes))? {
                        return Ok(true);
                    }
                    continue;
                }
                self.have_chunk = false;
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            match self.eval_group(rs) {
                Some(group) => {
                    // vectorized: one evaluation per item over the varying group
                    let n = match group {
                        Some(g) => rs.group(g).selected_len(),
                        None => 1,
                    };
                    if n == 0 {
                        continue;
                    }
                    let mut cols: Vec<(u16, Vector)> = Vec::with_capacity(self.width);
                    {
                        let ecx = EvalCtx::new(rs, cx.view(), group);
                        for (expr, out) in self.items {
                            let v = expr.eval(&ecx, &mut self.scratch)?;
                            cols.push((out.col, v.into_vector(n)));
                        }
                        for (src, c) in &self.moves {
                            let v = if Some(src.group) == group {
                                let g = rs.group(src.group);
                                match &g.sel {
                                    Some(s) => g.cols[src.col as usize].take(s),
                                    None => g.cols[src.col as usize].clone(),
                                }
                            } else {
                                let val = rs.scalar_at(*src).to_owned();
                                Vector::from_values(vec![val; n])
                            };
                            cols.push((*c, v));
                        }
                    }
                    cx.tick()?;
                    if self.finish_chunk(rs, cx, n, cols, None)? {
                        return Ok(true);
                    }
                }
                None => {
                    // several unflat groups: iterate the product, chunk by chunk
                    self.have_chunk = true;
                    self.row_cursor = 0;
                }
            }
        }
    }

    fn reset(&mut self) {
        self.have_chunk = false;
        self.row_cursor = 0;
        self.seen.clear();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
