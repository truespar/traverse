//! Joins: cartesian product (factorized replay of two materialized sides), hash join, and
//! OPTIONAL MATCH.
//!
//! Left runs to completion before right, so an earlier clause's writes are visible to a later
//! clause whatever the join shape.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::{CompiledExpr, EvalScratch};
use super::super::plan::{SeedRow, SubPlan};
use super::super::values::{JoinKey, hash_key, join_key};
use super::super::vector::{VECTOR_CAPACITY, Vector};
use super::subquery::SubRun;
use super::{PhysOperator, Staged, move_cols};

/// Cartesian product, left-major; both sides materialize and replay as two unflat groups.
pub struct CrossProductOp<'p> {
    left: Box<dyn PhysOperator + 'p>,
    right: Box<dyn PhysOperator + 'p>,
    left_groups: &'p [GroupId],
    right_groups: &'p [GroupId],
    left_out: GroupId,
    moves_left: Vec<(DataPos, u16)>,
    right_out: GroupId,
    moves_right: Vec<(DataPos, u16)>,
    left_st: Option<Staged>,
    right_st: Option<Staged>,
    li: usize,
    ri: usize,
}

impl<'p> CrossProductOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        left: Box<dyn PhysOperator + 'p>,
        right: Box<dyn PhysOperator + 'p>,
        left_groups: &'p [GroupId],
        right_groups: &'p [GroupId],
        left_out: GroupId,
        moves_left: &'p [(DataPos, DataPos)],
        right_out: GroupId,
        moves_right: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            left,
            right,
            left_groups,
            right_groups,
            left_out,
            moves_left: move_cols(moves_left),
            right_out,
            moves_right: move_cols(moves_right),
            left_st: None,
            right_st: None,
            li: 0,
            ri: 0,
        }
    }
}

impl PhysOperator for CrossProductOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.left_st.is_none() {
            let mut l = Staged::new(rs.group(self.left_out).cols.len());
            while self.left.next(rs, cx)? {
                l.push_moves(rs, self.left_groups, &self.moves_left)?;
                cx.tick()?;
            }
            l.finish()?;
            let mut r = Staged::new(rs.group(self.right_out).cols.len());
            // an empty left side is an empty product whatever the right side holds, and the
            // right side's effects must not run
            if !l.is_empty() {
                while self.right.next(rs, cx)? {
                    r.push_moves(rs, self.right_groups, &self.moves_right)?;
                    cx.tick()?;
                }
            }
            r.finish()?;
            self.left_st = Some(l);
            self.right_st = Some(r);
            self.li = 0;
            self.ri = 0;
        }
        let l = self.left_st.as_mut().unwrap();
        let r = self.right_st.as_mut().unwrap();
        if l.is_empty() || r.is_empty() || self.li >= l.len() {
            return Ok(false);
        }
        let l_end = (self.li + VECTOR_CAPACITY).min(l.len());
        let r_end = (self.ri + VECTOR_CAPACITY).min(r.len());
        l.emit_range(rs, self.left_out, self.li, l_end)?;
        r.emit_range(rs, self.right_out, self.ri, r_end)?;
        self.ri = r_end;
        if self.ri >= r.len() {
            self.ri = 0;
            self.li = l_end;
        }
        cx.tick()?;
        Ok(true)
    }

    fn reset(&mut self) {
        self.left_st = None;
        self.right_st = None;
        self.li = 0;
        self.ri = 0;
        self.left.reset();
        self.right.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.left.seed(rows.clone());
        self.right.seed(rows);
    }
}

/// The hashed build side of a join: rows keyed by their join key, in one map or partitioned
/// by key hash when the scheduler built it in parallel.
pub(crate) struct BuildTable {
    maps: Vec<FxHashMap<Vec<JoinKey>, Vec<u32>>>,
    /// The build rows' values, one column per left move.
    pub(crate) cols: Vec<Vector>,
}

impl BuildTable {
    pub(crate) fn new(maps: Vec<FxHashMap<Vec<JoinKey>, Vec<u32>>>, cols: Vec<Vector>) -> Self {
        Self { maps, cols }
    }

    /// No build rows?
    pub(crate) fn is_empty(&self) -> bool {
        self.maps.iter().all(FxHashMap::is_empty)
    }

    /// The build rows with `key`.
    #[inline]
    pub(crate) fn lookup(&self, key: &[JoinKey]) -> Option<&[u32]> {
        let map = match self.maps.len() {
            0 => return None,
            1 => &self.maps[0],
            n => &self.maps[(hash_key(key) % n as u64) as usize],
        };
        map.get(key).map(Vec::as_slice)
    }
}

/// Equi-join: the left side is hashed, the right streams; output is right-major.
pub struct HashJoinOp<'p> {
    left: Box<dyn PhysOperator + 'p>,
    right: Box<dyn PhysOperator + 'p>,
    left_keys: &'p [CompiledExpr],
    right_keys: &'p [CompiledExpr],
    left_groups: &'p [GroupId],
    right_groups: &'p [GroupId],
    out_group: GroupId,
    moves_left: Vec<(DataPos, u16)>,
    moves_right: Vec<(DataPos, u16)>,
    scratch: EvalScratch<'p>,
    table: Option<Arc<BuildTable>>,
    /// The table came prebuilt (a parallel worker's probe): it survives resets.
    prebuilt: bool,
    out: Option<Staged>,
    right_done: bool,
}

impl<'p> HashJoinOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        left: Box<dyn PhysOperator + 'p>,
        right: Box<dyn PhysOperator + 'p>,
        left_keys: &'p [CompiledExpr],
        right_keys: &'p [CompiledExpr],
        left_groups: &'p [GroupId],
        right_groups: &'p [GroupId],
        out_group: GroupId,
        moves_left: &'p [(DataPos, DataPos)],
        moves_right: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            left,
            right,
            left_keys,
            right_keys,
            left_groups,
            right_groups,
            out_group,
            moves_left: move_cols(moves_left),
            moves_right: move_cols(moves_right),
            scratch: EvalScratch::new(),
            table: None,
            prebuilt: false,
            out: None,
            right_done: false,
        }
    }

    /// A probe of `table`, built elsewhere, with the right side streaming through.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn probe(
        right: Box<dyn PhysOperator + 'p>,
        table: Arc<BuildTable>,
        right_keys: &'p [CompiledExpr],
        right_groups: &'p [GroupId],
        out_group: GroupId,
        moves_left: &'p [(DataPos, DataPos)],
        moves_right: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            left: Box::new(super::source::NoRowsOp),
            right,
            left_keys: &[],
            right_keys,
            left_groups: &[],
            right_groups,
            out_group,
            moves_left: move_cols(moves_left),
            moves_right: move_cols(moves_right),
            scratch: EvalScratch::new(),
            table: Some(table),
            prebuilt: true,
            out: None,
            right_done: false,
        }
    }
}

impl PhysOperator for HashJoinOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.table.is_none() {
            // build: the left side's rows, keyed
            let mut st = Staged::new(self.moves_left.len());
            let mut map: FxHashMap<Vec<JoinKey>, Vec<u32>> = FxHashMap::default();
            let mut charge = crate::memory::Charge::new();
            while self.left.next(rs, cx)? {
                let mut lanes = Vec::new();
                let total = rs.row_count(self.left_groups);
                for row in 0..total {
                    rs.row_lanes(self.left_groups, row, &mut lanes);
                    if row & 0xFFF == 0xFFF {
                        cx.check()?;
                    }
                    let mut key = Vec::with_capacity(self.left_keys.len());
                    for k in self.left_keys {
                        key.push(join_key(&k.eval_row(
                            rs,
                            cx.view(),
                            &lanes,
                            &mut self.scratch,
                        )?));
                    }
                    let idx = st.len() as u32;
                    let moves = &self.moves_left;
                    st.push_row(|cols| {
                        for (i, (src, _)) in moves.iter().enumerate() {
                            cols[i].push(rs.value_at(*src, &lanes));
                        }
                    })?;
                    charge.add(48 + 32 * key.len())?;
                    map.entry(key).or_default().push(idx);
                }
                cx.tick()?;
            }
            self.table = Some(Arc::new(BuildTable::new(vec![map], st.into_cols()?)));
        }
        loop {
            if let Some(out) = &mut self.out {
                if out.emit(rs, self.out_group)? {
                    return Ok(true);
                }
                self.out = None;
            }
            let table = self.table.as_ref().unwrap();
            if self.right_done || table.is_empty() {
                return Ok(false);
            }
            if !self.right.next(rs, cx)? {
                self.right_done = true;
                return Ok(false);
            }
            // probe: every right row's matches, left values then right values
            let width = rs.group(self.out_group).cols.len();
            let mut out = Staged::new(width);
            let mut lanes = Vec::new();
            let total = rs.row_count(self.right_groups);
            for row in 0..total {
                rs.row_lanes(self.right_groups, row, &mut lanes);
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
                let mut key = Vec::with_capacity(self.right_keys.len());
                for k in self.right_keys {
                    key.push(join_key(&k.eval_row(
                        rs,
                        cx.view(),
                        &lanes,
                        &mut self.scratch,
                    )?));
                }
                let Some(matches) = table.lookup(&key) else {
                    continue;
                };
                for &m in matches {
                    let moves_left = &self.moves_left;
                    let moves_right = &self.moves_right;
                    out.push_row(|cols| {
                        for (i, (_, c)) in moves_left.iter().enumerate() {
                            cols[*c as usize].push(table.cols[i].get(m as usize));
                        }
                        for (src, c) in moves_right {
                            cols[*c as usize].push(rs.value_at(*src, &lanes));
                        }
                    })?;
                }
            }
            cx.tick()?;
            if out.is_empty() {
                continue;
            }
            out.finish()?;
            self.out = Some(out);
        }
    }

    fn reset(&mut self) {
        if !self.prebuilt {
            self.table = None;
        }
        self.out = None;
        self.right_done = false;
        self.left.reset();
        self.right.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.left.seed(rows.clone());
        self.right.seed(rows);
    }
}

/// `OPTIONAL MATCH`: per input row the matches seeded with it, or the row with nulls.
pub struct OptionalMatchOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    body: SubRun<'p>,
    in_groups: &'p [GroupId],
    out_group: GroupId,
    moves: Vec<(DataPos, u16)>,
    body_out: &'p [(DataPos, DataPos)],
    staged: Option<Staged>,
    done: bool,
}

impl<'p> OptionalMatchOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        body: &'p SubPlan,
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
        body_out: &'p [(DataPos, DataPos)],
    ) -> Result<Self, CypherError> {
        Ok(Self {
            input,
            body: SubRun::new(body)?,
            in_groups,
            out_group,
            moves: move_cols(moves),
            body_out,
            staged: None,
            done: false,
        })
    }
}

impl PhysOperator for OptionalMatchOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(st) = &mut self.staged {
                if st.emit(rs, self.out_group)? {
                    return Ok(true);
                }
                self.staged = None;
            }
            if self.done || !self.input.next(rs, cx)? {
                self.done = true;
                return Ok(false);
            }
            let width = rs.group(self.out_group).cols.len();
            let mut st = Staged::new(width);
            let total = rs.row_count(self.in_groups);
            let mut lanes = Vec::new();
            let body_pos: Vec<DataPos> = self.body_out.iter().map(|(b, _)| *b).collect();
            for row in 0..total {
                rs.row_lanes(self.in_groups, row, &mut lanes);
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
                let seed = self.body.seed_row(rs, &lanes, None);
                let outer: Vec<CypherValue> = self
                    .moves
                    .iter()
                    .map(|(src, _)| rs.value_at(*src, &lanes).to_owned())
                    .collect();
                let moves = &self.moves;
                let body_out = self.body_out;
                let rows = self.body.run(seed, cx, &body_pos, |vals| {
                    st.push_row(|cols| {
                        for ((_, c), v) in moves.iter().zip(&outer) {
                            cols[*c as usize].push_owned(v.clone());
                        }
                        for ((_, out), v) in body_out.iter().zip(vals) {
                            cols[out.col as usize].push_owned(v);
                        }
                    })
                })?;
                if rows == 0 {
                    st.push_row(|cols| {
                        for ((_, c), v) in moves.iter().zip(&outer) {
                            cols[*c as usize].push_owned(v.clone());
                        }
                        for (_, out) in body_out {
                            cols[out.col as usize].push_owned(CypherValue::Null);
                        }
                    })?;
                }
                if row & 0xFF == 0xFF {
                    cx.check()?;
                }
            }
            self.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.staged = None;
        self.done = false;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
