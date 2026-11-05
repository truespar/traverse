//! The procedure boundary: procedures take rows as frames of the row layout and evaluate their
//! arguments against one; this evaluates such an expression on the vectorized engine.

use traverse_core::tx::view::GraphView;

use crate::ast::Expr;
use crate::error::CypherError;
use crate::frame::{Frame, SymbolTable};
use crate::types::CypherValue;

use super::chunk::{DataPos, ResultSet};
use super::expr::EvalScratch;
use super::layout::{Layout, SlotType};
use super::ops::install;
use super::vector::Vector;

/// Evaluates `expr` against one frame: every symbol is a column of a one-lane group.
pub fn eval_expr(
    expr: &Expr,
    frame: &Frame,
    graph: &dyn GraphView,
    symtab: &SymbolTable,
) -> Result<CypherValue, CypherError> {
    let width = symtab.width();
    let mut layout = Layout::new();
    for slot in 0..width {
        layout.bind(
            symtab.name(slot),
            DataPos::new(0, slot as u16),
            SlotType::Any,
        );
    }
    let compiled = super::plan::compile_expr_full(expr, &layout, graph)?;
    let mut rs = ResultSet::new(&[width as u16]);
    let cols: Vec<(u16, Vector)> = (0..width)
        .map(|slot| {
            (
                slot as u16,
                Vector::from_values(vec![frame[slot].to_cypher_value()]),
            )
        })
        .collect();
    install(&mut rs, 0, 1, cols);
    rs.groups[0].flat = Some(0);
    let mut sc = EvalScratch::new();
    compiled.eval_scalar(&rs, graph, &mut sc)
}
