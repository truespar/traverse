//! Checkpoints: a running store written to a file as of one commit, while reads and commits carry
//! on. The writer lock is held only to pair a snapshot with the redo-log position; the records then
//! stream out of that snapshot in batches.

use std::sync::atomic::Ordering;
use std::sync::{MutexGuard, PoisonError};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

use super::decode_ts;
use super::timestamp::HybridTimestamp;
use super::view::GraphView;
use super::{MAX_READERS, MvccStore, ReadTx};
use crate::error::{Error, Result};
use crate::graph::CatalogParts;
#[cfg(not(target_arch = "wasm32"))]
use crate::graph::DirtySet;
use crate::graph::edge::Edge;
use crate::graph::node::Node;
use crate::graph::types::{EdgeId, NodeId};
use crate::storage::codec::RecordSource;

/// Records read per structural read lock: short enough that a commit's structural change never
/// waits long.
const BATCH: usize = 1024;

/// The store as of one commit, to write to a file: a snapshot reader, the catalog as of that
/// commit, and the redo-log position that matches it (every commit logged below the position is in
/// the snapshot, none above). Holds back version GC while alive, like any snapshot.
pub struct Checkpoint<'a> {
    read: ReadTx<'a>,
    catalog: CatalogParts,
    position: u64,
    /// The store's checkpoint lock; `None` under a [`CommitPause`](super::CommitPause), which
    /// holds it.
    _serial: Option<MutexGuard<'a, ()>>,
}

impl MvccStore {
    /// A checkpoint at the newest commit. Takes the writer lock only to pair the snapshot with the
    /// log position. Waits while another checkpoint of this store is alive, so files are installed
    /// in commit order.
    pub fn checkpoint(&self) -> Result<Checkpoint<'_>> {
        let serial = self
            .checkpoint_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _writer = self.write_lock.lock();
        self.checkpoint_locked(Some(serial))
    }

    /// [`checkpoint`](Self::checkpoint) that also takes the graph's change tracking, at the same
    /// instant: changes up to the checkpoint's commit come back for an in-place save to rewrite,
    /// later ones stay for the next. `then` runs before the writer lock goes and its result comes
    /// back too: a save locks its file there, so one made later under the writer lock (a bulk
    /// load's) can't be written before it. Waits at most `timeout` (`None`: as long as it takes)
    /// for the writer lock, then fails with [`Busy`](Error::Busy).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn checkpoint_taking_changes<T>(
        &self,
        timeout: Option<Duration>,
        then: impl FnOnce() -> T,
    ) -> Result<(Checkpoint<'_>, DirtySet, T)> {
        let serial = self
            .checkpoint_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _writer = match timeout {
            Some(t) => self.write_lock.try_lock_for(t).ok_or(Error::Busy(t))?,
            None => self.write_lock.lock(),
        };
        let checkpoint = self.checkpoint_locked(Some(serial))?;
        let changes = {
            let _rw = self.graph_rw.write();
            // SAFETY: writer lock and structural write lock held
            unsafe { self.graph_mut() }.take_dirty()
        };
        Ok((checkpoint, changes, then()))
    }

    /// [`checkpoint`](Self::checkpoint) with the writer lock held, and the checkpoint lock (in
    /// `serial`, or by a pause).
    pub(super) fn checkpoint_locked<'a>(
        &'a self,
        serial: Option<MutexGuard<'a, ()>>,
    ) -> Result<Checkpoint<'a>> {
        let position = self.log_position();
        // the newest commit, durable or not: its records are below `position`. GC takes the writer
        // lock, so nothing this snapshot needs can go before the slot is registered
        let ts = self.last_commit_ts.load(Ordering::SeqCst);
        let slot = self
            .reader_registry
            .register(ts)
            .ok_or(Error::TooManyReaders(MAX_READERS))?;
        let read = ReadTx::new(self, decode_ts(ts), ts, slot);
        let catalog = {
            let _rw = self.graph_rw.read();
            // SAFETY: under the structural read lock; the writer lock keeps commits out, so the
            // live catalog is the snapshot's
            unsafe { self.graph() }.catalog_parts()
        };
        Ok(Checkpoint {
            read,
            catalog,
            position,
            _serial: serial,
        })
    }
}

impl Checkpoint<'_> {
    /// Where replay resumes for a file written from this checkpoint: stamp it into the file.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// The commit this checkpoint holds the store as of.
    pub fn timestamp(&self) -> HybridTimestamp {
        self.read.snapshot_ts()
    }

    /// [`timestamp`](Self::timestamp), encoded.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn commit(&self) -> u64 {
        self.read.snap()
    }

    /// Names, id counters, schema definitions and statistics as of the commit.
    pub(crate) fn catalog(&self) -> &CatalogParts {
        &self.catalog
    }
}

impl RecordSource for Checkpoint<'_> {
    fn for_each_record(
        &self,
        node: &mut dyn FnMut(&Node) -> Result<()>,
        edge: &mut dyn FnMut(&Edge) -> Result<()>,
    ) -> Result<()> {
        self.nodes_by_id(&self.read.nodes_vec(), node)?;
        self.edges_by_id(&self.read.all_edges_vec(), edge)
    }

    fn nodes_by_id(&self, ids: &[NodeId], f: &mut dyn FnMut(&Node) -> Result<()>) -> Result<()> {
        let mut nodes = Vec::with_capacity(BATCH.min(ids.len()));
        for ids in ids.chunks(BATCH) {
            nodes.clear();
            self.read.nodes_at(ids, &mut nodes);
            for node in &nodes {
                f(node)?;
            }
        }
        Ok(())
    }

    fn edges_by_id(&self, ids: &[EdgeId], f: &mut dyn FnMut(&Edge) -> Result<()>) -> Result<()> {
        let mut edges = Vec::with_capacity(BATCH.min(ids.len()));
        for ids in ids.chunks(BATCH) {
            edges.clear();
            self.read.edges_at(ids, &mut edges);
            for edge in &edges {
                f(edge)?;
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for Checkpoint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkpoint")
            .field("timestamp", &self.timestamp())
            .field("position", &self.position)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::graph::Graph;
    use crate::graph::property::PropertyValue;
    use crate::graph::types::{NodeId, PropertyKeyId};
    use crate::storage::codec;
    use crate::storage::page_manager::PageManager;

    fn v(n: i64) -> BTreeMap<String, PropertyValue> {
        BTreeMap::from([("v".to_string(), PropertyValue::Int(n))])
    }

    fn v_of(g: &Graph, id: NodeId) -> Option<PropertyValue> {
        let key = PropertyKeyId(g.strings().get("v")?);
        g.get_node(id).ok()?.properties.get(key)
    }

    /// What a checkpoint writes, loaded back.
    fn written(checkpoint: &Checkpoint<'_>) -> Graph {
        let mut pm = PageManager::create_in_memory();
        codec::write_checkpoint(&mut pm, checkpoint, &[]).unwrap();
        let pm = PageManager::open_from_bytes(pm.to_bytes()).unwrap();
        codec::load_graph(pm).unwrap().0
    }

    #[test]
    fn a_checkpoint_writes_the_store_as_of_its_commit() {
        let mut g = Graph::new();
        let a = g.create_node(&["P"], v(1));
        let b = g.create_node(&["P"], v(2));
        let gone = g.create_node(&["P"], v(3));
        let e = g.create_edge(a, b, "R", v(1)).unwrap();
        g.create_edge(b, gone, "R", BTreeMap::new()).unwrap();
        g.create_index("pv", "P", "v").unwrap();
        let store = MvccStore::new(g);
        let checkpoint = store.checkpoint().unwrap();

        // after it: a change, deletes, a new node, label and index, then a write still open
        let mut w = store.begin_write();
        GraphView::update_node_properties(&mut w, a, v(100)).unwrap();
        GraphView::delete_edge(&mut w, e).unwrap();
        GraphView::delete_node_cascading(&mut w, gone).unwrap();
        GraphView::create_node(&mut w, &["Q"], v(4)).unwrap();
        GraphView::create_index(&mut w, "qv", "Q", "v").unwrap();
        w.commit().unwrap();
        let mut open = store.begin_write();
        GraphView::create_node(&mut open, &["P"], v(5)).unwrap();

        let g = written(&checkpoint);
        assert_eq!((g.node_count(), g.edge_count()), (3, 2));
        assert_eq!(v_of(&g, a), Some(PropertyValue::Int(1)));
        assert_eq!(v_of(&g, gone), Some(PropertyValue::Int(3)));
        assert!(g.strings().get("Q").is_none(), "names are the checkpoint's");
        let indexes: Vec<String> = g
            .as_parts()
            .index_definitions
            .into_iter()
            .map(|d| d.0)
            .collect();
        assert_eq!(indexes, ["pv"]);
        open.abort();
    }

    #[test]
    fn commits_and_reads_carry_on_while_a_checkpoint_is_open() {
        let store = MvccStore::new(Graph::new());
        let checkpoint = store.checkpoint().unwrap();
        let mut w = store
            .try_begin_write(Duration::from_secs(10))
            .expect("a checkpoint doesn't hold the writer lock");
        GraphView::create_node(&mut w, &["N"], BTreeMap::new()).unwrap();
        w.commit().unwrap();
        assert_eq!(store.begin_read().unwrap().node_count(), 1);
        assert_eq!(written(&checkpoint).node_count(), 0);
    }

    #[test]
    fn checkpoints_and_pauses_take_turns() {
        let store = &MvccStore::new(Graph::new());
        let first = store.checkpoint().unwrap();
        let (done, waited) = mpsc::channel();
        std::thread::scope(|s| {
            let checkpointed = done.clone();
            s.spawn(move || {
                let _second = store.checkpoint().unwrap();
                checkpointed.send("checkpoint").unwrap();
            });
            s.spawn(move || {
                let _pause = store.pause_commits();
                done.send("pause").unwrap();
            });
            assert!(
                waited.recv_timeout(Duration::from_millis(200)).is_err(),
                "both wait for the open checkpoint"
            );
            drop(first);
            for _ in 0..2 {
                waited.recv_timeout(Duration::from_secs(10)).unwrap();
            }
        });
    }
}
