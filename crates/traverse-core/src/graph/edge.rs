use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Serialize};

use super::property::InternedPropertyMap;
use super::types::{EdgeId, EdgeTypeId, NodeId};

/// Directed edge.
#[derive(Clone, Debug, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct Edge {
    /// Edge ID.
    pub id: EdgeId,
    /// Edge type (interned).
    pub edge_type: EdgeTypeId,
    /// Source (start) node.
    pub source: NodeId,
    /// Target (end) node.
    pub target: NodeId,
    /// Properties (interned keys).
    pub properties: InternedPropertyMap,
}

impl Edge {
    /// New edge with no properties.
    pub fn new(id: EdgeId, edge_type: EdgeTypeId, source: NodeId, target: NodeId) -> Self {
        Self {
            id,
            edge_type,
            source,
            target,
            properties: InternedPropertyMap::new(),
        }
    }
}
