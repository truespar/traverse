//! Whole-pipeline parallelism. A read-only plan is cut at its pipeline breakers (sort,
//! aggregation, distinct, union, cross product, hash-join build, limit) into segments; every
//! segment runs morsel-parallel on rayon workers, each worker with an operator tree of its own
//! over a morsel of the segment's source: a slice of a leaf's ids, or a slice of the chunks a
//! breaker below produced. The breakers merge the workers' partial states in parallel too
//! (partitioned aggregate merge, parallel stable sort, partitioned dedup, partitioned join
//! build) and hand the next segment their output as chunks. Workers return results in morsel
//! order and every merge keeps that order, so a parallel run yields what the sequential one
//! would. Writes, procedures and `LOAD CSV` keep the plan sequential.

pub(crate) mod merge;
mod morsel;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;
use traverse_core::graph::types::Direction;
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::memory::Charge;
use crate::types::CypherValue;

use super::chunk::{Group, GroupId, ResultSet};
use super::ctx::{ExecCtx, ReadOnlyGraphView};
use super::exec::{RowChunk, chunk_rows};
use super::expr::eval::eval_constant;
use super::ops::aggregate::{AggState, GroupAcc};
use super::ops::flow::{PairReplayOp, ReplayOp};
use super::ops::join::BuildTable;
use super::ops::project::ProjectOp;
use super::ops::sort::SortState;
use super::ops::source::{LeafIds, leaf_ids, leaf_stand_in};
use super::ops::{PhysOperator, Substs, build_with};
use super::plan::{PhysNode, PhysicalPlan, SubPlan};
use super::values::index_key;
use super::values::{JoinKey, join_key_ref};
use super::vector::{VECTOR_CAPACITY, Vector};

/// A chunk as the workers hand it back: every group's state.
pub(crate) type Snapshot = Vec<Group>;

/// Parallel runs in flight: at `num_threads` every core is busy with concurrent queries and
/// intra-query parallelism only adds convoy overhead, so segments run on the caller's thread.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Rows (or other work units) below which a segment runs on one thread.
const MIN_PARALLEL_WORK: usize = 8192;

/// Can `plan` take the parallel path at all on `graph`?
pub(crate) fn eligible(plan: &PhysicalPlan, graph: &dyn GraphView) -> bool {
    graph.as_sync_ref().is_some()
        && !plan.mutating
        && rayon::current_num_threads() >= 2
        && plan.root.any(&|n| is_wide_leaf(n, graph))
}

/// A leaf that may enumerate enough rows to split: any but an index seek whose key the index
/// estimates narrow (a point lookup gains nothing from the scheduler and pays its setup).
fn is_wide_leaf(node: &PhysNode, graph: &dyn GraphView) -> bool {
    // a property seek through the label indexes: a point lookup, like a narrow index seek
    if let PhysNode::NodeScan { seek: Some(_), .. } = node {
        return false;
    }
    let PhysNode::IndexSeek {
        label,
        property,
        value,
        ..
    } = node
    else {
        return is_leaf_source(node);
    };
    let Ok(v) = eval_constant(value, graph) else {
        return true;
    };
    let Some(key) = index_key(&v) else {
        return true;
    };
    let (lo, hi) = (
        std::ops::Bound::Included(key.clone()),
        std::ops::Bound::Included(key),
    );
    match graph.index_range_estimate(label, property, &lo, &hi) {
        Some(share) => share * graph.count_nodes_by_label(label) as f64 >= MIN_PARALLEL_WORK as f64,
        None => true,
    }
}

/// Runs `plan` through the scheduler; `None` when it must run sequentially.
pub(crate) fn run_plan(
    plan: &PhysicalPlan,
    graph: &mut dyn GraphView,
) -> Result<Option<(Vec<RowChunk>, Charge)>, CypherError> {
    if !eligible(plan, &*graph) {
        return Ok(None);
    }
    let sync = graph
        .as_sync_ref()
        .expect("eligibility requires a sync view");
    let active = ACTIVE.fetch_add(1, Ordering::Relaxed);
    let _guard = ActiveGuard;
    let threads = rayon::current_num_threads();
    let (node_count, edge_count) = (sync.node_count().max(1), sync.edge_count());
    let sched = Sched {
        plan,
        sync,
        threads,
        query: crate::query::current(),
        bound: crate::executor::clone_bound_handle(),
        path_mode: crate::session::current_path_mode(),
        inline: active >= threads,
        charge: Mutex::new(Charge::new()),
        avg_degree: (edge_count as f64 / node_count as f64).max(1.0),
    };
    let columns = &plan.columns;
    let out_groups = &plan.out_groups;
    let out: Vec<RowChunk> = if plan.columns.is_empty() {
        sched.exec(&plan.root, None)?;
        Vec::new()
    } else if sched.is_segment_root(&plan.root) {
        // the root segment's workers emit result chunks straight from their chunks
        let seg = sched.discover(&plan.root);
        let live = live_groups(&plan.root);
        let parts: Vec<Vec<RowChunk>> = sched.run_segment(&seg, None, &live, |run| {
            let mut out = Vec::new();
            while run.next()? {
                let c = chunk_rows(&run.rs, columns, out_groups);
                if c.len > 0 {
                    run.charge(c.bytes())?;
                    out.push(c);
                }
            }
            Ok(out)
        })?;
        parts.into_iter().flatten().collect()
    } else {
        let snaps = sched.exec(&plan.root, None)?;
        let work = snaps.len() * VECTOR_CAPACITY;
        let chunks: Vec<RowChunk> = sched.map(snaps, work, |groups| {
            let rs = ResultSet { groups };
            Ok(chunk_rows(&rs, columns, out_groups))
        })?;
        chunks.into_iter().filter(|c| c.len > 0).collect()
    };
    let mut charge = sched.charge.into_inner().unwrap_or_else(|e| e.into_inner());
    for c in &out {
        charge.add(c.bytes())?;
    }
    charge.flush()?;
    Ok(Some((out, charge)))
}

/// Releases the in-flight count on every exit.
struct ActiveGuard;

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The scheduler of one plan run.
pub(crate) struct Sched<'p, 'g> {
    plan: &'p PhysicalPlan,
    sync: &'g (dyn GraphView + Sync),
    threads: usize,
    query: Option<Arc<crate::query::QueryContext>>,
    bound: Option<crate::executor::BoundHandle>,
    path_mode: crate::ast::PathMode,
    /// Every segment runs on the calling thread (the pool is saturated by other queries).
    inline: bool,
    /// The run's charge against the query's memory budget: every chunk and merge state the
    /// run holds on to, checked as it grows.
    charge: Mutex<Charge>,
    avg_degree: f64,
}

/// A leaf that enumerates ids: the morsel source of its segment.
fn is_leaf_source(node: &PhysNode) -> bool {
    matches!(
        node,
        PhysNode::NodeScan { .. }
            | PhysNode::NodeByIdRange { .. }
            | PhysNode::IndexSeek { .. }
            | PhysNode::IndexRange { .. }
            | PhysNode::TextIndexScan { .. }
            | PhysNode::PointIndexScan { .. }
            | PhysNode::EdgeTypeScan { .. }
    )
}

/// A leaf with one or a few rows: its segment runs as one morsel.
fn is_inline_leaf(node: &PhysNode) -> bool {
    matches!(
        node,
        PhysNode::EmptyRow { .. }
            | PhysNode::Seed { .. }
            | PhysNode::NodeByIdSeek { .. }
            | PhysNode::CountSource { .. }
            | PhysNode::ChainWeights { .. }
            | PhysNode::NoRows
            | PhysNode::Schema { .. }
            | PhysNode::Show { .. }
    )
}

/// Does the subtree (bodies included) only read?
fn subtree_read_only(node: &PhysNode) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n {
            PhysNode::CreateNode { .. }
            | PhysNode::CreateRel { .. }
            | PhysNode::SetProps { .. }
            | PhysNode::Remove { .. }
            | PhysNode::Delete { .. }
            | PhysNode::Merge { .. }
            | PhysNode::CallProcedure { .. }
            | PhysNode::CallInTransactions { .. }
            | PhysNode::ForEach { .. }
            | PhysNode::LoadCsv { .. }
            | PhysNode::Schema { .. }
            | PhysNode::TransactionCommand { .. } => return false,
            PhysNode::OptionalMatch { body, .. } | PhysNode::CallSubquery { body, .. } => {
                stack.push(&body.root);
            }
            _ => {}
        }
        stack.extend(n.children());
    }
    true
}

fn body_read_only(body: &SubPlan) -> bool {
    subtree_read_only(&body.root)
}

/// Can `node` run per morsel: does it treat lanes independently, with no state across chunks?
fn morsel_safe(node: &PhysNode) -> bool {
    match node {
        PhysNode::Filter { .. }
        | PhysNode::Flatten { .. }
        | PhysNode::Capture { .. }
        | PhysNode::AddColumn { .. }
        | PhysNode::Expand { .. }
        | PhysNode::ExpandCount { .. }
        | PhysNode::VarExpand { .. }
        | PhysNode::Unwind { .. }
        | PhysNode::IndexLookupJoin { .. }
        | PhysNode::VectorSearch { .. }
        | PhysNode::BindPath { .. }
        | PhysNode::BindQppGroups { .. }
        | PhysNode::BindVarLists { .. }
        | PhysNode::BindPathEndpoints { .. }
        | PhysNode::BindEdgeEndpoints { .. }
        | PhysNode::ShortestPath { .. }
        | PhysNode::WeightedShortestPath { .. } => true,
        PhysNode::Project { distinct, .. } => !distinct,
        PhysNode::DistinctExpand { global_dedup, .. } => !global_dedup,
        PhysNode::OptionalMatch { body, .. } | PhysNode::CallSubquery { body, .. } => {
            body_read_only(body)
        }
        _ => false,
    }
}

/// What a segment's bottom is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bottom {
    /// An id-enumerating leaf: morsels of its ids.
    Leaf,
    /// A one-row leaf: a single morsel.
    Inline,
    /// A breaker: morsels of the chunks it produces.
    Breaker,
    /// A cartesian product: morsels of chunk pairs of its materialized sides.
    Cross,
}

/// A maximal chain of per-morsel operators and what feeds it.
struct Segment<'p> {
    top: &'p PhysNode,
    bottom: &'p PhysNode,
    kind: Bottom,
    /// Hash joins inside the chain, whose build sides materialize first.
    joins: Vec<&'p PhysNode>,
    /// Expansion depth above the source (fan-out the morsel sizing accounts for).
    hops: usize,
    /// Direction of the first expansion above the source.
    dir: Option<Direction>,
    /// Every operator in the chain keeps the row count: a `LIMIT` above needs no more rows of
    /// the source than it emits.
    one_to_one: bool,
}

/// The morsels of a segment.
enum Morsels {
    Single,
    Ids(Arc<LeafIds>, Vec<std::ops::Range<usize>>),
    Chunks(Vec<Vec<Snapshot>>),
    Pairs(Arc<merge::Product>, Vec<std::ops::Range<usize>>),
}

/// One morsel handed to a worker.
enum MorselRef {
    Single,
    Ids(Arc<LeafIds>, std::ops::Range<usize>),
    Chunks(Vec<Snapshot>),
    Pairs(Arc<merge::Product>, std::ops::Range<usize>),
}

/// Drives a worker's operator tree chunk by chunk, stopping early for a `LIMIT` once the
/// workers together produced enough rows.
pub(crate) struct Run<'a, 'g, 'p> {
    op: Box<dyn PhysOperator + 'p>,
    /// The chunk.
    pub(crate) rs: ResultSet,
    /// The worker's context.
    pub(crate) cx: &'a mut ExecCtx<'g>,
    remaining: Option<&'a AtomicUsize>,
    live: &'a [GroupId],
    stopped: bool,
    /// The run's charge, for what the worker hands back.
    charge: &'a Mutex<Charge>,
}

impl Run<'_, '_, '_> {
    /// The next chunk with rows; `false` when the segment is done (or stopped early).
    pub(crate) fn next(&mut self) -> Result<bool, CypherError> {
        if self.stopped {
            return Ok(false);
        }
        if let Some(r) = self.remaining
            && r.load(Ordering::Relaxed) == 0
        {
            return Ok(false);
        }
        loop {
            if !self.op.next(&mut self.rs, self.cx)? {
                return Ok(false);
            }
            let rows = self.rs.row_count(self.live);
            if rows == 0 {
                continue;
            }
            if let Some(r) = self.remaining {
                let prev = r
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                        Some(v.saturating_sub(rows))
                    })
                    .unwrap_or(0);
                if prev <= rows {
                    self.stopped = true;
                }
            }
            return Ok(true);
        }
    }

    /// The current chunk's live groups, copied out and charged; the others (stale earlier
    /// chunks of groups a consumer replaced) come back empty.
    pub(crate) fn snapshot(&mut self) -> Result<Snapshot, CypherError> {
        let s: Snapshot = self
            .rs
            .groups
            .iter()
            .enumerate()
            .map(|(g, grp)| {
                if self.live.binary_search(&(g as GroupId)).is_ok() {
                    grp.clone()
                } else {
                    Group::with_cols(grp.cols.len())
                }
            })
            .collect();
        self.charge(snapshot_bytes(&s))?;
        Ok(s)
    }

    /// Charges `bytes` the worker hands back.
    pub(crate) fn charge(&mut self, bytes: usize) -> Result<(), CypherError> {
        self.charge
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add(bytes)
    }
}

/// Heap bytes of a snapshot's columns.
fn snapshot_bytes(s: &Snapshot) -> usize {
    s.iter()
        .flat_map(|g| g.cols.iter())
        .map(Vector::heap_bytes)
        .sum::<usize>()
        + 64
}

impl<'p, 'g> Sched<'p, 'g> {
    /// Runs `f` with a worker's context: the statement entered, bound capture wired, the graph
    /// as a read-only view.
    fn in_worker<T>(
        &self,
        f: impl FnOnce(&mut ExecCtx<'_>) -> Result<T, CypherError>,
    ) -> Result<T, CypherError> {
        let _query = crate::query::enter_opt(self.query.as_ref());
        crate::executor::set_bound_handle(self.bound.clone());
        let _pm = crate::session::set_path_mode(self.path_mode);
        let result = (|| {
            crate::query::check_interrupt()?;
            let mut adapter = ReadOnlyGraphView(self.sync);
            let mut cx = ExecCtx::new(&mut adapter);
            f(&mut cx)
        })();
        crate::executor::set_bound_handle(None);
        result
    }

    /// Maps `items` on the workers (or inline, when `work` is small), results in item order.
    fn map<I: Send, T: Send>(
        &self,
        items: Vec<I>,
        work: usize,
        f: impl Fn(I) -> Result<T, CypherError> + Sync + Send,
    ) -> Result<Vec<T>, CypherError> {
        if self.inline || items.len() <= 1 || work < MIN_PARALLEL_WORK {
            items.into_iter().map(f).collect()
        } else {
            items.into_par_iter().map(f).collect()
        }
    }

    /// Parallel for over `0..n` (inline when `work` is small).
    fn for_parts<T: Send>(
        &self,
        n: usize,
        work: usize,
        f: impl Fn(usize) -> Result<T, CypherError> + Sync + Send,
    ) -> Result<Vec<T>, CypherError> {
        if self.inline || n <= 1 || work < MIN_PARALLEL_WORK {
            (0..n).map(f).collect()
        } else {
            (0..n).into_par_iter().map(f).collect()
        }
    }

    /// Partitions for a parallel merge over `n` items.
    fn parts_for(&self, n: usize) -> usize {
        if self.inline || n < MIN_PARALLEL_WORK {
            1
        } else {
            self.threads.min(n / 2048).max(2)
        }
    }

    /// Charges `bytes` the run holds on to.
    fn charge(&self, bytes: usize) -> Result<(), CypherError> {
        self.charge
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add(bytes)
    }

    /// Does `node` head a segment (rather than being a breaker with a merge of its own)?
    fn is_segment_root(&self, node: &PhysNode) -> bool {
        !matches!(
            node,
            PhysNode::Aggregate { .. }
                | PhysNode::Sort { .. }
                | PhysNode::Project { distinct: true, .. }
                | PhysNode::Union { .. }
                | PhysNode::Eager { .. }
                | PhysNode::Limit { .. }
                | PhysNode::Skip { .. }
        ) && (is_leaf_source(node)
            || is_inline_leaf(node)
            || morsel_safe(node)
            || matches!(
                node,
                PhysNode::HashJoin { .. } | PhysNode::CrossProduct { .. }
            ))
    }

    /// The chain of per-morsel operators from `top` down to its source.
    fn discover(&self, top: &'p PhysNode) -> Segment<'p> {
        let mut cur = top;
        let mut joins = Vec::new();
        let mut hops = 0usize;
        let mut dir = None;
        let mut one_to_one = true;
        loop {
            if is_leaf_source(cur) {
                return Segment {
                    top,
                    bottom: cur,
                    kind: Bottom::Leaf,
                    joins,
                    hops,
                    dir,
                    one_to_one,
                };
            }
            if is_inline_leaf(cur) {
                return Segment {
                    top,
                    bottom: cur,
                    kind: Bottom::Inline,
                    joins,
                    hops,
                    dir,
                    one_to_one,
                };
            }
            match cur {
                PhysNode::HashJoin { right, .. } => {
                    joins.push(cur);
                    one_to_one = false;
                    cur = right;
                    continue;
                }
                PhysNode::CrossProduct { .. } => {
                    return Segment {
                        top,
                        bottom: cur,
                        kind: Bottom::Cross,
                        joins,
                        hops,
                        dir,
                        one_to_one,
                    };
                }
                _ => {}
            }
            if !morsel_safe(cur) {
                return Segment {
                    top,
                    bottom: cur,
                    kind: Bottom::Breaker,
                    joins,
                    hops,
                    dir,
                    one_to_one,
                };
            }
            match cur {
                PhysNode::Expand { dir: d, .. } => {
                    hops += 1;
                    dir = Some(*d);
                }
                PhysNode::VarExpand { max, dir: d, .. }
                | PhysNode::DistinctExpand { max, dir: d, .. } => {
                    hops += (*max).min(8);
                    dir = Some(*d);
                }
                _ => {}
            }
            if !matches!(
                cur,
                PhysNode::Capture { .. }
                    | PhysNode::AddColumn { .. }
                    | PhysNode::ExpandCount { .. }
                    | PhysNode::Flatten { .. }
                    | PhysNode::BindPath { .. }
                    | PhysNode::BindQppGroups { .. }
                    | PhysNode::BindVarLists { .. }
                    | PhysNode::BindPathEndpoints { .. }
                    | PhysNode::BindEdgeEndpoints { .. }
                    | PhysNode::Project { where_: None, .. }
            ) {
                one_to_one = false;
            }
            let ch = cur.children();
            debug_assert_eq!(ch.len(), 1, "a per-morsel operator has one input");
            cur = ch[0];
        }
    }

    /// The chunks `node` produces, every segment below it run in parallel. `limit`: the rows a
    /// `LIMIT` above needs at most (workers stop early, sorts keep a top-k).
    #[recursive::recursive]
    fn exec(&self, node: &'p PhysNode, limit: Option<usize>) -> Result<Vec<Snapshot>, CypherError> {
        match node {
            PhysNode::Aggregate { .. } => self.exec_aggregate(node),
            PhysNode::Sort { .. } => self.exec_sort(node, limit),
            PhysNode::Project { distinct: true, .. } => self.exec_distinct(node),
            PhysNode::Union { .. } => self.exec_union(node),
            PhysNode::Eager { input, .. } => self.exec(input, limit),
            PhysNode::Limit { .. } | PhysNode::Skip { .. } => self.exec_limit(node, limit),
            _ if is_leaf_source(node)
                || is_inline_leaf(node)
                || morsel_safe(node)
                || matches!(
                    node,
                    PhysNode::HashJoin { .. } | PhysNode::CrossProduct { .. }
                ) =>
            {
                let seg = self.discover(node);
                let live = live_groups(node);
                let out: Vec<Vec<Snapshot>> = self.run_segment(&seg, limit, &live, |run| {
                    let mut chunks = Vec::new();
                    while run.next()? {
                        chunks.push(run.snapshot()?);
                    }
                    Ok(chunks)
                })?;
                Ok(out.into_iter().flatten().collect())
            }
            _ => self.exec_sequential(node),
        }
    }

    /// Runs `seg` morsel by morsel; `work` drives each worker's tree and returns its result.
    fn run_segment<T: Send>(
        &self,
        seg: &Segment<'p>,
        limit: Option<usize>,
        live: &[GroupId],
        work: impl Fn(&mut Run<'_, '_, 'p>) -> Result<T, CypherError> + Sync,
    ) -> Result<Vec<T>, CypherError> {
        self.run_segment_with(seg, limit, live, Ok, work)
    }

    /// [`run_segment`](Self::run_segment) with the built tree wrapped by `wrap` (a breaker's
    /// per-morsel form on top of its input chain).
    fn run_segment_with<T: Send>(
        &self,
        seg: &Segment<'p>,
        limit: Option<usize>,
        live: &[GroupId],
        wrap: impl Fn(Box<dyn PhysOperator + 'p>) -> Result<Box<dyn PhysOperator + 'p>, CypherError>
        + Sync,
        work: impl Fn(&mut Run<'_, '_, 'p>) -> Result<T, CypherError> + Sync,
    ) -> Result<Vec<T>, CypherError> {
        let tables: Vec<Arc<BuildTable>> = seg
            .joins
            .iter()
            .map(|j| self.build_table(j))
            .collect::<Result<_, _>>()?;
        let morsels = match seg.kind {
            Bottom::Leaf => {
                let ids = self.in_worker(|cx| {
                    let rs = ResultSet::new(&self.plan.widths);
                    leaf_ids(seg.bottom, &rs, cx)
                })?;
                let ranges = self.morsel_ranges(&ids, seg);
                Morsels::Ids(Arc::new(ids), ranges)
            }
            Bottom::Inline => Morsels::Single,
            Bottom::Breaker => {
                // the breaker only needs the limit's rows when nothing above it drops or adds any
                let hint = if seg.one_to_one { limit } else { None };
                let snaps = self.exec(seg.bottom, hint)?;
                Morsels::Chunks(self.chunk_morsels(snaps, seg))
            }
            Bottom::Cross => {
                let product = self.product(seg.bottom)?;
                let pairs = product.pairs();
                let ranges = morsel::even_ranges(pairs, self.target_morsels(pairs));
                Morsels::Pairs(Arc::new(product), ranges)
            }
        };
        let remaining = limit.map(AtomicUsize::new);
        let items: Vec<MorselRef> = match morsels {
            Morsels::Single => vec![MorselRef::Single],
            Morsels::Ids(ids, ranges) => ranges
                .into_iter()
                .map(|r| MorselRef::Ids(ids.clone(), r))
                .collect(),
            Morsels::Chunks(chunks) => chunks.into_iter().map(MorselRef::Chunks).collect(),
            Morsels::Pairs(p, ranges) => ranges
                .into_iter()
                .map(|r| MorselRef::Pairs(p.clone(), r))
                .collect(),
        };
        let joins = &seg.joins;
        let tables = &tables;
        let remaining = remaining.as_ref();
        self.map(items, usize::MAX, |m| {
            self.in_worker(|cx| {
                let mut subs = Substs::none();
                for (j, t) in joins.iter().zip(tables) {
                    subs.probe(j, t.clone());
                }
                match m {
                    MorselRef::Single => {}
                    MorselRef::Ids(ids, range) => {
                        subs.stand_in(seg.bottom, leaf_stand_in(seg.bottom, &ids, range));
                    }
                    MorselRef::Chunks(chunks) => {
                        subs.stand_in(seg.bottom, Box::new(ReplayOp::new(chunks)));
                    }
                    MorselRef::Pairs(p, range) => {
                        subs.stand_in(seg.bottom, Box::new(PairReplayOp::new(p, range)));
                    }
                }
                let op = build_with(seg.top, &mut subs)?;
                debug_assert!(subs.is_empty(), "every stand-in is reached while building");
                let op = wrap(op)?;
                let mut run = Run {
                    op,
                    rs: ResultSet::new(&self.plan.widths),
                    cx,
                    remaining,
                    live,
                    stopped: false,
                    charge: &self.charge,
                };
                work(&mut run)
            })
        })
    }

    /// How many morsels a source of `work` units should be cut into.
    fn target_morsels(&self, work: usize) -> usize {
        if self.inline || work < MIN_PARALLEL_WORK {
            return 1;
        }
        (self.threads * 4).max(work.div_ceil(morsel::TARGET_WORK))
    }

    /// Cuts a leaf's ids into morsels of about equal work: the ids plus the fan-out the first
    /// expansion above the leaf has to visit. A bitmap leaf is cut in bit ranges, by set bits or
    /// by the work of each block of ids.
    fn morsel_ranges(&self, ids: &LeafIds, seg: &Segment<'p>) -> Vec<std::ops::Range<usize>> {
        let n = ids.len();
        if n == 0 {
            return Vec::new();
        }
        if let LeafIds::Bits { words, count, .. } = ids {
            return self.bitmap_ranges(words, *count, seg);
        }
        let weights: Option<Vec<u32>> = match (ids, seg.hops) {
            (LeafIds::Nodes(nodes), hops) if hops > 0 => {
                let dir = seg.dir.unwrap_or(Direction::Both);
                let deeper = self.avg_degree.powi(hops as i32 - 1);
                let mut degrees = Vec::with_capacity(n);
                self.sync.degrees(nodes, dir, &mut degrees);
                Some(
                    degrees
                        .into_iter()
                        .map(|d| (1.0 + d as f64 * deeper).min(u32::MAX as f64) as u32)
                        .collect(),
                )
            }
            _ => None,
        };
        match weights {
            Some(w) => {
                let total: usize = w.iter().map(|&x| x as usize).sum();
                morsel::weighted_ranges(&w, self.target_morsels(total))
            }
            None => morsel::even_ranges(n, self.target_morsels(n)),
        }
    }

    /// Morsels over a bitmap: blocks of ids weighted by their set bits (plus the fan-out above
    /// the leaf, read per block in parallel), cut into bit ranges of about equal work.
    fn bitmap_ranges(
        &self,
        words: &Arc<Vec<u64>>,
        count: usize,
        seg: &Segment<'p>,
    ) -> Vec<std::ops::Range<usize>> {
        const BLOCK_WORDS: usize = 256;
        let blocks = words.len().div_ceil(BLOCK_WORDS);
        let m = self.target_morsels(count);
        if m <= 1 || blocks == 0 {
            return std::iter::once(0..words.len() * 64).collect();
        }
        let hops = seg.hops;
        let dir = seg.dir.unwrap_or(Direction::Both);
        let deeper = self.avg_degree.powi(hops.saturating_sub(1) as i32);
        let block_work: Vec<u64> = self
            .for_parts(blocks, if hops > 0 { count } else { 0 }, |b| {
                let ws = &words[b * BLOCK_WORDS..((b + 1) * BLOCK_WORDS).min(words.len())];
                let set: usize = ws.iter().map(|w| w.count_ones() as usize).sum();
                if hops == 0 || set == 0 {
                    return Ok(set as u64);
                }
                let mut ids = Vec::with_capacity(set);
                for (i, &w) in ws.iter().enumerate() {
                    let mut bits = w;
                    while bits != 0 {
                        ids.push(traverse_core::graph::types::NodeId(
                            ((b * BLOCK_WORDS + i) * 64 + bits.trailing_zeros() as usize) as u64,
                        ));
                        bits &= bits - 1;
                    }
                }
                let mut degrees = Vec::with_capacity(ids.len());
                self.sync.degrees(&ids, dir, &mut degrees);
                Ok(ids.len() as u64 + (degrees.iter().sum::<usize>() as f64 * deeper) as u64)
            })
            .expect("block weights cannot fail");
        let total: u64 = block_work.iter().sum();
        let target = total.div_ceil(m as u64).max(1);
        let mut out = Vec::with_capacity(m);
        let mut start = 0usize;
        let mut acc = 0u64;
        for (b, &w) in block_work.iter().enumerate() {
            acc += w;
            if acc >= target {
                out.push(
                    start * BLOCK_WORDS * 64..((b + 1) * BLOCK_WORDS * 64).min(words.len() * 64),
                );
                start = b + 1;
                acc = 0;
            }
        }
        if start < blocks {
            out.push(start * BLOCK_WORDS * 64..words.len() * 64);
        }
        out
    }

    /// Groups a breaker's output chunks into morsels of about equal rows.
    fn chunk_morsels(&self, snaps: Vec<Snapshot>, seg: &Segment<'p>) -> Vec<Vec<Snapshot>> {
        let live = live_groups(seg.bottom);
        let rows: Vec<usize> = snaps.iter().map(|s| ResultSet::rows_of(s, &live)).collect();
        let scale = self.avg_degree.powi(seg.hops as i32);
        let total = (rows.iter().sum::<usize>() as f64 * scale) as usize;
        let m = self.target_morsels(total).min(snaps.len().max(1));
        if m <= 1 {
            return vec![snaps];
        }
        let target = total.div_ceil(m);
        let mut out: Vec<Vec<Snapshot>> = Vec::with_capacity(m);
        let mut cur: Vec<Snapshot> = Vec::new();
        let mut acc = 0usize;
        for (s, r) in snaps.into_iter().zip(rows) {
            cur.push(s);
            acc += (r as f64 * scale) as usize;
            if acc >= target {
                out.push(std::mem::take(&mut cur));
                acc = 0;
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// A hash join's build side as a shared table.
    fn build_table(&self, join: &'p PhysNode) -> Result<Arc<BuildTable>, CypherError> {
        let PhysNode::HashJoin {
            left,
            left_keys,
            left_groups,
            moves_left,
            ..
        } = join
        else {
            unreachable!("a join");
        };
        let snaps = self.exec(left, None)?;
        merge::build_table(self, snaps, left_keys, left_groups, moves_left)
    }

    /// Both sides of a cartesian product as rows.
    fn product(&self, node: &'p PhysNode) -> Result<merge::Product, CypherError> {
        let PhysNode::CrossProduct {
            left,
            right,
            left_groups,
            right_groups,
            left_out,
            moves_left,
            right_out,
            moves_right,
        } = node
        else {
            unreachable!("a cross product");
        };
        let l = self.exec(left, None)?;
        let lw = self.plan.widths[*left_out as usize] as usize;
        let rw = self.plan.widths[*right_out as usize] as usize;
        let (lcols, lrows) = merge::to_rows(self, l, left_groups, moves_left, lw)?;
        // an empty left side is an empty product whatever the right side holds
        let (rcols, rrows) = if lrows == 0 {
            ((0..rw).map(|_| Vector::empty()).collect(), 0)
        } else {
            let r = self.exec(right, None)?;
            merge::to_rows(self, r, right_groups, moves_right, rw)?
        };
        Ok(merge::Product::new(
            lcols, lrows, rcols, rrows, *left_out, *right_out,
        ))
    }

    fn exec_aggregate(&self, node: &'p PhysNode) -> Result<Vec<Snapshot>, CypherError> {
        let PhysNode::Aggregate {
            input,
            keys,
            aggs,
            post,
            in_groups,
            out_group,
        } = node
        else {
            unreachable!("an aggregate");
        };
        let seg = self.discover(input);
        let live = live_groups(input);
        let partials: Vec<Vec<GroupAcc>> = self.run_segment(&seg, None, &live, |run| {
            let mut st = AggState::new(keys, aggs, true);
            while run.next()? {
                st.consume_chunk(&run.rs, run.cx, in_groups, post.is_empty())?;
                run.cx.tick()?;
            }
            Ok(st.into_groups())
        })?;
        merge::aggregate(self, partials, keys, aggs, post, *out_group)
    }

    fn exec_sort(
        &self,
        node: &'p PhysNode,
        limit: Option<usize>,
    ) -> Result<Vec<Snapshot>, CypherError> {
        let PhysNode::Sort {
            input,
            keys,
            in_groups,
            out_group,
            moves,
            limit: own,
        } = node
        else {
            unreachable!("a sort");
        };
        let limit = limit.or_else(|| own.as_ref().and_then(|e| self.constant_count(e)));
        let mut seg = self.discover(input);
        // a top-k over not many more rows than it keeps: one heap on one worker beats a full
        // heap per worker plus a merge of them all
        if let Some(k) = limit
            && matches!(seg.kind, Bottom::Leaf)
            && self
                .leaf_rows(seg.bottom)
                .is_some_and(|rows| rows <= k.saturating_mul(self.threads).saturating_mul(2))
        {
            seg.kind = Bottom::Inline;
        }
        let live = live_groups(input);
        let width = self.plan.widths[*out_group as usize] as usize;
        let partials = self.run_segment(&seg, None, &live, |run| {
            let mut st = SortState::new(keys, in_groups, moves, width, limit);
            while run.next()? {
                st.consume_chunk(&run.rs, run.cx)?;
                run.cx.tick()?;
            }
            st.finish()
        })?;
        merge::sort(self, partials, keys, *out_group, limit)
    }

    fn exec_distinct(&self, node: &'p PhysNode) -> Result<Vec<Snapshot>, CypherError> {
        let PhysNode::Project {
            input,
            items,
            moves,
            in_groups,
            out_group,
            where_,
            ..
        } = node
        else {
            unreachable!("a projection");
        };
        let seg = self.discover(input);
        let out = *out_group;
        let live = [out];
        let width = self.plan.widths[out as usize] as usize;
        let parts: Vec<Vec<(Vec<Vector>, Vec<Vec<JoinKey>>)>> = self.run_segment_with(
            &seg,
            None,
            &live,
            |inner| {
                Ok(Box::new(ProjectOp::new(
                    inner,
                    items,
                    moves,
                    in_groups,
                    out,
                    true,
                    where_.as_ref(),
                )))
            },
            |run| {
                let mut chunks = Vec::new();
                while run.next()? {
                    let g = run.rs.group(out);
                    let lanes: Vec<u32> = g.selected_lanes().map(|l| l as u32).collect();
                    let cols: Vec<Vector> = g.cols.iter().map(|c| c.take(&lanes)).collect();
                    let keys: Vec<Vec<JoinKey>> = (0..lanes.len())
                        .map(|i| (0..width).map(|c| join_key_ref(cols[c].get(i))).collect())
                        .collect();
                    run.charge(
                        cols.iter().map(Vector::heap_bytes).sum::<usize>() + keys.len() * 48,
                    )?;
                    chunks.push((cols, keys));
                }
                Ok(chunks)
            },
        )?;
        let parts: Vec<(Vec<Vector>, Vec<Vec<JoinKey>>)> = parts.into_iter().flatten().collect();
        merge::distinct(self, parts, width, out)
    }

    fn exec_union(&self, node: &'p PhysNode) -> Result<Vec<Snapshot>, CypherError> {
        let PhysNode::Union {
            left,
            right,
            all,
            out_group,
            moves_left,
            moves_right,
            left_groups,
            right_groups,
        } = node
        else {
            unreachable!("a union");
        };
        let width = self.plan.widths[*out_group as usize] as usize;
        let l = self.exec(left, None)?;
        let (lcols, lrows) = merge::to_rows(self, l, left_groups, moves_left, width)?;
        let r = self.exec(right, None)?;
        let (rcols, rrows) = merge::to_rows(self, r, right_groups, moves_right, width)?;
        merge::union(self, lcols, lrows, rcols, rrows, *all, width, *out_group)
    }

    /// `LIMIT` / `SKIP`: the input runs with an early stop or a top-k hint, the cut itself is
    /// sequential over the replayed chunks (it only narrows them).
    fn exec_limit(
        &self,
        node: &'p PhysNode,
        limit: Option<usize>,
    ) -> Result<Vec<Snapshot>, CypherError> {
        let (input, hint) = match node {
            PhysNode::Limit {
                input,
                count,
                materialize,
                ..
            } => {
                let k = self.constant_count(count);
                // a materializing limit (writes below) never reaches the scheduler
                debug_assert!(!materialize);
                (input, k)
            }
            PhysNode::Skip { input, count, .. } => {
                let s = self.constant_count(count);
                (input, limit.zip(s).map(|(k, s)| k.saturating_add(s)))
            }
            _ => unreachable!("a limit or skip"),
        };
        let snaps = self.exec(input, hint)?;
        self.replay_through(node, input, snaps)
    }

    /// Rows a leaf enumerates, when the storage knows without a scan.
    fn leaf_rows(&self, leaf: &PhysNode) -> Option<usize> {
        match leaf {
            PhysNode::NodeScan { seek: Some(_), .. } => None,
            PhysNode::NodeScan { label: Some(l), .. } => Some(self.sync.count_nodes_by_label(l)),
            PhysNode::NodeScan { label: None, .. } => Some(self.sync.node_count()),
            PhysNode::EdgeTypeScan { edge_types, .. } => Some(
                edge_types
                    .iter()
                    .map(|t| self.sync.count_edges_by_type(t))
                    .sum(),
            ),
            _ => None,
        }
    }

    /// A `LIMIT` / `SKIP` count when it is a constant.
    fn constant_count(&self, count: &super::expr::CompiledExpr) -> Option<usize> {
        match eval_constant(count, self.sync) {
            Ok(CypherValue::Integer(k)) if k >= 0 => Some(k as usize),
            _ => None,
        }
    }

    /// A breaker without a parallel form: its input runs in parallel, the operator itself over
    /// the replayed chunks on this thread.
    fn exec_sequential(&self, node: &'p PhysNode) -> Result<Vec<Snapshot>, CypherError> {
        let ch = node.children();
        if ch.len() != 1 {
            // nested sources (a seeded body's first source joining its seed): the subtree as is
            return self.in_worker(|cx| {
                let mut op = build_with(node, &mut Substs::none())?;
                let mut rs = ResultSet::new(&self.plan.widths);
                let live = live_groups(node);
                let mut out = Vec::new();
                while op.next(&mut rs, cx)? {
                    if rs.row_count(&live) > 0 {
                        out.push(rs.groups.clone());
                    }
                }
                Ok(out)
            });
        }
        let input = ch[0];
        let snaps = self.exec(input, None)?;
        self.replay_through(node, input, snaps)
    }

    /// Runs `node` on this thread over `snaps` replayed in place of `input`.
    fn replay_through(
        &self,
        node: &'p PhysNode,
        input: &'p PhysNode,
        snaps: Vec<Snapshot>,
    ) -> Result<Vec<Snapshot>, CypherError> {
        self.in_worker(|cx| {
            let mut subs = Substs::none();
            subs.stand_in(input, Box::new(ReplayOp::new(snaps)));
            let mut op = build_with(node, &mut subs)?;
            let mut rs = ResultSet::new(&self.plan.widths);
            let live = live_groups(node);
            let mut out = Vec::new();
            while op.next(&mut rs, cx)? {
                if rs.row_count(&live) > 0 {
                    let s = rs.groups.clone();
                    self.charge(snapshot_bytes(&s))?;
                    out.push(s);
                }
            }
            Ok(out)
        })
    }

    /// Snapshots of `out_group` filled with `rows` rows of `cols`, in `order` (every row when
    /// `None`), chunk by chunk in parallel.
    pub(crate) fn rows_to_snapshots(
        &self,
        cols: Arc<Vec<Vector>>,
        rows: usize,
        order: Option<Arc<Vec<u32>>>,
        out_group: GroupId,
    ) -> Result<Vec<Snapshot>, CypherError> {
        let n = match &order {
            Some(o) => o.len(),
            None => rows,
        };
        let chunks = n.div_ceil(VECTOR_CAPACITY);
        let widths = &self.plan.widths;
        self.for_parts(chunks, n, |c| {
            let start = c * VECTOR_CAPACITY;
            let end = (start + VECTOR_CAPACITY).min(n);
            let taken: Vec<(u16, Vector)> = match &order {
                Some(o) => cols
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i as u16, v.take(&o[start..end])))
                    .collect(),
                None => cols
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i as u16, v.slice(start, end)))
                    .collect(),
            };
            let mut rs = ResultSet::new(widths);
            super::ops::install(&mut rs, out_group, end - start, taken);
            self.charge(snapshot_bytes(&rs.groups))?;
            Ok(rs.groups)
        })
    }
}

/// The groups that hold rows after `node` runs: a consumer's output group, else what its input
/// holds plus what it adds.
pub(crate) fn live_groups(node: &PhysNode) -> Vec<GroupId> {
    let mut out: Vec<GroupId> = match node {
        PhysNode::NodeScan { out, .. }
        | PhysNode::NodeByIdSeek { out, .. }
        | PhysNode::NodeByIdRange { out, .. }
        | PhysNode::IndexSeek { out, .. }
        | PhysNode::IndexRange { out, .. }
        | PhysNode::TextIndexScan { out, .. }
        | PhysNode::PointIndexScan { out, .. } => vec![out.group],
        PhysNode::EdgeTypeScan { from, rel, to, .. } => [Some(*from), *rel, Some(*to)]
            .into_iter()
            .flatten()
            .map(|p| p.group)
            .collect(),
        PhysNode::EmptyRow { group } | PhysNode::Seed { group, .. } => vec![*group],
        PhysNode::CountSource { out_group, .. }
        | PhysNode::ChainWeights { out_group, .. }
        | PhysNode::Project { out_group, .. }
        | PhysNode::Aggregate { out_group, .. }
        | PhysNode::Sort { out_group, .. }
        | PhysNode::Eager { out_group, .. }
        | PhysNode::Union { out_group, .. }
        | PhysNode::HashJoin { out_group, .. }
        | PhysNode::OptionalMatch { out_group, .. }
        | PhysNode::CallSubquery { out_group, .. }
        | PhysNode::CallInTransactions { out_group, .. }
        | PhysNode::ForEach { out_group, .. }
        | PhysNode::CallProcedure { out_group, .. }
        | PhysNode::BindPathEndpoints { out_group, .. }
        | PhysNode::BindEdgeEndpoints { out_group, .. } => vec![*out_group],
        PhysNode::CrossProduct {
            left_out,
            right_out,
            ..
        } => vec![*left_out, *right_out],
        PhysNode::Limit { input, consume, .. } | PhysNode::Skip { input, consume, .. } => {
            match consume {
                Some(c) => vec![c.out_group],
                None => live_groups(input),
            }
        }
        PhysNode::Expand {
            input, out_group, ..
        }
        | PhysNode::VarExpand {
            input, out_group, ..
        }
        | PhysNode::DistinctExpand {
            input, out_group, ..
        }
        | PhysNode::Unwind {
            input, out_group, ..
        }
        | PhysNode::IndexLookupJoin {
            input, out_group, ..
        }
        | PhysNode::VectorSearch {
            input, out_group, ..
        }
        | PhysNode::ShortestPath {
            input, out_group, ..
        }
        | PhysNode::WeightedShortestPath {
            input, out_group, ..
        } => {
            let mut g = live_groups(input);
            g.push(*out_group);
            g
        }
        PhysNode::Nest { input, inner } => {
            let mut g = live_groups(input);
            g.extend(live_groups(inner));
            g
        }
        other => {
            let mut g = Vec::new();
            for c in other.children() {
                g.extend(live_groups(c));
            }
            g
        }
    };
    out.sort_unstable();
    out.dedup();
    out
}
