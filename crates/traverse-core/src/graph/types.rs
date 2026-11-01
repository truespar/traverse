use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Node identifier.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct NodeId(pub u64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node#{}", self.0)
    }
}

/// Edge identifier.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct EdgeId(pub u64);

impl fmt::Display for EdgeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "edge#{}", self.0)
    }
}

/// Interned node label ID.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct LabelId(pub u32);

/// Interned edge type ID.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct EdgeTypeId(pub u32);

/// Interned property key ID.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
#[rkyv(derive(PartialEq, Eq, PartialOrd, Ord, Hash))]
pub struct PropertyKeyId(pub u32);

/// Compact edge topology stored in adjacency lists: enough to filter by type and reach neighbors
/// without looking up the full `Edge`.
#[derive(Debug, Clone, Copy)]
pub struct EdgeEndpointStored {
    /// Edge ID.
    pub edge_id: EdgeId,
    /// Edge type (interned).
    pub edge_type: EdgeTypeId,
    /// Neighbor reached via this edge.
    pub neighbor: NodeId,
}

/// Lightweight edge topology for traversal, no property clones.
#[derive(Debug, Clone, Copy)]
pub struct EdgeEndpoint {
    /// Edge ID.
    pub edge_id: EdgeId,
    /// Edge type (interned).
    pub edge_type: EdgeTypeId,
    /// Neighbor reached via this edge.
    pub neighbor: NodeId,
    /// Source node.
    pub source: NodeId,
    /// Target node.
    pub target: NodeId,
}

/// Traversal direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Outgoing edges (source -> target).
    Outgoing,
    /// Incoming edges (target -> source).
    Incoming,
    /// Both directions.
    Both,
}
