//! What every operator runs with: the graph view, the statement's context and the per-chunk
//! bookkeeping.

use std::sync::Arc;

use traverse_core::graph::types::{EdgeTypeId, LabelId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::query::QueryContext;

/// The execution context of one pipeline run.
pub struct ExecCtx<'g> {
    /// The graph the pipeline reads and, for writes, mutates.
    pub graph: &'g mut dyn GraphView,
    /// The running statement, entered on workers that take on its work.
    pub query: Option<Arc<QueryContext>>,
    /// Chunks produced so far, for periodic interrupt checks.
    chunks: u32,
    /// Rows the nearest streaming `LIMIT` above still wants while it pulls its input (see
    /// [`row_budget`](Self::row_budget)).
    row_budget: Option<usize>,
}

impl<'g> ExecCtx<'g> {
    /// A context over `graph` for the statement current on this thread.
    pub fn new(graph: &'g mut dyn GraphView) -> Self {
        Self {
            graph,
            query: crate::query::current(),
            chunks: 0,
            row_budget: None,
        }
    }

    /// Rows the nearest streaming `LIMIT` above the running operator still wants, `None` when
    /// every row is wanted. A hint, never a cut: producers whose chunks cost work per lane
    /// (an expansion filling a chunk in place) start that small and grow, so `LIMIT k` over a
    /// deep chain does about `k` rows of work per hop instead of a full chunk per hop.
    #[inline]
    pub fn row_budget(&self) -> Option<usize> {
        self.row_budget
    }

    /// Sets the row budget for the pulls that follow; returns the previous one, for the caller
    /// to restore once its input returns.
    #[inline]
    pub fn set_row_budget(&mut self, budget: Option<usize>) -> Option<usize> {
        std::mem::replace(&mut self.row_budget, budget)
    }

    /// The graph, read-only.
    #[inline]
    pub fn view(&self) -> &dyn GraphView {
        &*self.graph
    }

    /// Counts a produced chunk and checks for an interrupt every few chunks. Chunks hold up to
    /// 2048 lanes, so this is at most one check per few thousand rows.
    #[inline]
    pub fn tick(&mut self) -> Result<(), CypherError> {
        self.chunks = self.chunks.wrapping_add(1);
        if self.chunks & 0x3 == 0 {
            crate::query::check_interrupt()?;
        }
        Ok(())
    }

    /// Checks for an interrupt and the memory ceiling now.
    #[inline]
    pub fn check(&self) -> Result<(), CypherError> {
        crate::query::check()
    }

    /// Interned id of a property key, if any entity ever carried it.
    #[inline]
    pub fn key_id(&self, key: &str) -> Option<PropertyKeyId> {
        self.graph.strings().get(key).map(PropertyKeyId)
    }

    /// Interned id of a label, if any node ever carried it.
    #[inline]
    pub fn label_id(&self, label: &str) -> Option<LabelId> {
        self.graph.strings().get(label).map(LabelId)
    }

    /// Interned id of a relationship type, if any edge ever carried it.
    #[inline]
    pub fn type_id(&self, t: &str) -> Option<EdgeTypeId> {
        self.graph.strings().get(t).map(EdgeTypeId)
    }
}

/// Wraps `&dyn GraphView` as the `&mut dyn GraphView` an [`ExecCtx`] holds, for read-only
/// nested work (subquery expressions): every read forwards, every write is refused.
pub struct ReadOnlyView<'a>(pub &'a dyn GraphView);

impl<'a> GraphView for ReadOnlyView<'a> {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        self.0.as_sync_ref()
    }

    traverse_core::forward_graph_view_reads!(self => self.0);
    traverse_core::reject_graph_view_writes!();
}

/// Wraps `&(dyn GraphView + Sync)` as the `&mut dyn GraphView` an [`ExecCtx`] holds: every read
/// forwards, every write is refused. Parallel read pipelines run their workers on one of these.
pub struct ReadOnlyGraphView<'a>(pub &'a (dyn GraphView + Sync));

impl<'a> GraphView for ReadOnlyGraphView<'a> {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        Some(self.0)
    }

    traverse_core::forward_graph_view_reads!(self => self.0);
    traverse_core::reject_graph_view_writes!();
}
