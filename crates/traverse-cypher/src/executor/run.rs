//! Running a plan: materialized, lazy and streaming results.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::frame::{Frame, SlotValue, SymbolTable, build_symbol_table};
use crate::planner::LogicalPlan;
use crate::types::CypherValue;

use super::bound::{arm_bound_capture, bound_capture_armed, collect_entity_slots, flush_bound};
use super::context::{QueryStats, QueryType, end_execution, get_query_stats};
use super::describe::{
    classify_plan, classify_query, classify_query_from_hint, plan_columns, plan_has_projection,
    plan_is_mutating, plan_visible_columns,
};
use super::values::materialize_value;

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
    /// Entities BOUND in the result frames but not returned: the pattern variables
    /// behind a projection like `RETURN t.amount`. Lets visualizations highlight what
    /// the query touched instead of guessing from property values. Distinct, capped,
    /// unmaterialized NodeRef / EdgeRef values; filled by the materializing path,
    /// empty on streaming/lazy paths.
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

/// Lazy result pulling rows from the operator tree on demand. `'a` covers the `LogicalPlan` (via
/// the operators) and the `GraphView` (NodeRef/EdgeRef materialization).
pub struct CypherResultStream<'a> {
    columns: Vec<String>,
    symtab: SymbolTable,
    op: Box<dyn crate::operators::Operator + 'a>,
    graph: &'a mut dyn GraphView,
    query_type_hint: QueryType,
    plan_value: Option<CypherValue>,
    count: u32,
    /// The statement's context, entered for each pull (whichever thread it happens on).
    query: Option<std::sync::Arc<crate::query::QueryContext>>,
}

impl<'a> CypherResultStream<'a> {
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

    /// Next row from the operator tree, with NodeRef/EdgeRef materialized. `Ok(None)` when done.
    pub fn next_row(&mut self) -> Result<Option<Vec<CypherValue>>, CypherError> {
        let _query = crate::query::enter_opt(self.query.as_ref());
        let frame = self.op.next(self.graph, &self.symtab)?;
        let Some(mut frame) = frame else {
            return Ok(None);
        };

        let row: Vec<CypherValue> = self
            .columns
            .iter()
            .map(|col| {
                let sv = match self.symtab.slot(col) {
                    Some(slot) => std::mem::replace(&mut frame[slot], SlotValue::Null),
                    None => SlotValue::Null,
                };
                materialize_value(sv.to_cypher_value(), self.graph)
            })
            .collect();

        self.count = self.count.wrapping_add(1);
        if self.count & 0xFFF == 0 {
            crate::query::check_interrupt()?;
        }

        Ok(Some(row))
    }

    /// Finishes the stream and returns the footer. `&mut self` so `Drop` still runs (clearing
    /// params as a safety net); the plan is `take()`n.
    pub fn finish(&mut self) -> CypherResultFooter {
        let stats = get_query_stats();
        let query_type = classify_query_from_hint(self.query_type_hint, &stats);
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

/// Executes a logical plan against a graph.
pub fn execute_plan(
    plan: &LogicalPlan,
    graph: &mut dyn GraphView,
) -> Result<CypherResult, CypherError> {
    use crate::operators::pipeline_par::MorselOutput;

    let symtab = build_symbol_table(plan);
    let bound_guard = arm_bound_capture();

    // no projection (no RETURN): run for side effects, keep no rows, so a write over a huge input
    // or a cross product runs in constant memory
    if !plan_has_projection(plan) {
        let mut op = crate::operators::build_operator_tree(plan);
        crate::operators::drain_operator_discard(&mut *op, graph, &symtab)?;
        drop(op);
        let stats = get_query_stats();
        let query_type = classify_query(plan, &stats);
        return Ok(CypherResult {
            columns: Vec::new(),
            rows: Vec::new(),
            query_type,
            stats,
            plan: None,
            bound: bound_guard.finish(),
        });
    }

    let morsel_output =
        crate::operators::pipeline_par::try_execute_or_fallback(plan, graph, &symtab)?;
    let stats = get_query_stats();
    let query_type = classify_query(plan, &stats);

    let mut columns = plan_columns(plan);

    // RETURN *: columns from the input plan or the frames/chunks
    if columns.is_empty() {
        // first try the input plan's columns (works for all-Null results)
        if let LogicalPlan::Project { input, .. } = plan {
            columns = plan_visible_columns(input);
        }
        // fallback: scan frames for non-null slots (RETURN * without an inner projection)
        if columns.is_empty() {
            let width = symtab.width();
            let mut active = vec![false; width];
            let has_frames = match &morsel_output {
                MorselOutput::Flat(frames) => {
                    for frame in frames {
                        for i in 0..width {
                            if !frame[i].is_null() {
                                active[i] = true;
                            }
                        }
                    }
                    !frames.is_empty()
                }
                MorselOutput::Chunks(chunks) => {
                    let mut any = false;
                    for chunk in chunks {
                        for frame in chunk {
                            any = true;
                            for i in 0..width {
                                if !frame[i].is_null() {
                                    active[i] = true;
                                }
                            }
                        }
                    }
                    any
                }
                MorselOutput::Batched(_) => false,
            };
            if has_frames {
                columns = (0..width)
                    .filter(|&i| active[i])
                    .filter(|&i| !symtab.name(i).starts_with("__"))
                    .map(|i| symtab.name(i).to_string())
                    .collect();
            }
        }
    }

    // frames -> rows for Flat, Chunks and Batched. Chunks are freed as consumed (no
    // 2x peak from a full flatten); the result set is charged to the query memory
    // budget as it grows. Bound-entity provenance: scan each frame's bindings BEFORE
    // the steal, batched, one flush after the loop. Pass-through plans (RETURN *)
    // arrive here with full frames, so this covers them too.
    let mut bound_batch: Vec<CypherValue> = Vec::new();
    let mut charge = crate::memory::Charge::new();
    let mut convert_frame = |mut frame: Frame| -> Result<Vec<CypherValue>, CypherError> {
        if bound_capture_armed() {
            collect_entity_slots(&frame, &mut bound_batch);
        }
        let row: Vec<CypherValue> = columns
            .iter()
            .map(|col| {
                let sv = match symtab.slot(col) {
                    Some(slot) => std::mem::replace(&mut frame[slot], SlotValue::Null),
                    None => SlotValue::Null,
                };
                materialize_value(sv.to_cypher_value(), graph)
            })
            .collect();
        let bytes = 24
            + row.len() * std::mem::size_of::<CypherValue>()
            + row
                .iter()
                .map(crate::memory::value_heap_bytes)
                .sum::<usize>();
        charge.add(bytes)?;
        Ok(row)
    };

    let rows: Vec<Vec<CypherValue>> = match morsel_output {
        MorselOutput::Flat(frames) => frames
            .into_iter()
            .map(&mut convert_frame)
            .collect::<Result<_, _>>()?,
        MorselOutput::Chunks(chunks) => chunks
            .into_iter()
            .flat_map(|c| c.into_iter())
            .map(&mut convert_frame)
            .collect::<Result<_, _>>()?,
        MorselOutput::Batched(mut producer) => {
            // non-lazy path: run all batches and collect
            let sync_graph = graph.as_sync_ref().expect("Batched requires sync graph");
            let mut rows = Vec::new();
            while let Some(chunks) = producer.next_batch(sync_graph)? {
                for chunk in chunks {
                    for frame in chunk {
                        rows.push(convert_frame(frame)?);
                    }
                }
            }
            rows
        }
    };
    // rows now belong to the caller; the charge is released with this scope
    drop(charge);

    flush_bound(bound_batch);

    Ok(CypherResult {
        columns,
        rows,
        query_type,
        stats,
        plan: None,
        bound: bound_guard.finish(),
    })
}

// -- Lazy result (no bulk CypherValue materialization) -------------------

/// Lazy or materialized result from [`execute_plan_lazy`]. `Lazy` defers
/// `Frame -> CypherValue` conversion so callers pull rows on demand (e.g. Bolt
/// PULL) instead of allocating every row up front.
#[allow(clippy::large_enum_variant)] // one value per query; boxing would only add indirection
pub enum QueryResultLazy {
    /// Lazy rows backed by raw frames.
    Lazy(LazyResult),
    /// Fully materialized (writes, EXPLAIN, PROFILE, ...).
    Materialized(CypherResult),
}

/// Lazy rows backed by raw frames. [`next_row()`](Self::next_row) converts on demand; each morsel
/// chunk is freed once consumed, so peak memory is one chunk + output.
pub struct LazyResult {
    /// Column names.
    pub columns: Vec<String>,
    /// Query type classification.
    pub query_type: QueryType,
    /// Mutation counters.
    pub stats: QueryStats,
    /// Serialized plan (PROFILE).
    pub plan: Option<CypherValue>,
    source: LazySource,
    symtab: SymbolTable,
    total: usize,
    /// The statement's context, entered for each pull (whichever thread it happens on).
    query: Option<std::sync::Arc<crate::query::QueryContext>>,
}

#[allow(clippy::large_enum_variant)] // one value per query; boxing would only add indirection
enum LazySource {
    /// Morsel chunks, each freed after consumption.
    Chunks {
        chunks: Vec<Vec<Frame>>,
        current: std::vec::IntoIter<Frame>,
        next_chunk: usize,
    },
    /// Flat frame list.
    Flat { iter: std::vec::IntoIter<Frame> },
    /// Runs morsels on demand, one batch at a time.
    BatchedMorsels {
        producer: crate::operators::pipeline_par::MorselProducer,
        current_chunks: Vec<Vec<Frame>>,
        chunk_idx: usize,
        current_iter: std::vec::IntoIter<Frame>,
    },
}

impl LazySource {
    fn next_frame(
        &mut self,
        sync_graph: Option<&(dyn GraphView + Sync)>,
    ) -> Result<Option<Frame>, CypherError> {
        match self {
            LazySource::Chunks {
                chunks,
                current,
                next_chunk,
            } => loop {
                if let Some(frame) = current.next() {
                    return Ok(Some(frame));
                }
                if *next_chunk >= chunks.len() {
                    return Ok(None);
                }
                let chunk = std::mem::take(&mut chunks[*next_chunk]);
                *current = chunk.into_iter();
                *next_chunk += 1;
            },
            LazySource::Flat { iter } => Ok(iter.next()),
            LazySource::BatchedMorsels {
                producer,
                current_chunks,
                chunk_idx,
                current_iter,
            } => loop {
                if let Some(frame) = current_iter.next() {
                    return Ok(Some(frame));
                }
                *chunk_idx += 1;
                if *chunk_idx < current_chunks.len() {
                    let chunk = std::mem::take(&mut current_chunks[*chunk_idx]);
                    *current_iter = chunk.into_iter();
                    continue;
                }
                // batch exhausted: run the next one
                let graph = sync_graph.expect("BatchedMorsels requires sync graph");
                match producer.next_batch(graph)? {
                    Some(new_chunks) if !new_chunks.is_empty() => {
                        *current_chunks = new_chunks;
                        *chunk_idx = 0;
                        let chunk = std::mem::take(&mut current_chunks[0]);
                        *current_iter = chunk.into_iter();
                    }
                    _ => return Ok(None),
                }
            },
        }
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

    /// Next row, resolving NodeRef/EdgeRef through `graph`. `Ok(None)` when done.
    pub fn next_row(
        &mut self,
        graph: &dyn GraphView,
    ) -> Result<Option<Vec<CypherValue>>, CypherError> {
        let _query = crate::query::enter_opt(self.query.as_ref());
        let sync_graph = graph.as_sync_ref();
        let mut frame = match self.source.next_frame(sync_graph)? {
            Some(f) => f,
            None => return Ok(None),
        };
        let row = self
            .columns
            .iter()
            .map(|col| {
                let sv = match self.symtab.slot(col) {
                    Some(slot) => std::mem::replace(&mut frame[slot], SlotValue::Null),
                    None => SlotValue::Null,
                };
                materialize_value(sv.to_cypher_value(), graph)
            })
            .collect();
        Ok(Some(row))
    }

    /// Column -> slot index (None = always Null).
    pub fn column_slot_indices(&self) -> Vec<Option<usize>> {
        self.columns
            .iter()
            .map(|col| self.symtab.slot(col))
            .collect()
    }

    /// Next raw frame, without converting to `Vec<CypherValue>`.
    pub fn next_frame(&mut self, graph: &dyn GraphView) -> Result<Option<Frame>, CypherError> {
        let _query = crate::query::enter_opt(self.query.as_ref());
        let sync_graph = graph.as_sync_ref();
        self.source.next_frame(sync_graph)
    }

    /// Materializes the remaining rows (fallback for partial PULLs).
    pub fn into_materialized(mut self, graph: &dyn GraphView) -> Result<CypherResult, CypherError> {
        let query = self.query.clone();
        let _query = crate::query::enter_opt(query.as_ref());
        let cap = if self.total < 10_000_000 {
            self.total
        } else {
            0
        };
        let mut rows = Vec::with_capacity(cap);
        let mut n = 0u64;
        while let Some(row) = self.next_row(graph)? {
            rows.push(row);
            n += 1;
            // bound the buffer: a bounded PULL over a big streaming result lands here (total
            // unknown for Batched sources), so check periodically; check_query_limits
            // enforces the deadline and memory ceiling before the process OOMs
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

/// Executes a plan, lazily when possible: no bulk `Vec<Vec<CypherValue>>`; the
/// caller pulls rows via [`LazyResult::next_row()`] with a graph view for
/// NodeRef/EdgeRef materialization.
pub fn execute_plan_lazy(
    plan: &LogicalPlan,
    graph: &mut dyn GraphView,
) -> Result<QueryResultLazy, CypherError> {
    use crate::operators::pipeline_par::MorselOutput;

    // deep plans: rows are pulled after return on the caller's stack, so materialize on a dedicated
    // stack instead (see `stack`)
    let depth = crate::stack::check_plan_depth(plan)?;
    if depth > crate::stack::DEEP_PLAN_DEPTH {
        return Ok(QueryResultLazy::Materialized(
            crate::stack::with_stack_for_depth(depth, || execute_plan(plan, graph))?,
        ));
    }

    // no projection (no RETURN): side effects only, no rows kept
    if !plan_has_projection(plan) {
        return Ok(QueryResultLazy::Materialized(execute_plan(plan, graph)?));
    }

    let symtab = build_symbol_table(plan);
    let morsel_output =
        crate::operators::pipeline_par::try_execute_or_fallback(plan, graph, &symtab)?;
    let stats = get_query_stats();
    let query_type = classify_query(plan, &stats);

    let mut columns = plan_columns(plan);
    if columns.is_empty() {
        if let LogicalPlan::Project { input, .. } = plan {
            columns = plan_visible_columns(input);
        }
        // RETURN * fallback: scan frames by reference for active slots; for Batched, run the first
        // batch to find columns
        if columns.is_empty() {
            let width = symtab.width();
            let mut active = vec![false; width];
            let has_frames = match &morsel_output {
                MorselOutput::Flat(frames) => {
                    for frame in frames {
                        for i in 0..width {
                            if !frame[i].is_null() {
                                active[i] = true;
                            }
                        }
                    }
                    !frames.is_empty()
                }
                MorselOutput::Chunks(chunks) => {
                    let mut any = false;
                    for chunk in chunks {
                        for frame in chunk {
                            any = true;
                            for i in 0..width {
                                if !frame[i].is_null() {
                                    active[i] = true;
                                }
                            }
                        }
                    }
                    any
                }
                MorselOutput::Batched(_) => {
                    // can't peek without consuming: columns come from
                    // plan_columns/plan_visible_columns; if still empty, the streaming loop yields
                    // rows anyway
                    false
                }
            };
            if has_frames {
                columns = (0..width)
                    .filter(|&i| active[i])
                    .filter(|&i| !symtab.name(i).starts_with("__"))
                    .map(|i| symtab.name(i).to_string())
                    .collect();
            }
        }
    }

    // lazy source from the morsel output
    let (source, total) = match morsel_output {
        MorselOutput::Flat(frames) => {
            let total = frames.len();
            (
                LazySource::Flat {
                    iter: frames.into_iter(),
                },
                total,
            )
        }
        MorselOutput::Chunks(chunks) => {
            let total: usize = chunks.iter().map(|c| c.len()).sum();
            (
                LazySource::Chunks {
                    chunks,
                    current: Vec::new().into_iter(),
                    next_chunk: 0,
                },
                total,
            )
        }
        MorselOutput::Batched(producer) => {
            // total unknown until all batches ran: 0 as sentinel
            (
                LazySource::BatchedMorsels {
                    producer,
                    current_chunks: Vec::new(),
                    chunk_idx: 0,
                    current_iter: Vec::new().into_iter(),
                },
                0,
            )
        }
    };

    Ok(QueryResultLazy::Lazy(LazyResult {
        columns,
        query_type,
        stats,
        plan: None,
        source,
        symtab,
        total,
        query: crate::query::current(),
    }))
}

/// Executes a plan in streaming (row at a time) mode where possible. Falls back
/// to `Materialized` when the plan has no projection (no RETURN), would use morsel
/// parallelism, or its columns can't be determined statically (rare RETURN *).
pub fn execute_plan_streaming<'a>(
    plan: &'a LogicalPlan,
    graph: &'a mut dyn GraphView,
) -> Result<QueryExecution<'a>, CypherError> {
    // deep plans: the per-row pull chain recurses once per level on the consumer's stack, so
    // materialize on a dedicated stack (see `stack`)
    let depth = crate::stack::check_plan_depth(plan)?;
    if depth > crate::stack::DEEP_PLAN_DEPTH {
        return Ok(QueryExecution::Materialized(
            crate::stack::with_stack_for_depth(depth, || execute_plan(plan, graph))?,
        ));
    }

    // no projection: materialize (no rows, but stats are needed)
    if !plan_has_projection(plan) {
        return Ok(QueryExecution::Materialized(execute_plan(plan, graph)?));
    }

    // writes stream through the operators, so a mutating statement must run to completion however
    // many rows the client pulls (it may DISCARD after one): materialize
    if plan_is_mutating(plan) {
        return Ok(QueryExecution::Materialized(execute_plan(plan, graph)?));
    }

    // morsel parallelism: materialize (keeps parallel performance)
    if crate::operators::pipeline_par::would_parallelize(plan, graph) {
        return Ok(QueryExecution::Materialized(execute_plan(plan, graph)?));
    }

    let mut columns = plan_columns(plan);
    if columns.is_empty()
        && let LogicalPlan::Project { input, .. } = plan
    {
        columns = plan_visible_columns(input);
    }
    // columns unknown statically: fall back
    if columns.is_empty() {
        return Ok(QueryExecution::Materialized(execute_plan(plan, graph)?));
    }

    let symtab = build_symbol_table(plan);
    let op = crate::operators::build_operator_tree(plan);
    let query_type_hint = classify_plan(plan);

    Ok(QueryExecution::Streaming(CypherResultStream {
        columns,
        symtab,
        op,
        graph,
        query_type_hint,
        plan_value: None,
        count: 0,
        query: crate::query::current(),
    }))
}
