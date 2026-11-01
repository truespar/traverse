//! Read access: traversal, counts, scans, label lookups and adjacency snapshots.

use std::borrow::Cow;
use std::sync::Arc;

use rustc_hash::FxHashSet;

use crate::error::{Error, Result};
use crate::interner::StringTable;

use super::adjacency::AdjEntry;
use super::edge::Edge;
use super::node::Node;
use super::types::{Direction, EdgeEndpoint, EdgeId, EdgeTypeId, LabelId, NodeId};
use super::{Graph, IdCounters, MetaSnapshot, adjacency};

impl Graph {
    /// Neighbor IDs of `node` in `direction`, optionally filtered by edge type.
    pub fn neighbors(
        &self,
        node: NodeId,
        direction: Direction,
        edge_type_filter: Option<&str>,
    ) -> Result<Vec<NodeId>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        // type filter given but never interned: nothing matches
        let type_filter = match edge_type_filter {
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        let mut result = Vec::new();

        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(ep.neighbor());
                }
            }
        }

        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(ep.neighbor());
                }
            }
        }

        Ok(result)
    }

    /// Edges of `node` in `direction`, optionally filtered by edge type.
    pub fn edges_of(
        &self,
        node: NodeId,
        direction: Direction,
        edge_type_filter: Option<&str>,
    ) -> Result<Vec<Cow<'_, Edge>>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        // type filter given but never interned: nothing matches
        let type_filter = match edge_type_filter {
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        let mut result = Vec::new();

        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f)
                    && let Ok(edge) = self.get_edge(ep.edge_id())
                {
                    result.push(edge);
                }
            }
        }

        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f)
                    && let Ok(edge) = self.get_edge(ep.edge_id())
                {
                    result.push(edge);
                }
            }
        }

        Ok(result)
    }

    /// Outgoing adjacency entries: packed array slot for unmodified nodes, per-node overlay for
    /// mutated ones.
    #[inline]
    pub(crate) fn outgoing_entries(&self, node: NodeId) -> &[AdjEntry] {
        self.adj_out.neighbors(node)
    }

    /// Incoming adjacency entries: packed array slot for unmodified nodes, per-node overlay for
    /// mutated ones.
    #[inline]
    pub(crate) fn incoming_entries(&self, node: NodeId) -> &[AdjEntry] {
        self.adj_in.neighbors(node)
    }

    /// Neighbor IDs with a pre-resolved type filter.
    pub fn neighbors_filtered(
        &self,
        node: NodeId,
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<NodeId>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }
        let mut result = Vec::new();
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push(ep.neighbor());
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push(ep.neighbor());
                }
            }
        }
        Ok(result)
    }

    /// `(EdgeId, NodeId)` pairs with a pre-resolved type filter.
    pub fn edge_endpoints_filtered(
        &self,
        node: NodeId,
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<(EdgeId, NodeId)>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }
        let mut result = Vec::new();
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push((ep.edge_id(), ep.neighbor()));
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push((ep.edge_id(), ep.neighbor()));
                }
            }
        }
        Ok(result)
    }

    /// Edges from `from` to `to` in `direction`, filtered by type.
    pub fn edges_between(
        &self,
        from: NodeId,
        to: NodeId,
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
    ) -> Result<Vec<EdgeId>> {
        if !self.nodes.contains_key(&from) {
            return Err(Error::NodeNotFound(from));
        }
        let mut result = Vec::new();
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(from) {
                if ep.neighbor() == to
                    && type_filter
                        .as_ref()
                        .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push(ep.edge_id());
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(from) {
                if ep.neighbor() == to
                    && type_filter
                        .as_ref()
                        .is_none_or(|f| f.contains(&ep.edge_type()))
                {
                    result.push(ep.edge_id());
                }
            }
        }
        Ok(result)
    }

    /// Calls `f` per neighbor with a pre-resolved type filter. No allocation.
    pub fn for_each_neighbor(
        &self,
        node: NodeId,
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        f: &mut dyn FnMut(NodeId),
    ) -> Result<()> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }
        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|tf| tf.contains(&ep.edge_type()))
                {
                    f(ep.neighbor());
                }
            }
        }
        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter
                    .as_ref()
                    .is_none_or(|tf| tf.contains(&ep.edge_type()))
                {
                    f(ep.neighbor());
                }
            }
        }
        Ok(())
    }

    /// Inserts every neighbor of the frontier into `out`. No dyn dispatch.
    pub fn expand_frontier(
        &self,
        nodes: &[NodeId],
        direction: Direction,
        type_filter: &Option<FxHashSet<EdgeTypeId>>,
        out: &mut FxHashSet<NodeId>,
    ) -> Result<()> {
        for &node in nodes {
            if !self.nodes.contains_key(&node) {
                continue;
            }
            if matches!(direction, Direction::Outgoing | Direction::Both) {
                for ep in self.outgoing_entries(node) {
                    if type_filter
                        .as_ref()
                        .is_none_or(|tf| tf.contains(&ep.edge_type()))
                    {
                        out.insert(ep.neighbor());
                    }
                }
            }
            if matches!(direction, Direction::Incoming | Direction::Both) {
                for ep in self.incoming_entries(node) {
                    if type_filter
                        .as_ref()
                        .is_none_or(|tf| tf.contains(&ep.edge_type()))
                    {
                        out.insert(ep.neighbor());
                    }
                }
            }
        }
        Ok(())
    }

    /// Number of edges of `node` in `direction`.
    pub fn degree(&self, node: NodeId, direction: Direction) -> Result<usize> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        let out = match direction {
            Direction::Outgoing | Direction::Both => self.adj_out.degree(node),
            Direction::Incoming => 0,
        };

        let inc = match direction {
            Direction::Incoming | Direction::Both => self.adj_in.degree(node),
            Direction::Outgoing => 0,
        };

        Ok(out + inc)
    }

    // -- Queries --------------------------------------------------------

    /// Returns the number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the number of edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.edges.len() + self.propless.len()
    }

    /// One past the highest `NodeId.0` ever assigned. Sizes dense per-node arrays; adjacency is
    /// indexable by `0..highest_node_id()`.
    pub fn highest_node_id(&self) -> u64 {
        self.ids.next_node()
    }

    /// The id counters, shared with the store that owns this graph.
    pub(crate) fn id_counters(&self) -> Arc<IdCounters> {
        Arc::clone(&self.ids)
    }

    /// Returns an iterator over all nodes.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// All edges: borrowed if they have properties, synthesized if property-less.
    pub fn all_edges(&self) -> impl Iterator<Item = Cow<'_, Edge>> + '_ {
        self.edges.values().map(Cow::Borrowed).chain(
            self.propless
                .iter()
                .map(|(id, pt)| Cow::Owned(Edge::new(*id, pt.edge_type, pt.source, pt.target))),
        )
    }

    /// Returns all node IDs as a `Vec`.
    pub fn nodes_vec(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    /// Returns all edge IDs as a `Vec`.
    pub fn all_edges_vec(&self) -> Vec<EdgeId> {
        self.edges
            .keys()
            .chain(self.propless.keys())
            .copied()
            .collect()
    }

    /// Returns edge IDs connected to `node` in the given direction.
    pub fn edges_of_ids(
        &self,
        node: NodeId,
        direction: Direction,
        edge_type_filter: Option<&str>,
    ) -> Result<Vec<EdgeId>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        let type_filter = match edge_type_filter {
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        let mut result = Vec::new();

        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(ep.edge_id());
                }
            }
        }

        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(ep.edge_id());
                }
            }
        }

        Ok(result)
    }

    /// Topology (no properties) of the edges of `node`; reads fixed-size fields only.
    pub fn edges_with_neighbor(
        &self,
        node: NodeId,
        direction: Direction,
        edge_type_filter: Option<&str>,
    ) -> Result<Vec<EdgeEndpoint>> {
        if !self.nodes.contains_key(&node) {
            return Err(Error::NodeNotFound(node));
        }

        let type_filter = match edge_type_filter {
            Some(t) => match self.strings.get(t) {
                Some(id) => Some(EdgeTypeId(id)),
                None => return Ok(Vec::new()),
            },
            None => None,
        };

        let mut result = Vec::new();

        if matches!(direction, Direction::Outgoing | Direction::Both) {
            for ep in self.outgoing_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(EdgeEndpoint {
                        edge_id: ep.edge_id(),
                        edge_type: ep.edge_type(),
                        neighbor: ep.neighbor(),
                        source: node,
                        target: ep.neighbor(),
                    });
                }
            }
        }

        if matches!(direction, Direction::Incoming | Direction::Both) {
            for ep in self.incoming_entries(node) {
                if type_filter.is_none_or(|f| ep.edge_type() == f) {
                    result.push(EdgeEndpoint {
                        edge_id: ep.edge_id(),
                        edge_type: ep.edge_type(),
                        neighbor: ep.neighbor(),
                        source: ep.neighbor(),
                        target: node,
                    });
                }
            }
        }

        Ok(result)
    }

    /// `true` if the node has `label`. O(1).
    pub fn has_label(&self, node: NodeId, label: LabelId) -> bool {
        self.label_index
            .get(&label)
            .is_some_and(|s| s.contains(&node))
    }

    /// Returns node IDs that have the given label.
    pub fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        let lid = match self.strings.get(label) {
            Some(raw) => LabelId(raw),
            None => return Vec::new(),
        };
        self.label_index
            .get(&lid)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Adjacency borrowed for an algorithm run (the borrow keeps the graph unchanged).
    pub fn adjacency_snapshot(&self) -> adjacency::AdjacencySnapshot<'_> {
        adjacency::AdjacencySnapshot::new(
            std::borrow::Cow::Borrowed(&self.adj_out),
            std::borrow::Cow::Borrowed(&self.adj_in),
            self.highest_node_id(),
        )
    }

    /// Adjacency copied for a reader that outlives any borrow: shares the packed arrays, copies
    /// the overlay (kept small by [`Self::compact_adjacency`]).
    pub(crate) fn owned_adjacency_snapshot(&self) -> adjacency::AdjacencySnapshot<'static> {
        adjacency::AdjacencySnapshot::new(
            std::borrow::Cow::Owned(self.adj_out.clone()),
            std::borrow::Cow::Owned(self.adj_in.clone()),
            self.highest_node_id(),
        )
    }

    /// Folds adjacency overlays into fresh packed arrays once they cover more than an eighth of the
    /// nodes (and at least 1024): keeps reads on the packed fast path and snapshot copies small.
    /// Amortized O(1) per mutation.
    pub fn compact_adjacency(&mut self) {
        let threshold = (self.node_count() / 8).max(1024);
        if self.adj_out.overlay_len() > threshold {
            self.adj_out.compact();
        }
        if self.adj_in.overlay_len() > threshold {
            self.adj_in.compact();
        }
    }

    /// Returns a reference to the string table.
    pub fn strings(&self) -> &StringTable {
        &self.strings
    }

    /// Copy-on-write metadata a snapshot reader captures at its start (see [`MetaSnapshot`]).
    pub(crate) fn meta_snapshot(&self) -> MetaSnapshot {
        MetaSnapshot {
            strings: self.strings.snapshot(),
            index_catalog: Arc::clone(self.indexes.catalog()),
            edge_index_catalog: Arc::clone(self.edge_indexes.catalog()),
            constraints: Arc::clone(&self.constraints),
            stats: self.stats.clone(),
        }
    }
}
