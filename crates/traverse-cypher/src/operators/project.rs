//! Project operator: column projection with optional DISTINCT and WHERE.
//!
//! Without DISTINCT or WHERE it **streams**: one frame from the child,
//! projected, yielded straight away, so a LIMIT above stops early without
//! materializing the child's output.
//!
//! With DISTINCT or WHERE it blocks: drains the input, runs
//! [`exec_project_items`] (property batching, WHERE, DISTINCT), then emits
//! one row at a time.

use traverse_core::tx::view::GraphView;

use crate::ast::{Expr, ProjectionItem};
use crate::error::CypherError;
use crate::executor::{dedup_frames, eval_expr, exec_project_items, is_truthy, projection_name};
use crate::frame::{Frame, SymbolTable, new_frame};

use super::Operator;

pub(crate) struct ProjectOp<'p> {
    pub(crate) child: Box<dyn Operator + 'p>,
    pub(crate) items: &'p [ProjectionItem],
    pub(crate) distinct: bool,
    pub(crate) where_clause: Option<&'p Expr>,
    /// Precomputed (src_slot, dst_slot) pairs for streaming; `None` before the first call, `Some`
    /// if streaming is possible.
    streaming_map: Option<Vec<(usize, usize)>>,
    streaming_init: bool,
    /// Buffered results in blocking (DISTINCT / WHERE) mode.
    buffer: Option<Vec<Frame>>,
    pos: usize,
    /// Bound-entity provenance batch (executor::flush_bound on drop); bindings end here, so they're
    /// recorded here.
    bound_batch: Vec<crate::types::CypherValue>,
}

impl<'p> ProjectOp<'p> {
    pub(crate) fn new(
        child: Box<dyn Operator + 'p>,
        items: &'p [ProjectionItem],
        distinct: bool,
        where_clause: Option<&'p Expr>,
    ) -> Self {
        Self {
            child,
            items,
            distinct,
            where_clause,
            streaming_map: None,
            streaming_init: false,
            buffer: None,
            pos: 0,
            bound_batch: Vec::new(),
        }
    }

    /// Can we stream? No DISTINCT, no WHERE, every item a plain variable reference (`RETURN a, r,
    /// b`).
    fn try_init_streaming(&mut self, symtab: &SymbolTable) -> bool {
        if self.distinct || self.where_clause.is_some() {
            return false;
        }
        if self.items.is_empty() {
            // RETURN *: pure pass-through
            self.streaming_map = Some(Vec::new());
            return true;
        }
        // every item must be Expr::Variable with a resolvable slot
        let mut map = Vec::with_capacity(self.items.len());
        for item in self.items {
            if let Expr::Variable(var) = &item.expr {
                let dst_name = projection_name(item);
                if let (Some(src), Some(dst)) = (symtab.slot(var), symtab.slot(&dst_name)) {
                    map.push((src, dst));
                    continue;
                }
            }
            // not a plain variable: blocking
            return false;
        }
        self.streaming_map = Some(map);
        true
    }
}

impl Drop for ProjectOp<'_> {
    fn drop(&mut self) {
        crate::executor::flush_bound(std::mem::take(&mut self.bound_batch));
    }
}

impl<'p> Operator for ProjectOp<'p> {
    fn limit_hint(&mut self, limit: usize) {
        // no DISTINCT or WHERE: 1:1 row mapping, safe to propagate
        if !self.distinct && self.where_clause.is_none() {
            self.child.limit_hint(limit);
        }
    }

    fn next(
        &mut self,
        graph: &mut dyn GraphView,
        symtab: &SymbolTable,
    ) -> Result<Option<Frame>, CypherError> {
        // first call: streaming or blocking?
        if !self.streaming_init {
            self.streaming_init = true;
            self.try_init_streaming(symtab);
        }

        // -- streaming ---------------------------------------------
        if let Some(ref map) = self.streaming_map {
            if let Some(frame) = self.child.next(graph, symtab)? {
                if map.is_empty() {
                    // RETURN *: pass through
                    return Ok(Some(frame));
                }
                let mut nf = new_frame(symtab);
                for &(src, dst) in map {
                    nf[dst] = frame[src].clone();
                }
                if crate::executor::bound_capture_armed() {
                    crate::executor::collect_entity_slots(&frame, &mut self.bound_batch);
                    // streaming can go row-at-a-time for a long time; flush periodically so the
                    // capture fills even if an early LIMIT cancel never drops this op cleanly
                    if self.bound_batch.len() >= 4096 {
                        crate::executor::flush_bound(std::mem::take(&mut self.bound_batch));
                    }
                }
                return Ok(Some(nf));
            }
            return Ok(None);
        }

        // -- blocking (DISTINCT / WHERE / computed expressions) ----
        if self.buffer.is_none() {
            // drain the child, charged to the query memory budget as rows arrive
            let mut frames = crate::memory::RowBuffer::new();
            while let Some(frame) = self.child.next(graph, symtab)? {
                frames.push(frame)?;
            }
            if crate::executor::bound_capture_armed() {
                for f in &frames {
                    crate::executor::collect_entity_slots(f, &mut self.bound_batch);
                }
                crate::executor::flush_bound(std::mem::take(&mut self.bound_batch));
            }

            let result = if self.items.is_empty() {
                // RETURN *: all columns
                if let Some(wc) = self.where_clause {
                    let mut filtered = Vec::new();
                    for frame in &frames {
                        let val = eval_expr(wc, frame, &*graph, symtab)?;
                        if is_truthy(&val) {
                            filtered.push(frame.clone());
                        }
                    }
                    if self.distinct {
                        dedup_frames(filtered, symtab)
                    } else {
                        filtered
                    }
                } else if self.distinct {
                    dedup_frames(frames.into_vec(), symtab)
                } else {
                    frames.into_vec()
                }
            } else {
                exec_project_items(
                    &frames,
                    self.items,
                    self.distinct,
                    self.where_clause,
                    graph,
                    symtab,
                )?
            };
            self.buffer = Some(result);
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
