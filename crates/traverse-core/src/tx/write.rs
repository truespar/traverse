//! [`WriteTx`]: the single writer.
//!
//! Mutates the graph in place under the writer mutex. The first time it touches an
//! existing node or edge it links one image of the pre-transaction state into the
//! object's chain and records the object's memberships as ghosts; creations go into
//! a run. Snapshot readers resolve through those (see `read.rs`); abort restores
//! from the same images; commit publishes them all with one atomic store.

use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::sync::WriterGuard;

use rustc_hash::FxHashSet;

use super::mvcc::{CommitInfo, Delta, DeltaAction, DeltaArena, create_and_link_delta};
use super::timestamp::HybridTimestamp;
use super::versions::{EdgeGhost, NodeGhost};
use super::view::GraphView;
use super::{MvccStore, encode_ts};
use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::graph::constraint::{ConstraintDescriptor, ConstraintType};
use crate::graph::edge::Edge;
use crate::graph::node::Node;
use crate::graph::property::PropertyMap;
#[cfg(not(target_arch = "wasm32"))]
use crate::graph::property::PropertyValue;
use crate::graph::stats::GraphStatistics;
use crate::graph::types::{Direction, EdgeId, NodeId, PropertyKeyId};

#[cfg(not(target_arch = "wasm32"))]
use crate::storage::rlog::Op as RedoOp;

/// Stand-in when the rlog is compiled out (wasm32): same WriteTx fields on every target.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone)]
enum RedoOp {}

/// Redo-log `kind` offset marking a relationship (not node) constraint, so replay
/// routes to `create_relationship_constraint`. Node kinds are 1..=21, relationship
/// kinds the same + 100.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const REL_CONSTRAINT_KIND_OFFSET: u8 = 100;

/// Constraint type -> redo-log `kind` byte: 1 = UNIQUE, 2 = EXISTS, 3 = KEY,
/// 10..=21 = property type (in `PropertyTypeKind` declaration order).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn constraint_redo_kind(ctype: &ConstraintType) -> u8 {
    use crate::graph::constraint::PropertyTypeKind as K;
    match ctype {
        ConstraintType::Unique => 1,
        ConstraintType::Exists => 2,
        ConstraintType::NodeKey => 3,
        ConstraintType::Type(k) => match k {
            K::String => 10,
            K::Integer => 11,
            K::Float => 12,
            K::Boolean => 13,
            K::Point => 14,
            K::Date => 15,
            K::LocalTime => 16,
            K::ZonedTime => 17,
            K::LocalDatetime => 18,
            K::ZonedDatetime => 19,
            K::Duration => 20,
            K::List => 21,
        },
    }
}

/// How to undo one schema change on abort. Data changes need no log: the images and runs undo
/// them.
enum SchemaUndo {
    /// A node B-tree index (single or composite) was created.
    DropIndex(String),
    /// A node B-tree index was dropped; `property` joins composite keys with the separator.
    RestoreIndex {
        name: String,
        label: String,
        property: String,
    },
    DropEdgeIndex(String),
    RestoreEdgeIndex {
        name: String,
        edge_type: String,
        property: Option<String>,
    },
    DropConstraint(String),
    RestoreConstraint(ConstraintDescriptor),
    DropVectorIndex(String),
    RestoreVectorIndex {
        name: String,
        label: String,
        property: String,
        dim: usize,
        sim: crate::graph::vector::Similarity,
        params: crate::graph::vector::HnswParams,
    },
    DropFulltextIndex(String),
    RestoreFulltextIndex {
        name: String,
        labels: Vec<String>,
        properties: Vec<String>,
        analyzer: crate::graph::fulltext::Analyzer,
    },
    DropPointIndex(String),
    RestorePointIndex {
        name: String,
        label: String,
        property: String,
    },
    DropTextIndex(String),
    RestoreTextIndex {
        name: String,
        label: String,
        property: String,
    },
    RestoreStatistics(Option<Arc<GraphStatistics>>),
}

/// Write transaction mutating the graph in place.
///
/// Holds the writer mutex for its whole life. Commit publishes with one atomic
/// store; abort (or drop without commit) restores every touched object.
pub struct WriteTx<'a> {
    store: &'a MvccStore,
    begin_ts: HybridTimestamp,
    /// Arena for our deltas (stable pointers); retired at commit.
    delta_arena: DeltaArena,
    /// Commit metadata shared by our deltas, runs and ghost records.
    commit_info: Box<CommitInfo>,
    /// Created here: no image needed, abort deletes them.
    created_nodes: FxHashSet<NodeId>,
    created_edges: FxHashSet<EdgeId>,
    /// Id ranges of bulk creations (`[start, end)`), kept apart from the sets.
    created_node_ranges: Vec<(u64, u64)>,
    created_edge_ranges: Vec<(u64, u64)>,
    /// Existing objects imaged here (first touch done).
    touched_nodes: FxHashSet<NodeId>,
    touched_edges: FxHashSet<EdgeId>,
    /// Schema changes to undo on abort, oldest first.
    schema_undo: Vec<SchemaUndo>,
    /// Logical ops for the redo log, flushed to the WAL in `commit()`. Empty with no
    /// [`MvccStore::log_writer`] or with `skip_redo`.
    redo_log: Vec<RedoOp>,
    /// Skip redo encoding even with a writer attached; recovery sets it so replayed
    /// transactions aren't logged again.
    skip_redo: bool,
    committed: bool,
    /// Changes so far (see `GraphView::write_count`).
    mutations: u64,
    /// An auto-commit statement's transaction: `CALL { ... } IN TRANSACTIONS` may commit batches.
    batches: bool,
    /// The writer lock, held for the transaction's life (read only by native-only bulk loads).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    write_guard: WriterGuard<'a>,
    memory_budget: Option<usize>,
}

/// The image `ci` linked into a chain (its deltas sit at the head while it's the writer).
fn own_image<'a>(head: *const Delta, ci: *const CommitInfo) -> Option<&'a DeltaAction> {
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: we hold the write locks; our deltas live in our arena
        let d: &'a Delta = unsafe { &*cur };
        if !std::ptr::eq(d.commit_info, ci) {
            return None;
        }
        if d.action.is_image() {
            return Some(&d.action);
        }
        cur = d.next();
    }
    None
}

/// Unlinks `ci`'s deltas from the front of a chain; `true` if the chain is now empty.
fn pop_own(head: &AtomicPtr<Delta>, ci: *const CommitInfo) -> bool {
    let mut cur = head.load(Ordering::Relaxed);
    // SAFETY: we hold the write locks; chain deltas are alive
    while !cur.is_null() && std::ptr::eq(unsafe { (*cur).commit_info }, ci) {
        // SAFETY: as above
        cur = unsafe { (*cur).next() };
    }
    head.store(cur, Ordering::Release);
    cur.is_null()
}

/// String-keyed copy of an interned map, for the redo log.
#[cfg(not(target_arch = "wasm32"))]
fn redo_props(
    g: &Graph,
    props: &crate::graph::property::InternedPropertyMap,
) -> Vec<(String, PropertyValue)> {
    props
        .iter()
        .map(|(k, v)| (g.strings().resolve(k.0).to_string(), v))
        .collect()
}

impl<'a> WriteTx<'a> {
    pub(super) fn new(
        store: &'a MvccStore,
        begin_ts: HybridTimestamp,
        write_guard: WriterGuard<'a>,
    ) -> Self {
        let memory_budget = store.memory_budget();
        // no writer attached: skip redo up front (saves per-mutator allocations);
        // recovery's internal transactions use `set_skip_redo`
        #[cfg(not(target_arch = "wasm32"))]
        let skip_redo = store.log_writer.is_none();
        #[cfg(target_arch = "wasm32")]
        let skip_redo = true;
        Self {
            store,
            begin_ts,
            delta_arena: DeltaArena::new(),
            commit_info: Box::new(CommitInfo::new_uncommitted()),
            created_nodes: FxHashSet::default(),
            created_edges: FxHashSet::default(),
            created_node_ranges: Vec::new(),
            created_edge_ranges: Vec::new(),
            touched_nodes: FxHashSet::default(),
            touched_edges: FxHashSet::default(),
            schema_undo: Vec::new(),
            redo_log: Vec::new(),
            skip_redo,
            committed: false,
            mutations: 0,
            batches: false,
            write_guard,
            memory_budget,
        }
    }

    /// Marks this as an auto-commit statement's transaction: `CALL { ... } IN TRANSACTIONS` may
    /// then commit each batch on its own ([`GraphView::commit_batch`]) and roll back a failed one.
    /// Without it the transaction counts as explicit and such statements are refused.
    pub fn allow_batch_commits(&mut self) {
        self.batches = true;
    }

    /// Disables redo logging for this transaction; recovery uses it to replay without re-logging.
    pub fn set_skip_redo(&mut self) {
        self.skip_redo = true;
        self.redo_log.clear();
    }

    /// Pushes a redo op; no-op when redo logging is off.
    // on wasm32 every call site is cfg'd out (no RedoOp exists there), so this is dead by design
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    #[inline]
    fn record_redo(&mut self, _op: RedoOp) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.skip_redo {
                self.redo_log.push(_op);
            }
        }
    }

    #[inline]
    fn graph(&self) -> &Graph {
        // SAFETY: we hold the write_lock mutex.
        unsafe { self.store.graph() }
    }

    #[inline]
    fn graph_mut(&mut self) -> &mut Graph {
        // SAFETY: we hold the write_lock mutex.
        unsafe { self.store.graph_mut() }
    }

    /// The live graph as this writer sees it (its own writes included).
    pub(crate) fn graph_ref(&self) -> &Graph {
        self.graph()
    }

    /// The writer lock this transaction holds.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn writer_guard(&self) -> &WriterGuard<'a> {
        &self.write_guard
    }

    /// Begin timestamp.
    pub fn begin_ts(&self) -> HybridTimestamp {
        self.begin_ts
    }

    #[inline]
    fn ci(&self) -> *const CommitInfo {
        &*self.commit_info
    }

    /// First touch of an existing node: link its pre-transaction image and record its
    /// memberships as ghosts. Caller holds the structural write lock.
    fn touch_node(&mut self, id: NodeId) -> Result<()> {
        self.mutations += 1;
        if self.owns_node(id) || self.touched_nodes.contains(&id) {
            return Ok(());
        }
        let ci = self.ci();
        // SAFETY: writer mutex + structural write lock
        let g = unsafe { self.store.graph_mut() };
        let node = g.get_node(id)?;
        let image = DeltaAction::NodeImage {
            labels: node.labels.clone(),
            properties: node.properties.clone(),
        };
        let ghosts = g.node_ghosts(id);
        create_and_link_delta(&mut self.delta_arena, g.node_delta_head_mut(id), ci, image);
        g.versions_mut().record_node(id, ci, ghosts);
        self.touched_nodes.insert(id);
        Ok(())
    }

    /// Edge counterpart of [`Self::touch_node`].
    fn touch_edge(&mut self, id: EdgeId) -> Result<()> {
        self.mutations += 1;
        if self.owns_edge(id) || self.touched_edges.contains(&id) {
            return Ok(());
        }
        let ci = self.ci();
        // SAFETY: writer mutex + structural write lock
        let g = unsafe { self.store.graph_mut() };
        let edge = g.get_edge(id)?;
        let image = DeltaAction::EdgeImage {
            source: edge.source,
            target: edge.target,
            edge_type: edge.edge_type,
            properties: edge.properties.clone(),
        };
        let ghosts = g.edge_ghosts(id);
        create_and_link_delta(&mut self.delta_arena, g.edge_delta_head_mut(id), ci, image);
        g.versions_mut().record_edge(id, ci, ghosts);
        self.touched_edges.insert(id);
        Ok(())
    }

    /// After deleting a touched node: the marker and the "deleted" ghost.
    fn mark_node_deleted(&mut self, id: NodeId) {
        let ci = self.ci();
        // SAFETY: writer mutex + structural write lock
        let g = unsafe { self.store.graph_mut() };
        create_and_link_delta(
            &mut self.delta_arena,
            g.node_delta_head_mut(id),
            ci,
            DeltaAction::NodeDeleted,
        );
        g.versions_mut().record_node(id, ci, [NodeGhost::Deleted]);
    }

    /// After deleting a touched edge: the marker and the adjacency ghost.
    fn mark_edge_deleted(&mut self, id: EdgeId, source: NodeId, target: NodeId) {
        let ci = self.ci();
        // SAFETY: writer mutex + structural write lock
        let g = unsafe { self.store.graph_mut() };
        create_and_link_delta(
            &mut self.delta_arena,
            g.edge_delta_head_mut(id),
            ci,
            DeltaAction::EdgeDeleted,
        );
        g.versions_mut()
            .record_edge(id, ci, [EdgeGhost::Deleted { source, target }]);
    }

    fn note_created_node(&mut self, id: NodeId) {
        self.mutations += 1;
        let ci = self.ci();
        self.created_nodes.insert(id);
        self.graph_mut().versions_mut().node_runs.push(id.0, ci);
    }

    fn note_created_edge(&mut self, id: EdgeId) {
        self.mutations += 1;
        let ci = self.ci();
        self.created_edges.insert(id);
        self.graph_mut().versions_mut().edge_runs.push(id.0, ci);
    }

    fn owns_node(&self, id: NodeId) -> bool {
        self.created_nodes.contains(&id)
            || self
                .created_node_ranges
                .iter()
                .any(|&(a, b)| (a..b).contains(&id.0))
    }

    fn owns_edge(&self, id: EdgeId) -> bool {
        self.created_edges.contains(&id)
            || self
                .created_edge_ranges
                .iter()
                .any(|&(a, b)| (a..b).contains(&id.0))
    }

    /// Bulk node creation ([`Graph::bulk_nodes`]), for a load outside the redo log.
    pub(crate) fn bulk_nodes(
        &mut self,
        labels: &[&str],
        props: Vec<PropertyMap>,
    ) -> Result<NodeId> {
        debug_assert!(
            self.skip_redo,
            "bulk creation is for loads outside the redo log"
        );
        if let Some(p) = props.first() {
            self.check_memory_budget(labels, p)?;
        }
        let n = props.len();
        let _rw = self.store.graph_rw.write();
        let first = self.graph_mut().bulk_nodes(labels, props);
        self.mutations += n as u64;
        let ci = self.ci();
        self.created_node_ranges.push((first.0, first.0 + n as u64));
        self.graph_mut()
            .versions_mut()
            .node_runs
            .push_range(first.0, first.0 + n as u64, ci);
        Ok(first)
    }

    /// Bulk edge creation ([`Graph::bulk_edges`]), for a load outside the redo log.
    pub(crate) fn bulk_edges(
        &mut self,
        edge_type: &str,
        pairs: &[(NodeId, NodeId)],
    ) -> Result<EdgeId> {
        debug_assert!(
            self.skip_redo,
            "bulk creation is for loads outside the redo log"
        );
        self.check_edge_memory_budget(&PropertyMap::new())?;
        let n = pairs.len();
        let _rw = self.store.graph_rw.write();
        let first = self.graph_mut().bulk_edges(edge_type, pairs)?;
        self.mutations += n as u64;
        let ci = self.ci();
        self.created_edge_ranges.push((first.0, first.0 + n as u64));
        self.graph_mut()
            .versions_mut()
            .edge_runs
            .push_range(first.0, first.0 + n as u64, ci);
        Ok(first)
    }

    /// See [`Graph::set_bulk`].
    pub(crate) fn set_bulk(&mut self, on: bool) {
        let _rw = self.store.graph_rw.write();
        self.graph_mut().set_bulk(on);
    }

    fn check_memory_budget(&self, labels: &[&str], props: &PropertyMap) -> Result<()> {
        use crate::graph::{LABEL_INDEX_COST, NODE_BASE_COST, PROPERTY_BYTE};
        if let Some(budget) = self.memory_budget {
            let prop_estimate = props.len() * 16;
            let cost =
                NODE_BASE_COST + labels.len() * LABEL_INDEX_COST + prop_estimate * PROPERTY_BYTE;
            let current = if crate::memory::is_tracking_active() {
                crate::memory::allocated_bytes()
            } else {
                self.graph().estimated_memory()
            };
            if current + cost > budget {
                return Err(Error::MemoryLimitExceeded {
                    current,
                    limit: budget,
                });
            }
        }
        Ok(())
    }

    fn check_edge_memory_budget(&self, props: &PropertyMap) -> Result<()> {
        use crate::graph::{ADJ_ENTRY_COST, EDGE_BASE_COST, PROPERTY_BYTE};
        if let Some(budget) = self.memory_budget {
            let prop_estimate = props.len() * 16;
            let cost = EDGE_BASE_COST + ADJ_ENTRY_COST + prop_estimate * PROPERTY_BYTE;
            let current = if crate::memory::is_tracking_active() {
                crate::memory::allocated_bytes()
            } else {
                self.graph().estimated_memory()
            };
            if current + cost > budget {
                return Err(Error::MemoryLimitExceeded {
                    current,
                    limit: budget,
                });
            }
        }
        Ok(())
    }

    fn check_property_update_budget(&self, id: NodeId, props: &PropertyMap) -> Result<()> {
        use crate::graph::PROPERTY_BYTE;
        if let Some(budget) = self.memory_budget {
            let old_bytes = self
                .graph()
                .get_node(id)
                .map(|n| n.properties.data_len())
                .unwrap_or(0);
            let new_estimate = props.len() * 16 * PROPERTY_BYTE;
            if new_estimate > old_bytes * PROPERTY_BYTE {
                let growth = new_estimate - old_bytes * PROPERTY_BYTE;
                let current = if crate::memory::is_tracking_active() {
                    crate::memory::allocated_bytes()
                } else {
                    self.graph().estimated_memory()
                };
                if current + growth > budget {
                    return Err(Error::MemoryLimitExceeded {
                        current,
                        limit: budget,
                    });
                }
            }
        }
        Ok(())
    }

    fn check_edge_property_update_budget(&self, id: EdgeId, props: &PropertyMap) -> Result<()> {
        use crate::graph::PROPERTY_BYTE;
        if let Some(budget) = self.memory_budget {
            let g = self.graph();
            let old_bytes = g.get_edge(id).map(|e| e.properties.data_len()).unwrap_or(0);
            let new_estimate = props.len() * 16 * PROPERTY_BYTE;
            if new_estimate > old_bytes * PROPERTY_BYTE {
                let growth = new_estimate - old_bytes * PROPERTY_BYTE;
                let current = if crate::memory::is_tracking_active() {
                    crate::memory::allocated_bytes()
                } else {
                    g.estimated_memory()
                };
                if current + growth > budget {
                    return Err(Error::MemoryLimitExceeded {
                        current,
                        limit: budget,
                    });
                }
            }
        }
        Ok(())
    }

    /// Commits: one atomic store publishes every delta, run and ghost, then new snapshots start
    /// at our timestamp. Returns the commit timestamp.
    ///
    /// # WAL
    ///
    /// With a redo-log writer attached, the logical ops are appended **before** the
    /// commit is published. If the append fails, the transaction reverts in place and
    /// the error is returned; readers never see partial state.
    pub fn commit(mut self) -> Result<HybridTimestamp> {
        // the database is going away; what this transaction wrote goes with it
        if self.store.is_closed() {
            self.revert();
            self.committed = true; // no Drop revert
            return Err(Error::DatabaseClosed);
        }
        let commit_ts = self.store.clock.now();
        let encoded = encode_ts(commit_ts);

        // 1. APPEND the redo log (group commit, no fsync yet). The durability WAIT comes
        //    last, after the writer lock is released, so the next transaction builds during
        //    our fsync and concurrent commits share one sync (see rlog::SyncControl).
        #[cfg(not(target_arch = "wasm32"))]
        let mut durability: Option<(
            std::sync::Arc<crate::storage::rlog::SyncControl>,
            u64,
        )> = None;
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.skip_redo
                && !self.redo_log.is_empty()
                && let Some(writer) = self.store.log_writer.as_ref()
            {
                let ops_to_commit = std::mem::take(&mut self.redo_log);
                let mut lw = writer.lock().unwrap_or_else(|e| e.into_inner());
                match lw.append_batch(&ops_to_commit, encoded) {
                    Ok(outcome) => {
                        durability = Some((lw.sync_handle(), outcome.next_lsn));
                    }
                    Err(e) => {
                        drop(lw);
                        self.revert();
                        self.committed = true; // no Drop revert
                        return Err(e);
                    }
                }
            }
        }

        // 2-3. publish, retire, collect
        self.publish(encoded);

        // 4. release the writer lock, THEN wait for durability. State is already visible
        //    (WAL semantics: visible at commit, acked after flush). Safe because the WAL is
        //    sequential: a later commit can't be durable before ours. On fsync failure the
        //    sync control poisons itself and this and every later commit returns Err.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((sync, lsn)) = durability {
            let sync_on_commit = self.store.sync_on_commit();
            let store = self.store;
            drop(self); // releases the write lock; Drop won't revert (committed)
            if sync_on_commit {
                sync.wait_durable(lsn)?;
                // now visible: let go of our versions unless the next writer is already in (its
                // commit collects then)
                if let Some(_writer) = store.write_lock.try_lock() {
                    store.collect_locked(false);
                }
            } else {
                // async WAL: written to the log file now (a process crash keeps it), synced by the
                // background task; fails fast once a write or sync has failed
                sync.wait_written(lsn)?;
            }
            return Ok(commit_ts);
        }

        Ok(commit_ts)
    }

    /// Commits a transaction made durable by a snapshot its caller just wrote: redo off, the
    /// database flushed under our writer lock with our changes in it (bulk loads). Publishes like
    /// [`commit`](Self::commit), then marks the commit durable: the snapshot holds it and every
    /// commit before it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn commit_snapshotted(mut self) -> HybridTimestamp {
        self.set_bulk(false);
        debug_assert!(self.skip_redo && self.redo_log.is_empty());
        let commit_ts = self.store.clock.now();
        let encoded = encode_ts(commit_ts);
        self.publish(encoded);
        self.store.note_durable(encoded);
        // durable, and so visible, only now (the collect in `publish` ran before): let go of our
        // versions, as `commit` does once its log record is synced. Otherwise the load's created
        // runs stay until the next commit, and every reader until then takes the versioned paths
        self.store.collect_locked(false);
        // the load wrote its relationships into the adjacency overlays: packed, reads take the
        // packed path and a snapshot copies no overlay (a graph under 1024 nodes would otherwise
        // never fold, see `compact_adjacency`)
        let _rw = self.store.graph_rw.write();
        // SAFETY: we hold the writer lock and the structural write lock, as `MvccStore::gc`
        unsafe { self.store.graph_mut() }.pack_adjacency();
        commit_ts
    }

    /// Publishes the commit at `encoded`: our deltas first, then the timestamp new snapshots start
    /// from, so a snapshot at our timestamp always finds our deltas committed. Then retires the
    /// arena with its commit info (GC frees them once no snapshot can reach them) and collects
    /// what no open snapshot needs.
    fn publish(&mut self, encoded: u64) {
        self.commit_info.commit(encoded);
        self.store.last_commit_ts.store(encoded, Ordering::SeqCst);
        self.committed = true;
        {
            let arena = std::mem::take(&mut self.delta_arena);
            let ci = std::mem::replace(
                &mut self.commit_info,
                Box::new(CommitInfo::new_uncommitted()),
            );
            let _rw = self.store.graph_rw.write();
            self.graph_mut().versions_mut().retire(encoded, arena, ci);
        }
        self.store.collect_after_commit();
    }

    /// Commits the work so far (a `CALL { ... } IN TRANSACTIONS` batch) and carries on as a fresh
    /// transaction under the same writer lock. Logged and published like [`commit`](Self::commit),
    /// and as durable when it returns; the wait for the log happens under the lock, since this
    /// transaction keeps writing.
    fn commit_and_continue(&mut self) -> Result<HybridTimestamp> {
        if self.store.is_closed() {
            return Err(Error::DatabaseClosed);
        }
        let commit_ts = self.store.clock.now();
        let encoded = encode_ts(commit_ts);
        #[cfg(not(target_arch = "wasm32"))]
        let mut durability = None;
        #[cfg(not(target_arch = "wasm32"))]
        if !self.skip_redo
            && !self.redo_log.is_empty()
            && let Some(writer) = self.store.log_writer.as_ref()
        {
            let ops = std::mem::take(&mut self.redo_log);
            let mut lw = writer.lock().unwrap_or_else(|e| e.into_inner());
            // a failed append leaves this batch in the transaction: the statement fails and the
            // transaction is rolled back with it
            let outcome = match lw.append_batch(&ops, encoded) {
                Ok(outcome) => outcome,
                Err(e) => {
                    self.redo_log = ops;
                    return Err(e);
                }
            };
            durability = Some((lw.sync_handle(), outcome.next_lsn));
        }
        self.publish(encoded);
        // carry on as a fresh transaction: publish swapped in an empty arena and commit info
        self.committed = false;
        self.begin_ts = commit_ts;
        self.reset_local_state();
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((sync, lsn)) = durability {
            if self.store.sync_on_commit() {
                sync.wait_durable(lsn)?;
            } else {
                sync.wait_written(lsn)?;
            }
        }
        Ok(commit_ts)
    }

    /// Rolls back what this transaction wrote since its start or its last batch commit, and
    /// carries on (a failed `CALL { ... } IN TRANSACTIONS` batch).
    fn revert_and_continue(&mut self) {
        self.revert();
        // our deltas are unlinked: start over with a fresh arena and commit info
        self.delta_arena = DeltaArena::new();
        *self.commit_info = CommitInfo::new_uncommitted();
        self.reset_local_state();
    }

    /// Forgets what this transaction created and touched (after its changes were published or
    /// reverted).
    fn reset_local_state(&mut self) {
        self.created_nodes.clear();
        self.created_edges.clear();
        self.created_node_ranges.clear();
        self.created_edge_ranges.clear();
        self.touched_nodes.clear();
        self.touched_edges.clear();
        self.schema_undo.clear();
        self.redo_log.clear();
    }

    /// Aborts, restoring everything this transaction changed.
    pub fn abort(mut self) {
        self.revert();
        self.committed = true; // no double revert in Drop
    }

    /// Restores the pre-transaction state: drops what we created, puts touched objects back from
    /// our images (index maintenance included), unlinks our deltas, forgets our runs and ghosts,
    /// then undoes schema changes newest first.
    fn revert(&mut self) {
        let ci = self.ci();
        let _rw = self.store.graph_rw.write();
        // SAFETY: writer mutex + structural write lock
        let g = unsafe { self.store.graph_mut() };
        g.set_bulk(false);

        for &e in &self.created_edges {
            if g.has_edge(e) {
                let _ = g.delete_edge(e);
            }
        }
        for &n in &self.created_nodes {
            if g.get_node(n).is_ok() {
                let _ = g.delete_node_cascading(n);
            }
        }
        for &(a, b) in &self.created_edge_ranges {
            for e in a..b {
                let e = EdgeId(e);
                if g.has_edge(e) {
                    let _ = g.delete_edge(e);
                }
            }
        }
        for &(a, b) in &self.created_node_ranges {
            for n in a..b {
                let n = NodeId(n);
                if g.get_node(n).is_ok() {
                    let _ = g.delete_node_cascading(n);
                }
            }
        }
        // nodes first: restored edges need their endpoints
        for &n in &self.touched_nodes {
            if let Some(DeltaAction::NodeImage { labels, properties }) =
                own_image(g.node_delta_head(n), ci)
            {
                if g.get_node(n).is_ok() {
                    let _ = g.set_node_state(n, labels.clone(), properties.clone());
                } else {
                    g.restore_node(Node {
                        id: n,
                        labels: labels.clone(),
                        properties: properties.clone(),
                    });
                }
            }
        }
        for &e in &self.touched_edges {
            if let Some(DeltaAction::EdgeImage {
                source,
                target,
                edge_type,
                properties,
            }) = own_image(g.edge_delta_head(e), ci)
            {
                if g.has_edge(e) {
                    let _ = g.set_edge_state(e, properties.clone());
                } else {
                    let mut edge = Edge::new(e, *edge_type, *source, *target);
                    edge.properties = properties.clone();
                    g.restore_edge(edge);
                }
            }
        }
        // our deltas sit at the chain heads
        for &n in &self.touched_nodes {
            if g.node_delta_heads.get(&n).is_some_and(|h| pop_own(h, ci)) {
                g.node_delta_heads.remove(&n);
            }
        }
        for &e in &self.touched_edges {
            if g.edge_delta_heads.get(&e).is_some_and(|h| pop_own(h, ci)) {
                g.edge_delta_heads.remove(&e);
            }
        }
        g.versions_mut().discard(ci);

        for undo in self.schema_undo.drain(..).rev() {
            let _ = match undo {
                SchemaUndo::DropIndex(name) => g.drop_index(&name),
                SchemaUndo::RestoreIndex {
                    name,
                    label,
                    property,
                } => {
                    if property.contains(crate::graph::COMPOSITE_PROP_SEP) {
                        let parts: Vec<String> = property
                            .split(crate::graph::COMPOSITE_PROP_SEP)
                            .map(str::to_string)
                            .collect();
                        g.create_composite_index(&name, &label, &parts, false)
                    } else {
                        g.create_index(&name, &label, &property)
                    }
                }
                SchemaUndo::DropEdgeIndex(name) => g.drop_edge_index(&name),
                SchemaUndo::RestoreEdgeIndex {
                    name,
                    edge_type,
                    property,
                } => match property {
                    Some(p) if p.contains(crate::graph::COMPOSITE_PROP_SEP) => {
                        let parts: Vec<String> = p
                            .split(crate::graph::COMPOSITE_PROP_SEP)
                            .map(str::to_string)
                            .collect();
                        g.create_composite_index(&name, &edge_type, &parts, true)
                    }
                    p => g.create_edge_index(&name, &edge_type, p.as_deref()),
                },
                SchemaUndo::DropConstraint(name) => g.drop_constraint(&name),
                SchemaUndo::RestoreConstraint(desc) => g.restore_constraint(desc),
                SchemaUndo::DropVectorIndex(name) => g.drop_vector_index(&name),
                SchemaUndo::RestoreVectorIndex {
                    name,
                    label,
                    property,
                    dim,
                    sim,
                    params,
                } => g.create_vector_index(&name, &label, &property, dim, sim, params),
                SchemaUndo::DropFulltextIndex(name) => g.drop_fulltext_index(&name),
                SchemaUndo::RestoreFulltextIndex {
                    name,
                    labels,
                    properties,
                    analyzer,
                } => {
                    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
                    let props: Vec<&str> = properties.iter().map(String::as_str).collect();
                    g.create_fulltext_index(&name, &labels, &props, analyzer)
                }
                SchemaUndo::DropPointIndex(name) => g.drop_point_index(&name),
                SchemaUndo::RestorePointIndex {
                    name,
                    label,
                    property,
                } => g.create_point_index(&name, &label, &property),
                SchemaUndo::DropTextIndex(name) => g.drop_text_index(&name),
                SchemaUndo::RestoreTextIndex {
                    name,
                    label,
                    property,
                } => g.create_text_index(&name, &label, &property),
                SchemaUndo::RestoreStatistics(stats) => {
                    g.set_statistics(stats);
                    Ok(())
                }
            };
        }
        self.redo_log.clear();
    }

    /// Records the statistics to restore on abort (once per transaction).
    fn note_statistics_change(&mut self) {
        if !self
            .schema_undo
            .iter()
            .any(|u| matches!(u, SchemaUndo::RestoreStatistics(_)))
        {
            let old = self.graph().statistics_arc();
            self.schema_undo.push(SchemaUndo::RestoreStatistics(old));
        }
    }
}

impl Drop for WriteTx<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.revert();
        }
    }
}

/// Edges of `id` in both directions, deduplicated.
fn incident_edges(g: &Graph, id: NodeId) -> Vec<EdgeId> {
    let mut edges: Vec<EdgeId> = g
        .edges_of_ids(id, Direction::Both, None)
        .unwrap_or_default();
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// Redo inputs captured before a map moves into the graph.
#[cfg(not(target_arch = "wasm32"))]
fn map_for_redo(props: &PropertyMap) -> Vec<(String, PropertyValue)> {
    props.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

impl GraphView for WriteTx<'_> {
    /// Parallel-safe view of the graph: the writer holds the store's write lock and [`Graph`] is
    /// `Sync` for reads.
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        Some(self.graph())
    }

    fn write_count(&self) -> u64 {
        // schema changes push their undo; data changes go through touch / create
        self.mutations + self.schema_undo.len() as u64
    }

    fn is_explicit_transaction(&self) -> bool {
        !self.batches
    }

    fn can_commit_batches(&self) -> bool {
        self.batches
    }

    fn commit_batch(&mut self) -> Result<()> {
        if !self.batches {
            return Err(Error::Unsupported(
                "only an auto-commit statement's transaction commits in batches",
            ));
        }
        self.commit_and_continue().map(drop)
    }

    fn rollback_batch(&mut self) -> Result<()> {
        if !self.batches {
            return Err(Error::Unsupported(
                "only an auto-commit statement's transaction rolls back batches",
            ));
        }
        self.revert_and_continue();
        Ok(())
    }

    // the writer reads its own writes: the live graph
    crate::forward_graph_view_reads!(self => self.graph());

    fn create_node(&mut self, labels: &[&str], props: PropertyMap) -> Result<NodeId> {
        self.check_memory_budget(labels, &props)?;
        #[cfg(not(target_arch = "wasm32"))]
        let redo = (!self.skip_redo).then(|| {
            (
                labels.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                map_for_redo(&props),
            )
        });
        let _rw = self.store.graph_rw.write();
        let id = self.graph_mut().create_node(labels, props);
        self.note_created_node(id);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((labels, properties)) = redo {
            self.record_redo(RedoOp::CreateNode {
                node_id: id.0,
                labels,
                properties,
            });
        }
        Ok(id)
    }

    fn create_edge(
        &mut self,
        src: NodeId,
        tgt: NodeId,
        edge_type: &str,
        props: PropertyMap,
    ) -> Result<EdgeId> {
        self.check_edge_memory_budget(&props)?;
        #[cfg(not(target_arch = "wasm32"))]
        let redo = (!self.skip_redo).then(|| map_for_redo(&props));
        let _rw = self.store.graph_rw.write();
        let id = self.graph_mut().create_edge(src, tgt, edge_type, props)?;
        self.note_created_edge(id);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(properties) = redo {
            self.record_redo(RedoOp::CreateEdge {
                edge_id: id.0,
                src: src.0,
                dst: tgt.0,
                edge_type: edge_type.to_string(),
                properties,
            });
        }
        Ok(id)
    }

    fn create_node_with_id(
        &mut self,
        id: NodeId,
        labels: &[&str],
        props: PropertyMap,
    ) -> Result<()> {
        // replay forces the recorded id (and sets skip_redo); a transaction applying ids reserved
        // earlier logs them like any create
        self.check_memory_budget(labels, &props)?;
        #[cfg(not(target_arch = "wasm32"))]
        let redo = (!self.skip_redo).then(|| {
            (
                labels.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                map_for_redo(&props),
            )
        });
        let _rw = self.store.graph_rw.write();
        self.graph_mut().create_node_with_id(id, labels, props);
        self.note_created_node(id);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((labels, properties)) = redo {
            self.record_redo(RedoOp::CreateNode {
                node_id: id.0,
                labels,
                properties,
            });
        }
        Ok(())
    }

    fn create_edge_with_id(
        &mut self,
        id: EdgeId,
        src: NodeId,
        tgt: NodeId,
        edge_type: &str,
        props: PropertyMap,
    ) -> Result<()> {
        self.check_edge_memory_budget(&props)?;
        #[cfg(not(target_arch = "wasm32"))]
        let redo = (!self.skip_redo).then(|| map_for_redo(&props));
        let _rw = self.store.graph_rw.write();
        self.graph_mut()
            .create_edge_with_id(id, src, tgt, edge_type, props)?;
        self.note_created_edge(id);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(properties) = redo {
            self.record_redo(RedoOp::CreateEdge {
                edge_id: id.0,
                src: src.0,
                dst: tgt.0,
                edge_type: edge_type.to_string(),
                properties,
            });
        }
        Ok(())
    }

    fn update_node_properties(&mut self, id: NodeId, props: PropertyMap) -> Result<()> {
        self.check_property_update_budget(id, &props)?;
        #[cfg(not(target_arch = "wasm32"))]
        let redo = (!self.skip_redo).then(|| map_for_redo(&props));
        let _rw = self.store.graph_rw.write();
        self.touch_node(id)?;
        self.graph_mut().update_node_properties(id, props)?;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(properties) = redo {
            self.record_redo(RedoOp::SetNodeProperties {
                node_id: id.0,
                properties,
            });
        }
        Ok(())
    }

    fn update_edge_properties(&mut self, id: EdgeId, props: PropertyMap) -> Result<()> {
        self.check_edge_property_update_budget(id, &props)?;
        #[cfg(not(target_arch = "wasm32"))]
        let redo = (!self.skip_redo).then(|| map_for_redo(&props));
        let _rw = self.store.graph_rw.write();
        self.touch_edge(id)?;
        self.graph_mut().update_edge_properties(id, props)?;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(properties) = redo {
            self.record_redo(RedoOp::SetEdgeProperties {
                edge_id: id.0,
                properties,
            });
        }
        Ok(())
    }

    fn delete_node(&mut self, id: NodeId) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        if self.owns_node(id) {
            self.graph_mut().delete_node(id)?;
        } else {
            self.touch_node(id)?;
            self.graph_mut().delete_node(id)?;
            self.mark_node_deleted(id);
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::DeleteNode { node_id: id.0 });
        Ok(())
    }

    fn delete_node_cascading(&mut self, id: NodeId) -> Result<()> {
        // explicit DeleteEdge records before the DeleteNode: replay goes through the
        // non-cascading delete_node, so the log stays self-describing
        self.graph().get_node(id)?;
        for e in incident_edges(self.graph(), id) {
            self.delete_edge(e)?;
        }
        self.delete_node(id)
    }

    fn delete_edge(&mut self, id: EdgeId) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        if self.owns_edge(id) {
            self.graph_mut().delete_edge(id)?;
        } else {
            let (source, target) = {
                let e = self.graph().get_edge(id)?;
                (e.source, e.target)
            };
            self.touch_edge(id)?;
            self.graph_mut().delete_edge(id)?;
            self.mark_edge_deleted(id, source, target);
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::DeleteEdge { edge_id: id.0 });
        Ok(())
    }

    fn add_label(&mut self, id: NodeId, label: &str) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        self.touch_node(id)?;
        self.graph_mut().add_label(id, label)?;
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::AddLabel {
            node_id: id.0,
            label: label.to_string(),
        });
        Ok(())
    }

    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        self.touch_node(id)?;
        self.graph_mut().remove_label(id, label)?;
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::RemoveLabel {
            node_id: id.0,
            label: label.to_string(),
        });
        Ok(())
    }

    fn get_node_mut(&mut self, id: NodeId) -> Result<&mut Node> {
        // the image is linked before the node is handed out: from here on readers resolve this
        // node through it and never read the copy being changed in place
        let _rw = self.store.graph_rw.write();
        self.touch_node(id)?;
        self.graph_mut().get_node_mut(id)
    }

    fn get_edge_mut(&mut self, id: EdgeId) -> Result<&mut Edge> {
        let _rw = self.store.graph_rw.write();
        self.touch_edge(id)?;
        self.graph_mut().get_edge_mut(id)
    }

    // unindexing opens an in-place change: touch first, so snapshots keep finding the object
    // through its ghosts while its live index entries are gone

    fn unindex_node_prop(&mut self, id: NodeId, key: &str) {
        let _rw = self.store.graph_rw.write();
        if self.touch_node(id).is_ok() {
            GraphView::unindex_node_prop(self.graph_mut(), id, key);
        }
    }

    fn index_node_prop(&mut self, id: NodeId, key: &str) {
        {
            let _rw = self.store.graph_rw.write();
            GraphView::index_node_prop(self.graph_mut(), id, key);
        }
        self.log_node_properties(id);
    }

    fn unindex_edge_prop(&mut self, id: EdgeId, key: &str) {
        let _rw = self.store.graph_rw.write();
        if self.touch_edge(id).is_ok() {
            GraphView::unindex_edge_prop(self.graph_mut(), id, key);
        }
    }

    fn index_edge_prop(&mut self, id: EdgeId, key: &str) {
        {
            let _rw = self.store.graph_rw.write();
            GraphView::index_edge_prop(self.graph_mut(), id, key);
        }
        self.log_edge_properties(id);
    }

    fn unindex_node(&mut self, id: NodeId) {
        let _rw = self.store.graph_rw.write();
        if self.touch_node(id).is_ok() {
            GraphView::unindex_node(self.graph_mut(), id);
        }
    }

    fn index_node(&mut self, id: NodeId) {
        {
            let _rw = self.store.graph_rw.write();
            GraphView::index_node(self.graph_mut(), id);
        }
        self.log_node_properties(id);
    }

    fn unindex_edge(&mut self, id: EdgeId) {
        let _rw = self.store.graph_rw.write();
        if self.touch_edge(id).is_ok() {
            GraphView::unindex_edge(self.graph_mut(), id);
        }
    }

    fn index_edge(&mut self, id: EdgeId) {
        {
            let _rw = self.store.graph_rw.write();
            GraphView::index_edge(self.graph_mut(), id);
        }
        self.log_edge_properties(id);
    }

    fn intern_property_key(&mut self, key: &str) -> PropertyKeyId {
        let _rw = self.store.graph_rw.write();
        self.graph_mut().intern_property_key(key)
    }

    fn analyze_statistics(&mut self, labels: &[String]) {
        self.note_statistics_change();
        let _rw = self.store.graph_rw.write();
        GraphView::analyze_statistics(self.graph_mut(), labels);
    }

    // -- schema --

    fn create_index(&mut self, name: &str, label: &str, prop: &str) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        self.graph_mut().create_index(name, label, prop)?;
        self.schema_undo
            .push(SchemaUndo::DropIndex(name.to_string()));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::CreateIndex {
            name: name.to_string(),
            label: label.to_string(),
            property: prop.to_string(),
        });
        Ok(())
    }

    fn create_composite_index(
        &mut self,
        name: &str,
        label: &str,
        properties: &[String],
        is_relationship: bool,
    ) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        self.graph_mut()
            .create_composite_index(name, label, properties, is_relationship)?;
        self.schema_undo.push(if is_relationship {
            SchemaUndo::DropEdgeIndex(name.to_string())
        } else {
            SchemaUndo::DropIndex(name.to_string())
        });
        // the synthetic joined key: replay splits it and routes to create_composite_index
        #[cfg(not(target_arch = "wasm32"))]
        {
            let joined = properties.join(&crate::graph::COMPOSITE_PROP_SEP.to_string());
            self.record_redo(if is_relationship {
                RedoOp::CreateEdgeIndex {
                    name: name.to_string(),
                    edge_type: label.to_string(),
                    property: Some(joined),
                }
            } else {
                RedoOp::CreateIndex {
                    name: name.to_string(),
                    label: label.to_string(),
                    property: joined,
                }
            });
        }
        Ok(())
    }

    fn drop_index(&mut self, name: &str) -> Result<()> {
        let g = self.graph();
        let d = g
            .index_catalog()
            .get_descriptor_by_name(name)
            .ok_or_else(|| Error::IndexError(format!("index '{name}' not found")))?;
        let undo = SchemaUndo::RestoreIndex {
            name: name.to_string(),
            label: g.strings().resolve(d.label_id.0).to_string(),
            property: g.strings().resolve(d.property_key.0).to_string(),
        };
        let _rw = self.store.graph_rw.write();
        self.graph_mut().drop_index(name)?;
        self.schema_undo.push(undo);
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::DropIndex {
            name: name.to_string(),
        });
        Ok(())
    }

    fn create_vector_index(
        &mut self,
        name: &str,
        label: &str,
        property: &str,
        dim: usize,
        sim: crate::graph::vector::Similarity,
        params: crate::graph::vector::HnswParams,
    ) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        self.graph_mut()
            .create_vector_index(name, label, property, dim, sim, params)?;
        self.schema_undo
            .push(SchemaUndo::DropVectorIndex(name.to_string()));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::CreateVectorIndex {
            name: name.to_string(),
            label: label.to_string(),
            property: property.to_string(),
            dimensions: dim as u64,
            similarity: sim.as_str().to_string(),
            m: params.m as u64,
            ef_construction: params.ef_construction as u64,
            ef_search: params.ef_search as u64,
            seed: params.seed,
            quantize: params.quantize,
        });
        Ok(())
    }

    fn drop_vector_index(&mut self, name: &str) -> Result<()> {
        let (label, property, dim, sim, params) = self
            .graph()
            .vector_index_spec(name)
            .ok_or_else(|| Error::IndexError(format!("vector index '{name}' not found")))?;
        let _rw = self.store.graph_rw.write();
        self.graph_mut().drop_vector_index(name)?;
        self.schema_undo.push(SchemaUndo::RestoreVectorIndex {
            name: name.to_string(),
            label,
            property,
            dim,
            sim,
            params,
        });
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::DropVectorIndex {
            name: name.to_string(),
        });
        Ok(())
    }

    fn create_fulltext_index(
        &mut self,
        name: &str,
        labels: &[&str],
        properties: &[&str],
        analyzer: crate::graph::fulltext::Analyzer,
    ) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        self.graph_mut()
            .create_fulltext_index(name, labels, properties, analyzer)?;
        self.schema_undo
            .push(SchemaUndo::DropFulltextIndex(name.to_string()));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::CreateFulltextIndex {
            name: name.to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            properties: properties.iter().map(|s| s.to_string()).collect(),
            analyzer: analyzer.name().to_string(),
        });
        Ok(())
    }

    fn drop_fulltext_index(&mut self, name: &str) -> Result<()> {
        let (labels, properties, analyzer) = self
            .graph()
            .fulltext_index_spec(name)
            .ok_or_else(|| Error::IndexError(format!("full-text index '{name}' not found")))?;
        let _rw = self.store.graph_rw.write();
        self.graph_mut().drop_fulltext_index(name)?;
        self.schema_undo.push(SchemaUndo::RestoreFulltextIndex {
            name: name.to_string(),
            labels,
            properties,
            analyzer,
        });
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::DropFulltextIndex {
            name: name.to_string(),
        });
        Ok(())
    }

    fn create_point_index(&mut self, name: &str, label: &str, property: &str) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        self.graph_mut().create_point_index(name, label, property)?;
        self.schema_undo
            .push(SchemaUndo::DropPointIndex(name.to_string()));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::CreatePointIndex {
            name: name.to_string(),
            label: label.to_string(),
            property: property.to_string(),
        });
        Ok(())
    }

    fn drop_point_index(&mut self, name: &str) -> Result<()> {
        let (label, property) = self
            .graph()
            .point_index_spec(name)
            .ok_or_else(|| Error::IndexError(format!("point index '{name}' not found")))?;
        let _rw = self.store.graph_rw.write();
        self.graph_mut().drop_point_index(name)?;
        self.schema_undo.push(SchemaUndo::RestorePointIndex {
            name: name.to_string(),
            label,
            property,
        });
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::DropPointIndex {
            name: name.to_string(),
        });
        Ok(())
    }

    fn create_text_index(&mut self, name: &str, label: &str, property: &str) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        self.graph_mut().create_text_index(name, label, property)?;
        self.schema_undo
            .push(SchemaUndo::DropTextIndex(name.to_string()));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::CreateTextIndex {
            name: name.to_string(),
            label: label.to_string(),
            property: property.to_string(),
        });
        Ok(())
    }

    fn drop_text_index(&mut self, name: &str) -> Result<()> {
        let (label, property) = self
            .graph()
            .text_index_spec(name)
            .ok_or_else(|| Error::IndexError(format!("text index '{name}' not found")))?;
        let _rw = self.store.graph_rw.write();
        self.graph_mut().drop_text_index(name)?;
        self.schema_undo.push(SchemaUndo::RestoreTextIndex {
            name: name.to_string(),
            label,
            property,
        });
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::DropTextIndex {
            name: name.to_string(),
        });
        Ok(())
    }

    fn create_constraint(
        &mut self,
        name: &str,
        label: &str,
        prop: &str,
        ctype: ConstraintType,
    ) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        let kind = constraint_redo_kind(&ctype);
        let _rw = self.store.graph_rw.write();
        self.graph_mut()
            .create_constraint(name, label, prop, ctype)?;
        self.schema_undo
            .push(SchemaUndo::DropConstraint(name.to_string()));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::CreateConstraint {
            name: name.to_string(),
            label: label.to_string(),
            property: prop.to_string(),
            kind,
        });
        Ok(())
    }

    fn create_relationship_constraint(
        &mut self,
        name: &str,
        edge_type: &str,
        prop: &str,
        ctype: ConstraintType,
    ) -> Result<()> {
        // relationship constraints: redo `kind` + 100 so replay routes to
        // `create_relationship_constraint`
        #[cfg(not(target_arch = "wasm32"))]
        let kind = constraint_redo_kind(&ctype) + REL_CONSTRAINT_KIND_OFFSET;
        let _rw = self.store.graph_rw.write();
        self.graph_mut()
            .create_relationship_constraint(name, edge_type, prop, ctype)?;
        self.schema_undo
            .push(SchemaUndo::DropConstraint(name.to_string()));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::CreateConstraint {
            name: name.to_string(),
            label: edge_type.to_string(),
            property: prop.to_string(),
            kind,
        });
        Ok(())
    }

    fn create_composite_constraint(
        &mut self,
        name: &str,
        label: &str,
        properties: &[String],
        ctype: ConstraintType,
        is_relationship: bool,
    ) -> Result<()> {
        // redo: keys joined into `property` with COMPOSITE_PROP_SEP; kind uses the same scheme
        // (+100 for rel), so replay can split and route
        #[cfg(not(target_arch = "wasm32"))]
        let kind = constraint_redo_kind(&ctype)
            + if is_relationship {
                REL_CONSTRAINT_KIND_OFFSET
            } else {
                0
            };
        #[cfg(not(target_arch = "wasm32"))]
        let joined = properties.join(&crate::graph::COMPOSITE_PROP_SEP.to_string());
        let _rw = self.store.graph_rw.write();
        self.graph_mut().create_composite_constraint(
            name,
            label,
            properties,
            ctype,
            is_relationship,
        )?;
        self.schema_undo
            .push(SchemaUndo::DropConstraint(name.to_string()));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::CreateConstraint {
            name: name.to_string(),
            label: label.to_string(),
            property: joined,
            kind,
        });
        Ok(())
    }

    fn drop_constraint(&mut self, name: &str) -> Result<()> {
        let desc = self
            .graph()
            .constraint_registry()
            .get_by_name(name)
            .cloned()
            .ok_or_else(|| Error::ConstraintError(format!("constraint `{name}` not found")))?;
        let _rw = self.store.graph_rw.write();
        self.graph_mut().drop_constraint(name)?;
        self.schema_undo.push(SchemaUndo::RestoreConstraint(desc));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::DropConstraint {
            name: name.to_string(),
        });
        Ok(())
    }

    fn create_edge_index(&mut self, name: &str, edge_type: &str, prop: Option<&str>) -> Result<()> {
        let _rw = self.store.graph_rw.write();
        self.graph_mut().create_edge_index(name, edge_type, prop)?;
        self.schema_undo
            .push(SchemaUndo::DropEdgeIndex(name.to_string()));
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::CreateEdgeIndex {
            name: name.to_string(),
            edge_type: edge_type.to_string(),
            property: prop.map(|s| s.to_string()),
        });
        Ok(())
    }

    fn drop_edge_index(&mut self, name: &str) -> Result<()> {
        let g = self.graph();
        let d = g
            .edge_index_catalog()
            .get_descriptor_by_name(name)
            .ok_or_else(|| Error::IndexError(format!("edge index '{name}' not found")))?;
        let undo = SchemaUndo::RestoreEdgeIndex {
            name: name.to_string(),
            edge_type: g.strings().resolve(d.edge_type_id.0).to_string(),
            property: d
                .property_key
                .map(|pk| g.strings().resolve(pk.0).to_string()),
        };
        let _rw = self.store.graph_rw.write();
        self.graph_mut().drop_edge_index(name)?;
        self.schema_undo.push(undo);
        #[cfg(not(target_arch = "wasm32"))]
        self.record_redo(RedoOp::DropEdgeIndex {
            name: name.to_string(),
        });
        Ok(())
    }
}

impl WriteTx<'_> {
    /// Redo for an in-place property write: the node's full map after the bracket (a full-map
    /// replacement, replay-correct however narrow the in-memory maintenance was).
    fn log_node_properties(&mut self, id: NodeId) {
        #[cfg(not(target_arch = "wasm32"))]
        if !self.skip_redo {
            let g = self.graph();
            let Ok(n) = g.get_node(id) else { return };
            let properties = redo_props(g, &n.properties);
            self.record_redo(RedoOp::SetNodeProperties {
                node_id: id.0,
                properties,
            });
        }
        #[cfg(target_arch = "wasm32")]
        let _ = id;
    }

    /// Edge counterpart of [`Self::log_node_properties`].
    fn log_edge_properties(&mut self, id: EdgeId) {
        #[cfg(not(target_arch = "wasm32"))]
        if !self.skip_redo {
            let g = self.graph();
            let Ok(e) = g.get_edge(id) else { return };
            let properties = redo_props(g, &e.properties);
            self.record_redo(RedoOp::SetEdgeProperties {
                edge_id: id.0,
                properties,
            });
        }
        #[cfg(target_arch = "wasm32")]
        let _ = id;
    }
}
