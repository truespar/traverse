//! Row frames of the procedure boundary: a [`Frame`] is a `Vec<SlotValue>` indexed through a
//! [`SymbolTable`] (name -> slot). [`SlotValue`] (~40 bytes) keeps hot types (NodeRef, EdgeRef,
//! Integer, Float, Bool) inline and boxes cold ones (String, List, Map, Path, ...) in `Heap`.

use std::cell::RefCell;
use std::ops::{Deref, DerefMut, Index, IndexMut};

use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{EdgeId, EdgeTypeId, NodeId};

use crate::types::CypherValue;

/// Slot type inferred by the planner. Metadata only for now; typed compact storage (`Vec<NodeId>`
/// instead of `Vec<CypherValue>`) could build on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotType {
    /// Node reference (scan, expand target, path endpoints).
    NodeRef,
    /// Single-hop relationship reference.
    EdgeRef,
    /// Full node (CREATE).
    Node,
    /// Full edge (CREATE).
    Edge,
    /// Path (shortestPath, path binding).
    Path,
    /// Float (avg(), float expressions, weighted cost).
    Float,
    /// List (variable-length rel_var, collect()).
    List,
    /// Unknown / mixed (UNWIND alias, projections, parameters).
    Dynamic,
}

/// Compact per-slot value of a [`Frame`]. Hot variants (NodeRef, EdgeRef,
/// Integer, Float, Bool) are inline, no allocation; cold ones (String, List,
/// Map, Node, Edge, Path, temporals) are boxed in `Heap`. ~40 bytes vs ~80 for
/// [`CypherValue`], halving frame memory.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotValue {
    /// Null / unset.
    Null,
    /// Boolean value.
    Bool(bool),
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit float.
    Float(f64),
    /// Lightweight node reference.
    NodeRef(NodeId),
    /// Lightweight edge reference.
    EdgeRef {
        /// Edge id.
        id: EdgeId,
        /// Edge type id.
        edge_type_id: EdgeTypeId,
        /// Source node.
        source: NodeId,
        /// Target node.
        target: NodeId,
    },
    /// Inline single-hop path, no heap allocation. `outgoing`: edge goes start->end, else end->start.
    SingleHopPath {
        /// Start node.
        start: NodeId,
        /// End node.
        end: NodeId,
        /// Edge id.
        edge_id: EdgeId,
        /// Edge type id.
        edge_type: EdgeTypeId,
        /// `true`: edge.source = start, edge.target = end; else reversed.
        outgoing: bool,
    },
    /// Boxed CypherValue for cold types (String, List, Map, Node, Edge, Path, temporals).
    Heap(Box<CypherValue>),
}

impl SlotValue {
    /// Null / unset?
    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, SlotValue::Null)
    }

    /// -> [`CypherValue`]: inline variants copy cheaply, Heap clones the box.
    #[inline(never)]
    pub fn to_cypher_value(&self) -> CypherValue {
        match self {
            SlotValue::Null => CypherValue::Null,
            SlotValue::Bool(b) => CypherValue::Bool(*b),
            SlotValue::Integer(i) => CypherValue::Integer(*i),
            SlotValue::Float(f) => CypherValue::Float(*f),
            SlotValue::NodeRef(id) => CypherValue::NodeRef(*id),
            SlotValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            } => CypherValue::EdgeRef {
                id: *id,
                edge_type_id: *edge_type_id,
                source: *source,
                target: *target,
            },
            SlotValue::SingleHopPath {
                start,
                end,
                edge_id,
                edge_type,
                outgoing,
            } => {
                let (source, target) = if *outgoing {
                    (*start, *end)
                } else {
                    (*end, *start)
                };
                CypherValue::Path {
                    nodes: vec![CypherValue::NodeRef(*start), CypherValue::NodeRef(*end)],
                    edges: vec![CypherValue::EdgeRef {
                        id: *edge_id,
                        edge_type_id: *edge_type,
                        source,
                        target,
                    }],
                }
            }
            SlotValue::Heap(cv) => cv.as_ref().clone(),
        }
    }

    /// Owned conversion - like [`Self::to_cypher_value`] but consumes the
    /// slot, so heap values move instead of cloning.
    pub fn into_cypher_value(self) -> CypherValue {
        match self {
            SlotValue::Heap(cv) => *cv,
            other => other.to_cypher_value(),
        }
    }

    /// [`PropertyValue`] -> [`SlotValue`] directly, skipping [`CypherValue`] for hot types.
    #[inline(never)]
    #[recursive::recursive]
    pub fn from_property(pv: PropertyValue) -> SlotValue {
        match pv {
            PropertyValue::Null => SlotValue::Null,
            PropertyValue::Bool(b) => SlotValue::Bool(b),
            PropertyValue::Int(i) => SlotValue::Integer(i),
            PropertyValue::Float(f) => SlotValue::Float(f),
            other => SlotValue::Heap(Box::new(CypherValue::from_property(&other))),
        }
    }

    /// Type name for error messages.
    #[inline(never)]
    pub fn type_name(&self) -> &'static str {
        match self {
            SlotValue::Null => "NULL",
            SlotValue::Bool(_) => "BOOLEAN",
            SlotValue::Integer(_) => "INTEGER",
            SlotValue::Float(_) => "FLOAT",
            SlotValue::NodeRef(_) => "NODE",
            SlotValue::EdgeRef { .. } => "RELATIONSHIP",
            SlotValue::SingleHopPath { .. } => "PATH",
            SlotValue::Heap(cv) => cv.type_name(),
        }
    }
}

impl From<CypherValue> for SlotValue {
    #[inline(never)]
    fn from(cv: CypherValue) -> Self {
        match cv {
            CypherValue::Null => SlotValue::Null,
            CypherValue::Bool(b) => SlotValue::Bool(b),
            CypherValue::Integer(i) => SlotValue::Integer(i),
            CypherValue::Float(f) => SlotValue::Float(f),
            CypherValue::NodeRef(id) => SlotValue::NodeRef(id),
            CypherValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            } => SlotValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            },
            other => SlotValue::Heap(Box::new(other)),
        }
    }
}

/// Variable name -> contiguous slot index of a [`Frame`].
#[derive(Clone)]
pub struct SymbolTable {
    names: Vec<String>,
    // FxHashMap: `slot(name)` runs for every Variable/Property reference on every row; FxHash on
    // short names is several times faster than SipHash
    lookup: rustc_hash::FxHashMap<String, usize>,
    types: Vec<SlotType>,
}

impl Default for SymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolTable {
    /// Empty table.
    pub fn new() -> Self {
        Self {
            names: Vec::new(),
            lookup: rustc_hash::FxHashMap::default(),
            types: Vec::new(),
        }
    }

    /// Slot count.
    pub fn width(&self) -> usize {
        self.names.len()
    }

    /// Get-or-allocate a slot for `name` as [`SlotType::Dynamic`].
    pub fn add(&mut self, name: String) -> usize {
        self.add_typed(name, SlotType::Dynamic)
    }

    /// Get-or-allocate a slot for `name` with a known type; an existing slot keeps its type.
    pub fn add_typed(&mut self, name: String, ty: SlotType) -> usize {
        if let Some(&slot) = self.lookup.get(&name) {
            return slot;
        }
        let slot = self.names.len();
        self.lookup.insert(name.clone(), slot);
        self.names.push(name);
        self.types.push(ty);
        slot
    }

    /// Inferred type of a slot.
    pub fn slot_type(&self, slot: usize) -> SlotType {
        self.types[slot]
    }

    /// Slot of `name`, if registered.
    pub fn slot(&self, name: &str) -> Option<usize> {
        self.lookup.get(name).copied()
    }

    /// Name of a slot.
    pub fn name(&self, slot: usize) -> &str {
        &self.names[slot]
    }
}

// -- Thread-local frame pool ------------------------------------------

const MAX_POOL_SIZE: usize = 2048;

thread_local! {
    static FRAME_POOL: RefCell<Vec<Vec<SlotValue>>> = const { RefCell::new(Vec::new()) };
}

/// Pooled Vec filled with `Null` (for `new_frame`).
#[inline(never)]
fn take_from_pool(width: usize) -> Vec<SlotValue> {
    FRAME_POOL.with(|pool| {
        if let Some(mut vec) = pool.borrow_mut().pop() {
            // cleared in return_to_pool (len 0, capacity kept); resize writes Null into each slot
            vec.resize(width, SlotValue::Null);
            vec
        } else {
            vec![SlotValue::Null; width]
        }
    })
}

/// Pooled Vec for cloning; the caller overwrites every slot via `extend_from_slice`, so no zeroing.
#[inline(never)]
fn take_from_pool_for_clone() -> Vec<SlotValue> {
    FRAME_POOL.with(|pool| pool.borrow_mut().pop().unwrap_or_default())
}

#[inline(never)]
fn return_to_pool(mut vec: Vec<SlotValue>) {
    // drops any Heap boxes
    vec.clear();
    FRAME_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.len() < MAX_POOL_SIZE {
            pool.push(vec);
        }
        // else drop the vec (capacity freed)
    });
}

// -- Frame newtype ----------------------------------------------------

/// Record as a contiguous vector of compact slot values, indexed by [`SymbolTable`]. Pooled per
/// thread: `Drop` returns the `Vec` to a thread-local free list; `new_frame`/`Clone` reuse pooled
/// Vecs.
pub struct Frame {
    slots: Vec<SlotValue>,
}

impl Frame {
    /// Frame from an existing Vec (no pool).
    #[inline(always)]
    pub fn from_vec(slots: Vec<SlotValue>) -> Self {
        Self { slots }
    }
}

impl Deref for Frame {
    type Target = [SlotValue];
    #[inline(always)]
    fn deref(&self) -> &[SlotValue] {
        &self.slots
    }
}

impl DerefMut for Frame {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [SlotValue] {
        &mut self.slots
    }
}

impl Index<usize> for Frame {
    type Output = SlotValue;
    #[inline(always)]
    fn index(&self, idx: usize) -> &SlotValue {
        &self.slots[idx]
    }
}

impl IndexMut<usize> for Frame {
    #[inline(always)]
    fn index_mut(&mut self, idx: usize) -> &mut SlotValue {
        &mut self.slots[idx]
    }
}

impl Default for Frame {
    #[inline(always)]
    fn default() -> Self {
        Self { slots: Vec::new() }
    }
}

impl PartialEq for Frame {
    fn eq(&self, other: &Self) -> bool {
        self.slots == other.slots
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.slots.fmt(f)
    }
}

impl Clone for Frame {
    #[inline(never)]
    fn clone(&self) -> Self {
        // fast path: cleared pooled Vec (len 0, capacity kept) + extend_from_slice; no zeroing, no
        // dropping old slots
        let mut vec = take_from_pool_for_clone();
        vec.extend_from_slice(&self.slots);
        Self { slots: vec }
    }
}

impl Drop for Frame {
    #[inline(never)]
    fn drop(&mut self) {
        if self.slots.capacity() > 0 {
            let vec = std::mem::take(&mut self.slots);
            return_to_pool(vec);
        }
    }
}

/// New frame, every slot `Null`.
pub fn new_frame(symtab: &SymbolTable) -> Frame {
    Frame {
        slots: take_from_pool(symtab.width()),
    }
}
