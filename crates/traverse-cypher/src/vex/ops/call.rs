//! `CALL proc(...) YIELD ...`: the procedure boundary. Procedures take their input as row
//! frames and give rows back; the operator converts at the edges, runs the procedure once over
//! every input row and streams its output.

use crate::ast::Expr;
use crate::error::CypherError;
use crate::frame::{Frame, SlotValue, SymbolTable, new_frame};
use crate::procedures::{ProcCall, dispatch};

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::layout::Layout;
use super::super::plan::SeedRow;
use super::{PhysOperator, Staged};

/// The procedure call operator.
pub struct CallProcedureOp<'p> {
    input: Box<dyn PhysOperator + 'p>,
    name: &'p str,
    args: Option<&'p Vec<Expr>>,
    yield_columns: &'p [(String, String)],
    in_groups: &'p [GroupId],
    out_group: GroupId,
    in_layout: &'p Layout,
    out_layout: &'p Layout,
    staged: Option<Staged>,
}

impl<'p> CallProcedureOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: Box<dyn PhysOperator + 'p>,
        name: &'p str,
        args: Option<&'p Vec<Expr>>,
        yield_columns: &'p [(String, String)],
        in_groups: &'p [GroupId],
        out_group: GroupId,
        in_layout: &'p Layout,
        out_layout: &'p Layout,
    ) -> Self {
        Self {
            input,
            name,
            args,
            yield_columns,
            in_groups,
            out_group,
            in_layout,
            out_layout,
            staged: None,
        }
    }

    /// The symbol table of the boundary: every variable after the call, in layout order.
    fn symtab(&self) -> SymbolTable {
        let mut st = SymbolTable::new();
        for (name, _, _) in self.out_layout.bindings() {
            st.add(name.to_string());
        }
        st
    }
}

impl PhysOperator for CallProcedureOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.staged.is_none() {
            let symtab = self.symtab();
            // every input row as a frame
            let in_slots: Vec<(DataPos, usize)> = self
                .in_layout
                .bindings()
                .filter_map(|(name, pos, _)| symtab.slot(name).map(|s| (pos, s)))
                .collect();
            let mut frames: Vec<Frame> = Vec::new();
            let mut charge = crate::memory::Charge::new();
            while self.input.next(rs, cx)? {
                rs.for_each_row(self.in_groups, |lanes| {
                    let mut f = new_frame(&symtab);
                    for (pos, slot) in &in_slots {
                        f[*slot] = rs.value_at(*pos, lanes).to_owned().into();
                    }
                    charge.add(48 * symtab.width().max(1))?;
                    frames.push(f);
                    Ok(())
                })?;
                cx.tick()?;
            }
            let call = ProcCall {
                name: self.name,
                name_lower: self.name.to_ascii_lowercase(),
                args: self.args,
                yield_columns: self.yield_columns,
                frames: &frames,
                symtab: &symtab,
            };
            let out = dispatch(&call, cx.graph)?;
            drop(frames);
            // the output frames as staged rows at the output positions
            let out_slots: Vec<(u16, usize)> = self
                .out_layout
                .bindings()
                .filter_map(|(name, pos, _)| symtab.slot(name).map(|s| (pos.col, s)))
                .collect();
            let width = rs.group(self.out_group).cols.len();
            let mut st = Staged::new(width);
            for mut f in out {
                st.push_row(|cols| {
                    // moved out: the frame is spent
                    for (col, slot) in &out_slots {
                        let v = std::mem::replace(&mut f[*slot], SlotValue::Null);
                        cols[*col as usize].push_owned(v.into_cypher_value());
                    }
                })?;
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
