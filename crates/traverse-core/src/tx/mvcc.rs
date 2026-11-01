//! Per-object MVCC delta chains.
//!
//! The graph holds the latest state in place. A transaction that changes an
//! existing node or edge first links ONE image of its pre-transaction state into
//! the object's chain (plus a marker if it deletes it). The writer lock serializes
//! transactions, so a chain is in commit order, newest first: a snapshot reader
//! takes the oldest image newer than its snapshot (or of an uncommitted writer)
//! and never reads the live object a writer may be changing.
//!
//! Deltas live in a per-transaction arena, retired at commit and freed once GC
//! has cut every chain past them.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use smallvec::SmallVec;

use crate::graph::property::InternedPropertyMap;
use crate::graph::types::{EdgeTypeId, LabelId, NodeId};

// ---------------------------------------------------------------------------
// CommitInfo
// ---------------------------------------------------------------------------

/// `CommitInfo::timestamp` before commit: newer than every snapshot, so readers undo it.
pub const TXN_ID_SENTINEL: u64 = u64::MAX;

/// Metadata shared by all deltas of one transaction. One atomic `timestamp`
/// gives **O(1) commit**: switching it from the sentinel to the commit timestamp
/// publishes every delta at once.
pub struct CommitInfo {
    /// [`TXN_ID_SENTINEL`] before commit, then the encoded commit `HybridTimestamp`.
    pub timestamp: AtomicU64,
}

impl CommitInfo {
    /// New uncommitted `CommitInfo`.
    pub fn new_uncommitted() -> Self {
        Self {
            timestamp: AtomicU64::new(TXN_ID_SENTINEL),
        }
    }

    /// Committed yet?
    #[inline]
    pub fn is_committed(&self) -> bool {
        self.timestamp.load(Ordering::Acquire) != TXN_ID_SENTINEL
    }

    /// Publishes the commit timestamp atomically.
    #[inline]
    pub fn commit(&self, encoded_ts: u64) {
        debug_assert_ne!(encoded_ts, TXN_ID_SENTINEL);
        self.timestamp.store(encoded_ts, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for CommitInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ts = self.timestamp.load(Ordering::Relaxed);
        if ts == TXN_ID_SENTINEL {
            f.debug_struct("CommitInfo")
                .field("status", &"uncommitted")
                .finish()
        } else {
            f.debug_struct("CommitInfo")
                .field("commit_ts", &ts)
                .finish()
        }
    }
}

// ---------------------------------------------------------------------------
// Delta
// ---------------------------------------------------------------------------

/// One chain entry: what a transaction did to an object, with the state to restore.
///
/// # Safety
///
/// - `commit_info` points to a `CommitInfo` that outlives the delta (both are
///   retired together).
/// - `next` is read with `Acquire` lock-free and written under the structural
///   write lock.
pub struct Delta {
    /// The transaction's commit metadata, shared by all its deltas.
    pub(crate) commit_info: *const CommitInfo,

    /// Next (older) delta, or null at the tail.
    pub(crate) next: AtomicPtr<Delta>,

    /// What to restore.
    pub(crate) action: DeltaAction,
}

impl Delta {
    /// Commit timestamp, or `TXN_ID_SENTINEL` while uncommitted.
    #[inline]
    pub(crate) fn commit_ts(&self) -> u64 {
        // SAFETY: commit_info lives as long as the chain.
        unsafe { (*self.commit_info).timestamp.load(Ordering::Acquire) }
    }

    /// Next (older) delta, or null at the tail.
    #[inline]
    pub(crate) fn next(&self) -> *mut Delta {
        self.next.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for Delta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Delta")
            .field("action", &self.action)
            .field("next", &self.next.load(Ordering::Relaxed))
            .finish()
    }
}

// SAFETY: shared across threads through atomic pointer chains; mutated only under the structural
// write lock. `commit_info` is read-only after creation and lives as long as the delta.
unsafe impl Send for Delta {}
// SAFETY: see `Send` above
unsafe impl Sync for Delta {}

// ---------------------------------------------------------------------------
// DeltaAction
// ---------------------------------------------------------------------------

/// A chain entry's payload.
#[derive(Debug, Clone)]
pub enum DeltaAction {
    /// The node as it was before this transaction first touched it (it existed).
    NodeImage {
        /// Labels before the transaction.
        labels: SmallVec<[LabelId; 2]>,
        /// Properties before the transaction.
        properties: InternedPropertyMap,
    },
    /// This transaction deleted the node; its image is further down the chain.
    NodeDeleted,
    /// The edge as it was before this transaction first touched it (it existed).
    EdgeImage {
        /// Source node.
        source: NodeId,
        /// Target node.
        target: NodeId,
        /// Edge type.
        edge_type: EdgeTypeId,
        /// Properties before the transaction.
        properties: InternedPropertyMap,
    },
    /// This transaction deleted the edge; its image is further down the chain.
    EdgeDeleted,
}

impl DeltaAction {
    /// A full pre-transaction state (not a marker)?
    #[inline]
    pub(crate) fn is_image(&self) -> bool {
        matches!(
            self,
            DeltaAction::NodeImage { .. } | DeltaAction::EdgeImage { .. }
        )
    }
}

/// The image a snapshot at `snapshot` sees through a chain: the oldest image of a transaction it
/// can't see (committed after it, or not committed at all), else `None` (the live object is what
/// it sees). Chains are commit-ordered, so the walk stops at the first visible delta.
///
/// # Safety
///
/// Every delta reachable from `head` must stay alive for `'a`: the caller holds the structural
/// read lock and is registered, so GC can't free them.
#[inline]
pub(crate) unsafe fn image_at<'a>(snapshot: u64, head: *const Delta) -> Option<&'a DeltaAction> {
    let mut current = head;
    let mut image = None;
    while !current.is_null() {
        // SAFETY: forwarded to the caller
        let delta: &'a Delta = unsafe { &*current };
        let ts = delta.commit_ts();
        if ts != TXN_ID_SENTINEL && ts <= snapshot {
            break;
        }
        if delta.action.is_image() {
            image = Some(&delta.action);
        }
        current = delta.next();
    }
    image
}

/// Changed by a transaction committed after `snapshot` (or not committed)? The newest delta
/// decides: the OCC conflict test.
///
/// # Safety
///
/// As [`image_at`].
pub(crate) unsafe fn changed_since(snapshot: u64, head: *const Delta) -> bool {
    if head.is_null() {
        return false;
    }
    // SAFETY: forwarded to the caller
    let ts = unsafe { (*head).commit_ts() };
    ts == TXN_ID_SENTINEL || ts > snapshot
}

// ---------------------------------------------------------------------------
// DeltaArena
// ---------------------------------------------------------------------------

/// Largest arena page, in deltas.
const MAX_PAGE: usize = 256;

/// Per-transaction bump allocator for [`Delta`]s. Pages never grow past their
/// capacity, so pointers stay stable; page sizes grow geometrically from 4, so a
/// one-row transaction allocates little.
pub struct DeltaArena {
    pages: Vec<Vec<Delta>>,
}

impl DeltaArena {
    /// Empty arena.
    pub fn new() -> Self {
        Self { pages: Vec::new() }
    }

    /// Allocates a `Delta`; returns a pointer stable until the arena drops.
    pub fn alloc(&mut self, delta: Delta) -> *mut Delta {
        let full = self.pages.last().is_none_or(|p| p.len() == p.capacity());
        if full {
            let cap = self
                .pages
                .last()
                .map_or(4, |p| (p.capacity() * 4).min(MAX_PAGE));
            self.pages.push(Vec::with_capacity(cap));
        }
        let page = self.pages.last_mut().unwrap();
        let idx = page.len();
        debug_assert!(idx < page.capacity(), "arena page must not reallocate");
        page.push(delta);
        // SAFETY: capacity is never exceeded, so the page doesn't reallocate and the pointer lives
        // as long as the page.
        unsafe { page.as_mut_ptr().add(idx) }
    }

    /// Deltas allocated.
    pub fn len(&self) -> usize {
        self.pages.iter().map(|p| p.len()).sum()
    }

    /// No deltas allocated.
    pub fn is_empty(&self) -> bool {
        self.pages.iter().all(|p| p.is_empty())
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.pages
            .iter()
            .map(|p| p.capacity() * std::mem::size_of::<Delta>())
            .sum()
    }
}

impl Default for DeltaArena {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DeltaArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeltaArena")
            .field("pages", &self.pages.len())
            .field("total_deltas", &self.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Link a delta into an object's chain
// ---------------------------------------------------------------------------

/// Allocates a delta in the arena and prepends it to the chain at `head`; returns its pointer.
/// Caller holds the structural write lock.
#[inline]
pub(crate) fn create_and_link_delta(
    arena: &mut DeltaArena,
    head: &AtomicPtr<Delta>,
    commit_info: *const CommitInfo,
    action: DeltaAction,
) -> *mut Delta {
    let old_head = head.load(Ordering::Relaxed);
    let ptr = arena.alloc(Delta {
        commit_info,
        next: AtomicPtr::new(old_head),
        action,
    });
    head.store(ptr, Ordering::Release);
    ptr
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::property::PropertyValue;
    use crate::graph::types::PropertyKeyId;
    use std::ptr;

    fn image(n: i64) -> DeltaAction {
        let mut properties = InternedPropertyMap::default();
        properties.insert(PropertyKeyId(0), PropertyValue::Int(n));
        DeltaAction::NodeImage {
            labels: SmallVec::new(),
            properties,
        }
    }

    fn value(action: &DeltaAction) -> i64 {
        match action {
            DeltaAction::NodeImage { properties, .. } => match properties.get(PropertyKeyId(0)) {
                Some(PropertyValue::Int(n)) => n,
                _ => panic!("no value"),
            },
            _ => panic!("not an image"),
        }
    }

    #[test]
    fn commit_info_lifecycle() {
        let ci = CommitInfo::new_uncommitted();
        assert!(!ci.is_committed());
        ci.commit(42);
        assert!(ci.is_committed());
        assert_eq!(ci.timestamp.load(Ordering::Relaxed), 42);
    }

    #[test]
    fn arena_pages_grow_and_pointers_stay_put() {
        let mut arena = DeltaArena::new();
        let ci = CommitInfo::new_uncommitted();
        let ptrs: Vec<*mut Delta> = (0..600)
            .map(|i| {
                arena.alloc(Delta {
                    commit_info: &ci,
                    next: AtomicPtr::new(ptr::null_mut()),
                    action: image(i),
                })
            })
            .collect();
        assert_eq!(arena.len(), 600);
        // 4 + 16 + 64 + 256 + 256 + ...
        assert_eq!(arena.pages[0].capacity(), 4);
        assert_eq!(arena.pages[3].capacity(), MAX_PAGE);
        for (i, &p) in ptrs.iter().enumerate() {
            // SAFETY: arena pages never move
            assert_eq!(value(&unsafe { &*p }.action), i as i64);
        }
    }

    #[test]
    fn image_at_takes_the_oldest_invisible_image() {
        let mut arena = DeltaArena::new();
        let head = AtomicPtr::new(ptr::null_mut());
        let (t10, t20, t30) = (
            CommitInfo::new_uncommitted(),
            CommitInfo::new_uncommitted(),
            CommitInfo::new_uncommitted(),
        );
        t10.commit(10);
        t20.commit(20);
        t30.commit(30);
        // commit order: t10 wrote image 1, t20 image 2, t30 image 3 + delete marker
        create_and_link_delta(&mut arena, &head, &t10, image(1));
        create_and_link_delta(&mut arena, &head, &t20, image(2));
        create_and_link_delta(&mut arena, &head, &t30, image(3));
        create_and_link_delta(&mut arena, &head, &t30, DeltaAction::NodeDeleted);
        let h = head.load(Ordering::Acquire);
        // SAFETY: the arena outlives these reads
        unsafe {
            assert_eq!(value(image_at(5, h).unwrap()), 1);
            assert_eq!(value(image_at(15, h).unwrap()), 2);
            assert_eq!(value(image_at(25, h).unwrap()), 3);
            assert!(image_at(30, h).is_none());
            assert!(changed_since(25, h));
            assert!(!changed_since(30, h));
        }
        // an uncommitted writer's image hides its change from everyone
        let open = CommitInfo::new_uncommitted();
        create_and_link_delta(&mut arena, &head, &open, image(4));
        let h = head.load(Ordering::Acquire);
        // SAFETY: as above
        unsafe {
            assert_eq!(value(image_at(30, h).unwrap()), 4);
            assert!(changed_since(30, h));
        }
    }
}
