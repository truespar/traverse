//! Aggregate operator: blocking grouping and aggregation, plus the aggregate pushdowns into the
//! scan (`scan`) and past an expand (`expand`).

mod expand;
mod scan;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::exec_aggregate;
use crate::frame::{Frame, SymbolTable};
use crate::planner::LogicalPlan;

use super::Operator;

use expand::try_expand_aggregate_pushdown;
use scan::try_aggregate_pushdown;

pub(crate) use scan::{AGG_BATCH_SIZE, AggKind, AggSpec, ScanAcc};

// -- AggregateOp ------------------------------------------------------

pub(crate) struct AggregateOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<crate::memory::RowBuffer>,
    pos: usize,
}

impl<'p> AggregateOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for AggregateOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::Aggregate { input, keys, aggs } = self.plan else {
                unreachable!();
            };
            let frames =
                if let Some(result) = try_aggregate_pushdown(input, keys, aggs, graph, symtab)? {
                    crate::memory::RowBuffer::from_vec(result)?
                } else if let Some(result) =
                    try_expand_aggregate_pushdown(input, keys, aggs, graph, symtab)?
                {
                    crate::memory::RowBuffer::from_vec(result)?
                } else if let Some(specs) = super::aggregate_stream::try_plan(aggs, symtab) {
                    // one pass over the input; memory is the group table, not the rows
                    super::aggregate_stream::exec_streaming_aggregate(
                        input, keys, &specs, graph, symtab,
                    )?
                } else {
                    // DISTINCT / percentile / aggregates nested in expressions: materialize
                    // (charged) and aggregate the slice
                    let input_frames = super::exec_via_operators(input, graph, symtab)?;
                    crate::memory::RowBuffer::from_vec(exec_aggregate(
                        &input_frames,
                        keys,
                        aggs,
                        graph,
                        symtab,
                    )?)?
                };
            self.buffer = Some(frames);
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
