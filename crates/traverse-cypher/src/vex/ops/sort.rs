//! `ORDER BY`: every row materialized with its keys, sorted, emitted in chunks. The state is
//! separate from the operator so parallel workers can stage their morsels' rows and keys for the
//! scheduler's merge sort; under a `LIMIT` the state keeps only the top rows, rejecting the rest
//! against the worst kept row before they are staged at all.

use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::eval::EvalCtx;
use super::super::expr::scalar::compare_values;
use super::super::expr::{CompiledExpr, EvalScratch, Operand};
use super::super::plan::SeedRow;
use super::super::vector::{VECTOR_CAPACITY, ValueRef, Vector};
use super::{PhysOperator, Staged, move_cols};

/// Orders two rows' keys under the sort's directions.
fn order_keys(
    a: &[CypherValue],
    b: &[CypherValue],
    keys: &[(CompiledExpr, bool)],
) -> std::cmp::Ordering {
    for (i, (_, asc)) in keys.iter().enumerate() {
        let ord = compare_values(&a[i], &b[i]);
        let ord = if *asc { ord } else { ord.reverse() };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// Does staged row `a` sort after row `b` (ties: the later row is the worse one)?
fn worse(a: u32, b: u32, key_vals: &[CypherValue], keys: &[(CompiledExpr, bool)]) -> bool {
    let nk = keys.len();
    let ab = a as usize * nk;
    let bb = b as usize * nk;
    match order_keys(&key_vals[ab..ab + nk], &key_vals[bb..bb + nk], keys) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => a > b,
    }
}

/// The kept rows of a top-k as a heap of staged row indices, the worst kept row on top.
struct TopHeap {
    k: usize,
    rows: Vec<u32>,
}

impl TopHeap {
    fn new(k: usize) -> Self {
        Self {
            k,
            rows: Vec::with_capacity(k.min(1 << 16)),
        }
    }

    fn is_full(&self) -> bool {
        self.rows.len() >= self.k
    }

    /// The worst kept row.
    fn top(&self) -> Option<u32> {
        self.rows.first().copied()
    }

    fn push(&mut self, row: u32, key_vals: &[CypherValue], keys: &[(CompiledExpr, bool)]) {
        self.rows.push(row);
        let mut i = self.rows.len() - 1;
        while i > 0 {
            let p = (i - 1) / 2;
            if worse(self.rows[i], self.rows[p], key_vals, keys) {
                self.rows.swap(i, p);
                i = p;
            } else {
                break;
            }
        }
    }

    /// Replaces the worst row with `row` and restores the heap.
    fn replace_top(&mut self, row: u32, key_vals: &[CypherValue], keys: &[(CompiledExpr, bool)]) {
        self.rows[0] = row;
        let n = self.rows.len();
        let mut i = 0;
        loop {
            let l = 2 * i + 1;
            let r = l + 1;
            let mut m = i;
            if l < n && worse(self.rows[l], self.rows[m], key_vals, keys) {
                m = l;
            }
            if r < n && worse(self.rows[r], self.rows[m], key_vals, keys) {
                m = r;
            }
            if m == i {
                break;
            }
            self.rows.swap(i, m);
            i = m;
        }
    }
}

/// Staged rows with their sort keys.
pub(crate) struct SortState<'p> {
    keys: &'p [(CompiledExpr, bool)],
    in_groups: &'p [GroupId],
    moves: Vec<(DataPos, u16)>,
    width: usize,
    scratch: EvalScratch<'p>,
    st: Staged,
    key_vals: Vec<CypherValue>,
    charge: crate::memory::Charge,
    /// Keep only this many rows (a `LIMIT` above): the heap of kept rows, by staged index.
    top: Option<TopHeap>,
}

impl<'p> SortState<'p> {
    pub(crate) fn new(
        keys: &'p [(CompiledExpr, bool)],
        in_groups: &'p [GroupId],
        moves: &'p [(DataPos, DataPos)],
        width: usize,
        top: Option<usize>,
    ) -> Self {
        Self {
            keys,
            in_groups,
            moves: move_cols(moves),
            width,
            scratch: EvalScratch::new(),
            st: Staged::new(width),
            key_vals: Vec::new(),
            charge: crate::memory::Charge::new(),
            top: top.map(TopHeap::new),
        }
    }

    /// Stages the rows of the current chunk of `rs` with their keys.
    pub(crate) fn consume_chunk(
        &mut self,
        rs: &ResultSet,
        cx: &ExecCtx<'_>,
    ) -> Result<(), CypherError> {
        let nk = self.keys.len();
        let unflat: Vec<GroupId> = self
            .in_groups
            .iter()
            .copied()
            .filter(|&g| rs.group(g).active && rs.group(g).flat.is_none())
            .collect();
        if unflat.len() <= 1 {
            let group = unflat.first().copied();
            let ecx = EvalCtx::new(rs, cx.view(), group);
            let n = ecx.n;
            let mut ops: Vec<Operand<'_>> = Vec::with_capacity(nk);
            for (k, _) in self.keys {
                ops.push(k.eval(&ecx, &mut self.scratch)?);
            }
            if self.top.is_some() {
                // row by row against the boundary; the varying group's lane is its selected
                // position, the others stand on their current lane
                let mut lanes = vec![0usize; rs.groups.len()];
                for (g, grp) in rs.groups.iter().enumerate() {
                    if let Some(l) = grp.current_lane() {
                        lanes[g] = l;
                    }
                }
                // one integer key against an integer boundary: a plain comparison per lane
                let int_key: Option<(&[i64], &[bool])> = match (nk, &ops[0]) {
                    (1, Operand::Owned(Vector::Int { vals, valid })) if vals.len() == n => {
                        Some((vals, valid))
                    }
                    _ => None,
                };
                for i in 0..n {
                    let beats = match int_key {
                        Some((vals, valid)) if valid[i] => self.int_beats_boundary(vals[i]),
                        _ => self.beats_boundary(|j| ops[j].get(i)),
                    };
                    if !beats {
                        continue;
                    }
                    if let Some(g) = group {
                        lanes[g as usize] = rs.group(g).lane(i);
                    }
                    let vals: Vec<CypherValue> =
                        (0..nk).map(|j| ops[j].get(i).to_owned()).collect();
                    self.stage_row(rs, &lanes, vals)?;
                }
            } else {
                for i in 0..n {
                    for o in &ops {
                        self.key_vals.push(o.get(i).to_owned());
                    }
                }
                drop(ops);
                self.st.push_moves(rs, self.in_groups, &self.moves)?;
            }
        } else {
            let keys = self.keys;
            let graph = cx.view();
            let mut rows: Vec<(Vec<usize>, Vec<CypherValue>)> = Vec::new();
            {
                let scratch = &mut self.scratch;
                rs.for_each_row(self.in_groups, |lanes| {
                    let mut vals = Vec::with_capacity(nk);
                    for (k, _) in keys {
                        vals.push(k.eval_row(rs, graph, lanes, scratch)?);
                    }
                    rows.push((lanes.to_vec(), vals));
                    Ok(())
                })?;
            }
            for (lanes, vals) in rows {
                if self.top.is_some() && !self.beats_boundary(|j| ValueRef::of(&vals[j])) {
                    continue;
                }
                self.stage_row(rs, &lanes, vals)?;
            }
        }
        self.charge.add(nk * 48 * rs.row_count(self.in_groups))?;
        if let Some(h) = &self.top
            && self.st.len() >= h.k.saturating_mul(4).saturating_add(VECTOR_CAPACITY)
        {
            self.compact()?;
        }
        Ok(())
    }

    /// [`beats_boundary`](Self::beats_boundary) for one non-null integer key.
    #[inline]
    fn int_beats_boundary(&self, v: i64) -> bool {
        let Some(h) = &self.top else {
            return true;
        };
        if !h.is_full() {
            return true;
        }
        let Some(worst) = h.top() else {
            return true;
        };
        match &self.key_vals[worst as usize] {
            CypherValue::Integer(b) => {
                if self.keys[0].1 {
                    v < *b
                } else {
                    v > *b
                }
            }
            other => {
                let ord = compare_values(&CypherValue::Integer(v), other);
                let ord = if self.keys[0].1 { ord } else { ord.reverse() };
                ord == std::cmp::Ordering::Less
            }
        }
    }

    /// Does a row with these keys belong among the top rows (the heap not full yet, or the row
    /// ahead of the worst kept one)?
    fn beats_boundary<'v>(&self, row_key: impl Fn(usize) -> ValueRef<'v>) -> bool {
        let Some(h) = &self.top else {
            return true;
        };
        if !h.is_full() {
            return true;
        }
        let Some(worst) = h.top() else {
            return true;
        };
        let nk = self.keys.len();
        let base = worst as usize * nk;
        for (j, (_, asc)) in self.keys.iter().enumerate() {
            let v = row_key(j).to_owned();
            let ord = compare_values(&v, &self.key_vals[base + j]);
            let ord = if *asc { ord } else { ord.reverse() };
            match ord {
                std::cmp::Ordering::Less => return true,
                std::cmp::Ordering::Greater => return false,
                std::cmp::Ordering::Equal => {}
            }
        }
        // a tie keeps the earlier row
        false
    }

    /// Stages one row with its keys, maintaining the top-k heap.
    fn stage_row(
        &mut self,
        rs: &ResultSet,
        lanes: &[usize],
        vals: Vec<CypherValue>,
    ) -> Result<(), CypherError> {
        let row = self.st.len() as u32;
        let moves = &self.moves;
        self.st.push_row(|cols| {
            for (src, c) in moves {
                cols[*c as usize].push(rs.value_at(*src, lanes));
            }
        })?;
        self.key_vals.extend(vals);
        if let Some(h) = &mut self.top {
            if h.is_full() {
                h.replace_top(row, &self.key_vals, self.keys);
            } else {
                h.push(row, &self.key_vals, self.keys);
            }
        }
        Ok(())
    }

    /// Rebuilds the stage with only the kept rows.
    fn compact(&mut self) -> Result<(), CypherError> {
        let Some(h) = &self.top else {
            return Ok(());
        };
        let nk = self.keys.len();
        let mut order: Vec<u32> = h.rows.clone();
        order.sort_unstable();
        let cols = std::mem::replace(&mut self.st, Staged::new(self.width)).into_cols()?;
        let kept: Vec<Vector> = cols.iter().map(|c| c.take(&order)).collect();
        let mut st = Staged::new(self.width);
        for r in 0..order.len() {
            st.push_row(|cs| {
                for (c, v) in cs.iter_mut().zip(&kept) {
                    c.push(v.get(r));
                }
            })?;
        }
        let mut keys = Vec::with_capacity(order.len() * nk);
        for &r in &order {
            let base = r as usize * nk;
            keys.extend_from_slice(&self.key_vals[base..base + nk]);
        }
        self.st = st;
        self.key_vals = keys;
        // the kept rows sit at their new positions; the heap is rebuilt over them
        let k = h.k;
        let mut heap = TopHeap::new(k);
        for r in 0..order.len() as u32 {
            heap.push(r, &self.key_vals, self.keys);
        }
        self.top = Some(heap);
        Ok(())
    }

    /// The staged rows as columns with their keys (only the kept rows under a limit).
    pub(crate) fn finish(mut self) -> Result<(Vec<Vector>, Vec<CypherValue>, usize), CypherError> {
        if self.top.is_some() {
            self.compact()?;
        }
        let rows = self.st.len();
        let cols = self.st.into_cols()?;
        Ok((cols, self.key_vals, rows))
    }

    /// Row indices in sorted order (stable, so ties keep input order).
    pub(crate) fn order(
        key_vals: &[CypherValue],
        keys: &[(CompiledExpr, bool)],
        rows: usize,
    ) -> Vec<u32> {
        let nk = keys.len();
        let mut order: Vec<u32> = (0..rows as u32).collect();
        order.sort_by(|&a, &b| {
            let ba = a as usize * nk;
            let bb = b as usize * nk;
            order_keys(&key_vals[ba..ba + nk], &key_vals[bb..bb + nk], keys)
        });
        order
    }
}

/// The sort operator.
pub struct SortOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    keys: &'p [(CompiledExpr, bool)],
    in_groups: &'p [GroupId],
    out_group: GroupId,
    moves: &'p [(DataPos, DataPos)],
    limit: Option<&'p CompiledExpr>,
    sorted: Option<(Vec<Vector>, Vec<u32>)>,
    pos: usize,
}

impl<'p> SortOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        keys: &'p [(CompiledExpr, bool)],
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
        limit: Option<&'p CompiledExpr>,
    ) -> Self {
        Self {
            input,
            keys,
            in_groups,
            out_group,
            moves,
            limit,
            sorted: None,
            pos: 0,
        }
    }
}

impl PhysOperator for SortOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.sorted.is_none() {
            let width = rs.group(self.out_group).cols.len();
            let top = match self.limit {
                Some(e) => Some(super::eval_count(e, rs, cx, "LIMIT")?),
                None => None,
            };
            let mut st = SortState::new(self.keys, self.in_groups, self.moves, width, top);
            while self.input.next(rs, cx)? {
                st.consume_chunk(rs, cx)?;
                cx.tick()?;
            }
            let (cols, key_vals, rows) = st.finish()?;
            let mut order = SortState::order(&key_vals, self.keys, rows);
            if let Some(k) = top {
                order.truncate(k);
            }
            self.sorted = Some((cols, order));
            self.pos = 0;
        }
        let (cols, order) = self.sorted.as_ref().unwrap();
        if self.pos >= order.len() {
            return Ok(false);
        }
        let end = (self.pos + VECTOR_CAPACITY).min(order.len());
        let sel = &order[self.pos..end];
        let out: Vec<(u16, Vector)> = cols
            .iter()
            .enumerate()
            .map(|(c, v)| (c as u16, v.take(sel)))
            .collect();
        super::install(rs, self.out_group, end - self.pos, out);
        self.pos = end;
        Ok(true)
    }

    fn reset(&mut self) {
        self.sorted = None;
        self.pos = 0;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
