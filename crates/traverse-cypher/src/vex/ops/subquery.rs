//! Bodies run per outer row: `CALL { ... }`, `CALL { ... } IN TRANSACTIONS`, `FOREACH`, and the
//! shared runner OPTIONAL MATCH uses too.

use std::collections::BTreeMap;

use crate::ast::OnErrorBehavior;
use crate::error::CypherError;
use crate::types::CypherValue;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::expr::{CompiledExpr, EvalScratch};
use super::super::plan::{SeedRow, SeedSource, SubPlan};
use super::{PhysOperator, Staged, build_operator, move_cols};

/// A seeded body with its own result set, run once per seed row.
pub(crate) struct SubRun<'p> {
    op: Box<dyn PhysOperator + 'p>,
    rs: ResultSet,
    plan: &'p SubPlan,
}

impl<'p> SubRun<'p> {
    pub(crate) fn new(plan: &'p SubPlan) -> Result<Self, CypherError> {
        Ok(Self {
            op: build_operator(&plan.root)?,
            rs: ResultSet::new(&plan.widths),
            plan,
        })
    }

    /// The seed row for the outer row at `lanes`; a seed outside the outer result set (the FOREACH
    /// element) takes `extra`.
    pub(crate) fn seed_row(
        &self,
        rs: &ResultSet,
        lanes: &[usize],
        extra: Option<&CypherValue>,
    ) -> SeedRow {
        self.plan
            .seeds
            .iter()
            .map(|(src, _)| match src {
                SeedSource::Outer(p) if p.group == u16::MAX => {
                    extra.cloned().unwrap_or(CypherValue::Null)
                }
                SeedSource::Outer(p) => rs.value_at(*p, lanes).to_owned(),
                SeedSource::Local(_) => CypherValue::Null,
            })
            .collect()
    }

    /// Runs the body for `seed`, calling `f` with the values at `out` for every row.
    pub(crate) fn run<F>(
        &mut self,
        seed: SeedRow,
        cx: &mut ExecCtx<'_>,
        out: &[DataPos],
        mut f: F,
    ) -> Result<usize, CypherError>
    where
        F: FnMut(Vec<CypherValue>) -> Result<(), CypherError>,
    {
        self.op.seed(vec![seed]);
        self.op.reset();
        let mut rows = 0usize;
        while self.op.next(&mut self.rs, cx)? {
            let rs = &self.rs;
            rs.for_each_row(&self.plan.out_groups, |lanes| {
                rows += 1;
                let vals: Vec<CypherValue> = out
                    .iter()
                    .map(|p| {
                        if p.group == u16::MAX {
                            CypherValue::Null
                        } else {
                            rs.value_at(*p, lanes).to_owned()
                        }
                    })
                    .collect();
                f(vals)
            })?;
        }
        Ok(rows)
    }

    /// Runs the body for `seed`, discarding its rows.
    pub(crate) fn drain(&mut self, seed: SeedRow, cx: &mut ExecCtx<'_>) -> Result<(), CypherError> {
        self.op.seed(vec![seed]);
        self.op.reset();
        while self.op.next(&mut self.rs, cx)? {}
        Ok(())
    }
}

/// Shared shape of the per-row body operators: the input rows of one chunk at a time, each
/// producing output rows staged for emission.
struct PerRow<'p> {
    in_groups: &'p [GroupId],
    out_group: GroupId,
    moves: Vec<(DataPos, u16)>,
    staged: Option<Staged>,
}

impl<'p> PerRow<'p> {
    fn new(in_groups: &'p [GroupId], out_group: GroupId, moves: &'p [(DataPos, DataPos)]) -> Self {
        Self {
            in_groups,
            out_group,
            moves: move_cols(moves),
            staged: None,
        }
    }
}

/// `CALL { body }` per input row.
pub struct CallSubqueryOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    body: SubRun<'p>,
    body_out: &'p [(DataPos, DataPos)],
    per: PerRow<'p>,
    done: bool,
}

impl<'p> CallSubqueryOp<'p> {
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
            body_out,
            per: PerRow::new(in_groups, out_group, moves),
            done: false,
        })
    }
}

impl PhysOperator for CallSubqueryOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        loop {
            if let Some(st) = &mut self.per.staged {
                if st.emit(rs, self.per.out_group)? {
                    return Ok(true);
                }
                self.per.staged = None;
            }
            if self.done || !self.input.next(rs, cx)? {
                self.done = true;
                return Ok(false);
            }
            let width = rs.group(self.per.out_group).cols.len();
            let mut st = Staged::new(width);
            let total = rs.row_count(self.per.in_groups);
            let mut lanes = Vec::new();
            let body_pos: Vec<DataPos> = self.body_out.iter().map(|(b, _)| *b).collect();
            for row in 0..total {
                rs.row_lanes(self.per.in_groups, row, &mut lanes);
                if row & 0xFFF == 0xFFF {
                    cx.check()?;
                }
                let seed = self.body.seed_row(rs, &lanes, None);
                let outer: Vec<CypherValue> = self
                    .per
                    .moves
                    .iter()
                    .map(|(src, _)| rs.value_at(*src, &lanes).to_owned())
                    .collect();
                let moves = &self.per.moves;
                let body_out = self.body_out;
                self.body.run(seed, cx, &body_pos, |vals| {
                    st.push_row(|cols| {
                        for ((_, c), v) in moves.iter().zip(&outer) {
                            cols[*c as usize].push_owned(v.clone());
                        }
                        for ((_, out), v) in body_out.iter().zip(vals) {
                            cols[out.col as usize].push_owned(v);
                        }
                    })
                })?;
                if row & 0xFF == 0xFF {
                    cx.check()?;
                }
            }
            self.per.staged = Some(st);
        }
    }

    fn reset(&mut self) {
        self.per.staged = None;
        self.done = false;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// `CALL { body } IN TRANSACTIONS`.
pub struct CallInTransactionsOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    body: SubRun<'p>,
    batch_size: Option<&'p CompiledExpr>,
    on_error: OnErrorBehavior,
    report_status: Option<DataPos>,
    body_out: &'p [(DataPos, DataPos)],
    per: PerRow<'p>,
    scratch: EvalScratch<'p>,
}

impl<'p> CallInTransactionsOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        body: &'p SubPlan,
        batch_size: Option<&'p CompiledExpr>,
        on_error: OnErrorBehavior,
        report_status: Option<DataPos>,
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
        body_out: &'p [(DataPos, DataPos)],
    ) -> Result<Self, CypherError> {
        Ok(Self {
            input,
            body: SubRun::new(body)?,
            batch_size,
            on_error,
            report_status,
            body_out,
            per: PerRow::new(in_groups, out_group, moves),
            scratch: EvalScratch::new(),
        })
    }
}

impl PhysOperator for CallInTransactionsOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.per.staged.is_none() {
            // every outer row first (the batches commit as they go)
            let width = rs.group(self.per.out_group).cols.len();
            let mut outer_rows: Vec<(SeedRow, Vec<CypherValue>)> = Vec::new();
            while self.input.next(rs, cx)? {
                let total = rs.row_count(self.per.in_groups);
                let mut lanes = Vec::new();
                for row in 0..total {
                    rs.row_lanes(self.per.in_groups, row, &mut lanes);
                    if row & 0xFFF == 0xFFF {
                        cx.check()?;
                    }
                    let seed = self.body.seed_row(rs, &lanes, None);
                    let outer: Vec<CypherValue> = self
                        .per
                        .moves
                        .iter()
                        .map(|(src, _)| rs.value_at(*src, &lanes).to_owned())
                        .collect();
                    outer_rows.push((seed, outer));
                }
                cx.tick()?;
            }
            if cx.graph.is_explicit_transaction() {
                return Err(CypherError::semantic(
                    "CALL { ... } IN TRANSACTIONS runs only as an auto-commit statement, not \
                     inside an explicit transaction",
                )
                .with_status(crate::status::GqlStatus::ImplicitTransactionRequired));
            }
            let committing = cx.graph.can_commit_batches();
            if committing && cx.graph.write_count() > 0 {
                return Err(CypherError::semantic(
                    "CALL { ... } IN TRANSACTIONS can't follow writes in the same statement: its \
                     batches would commit them",
                ));
            }
            let outer_transaction = crate::query::with_current(|c| {
                c.and_then(|ctx| ctx.transaction())
                    .and_then(crate::transactions::display_id_of)
            });
            let batch = match self.batch_size {
                Some(e) => match e.eval_scalar(rs, cx.view(), &mut self.scratch)? {
                    CypherValue::Integer(n) if n > 0 => n as usize,
                    _ => {
                        return Err(CypherError::semantic(
                            "CALL { ... } IN TRANSACTIONS OF requires a positive integer batch size"
                                .to_string(),
                        ));
                    }
                },
                None => 1000,
            };
            let has_return = !self.body_out.is_empty();
            let mut st = Staged::new(width);
            let body_pos: Vec<DataPos> = self.body_out.iter().map(|(b, _)| *b).collect();
            let moves = &self.per.moves;
            let body_out = self.body_out;
            'batches: for (number, chunk) in outer_rows.chunks(batch).enumerate() {
                cx.check()?;
                let mut batch_error: Option<String> = None;
                let mut batch_rows: Vec<Vec<CypherValue>> = Vec::new();
                for (seed, outer) in chunk {
                    let r = self.body.run(seed.clone(), cx, &body_pos, |vals| {
                        if has_return {
                            let mut row = outer.clone();
                            row.extend(vals);
                            batch_rows.push(row);
                        }
                        Ok(())
                    });
                    match r {
                        Ok(_) => {}
                        Err(e @ CypherError::Interrupted { .. }) => return Err(e),
                        Err(e) => match self.on_error {
                            OnErrorBehavior::Fail => return Err(e),
                            OnErrorBehavior::Continue | OnErrorBehavior::Break => {
                                batch_error = Some(e.to_string());
                                break;
                            }
                        },
                    }
                }
                if batch_error.is_some() {
                    if committing {
                        cx.graph.rollback_batch()?;
                    }
                } else {
                    if committing {
                        cx.graph.commit_batch()?;
                    }
                    for row in batch_rows {
                        st.push_row(|cols| {
                            let (outer, body) = row.split_at(moves.len());
                            for ((_, c), v) in moves.iter().zip(outer) {
                                cols[*c as usize].push_owned(v.clone());
                            }
                            for ((_, out), v) in body_out.iter().zip(body) {
                                cols[out.col as usize].push_owned(v.clone());
                            }
                        })?;
                    }
                }
                if let Some(rs_pos) = self.report_status {
                    let id = match &outer_transaction {
                        Some(outer) => format!("{outer}/batch-{}", number + 1),
                        None => format!("batch-{}", number + 1),
                    };
                    let mut status = BTreeMap::new();
                    status.insert("started".to_string(), CypherValue::Bool(true));
                    status.insert(
                        "committed".to_string(),
                        CypherValue::Bool(batch_error.is_none()),
                    );
                    status.insert(
                        "errorMessage".to_string(),
                        match &batch_error {
                            Some(m) => CypherValue::String(m.clone()),
                            None => CypherValue::Null,
                        },
                    );
                    status.insert("transactionId".to_string(), CypherValue::String(id));
                    st.push_row(|cols| {
                        for c in 0..width {
                            if c == rs_pos.col as usize {
                                cols[c].push_owned(CypherValue::Map(status.clone()));
                            } else {
                                cols[c].push_owned(CypherValue::Null);
                            }
                        }
                    })?;
                }
                if batch_error.is_some() && self.on_error == OnErrorBehavior::Break {
                    break 'batches;
                }
            }
            // write-only unit subquery: the outer rows pass through, cardinality kept
            if !has_return && self.report_status.is_none() {
                st = Staged::new(width);
                for (_, outer) in &outer_rows {
                    st.push_row(|cols| {
                        for ((_, c), v) in moves.iter().zip(outer) {
                            cols[*c as usize].push_owned(v.clone());
                        }
                    })?;
                }
            }
            self.per.staged = Some(st);
        }
        self.per
            .staged
            .as_mut()
            .unwrap()
            .emit(rs, self.per.out_group)
    }

    fn reset(&mut self) {
        self.per.staged = None;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}

/// `FOREACH`: the body once per list element, input rows pass through.
pub struct ForEachOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    list: &'p CompiledExpr,
    body: SubRun<'p>,
    per: PerRow<'p>,
    scratch: EvalScratch<'p>,
}

impl<'p> ForEachOp<'p> {
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        list: &'p CompiledExpr,
        body: &'p SubPlan,
        in_groups: &'p [GroupId],
        out_group: GroupId,
        moves: &'p [(DataPos, DataPos)],
    ) -> Result<Self, CypherError> {
        Ok(Self {
            input,
            list,
            body: SubRun::new(body)?,
            per: PerRow::new(in_groups, out_group, moves),
            scratch: EvalScratch::new(),
        })
    }
}

impl PhysOperator for ForEachOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.per.staged.is_none() {
            let width = rs.group(self.per.out_group).cols.len();
            let mut st = Staged::new(width);
            // all input rows first, then the body over every iteration, clause by clause
            let mut iterations: Vec<SeedRow> = Vec::new();
            while self.input.next(rs, cx)? {
                let total = rs.row_count(self.per.in_groups);
                let mut lanes = Vec::new();
                for row in 0..total {
                    rs.row_lanes(self.per.in_groups, row, &mut lanes);
                    if row & 0xFFF == 0xFFF {
                        cx.check()?;
                    }
                    let list = self
                        .list
                        .eval_row(rs, cx.view(), &lanes, &mut self.scratch)?;
                    match list {
                        CypherValue::Null => {}
                        CypherValue::List(items) => {
                            for item in items {
                                iterations.push(self.body.seed_row(rs, &lanes, Some(&item)));
                            }
                        }
                        other => iterations.push(self.body.seed_row(rs, &lanes, Some(&other))),
                    }
                    let moves = &self.per.moves;
                    st.push_row(|cols| {
                        for (src, c) in moves {
                            cols[*c as usize].push(rs.value_at(*src, &lanes));
                        }
                    })?;
                }
                cx.tick()?;
            }
            for seed in iterations {
                self.body.drain(seed, cx)?;
            }
            self.per.staged = Some(st);
        }
        self.per
            .staged
            .as_mut()
            .unwrap()
            .emit(rs, self.per.out_group)
    }

    fn reset(&mut self) {
        self.per.staged = None;
        self.input.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.input.seed(rows);
    }
}
