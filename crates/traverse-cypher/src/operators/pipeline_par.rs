//! Morsel-driven pipeline parallelism.
//!
//! Read-only linear-chain plans with one leaf Scan over >= 10K nodes: node IDs
//! are split into morsels, each running the whole operator chain on a rayon
//! worker. Sort, DISTINCT and LIMIT are re-applied across the merged results.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use traverse_core::graph::types::NodeId;
use traverse_core::tx::view::GraphView;

use crate::ast::Expr;
use crate::error::CypherError;
use crate::frame::{Frame, SymbolTable};
use crate::planner::LogicalPlan;

/// Morsel pipeline result: flattened frames (post-processing needed) or raw chunks (no flatten,
/// less memory).
#[allow(clippy::large_enum_variant)] // one value per query; boxing would only add indirection
pub(crate) enum MorselOutput {
    /// Flattened (sort/distinct/limit needed a merge).
    Flat(Vec<Frame>),
    /// Raw morsel chunks, no post-processing; caller iterates lazily.
    Chunks(Vec<Vec<Frame>>),
    /// Batched producer: caller pulls one batch at a time, bounding memory.
    Batched(MorselProducer),
}

/// Active `MorselProducer`s. At `num_threads` every core is busy with concurrent
/// queries and intra-query rayon parallelism only adds convoy overhead, so
/// `next_batch()` goes sequential.
static ACTIVE_BATCH_PRODUCERS: AtomicUsize = AtomicUsize::new(0);

/// Concurrent `exec_morsel_pipeline` calls (all-at-once path for
/// DISTINCT/SORT/LIMIT). At `num_threads` each query already owns a core and
/// `par_chunks` would oversubscribe the pool, so fall back to sequential.
static ACTIVE_MORSEL_PIPELINES: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct MorselProducer {
    plan: LogicalPlan,
    node_ids: Vec<NodeId>,
    symtab: SymbolTable,
    morsel_size: usize,
    batch_offset: usize,
    limit_counter: Option<Arc<AtomicUsize>>,
    /// This producer incremented ACTIVE_BATCH_PRODUCERS.
    tracked: bool,
}

impl Drop for MorselProducer {
    fn drop(&mut self) {
        if self.tracked {
            ACTIVE_BATCH_PRODUCERS.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl MorselProducer {
    pub(crate) fn next_batch(
        &mut self,
        sync_graph: &(dyn GraphView + Sync),
    ) -> Result<Option<Vec<Vec<Frame>>>, CypherError> {
        if self.batch_offset >= self.node_ids.len() {
            return Ok(None);
        }
        let num_threads = rayon::current_num_threads();
        let end = self
            .node_ids
            .len()
            .min(self.batch_offset + self.morsel_size * num_threads);
        let batch = &self.node_ids[self.batch_offset..end];
        self.batch_offset = end;

        let limit_counter = &self.limit_counter;
        let plan = &self.plan;
        let symtab = &self.symtab;
        let morsel_size = self.morsel_size;

        // with all cores saturated by concurrent queries, intra-query rayon causes convoys
        // (par_chunks().collect() waits for ALL morsels on a shared pool); sequential lets each
        // Bolt handler thread work independently
        let active = ACTIVE_BATCH_PRODUCERS.load(Ordering::Relaxed);
        if active >= num_threads {
            // sequential: each morsel on the caller's Bolt handler thread
            let chunks: Result<Vec<Vec<Frame>>, CypherError> = batch
                .chunks(morsel_size)
                .map(|morsel| {
                    if let Some(ctr) = limit_counter
                        && ctr.load(Ordering::Relaxed) == 0
                    {
                        return Ok(Vec::new());
                    }
                    crate::query::check_interrupt()?;
                    set_morsel_ids(morsel.to_vec());
                    let mut op = super::build_operator_tree(plan);
                    let mut adapter = ReadOnlyGraphView(sync_graph);
                    let result = super::drain_operator(&mut *op, &mut adapter, symtab)
                        .map(crate::memory::RowBuffer::into_vec);
                    if let Some(ctr) = limit_counter
                        && let Ok(ref frames) = result
                    {
                        ctr.fetch_sub(
                            frames.len().min(ctr.load(Ordering::Relaxed)),
                            Ordering::Relaxed,
                        );
                    }
                    let _ = take_morsel_ids();
                    result
                })
                .collect();
            return Ok(Some(chunks?));
        }

        // parallel: rayon work-stealing
        use rayon::prelude::*;

        // the running query (parameters, clock, deadline, stop flag, memory account) is the caller's
        // thread-local (the Bolt handler during PULL), so hand it to the workers
        let query = crate::query::current();
        // bound-entity capture handle, propagated the same way; a worker without it collects
        // nothing
        let bound_handle = crate::executor::clone_bound_handle();

        let chunks: Result<Vec<Vec<Frame>>, CypherError> = batch
            .par_chunks(morsel_size)
            .map(|morsel| {
                let _query = crate::query::enter_opt(query.as_ref());
                crate::executor::set_bound_handle(bound_handle.clone());
                if let Some(ctr) = limit_counter
                    && ctr.load(Ordering::Relaxed) == 0
                {
                    return Ok(Vec::new());
                }
                crate::query::check_interrupt()?;
                set_morsel_ids(morsel.to_vec());
                let mut op = super::build_operator_tree(plan);
                let mut adapter = ReadOnlyGraphView(sync_graph);
                let result = super::drain_operator(&mut *op, &mut adapter, symtab)
                    .map(crate::memory::RowBuffer::into_vec);
                if let Some(ctr) = limit_counter
                    && let Ok(ref frames) = result
                {
                    ctr.fetch_sub(
                        frames.len().min(ctr.load(Ordering::Relaxed)),
                        Ordering::Relaxed,
                    );
                }
                let _ = take_morsel_ids();
                // rayon threads are pooled: a stale handle would leak this query's capture into the
                // next job
                crate::executor::set_bound_handle(None);
                result
            })
            .collect();

        Ok(Some(chunks?))
    }
}

/// Base morsel size for 0-1 hop queries (L2-friendly).
const MORSEL_BASE: usize = 65536;

/// Morsel size for batched streaming. Bigger morsels cut per-batch overhead
/// (operator tree, TLS, rayon scheduling) at the cost of peak memory per batch.
const STREAMING_MORSEL_BASE: usize = 2048;

/// Min node count for morsel parallelism (single-hop).
const MORSEL_MIN_NODES: usize = 10000;

/// Lower threshold for 2+ expand hops: fan-out from a few start nodes already yields big
/// intermediate results.
const MORSEL_MIN_NODES_MULTIHOP: usize = 32;

/// Try-lock for small morsel pipelines (node_ids < MORSEL_MIN_NODES): one at a time, so they don't
/// fight over rayon at high concurrency. Large ones always run.
static SMALL_MORSEL_ACTIVE: AtomicBool = AtomicBool::new(false);

// -- Thread-local morsel override --------------------------------------

thread_local! {
    static MORSEL_OVERRIDE: RefCell<Option<Vec<NodeId>>> = const { RefCell::new(None) };
}

fn set_morsel_ids(ids: Vec<NodeId>) {
    MORSEL_OVERRIDE.with(|m| *m.borrow_mut() = Some(ids));
}

pub(crate) fn take_morsel_ids() -> Option<Vec<NodeId>> {
    MORSEL_OVERRIDE.with(|m| m.borrow_mut().take())
}

// -- Pipeline detection ------------------------------------------------

enum MorselScanKind {
    /// Full label scan (all nodes if label is None).
    LabelScan { label: Option<String> },
    /// Index range scan; matching node IDs computed at dispatch.
    IndexRangeScan {
        label: String,
        property: String,
        op: crate::ast::CmpOp,
        value: Expr,
    },
}

struct MorselScanInfo {
    kind: MorselScanKind,
    has_sort: bool,
    has_distinct: bool,
}

/// `Some(info)` if the plan is a morselizable linear chain: one leaf Scan
/// feeding single-input operators, no writes, aggregates, multi-scan joins,
/// ShortestPath or CallProcedure.
///
/// Plain multi-hop Expand chains (2+ hops without DistinctExpand) are excluded:
/// memory-bandwidth bound, they get slower in parallel (DRAM saturation + merge
/// cost). Single-hop Expand and DistinctExpand (compute-heavy HashSet dedup) gain.
#[inline(never)]
fn detect_morsel_scan(plan: &LogicalPlan) -> Option<MorselScanInfo> {
    let mut current = plan;
    let mut has_sort = false;
    let mut has_distinct = false;
    let mut plain_expand_hops: usize = 0;

    loop {
        match current {
            // single-input operators we can morsel through
            LogicalPlan::Filter { input, .. } => {
                current = input;
            }
            LogicalPlan::Project {
                input, distinct, ..
            } => {
                if *distinct {
                    has_distinct = true;
                }
                current = input;
            }
            LogicalPlan::Sort { input, .. } => {
                has_sort = true;
                current = input;
            }
            LogicalPlan::Limit { input, .. } => {
                current = input;
            }
            // no Skip: per-morsel Skip skips N per morsel, not N globally
            LogicalPlan::Skip { .. } => return None,

            LogicalPlan::ExpandOut { input, .. }
            | LogicalPlan::ExpandIn { input, .. }
            | LogicalPlan::ExpandBoth { input, .. } => {
                plain_expand_hops += 1;
                current = input;
            }
            LogicalPlan::DistinctExpand { input, .. } => {
                current = input;
            }
            LogicalPlan::Unwind { input, .. } | LogicalPlan::LoadCsv { input, .. } => {
                current = input;
            }
            LogicalPlan::BindPath { input, .. }
            | LogicalPlan::BindPathEndpoints { input, .. }
            | LogicalPlan::BindEdgeEndpoints { input, .. } => {
                current = input;
            }

            LogicalPlan::Scan { label, .. } => {
                // plain multi-hop expand chains are memory-bandwidth bound (DRAM saturation + merge
                // cost beat parallelism): reject unless DISTINCT, where exec_morsel_pipeline
                // re-dedups globally and per-morsel expand + dedup gains. DistinctExpand (HashSet
                // BFS) is compute-bound and always allowed
                if plain_expand_hops >= 2 && !has_distinct {
                    return None;
                }
                return Some(MorselScanInfo {
                    kind: MorselScanKind::LabelScan {
                        label: label.clone(),
                    },
                    has_sort,
                    has_distinct,
                });
            }

            LogicalPlan::IndexRangeScan {
                label,
                property,
                op,
                value,
                ..
            } => {
                if plain_expand_hops >= 2 && !has_distinct {
                    return None;
                }
                return Some(MorselScanInfo {
                    kind: MorselScanKind::IndexRangeScan {
                        label: label.clone(),
                        property: property.clone(),
                        op: *op,
                        value: value.clone(),
                    },
                    has_sort,
                    has_distinct,
                });
            }

            // everything else isn't morselizable (Aggregate, ScanFilterProject, equality IndexScan,
            // writes, multi-input, ...)
            _ => return None,
        }
    }
}

// -- Adaptive morsel sizing -------------------------------------------

/// Morsel size by expansion depth: deep expansions blow up per source node, so smaller morsels
/// bound peak memory.
fn adaptive_morsel_base(plan: &LogicalPlan) -> usize {
    let hops = max_expand_hops(plan);
    match hops {
        0..=1 => MORSEL_BASE,
        2 => 16384,
        3 => 4096,
        _ => 2048,
    }
}

/// Total expansion depth (DistinctExpand hops + single-hop Expands).
#[recursive::recursive]
fn max_expand_hops(plan: &LogicalPlan) -> usize {
    match plan {
        LogicalPlan::DistinctExpand {
            input, max_hops, ..
        } => (*max_hops).max(max_expand_hops(input)),
        LogicalPlan::ExpandOut { input, .. }
        | LogicalPlan::ExpandIn { input, .. }
        | LogicalPlan::ExpandBoth { input, .. } => 1 + max_expand_hops(input),
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::LoadCsv { input, .. }
        | LogicalPlan::BindPath { input, .. }
        | LogicalPlan::BindPathEndpoints { input, .. }
        | LogicalPlan::BindEdgeEndpoints { input, .. } => max_expand_hops(input),
        _ => 0,
    }
}

// -- LIMIT detection --------------------------------------------------

/// Limit count (as usize), searching top-down; None if a Sort sits below the
/// Limit (no early cancel when sorting). Also resolves `Expr::Parameter("__sp_N")`:
/// the server's literal stripping turns `LIMIT 5` into `LIMIT $__sp0`, and missing
/// it would yield Nxmorsels rows instead of N.
#[recursive::recursive]
fn find_limit_count(plan: &LogicalPlan) -> Option<usize> {
    match plan {
        LogicalPlan::Limit { count, input } => {
            // early cancel is only safe when nothing *below* the Limit reorders or dedups. `ORDER
            // BY ... LIMIT n` plans as Limit(Project(Sort(...))): cancelling after n rows sorts an
            // arbitrary subset, not the global top-n. `DISTINCT ... LIMIT n` likewise: morsels dedup
            // only internally, so cross-morsel duplicates count toward n and later distinct values
            // are lost. Both use post-merge truncation
            if chain_contains_sort(input) || chain_contains_distinct(input) {
                None
            } else {
                limit_expr_as_usize(count)
            }
        }
        LogicalPlan::Sort { .. } => None,
        LogicalPlan::Project { input, .. } | LogicalPlan::Filter { input, .. } => {
            find_limit_count(input)
        }
        _ => None,
    }
}

/// Limit count for post-merge truncation. NOT sort-guarded like `find_limit_count`: truncation runs
/// after `post_process_sort`, which is exactly the global top-n. Walks through Sort too: for
/// `Sort(Limit(X))` the merged rows are a valid implementation-defined subset and truncation keeps
/// the row count.
#[recursive::recursive]
fn find_post_merge_limit(plan: &LogicalPlan) -> Option<usize> {
    match plan {
        LogicalPlan::Limit { count, .. } => limit_expr_as_usize(count),
        LogicalPlan::Sort { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Filter { input, .. } => find_post_merge_limit(input),
        _ => None,
    }
}

/// A Sort somewhere below in this linear chain?
#[recursive::recursive]
fn chain_contains_sort(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Sort { .. } => true,
        LogicalPlan::Limit { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Eager { input }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::ExpandOut { input, .. }
        | LogicalPlan::ExpandIn { input, .. }
        | LogicalPlan::ExpandBoth { input, .. }
        | LogicalPlan::DistinctExpand { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::LoadCsv { input, .. }
        | LogicalPlan::BindPath { input, .. }
        | LogicalPlan::BindPathEndpoints { input, .. }
        | LogicalPlan::BindEdgeEndpoints { input, .. } => chain_contains_sort(input),
        _ => false,
    }
}

/// A de-duplicating Project (DISTINCT) somewhere below? Per-morsel LIMIT early cancel is unsafe
/// over it (each morsel dedups alone).
#[recursive::recursive]
fn chain_contains_distinct(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Project { distinct: true, .. } => true,
        LogicalPlan::Limit { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Eager { input }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::ExpandOut { input, .. }
        | LogicalPlan::ExpandIn { input, .. }
        | LogicalPlan::ExpandBoth { input, .. }
        | LogicalPlan::DistinctExpand { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::LoadCsv { input, .. }
        | LogicalPlan::BindPath { input, .. }
        | LogicalPlan::BindPathEndpoints { input, .. }
        | LogicalPlan::BindEdgeEndpoints { input, .. } => chain_contains_distinct(input),
        _ => false,
    }
}

fn limit_expr_as_usize(count: &Expr) -> Option<usize> {
    match count {
        Expr::Literal(crate::ast::Literal::Integer(n)) if *n >= 0 => Some(*n as usize),
        Expr::Parameter(name) => match crate::executor::get_query_param(name)? {
            crate::types::CypherValue::Integer(n) if n >= 0 => Some(n as usize),
            _ => None,
        },
        _ => None,
    }
}

// -- Parallelism predicate ---------------------------------------------

/// Would this plan use morsel parallelism? The streaming executor then takes the materializing
/// path.
pub(crate) fn would_parallelize(plan: &LogicalPlan, graph: &dyn GraphView) -> bool {
    if let Some(sync_graph) = graph.as_sync_ref()
        && let Some(scan_info) = detect_morsel_scan(plan)
    {
        let count = match &scan_info.kind {
            MorselScanKind::LabelScan { label } => match label {
                Some(lbl) => sync_graph.nodes_by_label(lbl).len(),
                None => sync_graph.node_count(),
            },
            MorselScanKind::IndexRangeScan { label, .. } => sync_graph.nodes_by_label(label).len(),
        };
        let min_nodes = if max_expand_hops(plan) >= 2 {
            MORSEL_MIN_NODES_MULTIHOP
        } else {
            MORSEL_MIN_NODES
        };
        return count >= min_nodes;
    }
    false
}

// -- Entry point -------------------------------------------------------

/// Morsel-parallel execution, else sequential Volcano. Called from `execute_plan` instead of
/// building and draining directly.
#[inline(never)]
pub(crate) fn try_execute_or_fallback(
    plan: &LogicalPlan,
    graph: &mut dyn GraphView,
    symtab: &SymbolTable,
) -> Result<MorselOutput, CypherError> {
    if let Some(sync_graph) = graph.as_sync_ref()
        && let Some(scan_info) = detect_morsel_scan(plan)
    {
        let node_ids = match &scan_info.kind {
            MorselScanKind::LabelScan { label } => match label {
                Some(lbl) => sync_graph.nodes_by_label(lbl),
                None => sync_graph.nodes_vec(),
            },
            MorselScanKind::IndexRangeScan {
                label,
                property,
                op,
                value,
            } => {
                use std::ops::Bound;
                let val = crate::executor::eval_expr(
                    value,
                    &crate::frame::new_frame(symtab),
                    sync_graph,
                    symtab,
                )?;
                let key = crate::executor::cypher_value_to_index_key(&val);
                match key {
                    Some(k) => {
                        let (lower, upper) = match op {
                            crate::ast::CmpOp::Gt => (Bound::Excluded(k), Bound::Unbounded),
                            crate::ast::CmpOp::Gte => (Bound::Included(k), Bound::Unbounded),
                            crate::ast::CmpOp::Lt => (Bound::Unbounded, Bound::Excluded(k)),
                            crate::ast::CmpOp::Lte => (Bound::Unbounded, Bound::Included(k)),
                            _ => {
                                return Err(CypherError::runtime(
                                    "IndexRangeScan only supports Gt/Gte/Lt/Lte",
                                ));
                            }
                        };
                        sync_graph
                            .index_range_lookup(label, property, lower, upper)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|&nid| sync_graph.get_node(nid).is_ok())
                            .collect()
                    }
                    None => vec![],
                }
            }
        };
        let min_nodes = if max_expand_hops(plan) >= 2 {
            MORSEL_MIN_NODES_MULTIHOP
        } else {
            MORSEL_MIN_NODES
        };
        // sorted plans need every sort key evaluable on the merged post-chain frames; a key
        // that can't map through the projection disables morselization (None) and the
        // sequential path sorts correctly
        let post_sort_items = if scan_info.has_sort {
            resolve_post_merge_sort_items(plan)
        } else {
            Some(Vec::new())
        };
        if node_ids.len() >= min_nodes && post_sort_items.is_some() {
            let post_sort_items = post_sort_items.unwrap_or_default();
            let needs_post = scan_info.has_sort
                || scan_info.has_distinct
                || find_post_merge_limit(plan).is_some();

            if node_ids.len() < MORSEL_MIN_NODES {
                // small result (e.g. index range scan + multi-hop): parallelize only if no
                // other small pipeline runs, avoiding rayon contention under load
                if needs_post
                    && SMALL_MORSEL_ACTIVE
                        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    let result = exec_morsel_pipeline(
                        plan,
                        sync_graph,
                        &node_ids,
                        &scan_info,
                        &post_sort_items,
                        symtab,
                    );
                    SMALL_MORSEL_ACTIVE.store(false, Ordering::Relaxed);
                    return result;
                }
                // lock held or no post-processing: sequential
            } else {
                // large result: always parallel
                if !needs_post {
                    ACTIVE_BATCH_PRODUCERS.fetch_add(1, Ordering::Relaxed);
                    return Ok(MorselOutput::Batched(MorselProducer {
                        plan: plan.clone(),
                        node_ids,
                        symtab: symtab.clone(),
                        morsel_size: STREAMING_MORSEL_BASE,
                        batch_offset: 0,
                        limit_counter: None,
                        tracked: true,
                    }));
                }
                return exec_morsel_pipeline(
                    plan,
                    sync_graph,
                    &node_ids,
                    &scan_info,
                    &post_sort_items,
                    symtab,
                );
            }
        }
    }
    // fallback: sequential Volcano (StreamingExpandOp for single hop)
    let mut op = super::build_operator_tree(plan);
    let frames = super::drain_operator(&mut *op, graph, symtab)?;
    Ok(MorselOutput::Flat(frames.into_vec()))
}

// -- Morsel dispatch ---------------------------------------------------

#[inline(never)]
fn exec_morsel_pipeline(
    plan: &LogicalPlan,
    sync_graph: &(dyn GraphView + Sync),
    node_ids: &[NodeId],
    scan_info: &MorselScanInfo,
    post_sort_items: &[crate::ast::SortItem],
    symtab: &SymbolTable,
) -> Result<MorselOutput, CypherError> {
    // morsel size by expansion depth
    let num_threads = rayon::current_num_threads();
    let base = adaptive_morsel_base(plan);
    let max_morsels = num_threads * 4;
    let morsel_size = if node_ids.len() <= base * num_threads {
        // small result (e.g. index range scan): spread evenly over threads so there's actual
        // parallelism
        (node_ids.len() / num_threads).max(1)
    } else {
        (node_ids.len() / max_morsels + 1).max(base)
    };

    // LIMIT early cancellation: shared atomic counter
    let limit_count = find_limit_count(plan);
    let limit_counter = limit_count.map(|n| Arc::new(AtomicUsize::new(n)));

    // track for concurrency-aware scheduling: at high concurrency each Bolt handler
    // already owns a core, rayon adds scheduling overhead, and each morsel would
    // redo BFS dedup with its own global_seen. One morsel avoids both
    let prev_active = ACTIVE_MORSEL_PIPELINES.fetch_add(1, Ordering::Relaxed);
    // threshold num_threads/2: once half the cores run concurrent pipelines, morsel parallelism
    // costs more than it saves; early concurrent queries stay parallel, later ones mostly
    // single-threaded
    let use_sequential = prev_active >= num_threads / 2;

    let chunks = if use_sequential {
        // high concurrency: each query has its own Bolt handler thread, so run sequentially as ONE
        // morsel: no rayon contention, and DistinctExpand doesn't redo BFS discovery per morsel
        // without a shared global_seen
        set_morsel_ids(node_ids.to_vec());
        let mut op = super::build_operator_tree(plan);
        let mut adapter = ReadOnlyGraphView(sync_graph);
        let result = super::drain_operator(&mut *op, &mut adapter, symtab);
        let _ = take_morsel_ids();
        ACTIVE_MORSEL_PIPELINES.fetch_sub(1, Ordering::Relaxed);
        vec![result?.into_vec()]
    } else {
        // low concurrency: rayon work-stealing within the query
        use rayon::prelude::*;

        // the running query (parameters, clock, deadline, stop flag, memory account): a
        // thread-local of the request handler, else workers would see no parameters and no limits
        let query = crate::query::current();
        // bound-entity capture handle, same reason
        let bound_handle = crate::executor::clone_bound_handle();

        let morsel_results: Result<Vec<Vec<Frame>>, CypherError> = node_ids
            .par_chunks(morsel_size)
            .map(|morsel| {
                // the query onto this worker first, so every check in here (and in the operator
                // tree) sees it
                let _query = crate::query::enter_opt(query.as_ref());

                // limit counter before starting the morsel
                if let Some(ref ctr) = limit_counter
                    && ctr.load(Ordering::Relaxed) == 0
                {
                    return Ok(Vec::new());
                }
                // stopped before starting the morsel?
                crate::query::check_interrupt()?;

                // 1. bound-entity capture onto this worker
                crate::executor::set_bound_handle(bound_handle.clone());

                // 2. thread-local morsel override
                set_morsel_ids(morsel.to_vec());

                // 3. operator tree (ScanOp picks up the morsel IDs)
                let mut op = super::build_operator_tree(plan);

                // 4. per-thread graph adapter
                let mut adapter = ReadOnlyGraphView(sync_graph);

                // 5. drain
                let result = super::drain_operator(&mut *op, &mut adapter, symtab)
                    .map(crate::memory::RowBuffer::into_vec);

                crate::executor::set_bound_handle(None);

                // 6. add the result count to the limit counter
                if let Some(ref ctr) = limit_counter
                    && let Ok(ref frames) = result
                {
                    let n = frames.len();
                    ctr.fetch_sub(n.min(ctr.load(Ordering::Relaxed)), Ordering::Relaxed);
                }

                // 7. clear thread-local overrides (defensive)
                let _ = take_morsel_ids();

                result
            })
            .collect();
        ACTIVE_MORSEL_PIPELINES.fetch_sub(1, Ordering::Relaxed);
        morsel_results?
    };

    // the early-cancel counter is sort-guarded (None with a Sort below the Limit), but post-merge
    // truncation still applies: each morsel ran the whole chain, so the merged union of per-morsel
    // top-n's needs one sort + truncate for the global top-n
    let post_merge_limit = find_post_merge_limit(plan);

    // no post-processing: skip the flatten (halves peak memory), the caller walks chunks during row
    // conversion
    let needs_post = scan_info.has_sort || scan_info.has_distinct || post_merge_limit.is_some();
    if !needs_post {
        return Ok(MorselOutput::Chunks(chunks));
    }

    // flatten into a pre-sized Vec
    let total_len: usize = chunks.iter().map(|v| v.len()).sum();
    let mut all_frames: Vec<Frame> = Vec::with_capacity(total_len);
    for morsel_frames in chunks {
        all_frames.extend(morsel_frames);
    }

    // re-sort merged results
    if scan_info.has_sort && !post_sort_items.is_empty() {
        post_process_sort(&mut all_frames, post_sort_items, sync_graph, symtab);
    }

    // re-dedup merged results
    if scan_info.has_distinct {
        all_frames = crate::executor::dedup_frames(all_frames, symtab);
    }

    // post-merge limit, after the re-sort, so sorted plans truncate to the *global* top-n
    if let Some(n) = post_merge_limit {
        all_frames.truncate(n);
    }

    Ok(MorselOutput::Flat(all_frames))
}

// -- Post-processing helpers -------------------------------------------

/// Re-sorts merged frames with pre-resolved SortItems (see `resolve_post_merge_sort_items`).
#[inline(never)]
fn post_process_sort(
    frames: &mut [Frame],
    items: &[crate::ast::SortItem],
    graph: &(dyn GraphView + Sync),
    symtab: &SymbolTable,
) {
    // decorate-sort-undecorate: evaluate each key ONCE per frame, then sort an index permutation (n
    // evaluations instead of 2*n*log n); same as SortOp
    let keys: Vec<Vec<crate::types::CypherValue>> = frames
        .iter()
        .map(|f| {
            items
                .iter()
                .map(|item| {
                    crate::executor::eval_expr(&item.expr, f, graph, symtab)
                        .unwrap_or(crate::types::CypherValue::Null)
                })
                .collect()
        })
        .collect();

    let mut order: Vec<usize> = (0..frames.len()).collect();
    order.sort_by(|&ia, &ib| {
        for (col, item) in items.iter().enumerate() {
            let ord = crate::executor::compare_values(&keys[ia][col], &keys[ib][col]);
            let ord = if item.ascending { ord } else { ord.reverse() };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });

    // apply the permutation (each frame taken once, in order)
    let mut taken: Vec<Frame> = frames.iter_mut().map(std::mem::take).collect();
    for (dst, &src) in order.iter().enumerate() {
        frames[dst] = std::mem::take(&mut taken[src]);
    }
}

/// Sort items as expressions over the *merged post-chain frames* (each morsel
/// ran the whole chain, so frames are already projected).
///
/// `RETURN ... ORDER BY` plans as Project(Sort(...)) with pre-projection keys
/// (`ORDER BY id` -> `n.id`), but ProjectOp's frames carry only projected slots,
/// so `n.id` would be Null and the merge unsorted. Each key maps back through the
/// topmost projection to its output column; `None` if any can't, which skips
/// morselization (the sequential path sorts pre-projection, always correct).
fn resolve_post_merge_sort_items(plan: &LogicalPlan) -> Option<Vec<crate::ast::SortItem>> {
    let mut current = plan;
    // frames reflect the topmost projection: map against it
    let mut proj: Option<&[crate::ast::ProjectionItem]> = None;
    loop {
        match current {
            LogicalPlan::Limit { input, .. }
            | LogicalPlan::Skip { input, .. }
            | LogicalPlan::Filter { input, .. } => current = input,
            LogicalPlan::Project { input, items, .. } => {
                if proj.is_none() {
                    proj = Some(items);
                }
                current = input;
            }
            LogicalPlan::Sort { items, .. } => {
                let Some(proj_items) = proj else {
                    // no projection above the Sort: frames keep every pre-sort binding, items
                    // evaluate as is
                    return Some(items.clone());
                };
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(crate::ast::SortItem {
                        expr: resolve_sort_expr_to_column(&item.expr, proj_items)?,
                        ascending: item.ascending,
                    });
                }
                return Some(out);
            }
            _ => return None,
        }
    }
}

/// One sort expression -> post-projection column reference.
fn resolve_sort_expr_to_column(expr: &Expr, proj: &[crate::ast::ProjectionItem]) -> Option<Expr> {
    // a bare variable the projection carries through (alias or pass-through) keeps its slot
    if let Expr::Variable(name) = expr {
        let survives = proj.iter().any(|p| {
            p.alias.as_deref() == Some(name.as_str())
                || (p.alias.is_none() && matches!(&p.expr, Expr::Variable(v) if v == name))
        });
        if survives {
            return Some(expr.clone());
        }
    }
    // structural match with a projected expression -> sort by its column; Expr has no PartialEq, the
    // derived Debug output is deterministic and computed once per query
    let want = format!("{expr:?}");
    for p in proj {
        if format!("{:?}", p.expr) == want {
            if let Some(alias) = &p.alias {
                return Some(Expr::Variable(alias.clone()));
            }
            if let Expr::Variable(v) = &p.expr {
                return Some(Expr::Variable(v.clone()));
            }
        }
    }
    None
}

// -- ReadOnlyGraphView adapter -----------------------------------------

/// Wraps `&(dyn GraphView + Sync)` as the `&mut dyn GraphView` `Operator::next()` wants: every read
/// forwards, every write is refused (read-only pipelines never write).
pub(crate) struct ReadOnlyGraphView<'a>(pub(crate) &'a (dyn GraphView + Sync));

impl<'a> GraphView for ReadOnlyGraphView<'a> {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        Some(self.0)
    }

    traverse_core::forward_graph_view_reads!(self => self.0);
    traverse_core::reject_graph_view_writes!();
}
