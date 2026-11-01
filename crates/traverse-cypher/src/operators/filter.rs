//! Filter operator: streaming predicate evaluation.

use traverse_core::tx::view::GraphView;

use crate::ast::Expr;
use crate::bytecode::{self, CompiledExpr};
use crate::error::CypherError;
use crate::executor::{eval_expr, is_truthy};
use crate::frame::{Frame, SymbolTable};

use super::Operator;

pub(crate) struct FilterOp<'p> {
    pub(crate) child: Box<dyn Operator + 'p>,
    pub(crate) predicate: &'p Expr,
    /// Lazily compiled bytecode; `None` = not tried yet.
    compiled: Option<Option<CompiledExpr>>,
}

impl<'p> FilterOp<'p> {
    pub(crate) fn new(child: Box<dyn Operator + 'p>, predicate: &'p Expr) -> Self {
        Self {
            child,
            predicate,
            compiled: None,
        }
    }
}

impl<'p> Operator for FilterOp<'p> {
    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        // compile on first call
        if self.compiled.is_none() {
            self.compiled = Some(bytecode::try_compile(self.predicate, symtab));
        }
        let compiled = self.compiled.as_ref().unwrap();

        while let Some(frame) = self.child.next(graph, symtab)? {
            let pass = if let Some(bc) = compiled {
                bytecode::eval_bytecode(bc, &frame, &*graph)?
            } else {
                let val = eval_expr(self.predicate, &frame, &*graph, symtab)?;
                is_truthy(&val)
            };
            if pass {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }
}
