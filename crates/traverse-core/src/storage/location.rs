//! Entity ID -> on-disk page/slot index.

use rustc_hash::FxHashMap;

use crate::graph::types::{EdgeId, NodeId};

/// On-disk location of a record in a slotted page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLocation {
    /// Page holding the record.
    pub page_id: u64,
    /// Slot in the page.
    pub slot_idx: u16,
}

/// Entity ID -> on-disk record location.
/// Record locations by id, dense (ids are handed out densely); a sentinel page marks an id
/// without a record.
#[derive(Debug, Default)]
pub struct LocationIndex {
    nodes: Vec<RecordLocation>,
    edges: Vec<RecordLocation>,
}

const ABSENT: RecordLocation = RecordLocation {
    page_id: u64::MAX,
    slot_idx: 0,
};

fn set_at(v: &mut Vec<RecordLocation>, i: usize, loc: RecordLocation) {
    if i >= v.len() {
        v.resize(i + 1, ABSENT);
    }
    v[i] = loc;
}

fn get_at(v: &[RecordLocation], i: usize) -> Option<RecordLocation> {
    v.get(i).filter(|l| l.page_id != u64::MAX).copied()
}

fn take_at(v: &mut [RecordLocation], i: usize) -> Option<RecordLocation> {
    let loc = get_at(v, i)?;
    v[i] = ABSENT;
    Some(loc)
}

impl LocationIndex {
    /// Empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// An index from maps (the loader's).
    pub fn from_maps(
        nodes: FxHashMap<NodeId, RecordLocation>,
        edges: FxHashMap<EdgeId, RecordLocation>,
    ) -> Self {
        let mut idx = Self::default();
        for (id, loc) in nodes {
            idx.set_node(id, loc);
        }
        for (id, loc) in edges {
            idx.set_edge(id, loc);
        }
        idx
    }

    /// Sets a node's location.
    pub fn set_node(&mut self, id: NodeId, loc: RecordLocation) {
        set_at(&mut self.nodes, id.0 as usize, loc);
    }

    /// Sets an edge's location.
    pub fn set_edge(&mut self, id: EdgeId, loc: RecordLocation) {
        set_at(&mut self.edges, id.0 as usize, loc);
    }

    /// Node location, if known.
    pub fn get_node(&self, id: NodeId) -> Option<RecordLocation> {
        get_at(&self.nodes, id.0 as usize)
    }

    /// Edge location, if known.
    pub fn get_edge(&self, id: EdgeId) -> Option<RecordLocation> {
        get_at(&self.edges, id.0 as usize)
    }

    /// Forgets a node's location, returning it.
    pub fn remove_node(&mut self, id: NodeId) -> Option<RecordLocation> {
        take_at(&mut self.nodes, id.0 as usize)
    }

    /// Forgets an edge's location, returning it.
    pub fn remove_edge(&mut self, id: EdgeId) -> Option<RecordLocation> {
        take_at(&mut self.edges, id.0 as usize)
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.edges.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_node() {
        let mut idx = LocationIndex::new();
        let loc = RecordLocation {
            page_id: 1,
            slot_idx: 3,
        };
        idx.set_node(NodeId(42), loc);
        assert_eq!(idx.get_node(NodeId(42)), Some(loc));
        assert_eq!(idx.get_node(NodeId(99)), None);
    }

    #[test]
    fn set_and_get_edge() {
        let mut idx = LocationIndex::new();
        let loc = RecordLocation {
            page_id: 2,
            slot_idx: 0,
        };
        idx.set_edge(EdgeId(7), loc);
        assert_eq!(idx.get_edge(EdgeId(7)), Some(loc));
    }

    #[test]
    fn remove_node() {
        let mut idx = LocationIndex::new();
        let loc = RecordLocation {
            page_id: 0,
            slot_idx: 1,
        };
        idx.set_node(NodeId(1), loc);
        assert_eq!(idx.remove_node(NodeId(1)), Some(loc));
        assert_eq!(idx.get_node(NodeId(1)), None);
    }

    #[test]
    fn clear_removes_all() {
        let mut idx = LocationIndex::new();
        idx.set_node(
            NodeId(0),
            RecordLocation {
                page_id: 0,
                slot_idx: 0,
            },
        );
        idx.set_edge(
            EdgeId(0),
            RecordLocation {
                page_id: 1,
                slot_idx: 0,
            },
        );
        idx.clear();
        assert_eq!(idx.get_node(NodeId(0)), None);
        assert_eq!(idx.get_edge(EdgeId(0)), None);
    }
}
