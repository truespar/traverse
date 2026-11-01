//! Consistency checks on records read from a `.tvdb` file, before they become a [`Graph`]:
//! everything after trusts these invariants, so a damaged or crafted file must fail here.
//!
//! [`Graph`]: super::Graph

use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use rustc_hash::FxHashSet;

use super::LoadedEdge;
use super::node::Node;
use crate::error::{Error, Result};

/// Largest id counter a graph can hold: adjacency stores node and edge ids as `u32`.
pub(crate) const MAX_ID_COUNTER: u64 = u32::MAX as u64 + 1;

/// Bitset over an id space, filled from many threads.
struct IdSet(Vec<AtomicU64>);

impl IdSet {
    fn new(ids: u64) -> Result<Self> {
        let words = ids.div_ceil(64) as usize;
        let mut v = Vec::new();
        v.try_reserve_exact(words)
            .map_err(|_| Error::AllocationFailed(words * 8))?;
        v.extend((0..words).map(|_| AtomicU64::new(0)));
        Ok(Self(v))
    }

    /// Marks `id` (below the size); `false` if it was marked already.
    fn insert(&self, id: u64) -> bool {
        let bit = 1u64 << (id % 64);
        self.0[(id / 64) as usize].fetch_or(bit, Ordering::Relaxed) & bit == 0
    }

    fn contains(&self, id: u64) -> bool {
        self.0
            .get((id / 64) as usize)
            .is_some_and(|w| w.load(Ordering::Relaxed) & (1u64 << (id % 64)) != 0)
    }
}

fn corrupt(msg: impl Into<String>) -> Error {
    Error::InvalidFormat(msg.into())
}

/// Checks that loaded records fit together: id counters in range, ids below them and unique,
/// relationships between loaded nodes, every label, type and property key id naming a catalog
/// string, property data well-formed, catalog strings unique.
pub(crate) fn check_loaded(
    nodes: &[(Node, u64, u16)],
    edges: &[(LoadedEdge, u64, u16)],
    strings: &[String],
    next_node_id: u64,
    next_edge_id: u64,
) -> Result<()> {
    if next_node_id > MAX_ID_COUNTER || next_edge_id > MAX_ID_COUNTER {
        return Err(corrupt(format!(
            "id counters ({next_node_id} nodes, {next_edge_id} relationships) are past the \
             supported {MAX_ID_COUNTER}"
        )));
    }
    let names = u32::try_from(strings.len()).map_err(|_| corrupt("too many catalog strings"))?;
    let mut seen = FxHashSet::with_capacity_and_hasher(strings.len(), Default::default());
    if let Some(dup) = strings.iter().find(|s| !seen.insert(s.as_str())) {
        return Err(corrupt(format!("catalog string {dup:?} appears twice")));
    }
    drop(seen);

    // sized by the counters, which come from the file
    crate::memory::check_allocation(
        (next_node_id.div_ceil(8) + next_edge_id.div_ceil(8)) as usize,
    )?;
    let node_ids = IdSet::new(next_node_id)?;
    nodes.par_iter().try_for_each(|(node, _, _)| {
        let id = node.id.0;
        if id >= next_node_id {
            return Err(corrupt(format!(
                "node {id} is at or past the node counter {next_node_id}"
            )));
        }
        if !node_ids.insert(id) {
            return Err(corrupt(format!("node {id} is stored twice")));
        }
        if let Some(label) = node.labels.iter().find(|l| l.0 >= names) {
            return Err(corrupt(format!(
                "node {id} has label id {}, which names no string",
                label.0
            )));
        }
        node.properties
            .validate(names)
            .map_err(|e| corrupt(format!("node {id}: {e}")))
    })?;

    let edge_ids = IdSet::new(next_edge_id)?;
    edges.par_iter().try_for_each(|(edge, _, _)| {
        let id = edge.id().0;
        if id >= next_edge_id {
            return Err(corrupt(format!(
                "relationship {id} is at or past the relationship counter {next_edge_id}"
            )));
        }
        if !edge_ids.insert(id) {
            return Err(corrupt(format!("relationship {id} is stored twice")));
        }
        if edge.edge_type().0 >= names {
            return Err(corrupt(format!(
                "relationship {id} has type id {}, which names no string",
                edge.edge_type().0
            )));
        }
        for end in [edge.source(), edge.target()] {
            if !node_ids.contains(end.0) {
                return Err(corrupt(format!(
                    "relationship {id} connects node {}, which isn't stored",
                    end.0
                )));
            }
        }
        match edge {
            LoadedEdge::Full(e) => e
                .properties
                .validate(names)
                .map_err(|err| corrupt(format!("relationship {id}: {err}"))),
            LoadedEdge::Propless { .. } => Ok(()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::edge::Edge;
    use crate::graph::types::{EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};

    fn node(id: u64, label: u32) -> (Node, u64, u16) {
        let mut labels = smallvec::SmallVec::new();
        labels.push(LabelId(label));
        (
            Node {
                id: NodeId(id),
                labels,
                properties: Default::default(),
            },
            0,
            0,
        )
    }

    fn edge(id: u64, source: u64, target: u64) -> (LoadedEdge, u64, u16) {
        (
            LoadedEdge::Propless {
                id: EdgeId(id),
                edge_type: EdgeTypeId(1),
                source: NodeId(source),
                target: NodeId(target),
            },
            0,
            0,
        )
    }

    fn strings() -> Vec<String> {
        vec!["Person".into(), "KNOWS".into(), "name".into()]
    }

    fn check(nodes: &[(Node, u64, u16)], edges: &[(LoadedEdge, u64, u16)]) -> Result<()> {
        check_loaded(nodes, edges, &strings(), 10, 10)
    }

    #[test]
    fn accepts_a_consistent_graph() {
        check(&[node(0, 0), node(3, 0)], &[edge(0, 0, 3), edge(5, 3, 3)]).unwrap();
    }

    #[test]
    fn rejects_records_that_dont_fit_together() {
        let bad = [
            check(&[node(10, 0)], &[]),
            check(&[node(1, 0), node(1, 0)], &[]),
            check(&[node(1, 3)], &[]),
            check(&[node(1, 0)], &[edge(0, 1, 2)]),
            check(&[node(1, 0)], &[edge(10, 1, 1)]),
            check(&[node(1, 0)], &[edge(0, 1, 1), edge(0, 1, 1)]),
            check(
                &[node(1, 0)],
                &[(
                    LoadedEdge::Propless {
                        id: EdgeId(0),
                        edge_type: EdgeTypeId(7),
                        source: NodeId(1),
                        target: NodeId(1),
                    },
                    0,
                    0,
                )],
            ),
            check_loaded(&[], &[], &["a".into(), "a".into()], 0, 0),
            check_loaded(&[], &[], &strings(), MAX_ID_COUNTER + 1, 0),
        ];
        for (i, r) in bad.into_iter().enumerate() {
            assert!(matches!(r, Err(Error::InvalidFormat(_))), "case {i}: {r:?}");
        }
    }

    #[test]
    fn checks_property_data_and_key_ids() {
        let mut props = crate::graph::property::InternedPropertyMap::new();
        props.insert(
            PropertyKeyId(2),
            crate::graph::property::PropertyValue::Int(1),
        );
        let mut ok = node(1, 0);
        ok.0.properties = props.clone();
        check(&[ok], &[]).unwrap();

        let mut unknown_key = crate::graph::property::InternedPropertyMap::new();
        unknown_key.insert(
            PropertyKeyId(9),
            crate::graph::property::PropertyValue::Int(1),
        );
        let e = Edge {
            id: EdgeId(0),
            edge_type: EdgeTypeId(1),
            source: NodeId(1),
            target: NodeId(1),
            properties: unknown_key,
        };
        let mut n = node(1, 0);
        n.0.properties = props;
        assert!(check(&[n], &[(LoadedEdge::Full(e), 0, 0)]).is_err());
    }
}
