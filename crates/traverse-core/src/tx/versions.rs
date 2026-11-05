//! Version metadata beside the delta chains.
//!
//! * **Created runs**: the id ranges each transaction created, so a snapshot can
//!   hide entities created after it without a delta per new entity (bulk creates
//!   stay cheap).
//! * **Ghosts**: memberships an entity had (labels, edge type, B-tree index keys,
//!   adjacency of a deleted edge) when a transaction first touched it. The live
//!   structures only list current state; a snapshot reader adds the ghosts as
//!   candidates and re-checks each one at its snapshot, so over-recording costs
//!   time, never correctness.
//!
//! Both are keyed by the owning transaction's [`CommitInfo`]: an abort drops them,
//! GC drops them once every open snapshot is past their commit.

use std::collections::BTreeMap;
use std::ops::Bound;

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use super::mvcc::{CommitInfo, DeltaArena, TXN_ID_SENTINEL};
use crate::graph::property::IndexKey;
use crate::graph::types::{EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};

/// Ids `[start, end)` created by one transaction.
#[derive(Debug, Clone, Copy)]
struct Run {
    end: u64,
    commit_info: *const CommitInfo,
}

/// Created-id runs of recent transactions, by start id.
#[derive(Debug, Default)]
pub(crate) struct CreatedRuns {
    runs: BTreeMap<u64, Run>,
}

impl CreatedRuns {
    /// Records `id` as created by `ci`, extending that transaction's run when contiguous.
    pub(crate) fn push(&mut self, id: u64, ci: *const CommitInfo) {
        if let Some((_, run)) = self.runs.range_mut(..id).next_back()
            && run.end == id
            && std::ptr::eq(run.commit_info, ci)
        {
            run.end = id + 1;
            return;
        }
        self.runs.insert(
            id,
            Run {
                end: id + 1,
                commit_info: ci,
            },
        );
    }

    /// `[start, end)` created by `ci`, as one run.
    pub(crate) fn push_range(&mut self, start: u64, end: u64, ci: *const CommitInfo) {
        if start >= end {
            return;
        }
        if let Some((_, run)) = self.runs.range_mut(..start).next_back()
            && run.end == start
            && std::ptr::eq(run.commit_info, ci)
        {
            run.end = end;
            return;
        }
        self.runs.insert(
            start,
            Run {
                end,
                commit_info: ci,
            },
        );
    }

    /// The transaction that created `id`, if it's recent.
    #[inline]
    pub(crate) fn creator(&self, id: u64) -> Option<*const CommitInfo> {
        if self.runs.is_empty() {
            return None;
        }
        let (_, run) = self.runs.range(..=id).next_back()?;
        (id < run.end).then_some(run.commit_info)
    }

    /// Ids created by transactions `hidden` says a snapshot can't see.
    pub(crate) fn hidden_ids(
        &self,
        mut hidden: impl FnMut(*const CommitInfo) -> bool,
    ) -> impl Iterator<Item = u64> + '_ {
        let ranges: Vec<(u64, u64)> = self
            .runs
            .iter()
            .filter(|(_, r)| hidden(r.commit_info))
            .map(|(&s, r)| (s, r.end))
            .collect();
        ranges.into_iter().flat_map(|(s, e)| s..e)
    }

    fn retain(&mut self, mut keep: impl FnMut(*const CommitInfo) -> bool) {
        self.runs.retain(|_, r| keep(r.commit_info));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
}

/// A node membership recorded at first touch.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NodeGhost {
    Label(LabelId),
    Index(LabelId, PropertyKeyId, IndexKey),
    Deleted,
}

/// An edge membership recorded at first touch.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EdgeGhost {
    Type(EdgeTypeId),
    Index(EdgeTypeId, PropertyKeyId, IndexKey),
    Deleted { source: NodeId, target: NodeId },
}

#[derive(Debug)]
struct Record<G> {
    commit_info: *const CommitInfo,
    items: SmallVec<[G; 4]>,
}

type KeyedIds<T> = FxHashMap<(u32, PropertyKeyId), BTreeMap<IndexKey, FxHashSet<T>>>;

/// A committed transaction's arena and commit info, kept until no snapshot can reach its deltas.
#[derive(Debug)]
struct Retired {
    ts: u64,
    _arena: DeltaArena,
    _commit_info: Box<CommitInfo>,
}

/// Runs + ghosts of one graph; see the module docs.
#[derive(Debug, Default)]
pub(crate) struct Versions {
    pub(crate) node_runs: CreatedRuns,
    pub(crate) edge_runs: CreatedRuns,
    node_records: FxHashMap<NodeId, SmallVec<[Record<NodeGhost>; 1]>>,
    edge_records: FxHashMap<EdgeId, SmallVec<[Record<EdgeGhost>; 1]>>,
    labels: FxHashMap<LabelId, FxHashSet<NodeId>>,
    node_index: KeyedIds<NodeId>,
    deleted_nodes: FxHashSet<NodeId>,
    types: FxHashMap<EdgeTypeId, FxHashSet<EdgeId>>,
    edge_index: KeyedIds<EdgeId>,
    deleted_edges: FxHashSet<EdgeId>,
    deleted_out: FxHashMap<NodeId, FxHashSet<EdgeId>>,
    deleted_in: FxHashMap<NodeId, FxHashSet<EdgeId>>,
    /// Committed transactions' deltas, freed by [`Self::collect`] / [`Self::clear`].
    retired: Vec<Retired>,
}

// SAFETY: the raw `CommitInfo` pointers are only compared and read through atomics; the
// `CommitInfo`s outlive their entries (retired with the transaction's deltas), and all mutation
// happens under the store's structural write lock.
unsafe impl Send for Versions {}
// SAFETY: see `Send`
unsafe impl Sync for Versions {}

/// Removes `id` from `map[key]`, dropping the emptied set.
fn unset<K: std::hash::Hash + Eq, T: std::hash::Hash + Eq>(
    map: &mut FxHashMap<K, FxHashSet<T>>,
    key: &K,
    id: &T,
) {
    if let Some(set) = map.get_mut(key) {
        set.remove(id);
        if set.is_empty() {
            map.remove(key);
        }
    }
}

fn unset_keyed<T: std::hash::Hash + Eq>(
    map: &mut KeyedIds<T>,
    key: (u32, PropertyKeyId),
    value: &IndexKey,
    id: &T,
) {
    if let Some(tree) = map.get_mut(&key) {
        if let Some(set) = tree.get_mut(value) {
            set.remove(id);
            if set.is_empty() {
                tree.remove(value);
            }
        }
        if tree.is_empty() {
            map.remove(&key);
        }
    }
}

/// Committed at or before `watermark` (uncommitted never qualifies)?
fn settled(ci: *const CommitInfo, watermark: u64) -> bool {
    // SAFETY: a CommitInfo outlives every entry pointing at it
    let ts = unsafe { (*ci).timestamp.load(std::sync::atomic::Ordering::Acquire) };
    ts != TXN_ID_SENTINEL && ts <= watermark
}

impl Versions {
    /// Nothing recorded (readers can take the plain fast paths)?
    pub(crate) fn is_empty(&self) -> bool {
        self.node_runs.is_empty()
            && self.edge_runs.is_empty()
            && self.node_records.is_empty()
            && self.edge_records.is_empty()
    }

    /// Records node memberships for transaction `ci` (appends to its record if one exists).
    pub(crate) fn record_node(
        &mut self,
        id: NodeId,
        ci: *const CommitInfo,
        items: impl IntoIterator<Item = NodeGhost>,
    ) {
        let records = self.node_records.entry(id).or_default();
        let pos = match records.iter().position(|r| std::ptr::eq(r.commit_info, ci)) {
            Some(p) => p,
            None => {
                records.push(Record {
                    commit_info: ci,
                    items: SmallVec::new(),
                });
                records.len() - 1
            }
        };
        for item in items {
            match &item {
                NodeGhost::Label(l) => {
                    self.labels.entry(*l).or_default().insert(id);
                }
                NodeGhost::Index(l, k, v) => {
                    self.node_index
                        .entry((l.0, *k))
                        .or_default()
                        .entry(v.clone())
                        .or_default()
                        .insert(id);
                }
                NodeGhost::Deleted => {
                    self.deleted_nodes.insert(id);
                }
            }
            records[pos].items.push(item);
        }
    }

    /// Edge counterpart of [`Self::record_node`].
    pub(crate) fn record_edge(
        &mut self,
        id: EdgeId,
        ci: *const CommitInfo,
        items: impl IntoIterator<Item = EdgeGhost>,
    ) {
        let records = self.edge_records.entry(id).or_default();
        let pos = match records.iter().position(|r| std::ptr::eq(r.commit_info, ci)) {
            Some(p) => p,
            None => {
                records.push(Record {
                    commit_info: ci,
                    items: SmallVec::new(),
                });
                records.len() - 1
            }
        };
        for item in items {
            match &item {
                EdgeGhost::Type(t) => {
                    self.types.entry(*t).or_default().insert(id);
                }
                EdgeGhost::Index(t, k, v) => {
                    self.edge_index
                        .entry((t.0, *k))
                        .or_default()
                        .entry(v.clone())
                        .or_default()
                        .insert(id);
                }
                EdgeGhost::Deleted { source, target } => {
                    self.deleted_edges.insert(id);
                    self.deleted_out.entry(*source).or_default().insert(id);
                    self.deleted_in.entry(*target).or_default().insert(id);
                }
            }
            records[pos].items.push(item);
        }
    }

    /// Drops every run and record of transactions `drop` selects.
    fn drop_where(&mut self, mut drop: impl FnMut(*const CommitInfo) -> bool) {
        self.node_runs.retain(|ci| !drop(ci));
        self.edge_runs.retain(|ci| !drop(ci));

        let mut emptied = Vec::new();
        for (&id, records) in self.node_records.iter_mut() {
            let mut gone: SmallVec<[NodeGhost; 4]> = SmallVec::new();
            records.retain(|r| {
                if drop(r.commit_info) {
                    gone.extend(r.items.iter().cloned());
                    false
                } else {
                    true
                }
            });
            for item in gone {
                // another live record may still list the same membership
                if records.iter().any(|r| r.items.contains(&item)) {
                    continue;
                }
                match &item {
                    NodeGhost::Label(l) => unset(&mut self.labels, l, &id),
                    NodeGhost::Index(l, k, v) => {
                        unset_keyed(&mut self.node_index, (l.0, *k), v, &id)
                    }
                    NodeGhost::Deleted => {
                        self.deleted_nodes.remove(&id);
                    }
                }
            }
            if records.is_empty() {
                emptied.push(id);
            }
        }
        for id in emptied {
            self.node_records.remove(&id);
        }

        let mut emptied = Vec::new();
        for (&id, records) in self.edge_records.iter_mut() {
            let mut gone: SmallVec<[EdgeGhost; 4]> = SmallVec::new();
            records.retain(|r| {
                if drop(r.commit_info) {
                    gone.extend(r.items.iter().cloned());
                    false
                } else {
                    true
                }
            });
            for item in gone {
                if records.iter().any(|r| r.items.contains(&item)) {
                    continue;
                }
                match &item {
                    EdgeGhost::Type(t) => unset(&mut self.types, t, &id),
                    EdgeGhost::Index(t, k, v) => {
                        unset_keyed(&mut self.edge_index, (t.0, *k), v, &id)
                    }
                    EdgeGhost::Deleted { source, target } => {
                        self.deleted_edges.remove(&id);
                        unset(&mut self.deleted_out, source, &id);
                        unset(&mut self.deleted_in, target, &id);
                    }
                }
            }
            if records.is_empty() {
                emptied.push(id);
            }
        }
        for id in emptied {
            self.edge_records.remove(&id);
        }
    }

    /// Keeps a committed transaction's arena and commit info alive until collection.
    pub(crate) fn retire(&mut self, ts: u64, arena: DeltaArena, commit_info: Box<CommitInfo>) {
        self.retired.push(Retired {
            ts,
            _arena: arena,
            _commit_info: commit_info,
        });
    }

    /// Forgets an aborted transaction.
    pub(crate) fn discard(&mut self, ci: *const CommitInfo) {
        self.drop_where(|c| std::ptr::eq(c, ci));
    }

    /// Forgets what every snapshot at or after `watermark` can do without, then frees the retired
    /// transactions at or before it (the caller has cut every chain there).
    pub(crate) fn collect(&mut self, watermark: u64) {
        self.drop_where(|c| settled(c, watermark));
        self.retired.retain(|r| r.ts > watermark);
    }

    /// Forgets everything (no snapshot is open).
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    // -- reader lookups -----------------------------------------------

    /// Nodes that had `label` before a recent change.
    pub(crate) fn label_ghosts(&self, label: LabelId) -> impl Iterator<Item = NodeId> + '_ {
        self.labels.get(&label).into_iter().flatten().copied()
    }

    /// Nodes whose `(label, key)` index entry was `value` before a recent change.
    pub(crate) fn node_index_ghosts(
        &self,
        label: LabelId,
        key: PropertyKeyId,
        value: &IndexKey,
    ) -> impl Iterator<Item = NodeId> + '_ {
        self.node_index
            .get(&(label.0, key))
            .and_then(|t| t.get(value))
            .into_iter()
            .flatten()
            .copied()
    }

    /// Nodes whose `(label, key)` index entry was in the range before a recent change.
    pub(crate) fn node_index_range_ghosts<'a>(
        &'a self,
        label: LabelId,
        key: PropertyKeyId,
        lower: Bound<&'a IndexKey>,
        upper: Bound<&'a IndexKey>,
    ) -> impl Iterator<Item = NodeId> + 'a {
        self.node_index
            .get(&(label.0, key))
            .into_iter()
            .flat_map(move |t| t.range((lower, upper)))
            .flat_map(|(_, ids)| ids.iter().copied())
    }

    /// Recently deleted nodes.
    pub(crate) fn deleted_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.deleted_nodes.iter().copied()
    }

    /// Edges that had `edge_type` before a recent change.
    pub(crate) fn type_ghosts(&self, edge_type: EdgeTypeId) -> impl Iterator<Item = EdgeId> + '_ {
        self.types.get(&edge_type).into_iter().flatten().copied()
    }

    /// Edges whose `(type, key)` index entry was `value` before a recent change.
    pub(crate) fn edge_index_ghosts(
        &self,
        edge_type: EdgeTypeId,
        key: PropertyKeyId,
        value: &IndexKey,
    ) -> impl Iterator<Item = EdgeId> + '_ {
        self.edge_index
            .get(&(edge_type.0, key))
            .and_then(|t| t.get(value))
            .into_iter()
            .flatten()
            .copied()
    }

    /// Recently deleted edges.
    pub(crate) fn deleted_edges(&self) -> impl Iterator<Item = EdgeId> + '_ {
        self.deleted_edges.iter().copied()
    }

    /// Recently deleted edges out of `node`.
    pub(crate) fn deleted_out(&self, node: NodeId) -> impl Iterator<Item = EdgeId> + '_ {
        self.deleted_out.get(&node).into_iter().flatten().copied()
    }

    /// Recently deleted edges into `node`.
    pub(crate) fn deleted_in(&self, node: NodeId) -> impl Iterator<Item = EdgeId> + '_ {
        self.deleted_in.get(&node).into_iter().flatten().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed(ts: u64) -> Box<CommitInfo> {
        let ci = Box::new(CommitInfo::new_uncommitted());
        ci.commit(ts);
        ci
    }

    #[test]
    fn runs_extend_and_split_per_transaction() {
        let a = committed(10);
        let b = committed(20);
        let mut runs = CreatedRuns::default();
        runs.push(0, &*a);
        runs.push(1, &*a);
        runs.push(2, &*b);
        runs.push(3, &*a);
        assert!(std::ptr::eq(runs.creator(1).unwrap(), &*a));
        assert!(std::ptr::eq(runs.creator(2).unwrap(), &*b));
        assert!(std::ptr::eq(runs.creator(3).unwrap(), &*a));
        assert!(runs.creator(4).is_none());
        let hidden: Vec<u64> = runs
            .hidden_ids(|ci| {
                // SAFETY: `a` and `b` outlive `runs`
                unsafe { (*ci).timestamp.load(std::sync::atomic::Ordering::Relaxed) > 15 }
            })
            .collect();
        assert_eq!(hidden, vec![2]);
    }

    #[test]
    fn ghosts_survive_while_another_record_lists_them() {
        let a = committed(10);
        let b = committed(20);
        let mut v = Versions::default();
        let n = NodeId(7);
        v.record_node(n, &*a, [NodeGhost::Label(LabelId(1))]);
        v.record_node(n, &*b, [NodeGhost::Label(LabelId(1)), NodeGhost::Deleted]);
        v.collect(10);
        assert_eq!(v.label_ghosts(LabelId(1)).collect::<Vec<_>>(), vec![n]);
        assert_eq!(v.deleted_nodes().count(), 1);
        v.collect(20);
        assert!(v.is_empty());
        assert_eq!(v.label_ghosts(LabelId(1)).count(), 0);
        assert_eq!(v.deleted_nodes().count(), 0);
    }

    #[test]
    fn discard_drops_one_transaction() {
        let a = committed(10);
        let b = Box::new(CommitInfo::new_uncommitted());
        let mut v = Versions::default();
        v.record_edge(
            EdgeId(3),
            &*b,
            [EdgeGhost::Deleted {
                source: NodeId(1),
                target: NodeId(2),
            }],
        );
        v.edge_runs.push(9, &*a);
        v.discard(&*b);
        assert_eq!(v.deleted_out(NodeId(1)).count(), 0);
        assert!(v.edge_runs.creator(9).is_some());
        // uncommitted entries are never collected by a watermark
        v.record_node(NodeId(1), &*b, [NodeGhost::Deleted]);
        v.collect(u64::MAX - 1);
        assert_eq!(v.deleted_nodes().count(), 1);
    }
}
