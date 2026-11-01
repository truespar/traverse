use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::property::InternedPropertyMap;
use super::types::{LabelId, NodeId};

/// Graph node.
#[derive(Clone, Debug, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct Node {
    /// Node ID.
    pub id: NodeId,
    /// Labels (usually 1-2).
    pub labels: SmallVec<[LabelId; 2]>,
    /// Properties (interned keys).
    pub properties: InternedPropertyMap,
}

impl Node {
    /// Has `label`?
    pub fn has_label(&self, label: LabelId) -> bool {
        self.labels.contains(&label)
    }
}
