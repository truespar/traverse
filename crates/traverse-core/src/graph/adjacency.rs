//! Dense slot-indexed adjacency storage (CSR-like): O(1) array indexing instead
//! of hash + pointer chase.
//!
//! ## Packed entries
//!
//! 12-byte `AdjEntry` (u32 edge_id, neighbor, edge_type) instead of the 24-byte
//! `EdgeEndpointStored`: half the memory, twice the entries per cache line in BFS.
//!
//! ## Mutation
//!
//! The packed arrays never change after construction: they're `Arc<[T]>`, shared by
//! [`AdjacencySnapshot`]s. The first mutation creates a boxed overlay map; a node's
//! first mutation copies its packed entries into a per-node `Vec` there, and its
//! later reads/writes use the overlay. Untouched nodes keep reading the packed
//! array. [`AdjacencyStore::compact`] folds the overlay into fresh packed arrays.
//!
//! ## Cache layout
//!
//! `Arc<[T]>` is a fat pointer like `Vec`'s (indexing loads nothing extra) and the
//! overlay is `Option<Box<...>>` (one nullable pointer), so the struct stays within
//! one 64-byte line and `is_none()` hits the same line as `slots`/`edges`.

use std::borrow::Cow;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::error::Result;
use crate::graph::types::{Direction, EdgeEndpointStored, EdgeId, EdgeTypeId, NodeId};
use crate::tx::view::GraphView;

/// Packed adjacency entry: 12 bytes vs 24 for `EdgeEndpointStored`. u32 IDs (up to 4 billion
/// nodes/edges).
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct AdjEntry {
    edge_id: u32,
    neighbor: u32,
    edge_type: u32,
}

impl AdjEntry {
    /// Edge ID.
    #[inline(always)]
    pub fn edge_id(&self) -> EdgeId {
        EdgeId(self.edge_id as u64)
    }

    /// Edge type ID.
    #[inline(always)]
    pub fn edge_type(&self) -> EdgeTypeId {
        EdgeTypeId(self.edge_type)
    }

    /// Neighbor node ID.
    #[inline(always)]
    pub fn neighbor(&self) -> NodeId {
        NodeId(self.neighbor as u64)
    }

    /// Packs an `EdgeEndpointStored`.
    #[inline]
    fn from_endpoint(ep: &EdgeEndpointStored) -> Self {
        debug_assert!(
            ep.edge_id.0 <= u32::MAX as u64,
            "EdgeId {} overflows u32 in AdjEntry",
            ep.edge_id.0
        );
        debug_assert!(
            ep.neighbor.0 <= u32::MAX as u64,
            "NodeId {} overflows u32 in AdjEntry",
            ep.neighbor.0
        );
        Self {
            edge_id: ep.edge_id.0 as u32,
            neighbor: ep.neighbor.0 as u32,
            edge_type: ep.edge_type.0,
        }
    }
}

/// Offset and length of a node's slice in the packed array.
#[derive(Debug, Clone, Copy, Default)]
struct AdjSlot {
    start: u32,
    len: u32,
}

/// Per-node overlay of mutated adjacency lists.
type Overlay = FxHashMap<u32, Vec<AdjEntry>>;

/// Dense adjacency store indexed by `NodeId.0`.
///
/// `slots[node_id]` = `(start, len)` into the packed `edges`; empty nodes have
/// `len == 0`, nodes past `slots.len()` return `&[]`. Mutations copy-on-write
/// into a boxed overlay; the packed arrays are never modified, so a clone shares
/// them and copies only the overlay.
#[derive(Debug, Clone)]
pub struct AdjacencyStore {
    slots: Arc<[AdjSlot]>,
    edges: Arc<[AdjEntry]>,
    /// Overlay, present only after mutations. `None` check is a null test on the same cache line as
    /// `slots`/`edges`.
    modified: Option<Box<Overlay>>,
}

impl Default for AdjacencyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AdjacencyStore {
    /// Empty store.
    pub fn new() -> Self {
        Self {
            slots: Arc::from(Vec::new()),
            edges: Arc::from(Vec::new()),
            modified: None,
        }
    }

    /// Heap memory used.
    pub fn memory_usage(&self) -> usize {
        let slots = self.slots.len() * std::mem::size_of::<AdjSlot>();
        let edges = self.edges.len() * std::mem::size_of::<AdjEntry>();
        let overlay = self.modified.as_deref().map_or(0, |o| {
            o.values()
                .map(|v| v.capacity() * std::mem::size_of::<AdjEntry>() + 32)
                .sum()
        });
        slots + edges + overlay
    }

    /// Nodes whose lists live in the overlay (compaction policy input).
    pub fn overlay_len(&self) -> usize {
        self.modified.as_deref().map_or(0, |o| o.len())
    }

    /// Folds the overlay into fresh packed arrays. Snapshots holding the old arrays keep them.
    pub fn compact(&mut self) {
        let Some(overlay) = self.modified.take() else {
            return;
        };
        let slot_count = overlay
            .keys()
            .map(|&k| k as usize + 1)
            .max()
            .unwrap_or(0)
            .max(self.slots.len());
        let list = |idx: usize| -> &[AdjEntry] {
            match overlay.get(&(idx as u32)) {
                Some(v) => v,
                None if idx < self.slots.len() => {
                    let slot = self.slots[idx];
                    &self.edges[slot.start as usize..(slot.start + slot.len) as usize]
                }
                None => &[],
            }
        };
        let mut slots = Vec::with_capacity(slot_count);
        let mut offset = 0u32;
        for idx in 0..slot_count {
            let len = list(idx).len() as u32;
            slots.push(AdjSlot { start: offset, len });
            offset += len;
        }
        let mut edges = Arc::<[AdjEntry]>::new_uninit_slice(offset as usize);
        {
            let out = Arc::get_mut(&mut edges).expect("fresh allocation");
            let mut pos = 0usize;
            for idx in 0..slot_count {
                for e in list(idx) {
                    out[pos].write(*e);
                    pos += 1;
                }
            }
            debug_assert_eq!(pos, out.len());
        }
        // SAFETY: the loop above wrote every one of the `offset` entries
        self.edges = unsafe { edges.assume_init() };
        self.slots = slots.into();
    }

    /// Dense store via a two-pass counting sort. Allocates only degree counts
    /// (`Vec<u32>` per node), the slots and the exactly sized packed array; no
    /// intermediate collection, `make_iter` is just called twice.
    ///
    /// * `make_iter`: closure returning `(local_node, edge_id, edge_type, neighbor)`.
    /// * `max_node_id`: exclusive slot bound; edges with `local_node` past it are skipped.
    pub fn from_edges<F, I>(make_iter: F, max_node_id: u64) -> Self
    where
        F: Fn() -> I,
        I: Iterator<Item = (NodeId, EdgeId, EdgeTypeId, NodeId)>,
    {
        Self::try_from_edges(make_iter, max_node_id)
            .unwrap_or_else(|e| panic!("building adjacency for {max_node_id} node ids: {e}"))
    }

    /// Temporary bytes [`try_from_edges`](Self::try_from_edges) needs per id space of
    /// `max_node_id`.
    pub(crate) fn build_bytes(max_node_id: u64) -> usize {
        (max_node_id as usize)
            .saturating_mul(std::mem::size_of::<u32>() + std::mem::size_of::<AdjSlot>())
    }

    /// [`from_edges`](Self::from_edges) that reports an allocation it can't make (the id space
    /// comes from a file) instead of aborting.
    pub fn try_from_edges<F, I>(make_iter: F, max_node_id: u64) -> crate::error::Result<Self>
    where
        F: Fn() -> I,
        I: Iterator<Item = (NodeId, EdgeId, EdgeTypeId, NodeId)>,
    {
        let slot_count = usize::try_from(max_node_id)
            .map_err(|_| crate::error::Error::AllocationFailed(usize::MAX))?;
        let alloc_failed =
            |_| crate::error::Error::AllocationFailed(Self::build_bytes(max_node_id));

        // pass 1: degrees
        let mut degrees = Vec::new();
        degrees
            .try_reserve_exact(slot_count)
            .map_err(alloc_failed)?;
        degrees.resize(slot_count, 0u32);
        for (local, _, _, _) in make_iter() {
            let idx = local.0 as usize;
            if idx < slot_count {
                degrees[idx] += 1;
            }
        }

        // prefix sum -> start offsets
        let mut slots = Vec::new();
        slots.try_reserve_exact(slot_count).map_err(alloc_failed)?;
        let mut offset = 0u32;
        for &deg in &degrees {
            slots.push(AdjSlot {
                start: offset,
                len: deg,
            });
            offset += deg;
        }

        // packed array in one allocation, built in place (no Vec -> Arc copy of the big array)
        let total = offset as usize;
        let edges = Arc::<[AdjEntry]>::new_zeroed_slice(total);
        // SAFETY: AdjEntry is three u32s, all-zero is a valid value
        let mut edges: Arc<[AdjEntry]> = unsafe { edges.assume_init() };
        let packed = Arc::get_mut(&mut edges).expect("fresh allocation");

        // reuse degrees as write cursors (reset to 0)
        let mut cursors = degrees;
        for c in cursors.iter_mut() {
            *c = 0;
        }

        // pass 2: scatter edges into their slots
        for (local, eid, etype, neighbor) in make_iter() {
            let idx = local.0 as usize;
            if idx < slot_count {
                let pos = slots[idx].start + cursors[idx];
                cursors[idx] += 1;
                packed[pos as usize] = AdjEntry {
                    edge_id: eid.0 as u32,
                    neighbor: neighbor.0 as u32,
                    edge_type: etype.0,
                };
            }
        }

        Ok(Self {
            slots: slots.into(),
            edges,
            modified: None,
        })
    }

    /// Packed slice of a node (ignores the overlay).
    #[inline]
    fn packed_slice(&self, idx: usize) -> &[AdjEntry] {
        if idx >= self.slots.len() {
            return &[];
        }
        let slot = self.slots[idx];
        &self.edges[slot.start as usize..(slot.start + slot.len) as usize]
    }

    /// Adjacency slice of a node, O(1), no hashing. Fast path: null check of the
    /// overlay (same cache line as `slots`/`edges`) + array index. After mutations:
    /// outlined map lookup.
    #[inline(always)]
    pub fn neighbors(&self, node: NodeId) -> &[AdjEntry] {
        if self.modified.is_none() {
            self.packed_slice(node.0 as usize)
        } else {
            self.neighbors_modified(node)
        }
    }

    /// `neighbors()` slow path with an overlay.
    #[inline(never)]
    fn neighbors_modified(&self, node: NodeId) -> &[AdjEntry] {
        let idx = node.0 as usize;
        if let Some(overlay) = self.modified.as_deref()
            && let Some(vec) = overlay.get(&(idx as u32))
        {
            return vec.as_slice();
        }
        self.packed_slice(idx)
    }

    /// Neighbor count of a node. O(1).
    #[inline(always)]
    pub fn degree(&self, node: NodeId) -> usize {
        if self.modified.is_none() {
            let idx = node.0 as usize;
            if idx >= self.slots.len() {
                0
            } else {
                self.slots[idx].len as usize
            }
        } else {
            self.degree_modified(node)
        }
    }

    /// `degree()` slow path with an overlay.
    #[inline(never)]
    fn degree_modified(&self, node: NodeId) -> usize {
        let idx = node.0 as usize;
        if let Some(overlay) = self.modified.as_deref()
            && let Some(vec) = overlay.get(&(idx as u32))
        {
            return vec.len();
        }
        if idx >= self.slots.len() {
            0
        } else {
            self.slots[idx].len as usize
        }
    }

    // -- Mutations -----------------------------------------------------

    /// Creates the overlay if needed and copies the node's packed list into it on first access.
    fn ensure_modified(&mut self, idx: u32) {
        if self.modified.is_none() {
            self.modified = Some(Box::new(FxHashMap::default()));
        }
        let overlay = self.modified.as_deref_mut().unwrap();
        if let std::collections::hash_map::Entry::Vacant(e) = overlay.entry(idx) {
            let idx_usize = idx as usize;
            let packed = if idx_usize < self.slots.len() {
                let slot = self.slots[idx_usize];
                self.edges[slot.start as usize..(slot.start + slot.len) as usize].to_vec()
            } else {
                Vec::new()
            };
            e.insert(packed);
        }
    }

    /// Appends an endpoint to a node's list.
    pub fn push(&mut self, node: NodeId, ep: EdgeEndpointStored) {
        let idx = node.0 as u32;
        self.ensure_modified(idx);
        self.modified
            .as_deref_mut()
            .unwrap()
            .get_mut(&idx)
            .unwrap()
            .push(AdjEntry::from_endpoint(&ep));
    }

    /// Removes edge `edge_id` from a node's list.
    pub fn remove(&mut self, node: NodeId, edge_id: EdgeId) {
        let idx = node.0 as u32;
        let eid32 = edge_id.0 as u32;
        self.ensure_modified(idx);
        self.modified
            .as_deref_mut()
            .unwrap()
            .get_mut(&idx)
            .unwrap()
            .retain(|ep| ep.edge_id != eid32);
    }

    /// Clears a node's list (node deletion): masks packed entries with an empty Vec, else drops the
    /// overlay entry.
    pub fn clear_node(&mut self, node: NodeId) {
        let idx = node.0 as u32;
        let idx_usize = idx as usize;
        if idx_usize < self.slots.len() && self.slots[idx_usize].len > 0 {
            // packed entries exist: mask with an empty vec
            if self.modified.is_none() {
                self.modified = Some(Box::new(FxHashMap::default()));
            }
            self.modified
                .as_deref_mut()
                .unwrap()
                .insert(idx, Vec::new());
        } else if let Some(overlay) = self.modified.as_deref_mut() {
            // no packed entries: dropping the overlay entry is enough
            overlay.remove(&idx);
        }
    }
}

/// Whole-graph adjacency for algorithms, fixed for its lifetime: borrowed from a view that can't
/// change while borrowed (`Graph`, the writer's own `WriteTx`), a cheap copy of a quiescent store
/// (shared packed arrays plus the small overlay), or a projection built from snapshot reads.
#[derive(Debug, Clone)]
pub struct AdjacencySnapshot<'a> {
    out: Cow<'a, AdjacencyStore>,
    inc: Cow<'a, AdjacencyStore>,
    highest_node_id: u64,
}

impl<'a> AdjacencySnapshot<'a> {
    /// Snapshot over existing stores.
    pub fn new(
        out: Cow<'a, AdjacencyStore>,
        inc: Cow<'a, AdjacencyStore>,
        highest_node_id: u64,
    ) -> Self {
        Self {
            out,
            inc,
            highest_node_id,
        }
    }

    /// The view's own snapshot, else a projection built from its per-node reads.
    pub fn of(view: &'a dyn GraphView) -> Result<Self> {
        match view.adjacency_snapshot() {
            Some(snap) => Ok(snap),
            None => Self::build(view),
        }
    }

    /// Projection built from per-node reads: exact for any view, O(V + E).
    pub fn build(view: &dyn GraphView) -> Result<AdjacencySnapshot<'static>> {
        let highest = view.highest_node_id();
        let mut edges: Vec<(NodeId, EdgeId, EdgeTypeId, NodeId)> = Vec::new();
        for node in view.nodes_vec() {
            for ep in view.edges_with_neighbor(node, Direction::Outgoing, None)? {
                edges.push((node, ep.edge_id, ep.edge_type, ep.neighbor));
            }
        }
        let highest = edges
            .iter()
            .map(|e| e.0.0.max(e.3.0) + 1)
            .max()
            .unwrap_or(0)
            .max(highest);
        let (out, inc) = rayon::join(
            || AdjacencyStore::from_edges(|| edges.iter().copied(), highest),
            || {
                AdjacencyStore::from_edges(
                    || edges.iter().map(|&(s, e, t, d)| (d, e, t, s)),
                    highest,
                )
            },
        );
        Ok(AdjacencySnapshot {
            out: Cow::Owned(out),
            inc: Cow::Owned(inc),
            highest_node_id: highest,
        })
    }

    /// Outgoing entries of `node`.
    #[inline(always)]
    pub fn outgoing(&self, node: NodeId) -> &[AdjEntry] {
        self.out.neighbors(node)
    }

    /// Incoming entries of `node`.
    #[inline(always)]
    pub fn incoming(&self, node: NodeId) -> &[AdjEntry] {
        self.inc.neighbors(node)
    }

    /// One past the highest node id; slices are indexable by `0..highest_node_id()`.
    pub fn highest_node_id(&self) -> u64 {
        self.highest_node_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(edge: u64, neighbor: u64) -> EdgeEndpointStored {
        EdgeEndpointStored {
            edge_id: EdgeId(edge),
            edge_type: EdgeTypeId(0),
            neighbor: NodeId(neighbor),
        }
    }

    fn ids(entries: &[AdjEntry]) -> Vec<u64> {
        entries.iter().map(|e| e.edge_id().0).collect()
    }

    #[test]
    fn compact_folds_the_overlay_and_keeps_old_clones_intact() {
        let mut store = AdjacencyStore::from_edges(
            || {
                [
                    (NodeId(0), EdgeId(0), EdgeTypeId(0), NodeId(1)),
                    (NodeId(1), EdgeId(1), EdgeTypeId(0), NodeId(2)),
                ]
                .into_iter()
            },
            3,
        );
        store.push(NodeId(0), ep(2, 2));
        store.remove(NodeId(1), EdgeId(1));
        store.push(NodeId(5), ep(3, 0));
        let before = store.clone();
        store.compact();
        assert_eq!(store.overlay_len(), 0);
        assert_eq!(ids(store.neighbors(NodeId(0))), vec![0, 2]);
        assert!(store.neighbors(NodeId(1)).is_empty());
        assert_eq!(ids(store.neighbors(NodeId(5))), vec![3]);
        assert!(store.neighbors(NodeId(9)).is_empty());
        // the clone still reads its own arrays + overlay
        assert_eq!(ids(before.neighbors(NodeId(0))), vec![0, 2]);
        assert_eq!(ids(before.neighbors(NodeId(5))), vec![3]);
    }
}
