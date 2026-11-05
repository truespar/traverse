//! Flow operators: Flatten, Filter, AddColumn, Unwind, Limit, Skip, Eager, Union.

use rustc_hash::FxHashSet;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::eval::EvalCtx;
use super::super::expr::{CompiledExpr, EvalScratch, Operand};
use super::super::plan::{Consume, SeedRow};
use super::super::values::{JoinKey, join_key_ref};
use super::super::vector::{VECTOR_CAPACITY, Vector};
use super::{PhysOperator, Staged, eval_count, install, move_cols};

/// Iterates the selected lanes of a group one at a time.
pub struct FlattenOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    group: GroupId,
    pos: usize,
    have_chunk: bool,
    /// The other groups' selections as the chunk arrived: the chain above runs once per
    /// position and may narrow them, so every position starts from the same input.
    sels: Vec<Option<Vec<u32>>>,
}

impl<'p> FlattenOp<'p> {
    pub(crate) fn new(input: Box<dyn PhysOperator + 'p>, group: GroupId) -> Self {
        Self {
            input,
            group,
            pos: 0,
            have_chunk: false,
            sels: Vec::new(),
        }
    }

    fn restore(&self, rs: &mut ResultSet) {
        for (g, sel) in self.sels.iter().enumerate() {
            if g != self.group as usize {
                rs.groups[g].sel = sel.clone();
            }
        }
    }
}

impl PhysOperator for FlattenOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if self.have_chunk {
                self.pos += 1;
                if self.pos < rs.group(self.group).selected_len() {
                    self.restore(rs);
                    rs.group_mut(self.group).flat = Some(self.pos);
                    return Ok(true);
                }
            }
            if !self.input.next(rs, cx)? {
                self.have_chunk = false;
                return Ok(false);
            }
            self.have_chunk = true;
            self.pos = 0;
            if rs.group(self.group).selected_len() == 0 {
                continue;
            }
            self.sels.clear();
            self.sels.extend(rs.groups.iter().map(|g| g.sel.clone()));
            rs.group_mut(self.group).flat = Some(0);
            return Ok(true);
        }
    }

    fn reset(&mut self) {
        self.have_chunk = false;
        self.pos = 0;
        self.sels.clear();
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Keeps the lanes of the evaluation group where the predicate is true.
pub struct FilterOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    pred: &'p CompiledExpr,
    group: Option<GroupId>,
    scratch: EvalScratch<'p>,
}

impl<'p> FilterOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        pred: &'p CompiledExpr,
        group: Option<GroupId>,
    ) -> Self {
        Self {
            input,
            pred,
            group,
            scratch: EvalScratch::new(),
        }
    }
}

impl PhysOperator for FilterOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            let keep: Option<Vec<u32>> = {
                let ecx = EvalCtx::new(rs, cx.view(), self.group);
                let n = ecx.n;
                let mask = self.pred.eval(&ecx, &mut self.scratch)?;
                match self.group {
                    None => {
                        // a scalar predicate keeps or drops the whole chunk
                        let v = mask.get(0);
                        if super::super::expr::kernels::lane_bool3(v) == Some(true) {
                            return Ok(true);
                        }
                        continue;
                    }
                    Some(_) => {
                        let mut keep = Vec::with_capacity(n);
                        match &mask {
                            Operand::Owned(Vector::Bool { vals, valid }) => {
                                for i in 0..n {
                                    if valid[i] && vals[i] {
                                        keep.push(i as u32);
                                    }
                                }
                            }
                            other => {
                                for i in 0..n {
                                    if super::super::expr::kernels::lane_bool3(other.get(i))
                                        == Some(true)
                                    {
                                        keep.push(i as u32);
                                    }
                                }
                            }
                        }
                        Some(keep)
                    }
                }
            };
            if let Some(keep) = keep {
                let g = self.group.unwrap();
                if keep.is_empty() {
                    continue;
                }
                let all = keep.len() == rs.group(g).selected_len();
                if !all {
                    rs.group_mut(g).narrow(keep);
                }
                return Ok(true);
            }
        }
    }

    fn reset(&mut self) {
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Computes an expression into a new column of its evaluation group.
pub struct AddColumnOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    expr: &'p CompiledExpr,
    out: DataPos,
    scratch: EvalScratch<'p>,
}

impl<'p> AddColumnOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        expr: &'p CompiledExpr,
        out: DataPos,
    ) -> Self {
        Self {
            input,
            expr,
            out,
            scratch: EvalScratch::new(),
        }
    }
}

impl PhysOperator for AddColumnOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if !self.input.next(rs, cx)? {
            return Ok(false);
        }
        let group = self.out.group;
        let active = rs.group(group).active;
        let ecx = EvalCtx::new(rs, cx.view(), if active { Some(group) } else { None });
        let n = ecx.n;
        let v = self.expr.eval(&ecx, &mut self.scratch)?;
        let vec = expand_to_lanes(v, rs.group(group), n, active);
        if active {
            rs.group_mut(group).cols[self.out.col as usize] = vec;
        } else {
            install(rs, group, 1, vec![(self.out.col, vec)]);
        }
        Ok(true)
    }

    fn reset(&mut self) {
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// A computed operand as a full column of `group` (one lane per physical lane, values at the
/// selected lanes), or a one-lane vector for a scalar group.
fn expand_to_lanes(
    v: Operand<'_>,
    group: &super::super::chunk::Group,
    n: usize,
    active: bool,
) -> Vector {
    if !active {
        return v.into_vector(1);
    }
    let len = group.len;
    match (&group.sel, v) {
        (None, v) => v.into_vector(n),
        (Some(sel), v) => {
            // scatter the selected values into a column of the group's physical length
            let mut vals: Vec<CypherValue> = vec![CypherValue::Null; len];
            for (i, &lane) in sel.iter().enumerate() {
                vals[lane as usize] = v.get(i).to_owned();
            }
            Vector::from_values(vals)
        }
    }
}

/// Elements of a list as rows of a new group.
pub struct UnwindOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    list: &'p CompiledExpr,
    out: DataPos,
    out_group: GroupId,
    scratch: EvalScratch<'p>,
    pending: Vec<CypherValue>,
    pos: usize,
}

impl<'p> UnwindOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        list: &'p CompiledExpr,
        out: DataPos,
        out_group: GroupId,
    ) -> Self {
        Self {
            input,
            list,
            out,
            out_group,
            scratch: EvalScratch::new(),
            pending: Vec::new(),
            pos: 0,
        }
    }
}

impl PhysOperator for UnwindOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if self.pos < self.pending.len() {
                let end = (self.pos + VECTOR_CAPACITY).min(self.pending.len());
                let items: Vec<CypherValue> = self.pending[self.pos..end].to_vec();
                self.pos = end;
                if self.pos >= self.pending.len() {
                    self.pending.clear();
                    self.pos = 0;
                }
                let n = items.len();
                install(
                    rs,
                    self.out_group,
                    n,
                    vec![(self.out.col, Vector::from_values(items))],
                );
                cx.tick()?;
                return Ok(true);
            }
            if !self.input.next(rs, cx)? {
                return Ok(false);
            }
            let v = self.list.eval_scalar(rs, cx.view(), &mut self.scratch)?;
            match v {
                CypherValue::List(items) => {
                    self.pending = items;
                    self.pos = 0;
                }
                CypherValue::Null => {}
                // Neo4j treats a non-list as a singleton list
                other => {
                    self.pending = vec![other];
                    self.pos = 0;
                }
            }
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.pos = 0;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// `LIMIT`: narrows the single unflat group, or consumes rows into a flat group.
pub struct LimitOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    count: &'p CompiledExpr,
    in_groups: &'p [GroupId],
    materialize: bool,
    consume: Option<&'p Consume>,
    remaining: Option<usize>,
    staged: Option<Staged>,
}

impl<'p> LimitOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        count: &'p CompiledExpr,
        in_groups: &'p [GroupId],
        materialize: bool,
        consume: Option<&'p Consume>,
    ) -> Self {
        Self {
            input,
            count,
            in_groups,
            materialize,
            consume,
            remaining: None,
            staged: None,
        }
    }
}

impl PhysOperator for LimitOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.remaining.is_none() {
            self.remaining = Some(eval_count(self.count, rs, cx, "LIMIT")?);
        }
        match self.consume {
            None => {
                let remaining = self.remaining.unwrap();
                if remaining == 0 {
                    return Ok(false);
                }
                let outer = cx.set_row_budget(Some(remaining));
                let more = self.input.next(rs, cx);
                cx.set_row_budget(outer);
                if !more? {
                    return Ok(false);
                }
                let rows = rs.row_count(self.in_groups);
                if rows <= remaining {
                    self.remaining = Some(remaining - rows);
                    return Ok(true);
                }
                // cut the single unflat group to the first `remaining` positions
                let unflat: Vec<GroupId> = self
                    .in_groups
                    .iter()
                    .copied()
                    .filter(|&g| rs.group(g).active && rs.group(g).flat.is_none())
                    .collect();
                if let Some(&g) = unflat.first() {
                    let keep: Vec<u32> = (0..remaining as u32).collect();
                    rs.group_mut(g).narrow(keep);
                }
                self.remaining = Some(0);
                Ok(true)
            }
            Some(consume) => {
                if self.staged.is_none() {
                    let width = rs.group(consume.out_group).cols.len();
                    let mut st = Staged::new(width);
                    let moves = move_cols(&consume.moves);
                    let mut remaining = self.remaining.unwrap();
                    loop {
                        // a materializing limit runs its input to the end (writes below)
                        let budget = (!self.materialize && remaining > 0).then_some(remaining);
                        let outer = cx.set_row_budget(budget);
                        let more = self.input.next(rs, cx);
                        cx.set_row_budget(outer);
                        if !more? {
                            break;
                        }
                        let rows = rs.row_count(self.in_groups);
                        if remaining > 0 {
                            let take = rows.min(remaining);
                            let mut seen = 0usize;
                            rs.for_each_row(self.in_groups, |lanes| {
                                if seen >= take {
                                    return Ok(());
                                }
                                seen += 1;
                                st.push_row(|cols| {
                                    for (src, c) in &moves {
                                        cols[*c as usize].push(rs.value_at(*src, lanes));
                                    }
                                })
                            })?;
                            remaining -= take;
                        }
                        if remaining == 0 && !self.materialize {
                            break;
                        }
                        cx.tick()?;
                    }
                    self.staged = Some(st);
                }
                self.staged.as_mut().unwrap().emit(rs, consume.out_group)
            }
        }
    }

    fn reset(&mut self) {
        self.remaining = None;
        self.staged = None;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// `SKIP`: drops the first rows.
pub struct SkipOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    count: &'p CompiledExpr,
    in_groups: &'p [GroupId],
    consume: Option<&'p Consume>,
    to_skip: Option<usize>,
    staged: Option<Staged>,
}

impl<'p> SkipOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        count: &'p CompiledExpr,
        in_groups: &'p [GroupId],
        consume: Option<&'p Consume>,
    ) -> Self {
        Self {
            input,
            count,
            in_groups,
            consume,
            to_skip: None,
            staged: None,
        }
    }
}

impl PhysOperator for SkipOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.to_skip.is_none() {
            self.to_skip = Some(eval_count(self.count, rs, cx, "SKIP")?);
        }
        match self.consume {
            None => loop {
                // under a LIMIT the rows still to skip come on top of what it wants
                let outer = cx.row_budget();
                let widened = outer.map(|b| b.saturating_add(self.to_skip.unwrap()));
                cx.set_row_budget(widened);
                let more = self.input.next(rs, cx);
                cx.set_row_budget(outer);
                if !more? {
                    return Ok(false);
                }
                let skip = self.to_skip.unwrap();
                if skip == 0 {
                    return Ok(true);
                }
                let rows = rs.row_count(self.in_groups);
                if rows <= skip {
                    self.to_skip = Some(skip - rows);
                    continue;
                }
                let unflat: Vec<GroupId> = self
                    .in_groups
                    .iter()
                    .copied()
                    .filter(|&g| rs.group(g).active && rs.group(g).flat.is_none())
                    .collect();
                if let Some(&g) = unflat.first() {
                    let keep: Vec<u32> = (skip as u32..rows as u32).collect();
                    rs.group_mut(g).narrow(keep);
                }
                self.to_skip = Some(0);
                return Ok(true);
            },
            Some(consume) => {
                if self.staged.is_none() {
                    let width = rs.group(consume.out_group).cols.len();
                    let mut st = Staged::new(width);
                    let moves = move_cols(&consume.moves);
                    let mut skip = self.to_skip.unwrap();
                    while self.input.next(rs, cx)? {
                        let mut seen = 0usize;
                        rs.for_each_row(self.in_groups, |lanes| {
                            seen += 1;
                            if seen <= skip {
                                return Ok(());
                            }
                            st.push_row(|cols| {
                                for (src, c) in &moves {
                                    cols[*c as usize].push(rs.value_at(*src, lanes));
                                }
                            })
                        })?;
                        skip = skip.saturating_sub(seen);
                        cx.tick()?;
                    }
                    self.staged = Some(st);
                }
                self.staged.as_mut().unwrap().emit(rs, consume.out_group)
            }
        }
    }

    fn reset(&mut self) {
        self.to_skip = None;
        self.staged = None;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Runs the input to completion, then emits its rows.
pub struct EagerOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    in_groups: &'p [GroupId],
    out_group: GroupId,
    moves: Vec<(DataPos, u16)>,
    staged: Option<Staged>,
}

impl<'p> EagerOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
    ) -> Self {
        Self {
            input,
            in_groups,
            out_group,
            moves: move_cols(moves),
            staged: None,
        }
    }
}

impl PhysOperator for EagerOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.staged.is_none() {
            let width = rs.group(self.out_group).cols.len();
            let mut st = Staged::new(width);
            while self.input.next(rs, cx)? {
                st.push_moves(rs, self.in_groups, &self.moves)?;
                cx.tick()?;
            }
            self.staged = Some(st);
        }
        self.staged.as_mut().unwrap().emit(rs, self.out_group)
    }

    fn reset(&mut self) {
        self.staged = None;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Rows of the left then the right plan, deduplicated unless `all`.
pub struct UnionOp<'p> {
    left: Box<dyn PhysOperator + 'p>,
    right: Box<dyn PhysOperator + 'p>,
    all: bool,
    out_group: GroupId,
    moves_left: Vec<(DataPos, u16)>,
    moves_right: Vec<(DataPos, u16)>,
    left_groups: &'p [GroupId],
    right_groups: &'p [GroupId],
    staged: Option<Staged>,
    order: Vec<u32>,
}

impl<'p> UnionOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        left: Box<dyn PhysOperator + 'p>,
        right: Box<dyn PhysOperator + 'p>,
        all: bool,
        out_group: GroupId,
        moves_left: &'p [(DataPos, DataPos)],
        moves_right: &'p [(DataPos, DataPos)],
        left_groups: &'p [GroupId],
        right_groups: &'p [GroupId],
    ) -> Self {
        Self {
            left,
            right,
            all,
            out_group,
            moves_left: move_cols(moves_left),
            moves_right: move_cols(moves_right),
            left_groups,
            right_groups,
            staged: None,
            order: Vec::new(),
        }
    }
}

impl PhysOperator for UnionOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.staged.is_none() {
            let width = rs.group(self.out_group).cols.len();
            let mut st = Staged::new(width);
            // a column only one side binds is null on the other
            let filled_left: Vec<bool> = (0..width)
                .map(|c| self.moves_left.iter().any(|(_, oc)| *oc as usize == c))
                .collect();
            let filled_right: Vec<bool> = (0..width)
                .map(|c| self.moves_right.iter().any(|(_, oc)| *oc as usize == c))
                .collect();
            while self.left.next(rs, cx)? {
                let moves = &self.moves_left;
                rs.for_each_row(self.left_groups, |lanes| {
                    st.push_row(|cols| {
                        for (src, c) in moves {
                            cols[*c as usize].push(rs.value_at(*src, lanes));
                        }
                        for (c, ok) in filled_left.iter().enumerate() {
                            if !ok {
                                cols[c].push_owned(CypherValue::Null);
                            }
                        }
                    })
                })?;
                cx.tick()?;
            }
            while self.right.next(rs, cx)? {
                let moves = &self.moves_right;
                rs.for_each_row(self.right_groups, |lanes| {
                    st.push_row(|cols| {
                        for (src, c) in moves {
                            cols[*c as usize].push(rs.value_at(*src, lanes));
                        }
                        for (c, ok) in filled_right.iter().enumerate() {
                            if !ok {
                                cols[c].push_owned(CypherValue::Null);
                            }
                        }
                    })
                })?;
                cx.tick()?;
            }
            st.finish()?;
            if !self.all {
                let mut seen: FxHashSet<Vec<JoinKey>> = FxHashSet::default();
                let mut order = Vec::new();
                for row in 0..st.len() {
                    let key: Vec<JoinKey> =
                        (0..width).map(|c| join_key_ref(st.value(row, c))).collect();
                    if seen.insert(key) {
                        order.push(row as u32);
                    }
                }
                self.order = order;
            } else {
                self.order = (0..st.len() as u32).collect();
            }
            self.staged = Some(st);
        }
        let st = self.staged.as_mut().unwrap();
        st.emit_ordered(rs, self.out_group, &self.order)
    }

    fn reset(&mut self) {
        self.staged = None;
        self.order.clear();
        self.left.reset();
        self.right.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.left.seed(rows.clone());
        self.right.seed(rows);
    }
}

/// Bound-entity capture: records the entities the chunk's rows hold when a query armed capture.
pub struct CaptureOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    positions: &'p [DataPos],
    /// The groups the positions live in: a chunk with no row over them captures nothing.
    groups: Vec<GroupId>,
}

impl<'p> CaptureOp<'p> {
    pub(crate) fn new(input: Box<dyn PhysOperator + 'p>, positions: &'p [DataPos]) -> Self {
        let mut groups: Vec<GroupId> = positions.iter().map(|p| p.group).collect();
        groups.sort_unstable();
        groups.dedup();
        Self {
            input,
            positions,
            groups,
        }
    }
}

impl PhysOperator for CaptureOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if !self.input.next(rs, cx)? {
            return Ok(false);
        }
        if crate::executor::bound_capture_armed() && rs.row_count(&self.groups) > 0 {
            let mut batch: Vec<CypherValue> = Vec::new();
            for p in self.positions {
                let g = rs.group(p.group);
                if !g.active {
                    continue;
                }
                let col = &g.cols[p.col as usize];
                let lanes: Vec<usize> = match g.current_lane() {
                    Some(l) => vec![l],
                    None => g.selected_lanes().collect(),
                };
                for lane in lanes {
                    match col.get(lane) {
                        super::super::vector::ValueRef::Node(id) => {
                            batch.push(CypherValue::NodeRef(id))
                        }
                        v @ super::super::vector::ValueRef::Edge { .. } => batch.push(v.to_owned()),
                        super::super::vector::ValueRef::Any(CypherValue::Node { id, .. }) => {
                            batch.push(CypherValue::NodeRef(*id));
                        }
                        super::super::vector::ValueRef::Any(
                            v @ (CypherValue::EdgeRef { .. } | CypherValue::Edge { .. }),
                        ) => batch.push(v.clone()),
                        _ => {}
                    }
                }
            }
            crate::executor::flush_bound(batch);
        }
        Ok(true)
    }

    fn reset(&mut self) {
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// Replays chunk snapshots (every group's state) in order; the parallel scheduler feeds the
/// sequential tail of a plan from it.
pub struct ReplayOp {
    chunks: Vec<Vec<super::super::chunk::Group>>,
    pos: usize,
}

impl ReplayOp {
    pub(crate) fn new(chunks: Vec<Vec<super::super::chunk::Group>>) -> Self {
        Self { chunks, pos: 0 }
    }
}

impl PhysOperator for ReplayOp {
    fn next(&mut self, rs: &mut ResultSet, _cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.pos >= self.chunks.len() {
            return Ok(false);
        }
        rs.groups = std::mem::take(&mut self.chunks[self.pos]);
        self.pos += 1;
        Ok(true)
    }

    fn reset(&mut self) {
        self.pos = 0;
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}

/// Replays a range of chunk pairs of a materialized cartesian product, left-major: the
/// parallel scheduler's workers each take a range of the pairs.
pub struct PairReplayOp {
    product: std::sync::Arc<super::super::parallel::merge::Product>,
    range: std::ops::Range<usize>,
    pos: usize,
}

impl PairReplayOp {
    pub(crate) fn new(
        product: std::sync::Arc<super::super::parallel::merge::Product>,
        range: std::ops::Range<usize>,
    ) -> Self {
        let pos = range.start;
        Self {
            product,
            range,
            pos,
        }
    }
}

impl PhysOperator for PairReplayOp {
    fn next(&mut self, rs: &mut ResultSet, _cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.pos >= self.range.end {
            return Ok(false);
        }
        let p = &self.product;
        let rc = p.right_chunks().max(1);
        let li = self.pos / rc;
        let ri = self.pos % rc;
        let ls = li * VECTOR_CAPACITY;
        let le = (ls + VECTOR_CAPACITY).min(p.left_rows);
        let rstart = ri * VECTOR_CAPACITY;
        let rend = (rstart + VECTOR_CAPACITY).min(p.right_rows);
        let left: Vec<(u16, Vector)> = p
            .left
            .iter()
            .enumerate()
            .map(|(c, v)| (c as u16, v.slice(ls, le)))
            .collect();
        let right: Vec<(u16, Vector)> = p
            .right
            .iter()
            .enumerate()
            .map(|(c, v)| (c as u16, v.slice(rstart, rend)))
            .collect();
        install(rs, p.left_out, le - ls, left);
        install(rs, p.right_out, rend - rstart, right);
        self.pos += 1;
        Ok(true)
    }

    fn reset(&mut self) {
        self.pos = self.range.start;
    }

    fn seed(&mut self, _rows: Vec<SeedRow>) {}
}
