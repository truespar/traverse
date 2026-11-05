//! Running a compiled plan: materialized results, lazy results (rows read on demand from the
//! engine's typed chunks), and streams (rows pulled from the operators one at a time).

use std::sync::Arc;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::memory::Charge;
use crate::planner::LogicalPlan;
use crate::query::QueryContext;
use crate::types::CypherValue;
use crate::vex::exec::{self, RowChunk, Stream};
use crate::vex::plan::PhysicalPlan;
use crate::vex::vector::ValueRef;

use super::bound::arm_bound_capture;
use super::context::{QueryStats, QueryType, end_execution, get_query_stats};

/// Result of a Cypher query.
#[derive(Debug)]
pub struct CypherResult {
    /// Column names.
    pub columns: Vec<String>,
    /// Result rows.
    pub rows: Vec<Vec<CypherValue>>,
    /// Query type classification.
    pub query_type: QueryType,
    /// Mutation counters.
    pub stats: QueryStats,
    /// Serialized query plan (EXPLAIN / PROFILE).
    pub plan: Option<CypherValue>,
    /// Entities the patterns bound but the result doesn't return: the variables behind a
    /// projection like `RETURN t.amount`, so a visualization can show what the query touched.
    /// Distinct, capped, unmaterialized node and relationship references; filled by the
    /// materializing path, empty on the lazy and streaming paths.
    pub bound: Vec<CypherValue>,
}

/// Metadata available once a streaming result is fully consumed.
pub struct CypherResultFooter {
    /// Query type, refined with runtime stats.
    pub query_type: QueryType,
    /// Mutation counters accumulated while streaming.
    pub stats: QueryStats,
    /// Serialized query plan (EXPLAIN / PROFILE).
    pub plan: Option<CypherValue>,
}

/// Query type of a compiled plan given the mutations observed so far.
pub fn classify(plan: &PhysicalPlan, stats: &QueryStats) -> QueryType {
    if plan.schema {
        return QueryType::Schema;
    }
    let mutating = plan.mutating || !stats.is_empty();
    match (mutating, plan.has_projection) {
        (true, true) => QueryType::ReadWrite,
        (true, false) => QueryType::Write,
        _ => QueryType::Read,
    }
}

/// The plan as a nested map of operators, for EXPLAIN and PROFILE.
pub fn plan_to_cypher_value(plan: &PhysicalPlan) -> CypherValue {
    crate::vex::plan::explain::plan_to_value(plan)
}

fn column_names(plan: &PhysicalPlan) -> Vec<String> {
    plan.columns.iter().map(|(n, _)| n.clone()).collect()
}

/// Compiles and executes a logical plan against a graph.
pub fn execute_plan(
    plan: &LogicalPlan,
    graph: &mut dyn GraphView,
) -> Result<CypherResult, CypherError> {
    let physical = crate::vex::plan::compile(plan, &*graph)?;
    execute_physical(&physical, graph)
}

/// Executes a compiled plan, materializing every row.
pub fn execute_physical(
    plan: &PhysicalPlan,
    graph: &mut dyn GraphView,
) -> Result<CypherResult, CypherError> {
    let bound_guard = arm_bound_capture();
    let (chunks, charge) = exec::run_chunks(plan, graph)?;
    let stats = get_query_stats();
    let query_type = classify(plan, &stats);
    let rows = exec::materialize_rows(chunks, &*graph)?;
    // the rows now belong to the caller
    drop(charge);
    Ok(CypherResult {
        columns: column_names(plan),
        rows,
        query_type,
        stats,
        plan: None,
        bound: bound_guard.finish(),
    })
}

/// Lazy result pulling rows from the operator tree on demand. `'a` covers the plan (through the
/// operators) and the graph view.
pub struct CypherResultStream<'a> {
    columns: Vec<String>,
    stream: Stream<'a>,
    query_type_hint: QueryType,
    plan_value: Option<CypherValue>,
    count: u32,
    /// The statement's context, entered for each pull (whichever thread it happens on).
    query: Option<Arc<QueryContext>>,
}

impl CypherResultStream<'_> {
    /// Output column names.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Makes the statement current on this thread for the scope's life, so a loop pulling many
    /// rows enters it once. Pulls work without it too.
    #[must_use = "the statement stops being current when this value is dropped"]
    pub fn enter(&self) -> crate::query::QueryScope {
        crate::query::enter_opt(self.query.as_ref())
    }

    /// The next row with references resolved; `Ok(None)` when done.
    pub fn next_row(&mut self) -> Result<Option<Vec<CypherValue>>, CypherError> {
        let _query = crate::query::enter_opt(self.query.as_ref());
        let row = self.stream.next_row()?;
        if row.is_some() {
            self.count = self.count.wrapping_add(1);
            if self.count & 0xFFF == 0 {
                crate::query::check_interrupt()?;
            }
        }
        Ok(row)
    }

    /// Finishes the stream and returns the footer. `&mut self` so `Drop` still runs; the plan is
    /// taken.
    pub fn finish(&mut self) -> CypherResultFooter {
        let stats = get_query_stats();
        let query_type = if self.query_type_hint == QueryType::Read && !stats.is_empty() {
            QueryType::ReadWrite
        } else {
            self.query_type_hint
        };
        end_execution();
        CypherResultFooter {
            query_type,
            stats,
            plan: self.plan_value.take(),
        }
    }
}

impl Drop for CypherResultStream<'_> {
    fn drop(&mut self) {
        // safety net when finish() wasn't called
        end_execution();
    }
}

/// Streaming or materialized query result.
pub enum QueryExecution<'a> {
    /// Lazy, row at a time.
    Streaming(CypherResultStream<'a>),
    /// Fully materialized.
    Materialized(CypherResult),
}

/// Executes a compiled plan in streaming (row at a time) mode where possible. A plan without a
/// projection, a writing plan (it runs to completion however many rows the client takes), a
/// parallel plan (its workers run to completion anyway) or a very deep plan materializes.
pub fn execute_physical_streaming<'a>(
    plan: &'a PhysicalPlan,
    graph: &'a mut dyn GraphView,
) -> Result<QueryExecution<'a>, CypherError> {
    if plan.depth > crate::stack::DEEP_PLAN_DEPTH {
        return Ok(QueryExecution::Materialized(
            crate::stack::with_stack_for_depth(plan.depth, || execute_physical(plan, graph))?,
        ));
    }
    if !plan.has_projection || plan.mutating || crate::vex::parallel::eligible(plan, &*graph) {
        return Ok(QueryExecution::Materialized(execute_physical(plan, graph)?));
    }
    let query_type_hint = classify(plan, &QueryStats::default());
    let columns = column_names(plan);
    let stream = Stream::new(plan, graph)?;
    Ok(QueryExecution::Streaming(CypherResultStream {
        columns,
        stream,
        query_type_hint,
        plan_value: None,
        count: 0,
        query: crate::query::current(),
    }))
}

/// Lazy or materialized result from [`execute_physical_lazy`]: `Lazy` keeps the engine's typed
/// chunks and converts rows as the caller reads them.
#[allow(clippy::large_enum_variant)] // one value per query; boxing would only add indirection
pub enum QueryResultLazy {
    /// Rows read on demand.
    Lazy(LazyResult),
    /// Fully materialized (writes, EXPLAIN, PROFILE, ...).
    Materialized(CypherResult),
}

/// Rows kept as typed column chunks; [`next_row`](Self::next_row) converts on demand, and
/// [`next_lanes`](Self::next_lanes) hands out the typed lanes for encoders that need no
/// conversion. A chunk is freed once read, so peak memory is the chunks plus the output.
pub struct LazyResult {
    /// Column names.
    pub columns: Vec<String>,
    /// Query type classification.
    pub query_type: QueryType,
    /// Mutation counters.
    pub stats: QueryStats,
    /// Serialized plan (PROFILE).
    pub plan: Option<CypherValue>,
    chunks: Vec<RowChunk>,
    chunk: usize,
    row: usize,
    total: usize,
    /// The statement's context, entered for each pull (whichever thread it happens on).
    query: Option<Arc<QueryContext>>,
    /// The chunks' memory, charged to the query until the result drops.
    _charge: Charge,
}

/// One row of a [`LazyResult`] as typed lanes.
pub struct LaneRow<'a> {
    chunk: &'a RowChunk,
    row: usize,
}

impl<'a> LaneRow<'a> {
    /// Columns.
    pub fn len(&self) -> usize {
        self.chunk.cols.len()
    }

    /// No columns?
    pub fn is_empty(&self) -> bool {
        self.chunk.cols.is_empty()
    }

    /// The lane at column `col`.
    #[inline]
    pub fn get(&self, col: usize) -> ValueRef<'a> {
        self.chunk.get(self.row, col)
    }
}

impl LazyResult {
    /// Makes the statement current on this thread for the scope's life, so a loop pulling many
    /// rows enters it once. Pulls work without it too.
    #[must_use = "the statement stops being current when this value is dropped"]
    pub fn enter(&self) -> crate::query::QueryScope {
        crate::query::enter_opt(self.query.as_ref())
    }

    /// Total rows.
    pub fn total(&self) -> usize {
        self.total
    }

    /// The next row's typed lanes; `None` when done.
    pub fn next_lanes(&mut self) -> Option<LaneRow<'_>> {
        loop {
            let len = self.chunks.get(self.chunk)?.len;
            if self.row < len {
                let row = self.row;
                self.row += 1;
                return Some(LaneRow {
                    chunk: &self.chunks[self.chunk],
                    row,
                });
            }
            // the chunk is read: free it
            self.chunks[self.chunk] = RowChunk {
                cols: Vec::new(),
                len: 0,
            };
            self.chunk += 1;
            self.row = 0;
        }
    }

    /// Next row, resolving references through `graph`. `Ok(None)` when done.
    pub fn next_row(
        &mut self,
        graph: &dyn GraphView,
    ) -> Result<Option<Vec<CypherValue>>, CypherError> {
        let _query = crate::query::enter_opt(self.query.as_ref());
        let Some(lanes) = self.next_lanes() else {
            return Ok(None);
        };
        Ok(Some(
            (0..lanes.len())
                .map(|c| exec::row_value(lanes.get(c), graph))
                .collect(),
        ))
    }

    /// Materializes the remaining rows (fallback for partial PULLs).
    pub fn into_materialized(mut self, graph: &dyn GraphView) -> Result<CypherResult, CypherError> {
        let query = self.query.clone();
        let _query = crate::query::enter_opt(query.as_ref());
        let mut rows = Vec::with_capacity(self.total);
        let mut n = 0u64;
        while let Some(row) = self.next_row(graph)? {
            rows.push(row);
            n += 1;
            if n & 0xFFF == 0 {
                crate::query::check()?;
            }
        }
        Ok(CypherResult {
            columns: self.columns,
            rows,
            query_type: self.query_type,
            stats: self.stats,
            plan: self.plan,
            bound: Vec::new(),
        })
    }
}

/// Executes a compiled plan, keeping its rows as typed chunks for the caller to read on demand.
/// A plan without a projection or a very deep plan materializes.
pub fn execute_physical_lazy(
    plan: &PhysicalPlan,
    graph: &mut dyn GraphView,
) -> Result<QueryResultLazy, CypherError> {
    if plan.depth > crate::stack::DEEP_PLAN_DEPTH {
        return Ok(QueryResultLazy::Materialized(
            crate::stack::with_stack_for_depth(plan.depth, || execute_physical(plan, graph))?,
        ));
    }
    if !plan.has_projection {
        return Ok(QueryResultLazy::Materialized(execute_physical(
            plan, graph,
        )?));
    }
    let (chunks, charge) = exec::run_chunks(plan, graph)?;
    let stats = get_query_stats();
    let query_type = classify(plan, &stats);
    let total = chunks.iter().map(|c| c.len).sum();
    Ok(QueryResultLazy::Lazy(LazyResult {
        columns: column_names(plan),
        query_type,
        stats,
        plan: None,
        chunks,
        chunk: 0,
        row: 0,
        total,
        query: crate::query::current(),
        _charge: charge,
    }))
}
