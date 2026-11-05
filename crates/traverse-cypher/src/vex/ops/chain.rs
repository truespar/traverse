//! Counting paths along a chain of hops without building them: a per-node path count carried
//! hop by hop over the adjacency (the last hop folds into the counts, or into one total). The
//! carried count is a count of walks, which is the count of relationship-distinct paths when
//! the graph has no self-loop; with one, or when the view can't say, the exact plan runs.

use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use traverse_core::graph::batch::AdjBatch;
use traverse_core::graph::types::{Direction, LabelId, NodeId};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;

use super::super::chunk::{DataPos, GroupId, ResultSet};
use super::super::ctx::ExecCtx;
use super::super::plan::{ChainHop, SeedRow};
use super::super::vector::{VECTOR_CAPACITY, Vector, VectorBuilder};
use super::expand::type_filter;
use super::{PhysOperator, install};

/// Sources per task in the parallel passes.
const CHUNK: usize = 2048;

/// The path counts at the end of a chain, as `(node, count)` rows, or one total.
pub struct ChainWeightsOp<'p> {
    out: DataPos,
    weight: DataPos,
    out_group: GroupId,
    from_label: Option<&'p str>,
    hops: &'p [ChainHop],
    count_only: bool,
    /// The exact plan, run when the fast path doesn't apply.
    exact: Box<dyn PhysOperator + 'p>,
    /// Where the exact plan puts the node and the count.
    exact_cols: (DataPos, DataPos),
    mode: Option<Mode>,
    rows: Vec<(NodeId, u64)>,
    total: u64,
    pos: usize,
    done: bool,
}

enum Mode {
    Fast,
    Exact,
}

impl<'p> ChainWeightsOp<'p> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        out: DataPos,
        weight: DataPos,
        out_group: GroupId,
        from_label: Option<&'p str>,
        hops: &'p [ChainHop],
        count_only: bool,
        exact: Box<dyn PhysOperator + 'p>,
        exact_cols: (DataPos, DataPos),
    ) -> Self {
        Self {
            out,
            weight,
            out_group,
            from_label,
            hops,
            count_only,
            exact,
            exact_cols,
            mode: None,
            rows: Vec::new(),
            total: 0,
            pos: 0,
            done: false,
        }
    }

    /// Runs the carried counts over the chain.
    fn compute(&mut self, cx: &ExecCtx<'_>) -> Result<(), CypherError> {
        let graph = cx.view();
        let n_ids = graph.highest_node_id() as usize;
        let mut charge = crate::memory::Charge::new();
        charge.add(n_ids * 16)?;
        let mut cur: Vec<u64> = vec![0; n_ids];
        match self.from_label {
            Some(l) => match graph.label_bitmap(l) {
                Some(words) => {
                    for (w, &word) in words.iter().enumerate() {
                        let mut bits = word;
                        while bits != 0 {
                            let v = w * 64 + bits.trailing_zeros() as usize;
                            if v < n_ids {
                                cur[v] = 1;
                            }
                            bits &= bits - 1;
                        }
                    }
                }
                None => {
                    for v in graph.nodes_by_label(l) {
                        if (v.0 as usize) < n_ids {
                            cur[v.0 as usize] = 1;
                        }
                    }
                }
            },
            None => {
                for v in graph.nodes_vec() {
                    if (v.0 as usize) < n_ids {
                        cur[v.0 as usize] = 1;
                    }
                }
            }
        }
        let query = crate::query::current();
        let sync = graph.as_sync_ref();
        for (i, hop) in self.hops.iter().enumerate() {
            let last = i + 1 == self.hops.len();
            let tf = type_filter(graph, &hop.edge_types);
            let covers = hop
                .to_label
                .as_deref()
                .is_none_or(|l| graph.count_nodes_by_label(l) == graph.node_count());
            let label: Option<LabelId> = if covers {
                None
            } else {
                hop.to_label.as_deref().and_then(|l| cx.label_id(l))
            };
            let active: Vec<NodeId> = cur
                .iter()
                .enumerate()
                .filter(|(_, w)| **w > 0)
                .map(|(v, _)| NodeId(v as u64))
                .collect();
            cx.check()?;
            if last && self.count_only {
                // the total: every source's count times its qualifying edges
                let typed_ok = tf.is_none()
                    || (hop.edge_types.len() == 1
                        && graph.relationship_type_names() == hop.edge_types);
                let by_degree = covers && typed_ok && hop.dir != Direction::Both;
                let cur = &cur;
                let per_chunk = |g: &dyn GraphView, chunk: &[NodeId]| -> Result<u64, CypherError> {
                    let mut sum = 0u64;
                    if by_degree {
                        let mut degrees = Vec::with_capacity(chunk.len());
                        g.degrees(chunk, hop.dir, &mut degrees);
                        for (v, d) in chunk.iter().zip(degrees) {
                            sum += cur[v.0 as usize] * d as u64;
                        }
                    } else {
                        let mut batch = AdjBatch::new();
                        g.expand_batch(chunk, hop.dir, &tf, &mut batch)?;
                        let has: Option<Vec<bool>> = label.map(|lid| {
                            let mut has = Vec::with_capacity(batch.len());
                            g.nodes_have_label(&batch.neighbor, lid, &mut has);
                            has
                        });
                        for (k, &lane) in batch.src_lane.iter().enumerate() {
                            if has.as_ref().is_none_or(|h| h[k]) {
                                sum += cur[chunk[lane as usize].0 as usize];
                            }
                        }
                    }
                    Ok(sum)
                };
                self.total = match sync {
                    Some(s) if active.len() >= CHUNK * 2 => active
                        .par_chunks(CHUNK)
                        .map(|chunk| {
                            let _q = crate::query::enter_opt(query.as_ref());
                            crate::query::check_interrupt()?;
                            per_chunk(s, chunk)
                        })
                        .try_reduce(|| 0u64, |a, b| Ok(a + b))?,
                    _ => {
                        let mut t = 0u64;
                        for chunk in active.chunks(CHUNK) {
                            cx.check()?;
                            t += per_chunk(graph, chunk)?;
                        }
                        t
                    }
                };
                return Ok(());
            }
            // the first hop from every node is each target's degree the other way round
            let uniform = i == 0
                && self
                    .from_label
                    .is_none_or(|l| graph.count_nodes_by_label(l) == graph.node_count());
            let typed_ok = tf.is_none()
                || (hop.edge_types.len() == 1 && graph.relationship_type_names() == hop.edge_types);
            if uniform && typed_ok && hop.dir != Direction::Both {
                let back = match hop.dir {
                    Direction::Outgoing => Direction::Incoming,
                    _ => Direction::Outgoing,
                };
                let targets: Vec<NodeId> = match (&hop.to_label, label) {
                    (Some(l), Some(_)) => graph.nodes_by_label(l),
                    _ => graph.nodes_vec(),
                };
                let mut next = vec![0u64; n_ids];
                let per_chunk =
                    |g: &dyn GraphView, chunk: &[NodeId]| -> Result<Vec<u64>, CypherError> {
                        let mut degrees = Vec::with_capacity(chunk.len());
                        g.degrees(chunk, back, &mut degrees);
                        Ok(degrees.into_iter().map(|d| d as u64).collect())
                    };
                let counts: Vec<Vec<u64>> = match sync {
                    Some(s) if targets.len() >= CHUNK * 2 => targets
                        .par_chunks(CHUNK)
                        .map(|chunk| {
                            let _q = crate::query::enter_opt(query.as_ref());
                            crate::query::check_interrupt()?;
                            per_chunk(s, chunk)
                        })
                        .collect::<Result<_, _>>()?,
                    _ => {
                        let mut out = Vec::new();
                        for chunk in targets.chunks(CHUNK) {
                            cx.check()?;
                            out.push(per_chunk(graph, chunk)?);
                        }
                        out
                    }
                };
                for (chunk, degs) in targets.chunks(CHUNK).zip(counts) {
                    for (v, d) in chunk.iter().zip(degs) {
                        if (v.0 as usize) < n_ids {
                            next[v.0 as usize] = d;
                        }
                    }
                }
                cur = next;
                continue;
            }
            // carry the counts over the hop
            let next: Vec<AtomicU64> = (0..n_ids).map(|_| AtomicU64::new(0)).collect();
            let cur_ref = &cur;
            let next_ref = &next;
            let per_chunk = |g: &dyn GraphView, chunk: &[NodeId]| -> Result<(), CypherError> {
                let mut batch = AdjBatch::new();
                g.expand_batch(chunk, hop.dir, &tf, &mut batch)?;
                let has: Option<Vec<bool>> = label.map(|lid| {
                    let mut has = Vec::with_capacity(batch.len());
                    g.nodes_have_label(&batch.neighbor, lid, &mut has);
                    has
                });
                for (k, &lane) in batch.src_lane.iter().enumerate() {
                    if has.as_ref().is_none_or(|h| h[k]) {
                        let w = cur_ref[chunk[lane as usize].0 as usize];
                        let to = batch.neighbor[k].0 as usize;
                        if to < n_ids {
                            next_ref[to].fetch_add(w, Ordering::Relaxed);
                        }
                    }
                }
                Ok(())
            };
            match sync {
                Some(s) if active.len() >= CHUNK * 2 => {
                    active.par_chunks(CHUNK).try_for_each(|chunk| {
                        let _q = crate::query::enter_opt(query.as_ref());
                        crate::query::check_interrupt()?;
                        per_chunk(s, chunk)
                    })?;
                }
                _ => {
                    for chunk in active.chunks(CHUNK) {
                        cx.check()?;
                        per_chunk(graph, chunk)?;
                    }
                }
            }
            cur = next.into_iter().map(|a| a.into_inner()).collect();
        }
        self.rows = cur
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 0)
            .map(|(v, w)| (NodeId(v as u64), *w))
            .collect();
        charge.add(self.rows.len() * 16)?;
        charge.flush()?;
        Ok(())
    }
}

impl PhysOperator for ChainWeightsOp<'_> {
    fn next(&mut self, rs: &mut ResultSet, cx: &mut ExecCtx<'_>) -> Result<bool, CypherError> {
        if self.mode.is_none() {
            // a chain of several hops keeps its relationships distinct; carried counts can't
            // tell a path reusing a self-loop from one that doesn't
            let fast = self.hops.len() < 2 || cx.view().self_loops() == Some(0);
            if fast {
                self.compute(cx)?;
                self.mode = Some(Mode::Fast);
            } else {
                self.mode = Some(Mode::Exact);
            }
        }
        match self.mode.as_ref().unwrap() {
            Mode::Exact => {
                if !self.exact.next(rs, cx)? {
                    return Ok(false);
                }
                let (node_pos, count_pos) = self.exact_cols;
                let g = rs.group(count_pos.group);
                let sel: Vec<u32> = g.selected_lanes().map(|l| l as u32).collect();
                let n = sel.len();
                let mut cols = vec![(self.weight.col, g.cols[count_pos.col as usize].take(&sel))];
                if !self.count_only {
                    cols.push((self.out.col, g.cols[node_pos.col as usize].take(&sel)));
                }
                install(rs, self.out_group, n, cols);
                Ok(true)
            }
            Mode::Fast => {
                if self.done {
                    return Ok(false);
                }
                if self.count_only {
                    self.done = true;
                    install(
                        rs,
                        self.out_group,
                        1,
                        vec![(
                            self.weight.col,
                            Vector::ints(vec![self.total.min(i64::MAX as u64) as i64]),
                        )],
                    );
                    return Ok(true);
                }
                if self.pos >= self.rows.len() {
                    self.done = true;
                    return Ok(false);
                }
                let end = (self.pos + VECTOR_CAPACITY).min(self.rows.len());
                let mut nodes = VectorBuilder::with_capacity(end - self.pos);
                let mut weights = Vec::with_capacity(end - self.pos);
                for (v, w) in &self.rows[self.pos..end] {
                    nodes.push_node(*v);
                    weights.push((*w).min(i64::MAX as u64) as i64);
                }
                let n = end - self.pos;
                self.pos = end;
                install(
                    rs,
                    self.out_group,
                    n,
                    vec![
                        (self.out.col, nodes.finish()),
                        (self.weight.col, Vector::ints(weights)),
                    ],
                );
                cx.tick()?;
                Ok(true)
            }
        }
    }

    fn reset(&mut self) {
        self.mode = None;
        self.rows.clear();
        self.total = 0;
        self.pos = 0;
        self.done = false;
        self.exact.reset();
    }

    fn seed(&mut self, rows: Vec<SeedRow>) {
        self.exact.seed(rows);
    }
}
