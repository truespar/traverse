//! `Eager`: runs the input to completion before emitting anything. The optimizer's eagerness
//! analysis (`optimizer::eager`) puts it wherever a streaming write would otherwise be seen out of
//! clause order. The buffer is charged to the query's memory budget, so an `Eager` over too large
//! an input fails the query with the memory-limit error, not the process.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::frame::{Frame, SymbolTable};
use crate::memory::RowBuffer;
use crate::planner::LogicalPlan;

use super::Operator;

pub(crate) struct EagerOp<'p> {
    plan: &'p LogicalPlan,
    buffer: Option<RowBuffer>,
    pos: usize,
}

impl<'p> EagerOp<'p> {
    pub(crate) fn new(plan: &'p LogicalPlan) -> Self {
        Self {
            plan,
            buffer: None,
            pos: 0,
        }
    }
}

impl<'p> Operator for EagerOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        if self.buffer.is_none() {
            let LogicalPlan::Eager { input } = self.plan else {
                unreachable!();
            };
            self.buffer = Some(super::exec_via_operators(input, graph, symtab)?);
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
