//! Graph <-> [`GraphParts`]: the decomposed form snapshots save and load, and the [`CatalogParts`]
//! a save writes next to the records.

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::interner::StringTable;

use super::adjacency::AdjacencyStore;
use super::constraint::{ConstraintEntity, ConstraintType, PropertyTypeKind};
use super::edge::Edge;
use super::index::{EdgeIndexRegistry, IndexRegistry};
use super::node::Node;
use super::stats::GraphStatistics;
use super::types::{EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};
use super::{
    ADJ_ENTRY_COST, EDGE_BASE_COST, Graph, IdCounters, LABEL_INDEX_COST, LoadedEdge,
    NODE_BASE_COST, PROPERTY_BYTE, ProplessEdge, load_check,
};

/// Joins the keys of a COMPOSITE constraint in the persisted `property` string
/// (ASCII unit separator, never in real names). Old snapshots have none and
/// decode as single-property constraints.
pub(crate) const COMPOSITE_PROP_SEP: char = '\u{1F}';

/// Constraint type -> on-disk string in `GraphParts.constraint_definitions`.
/// UNIQUE / EXISTS keep their lowercase spellings, property types use
/// `"type:<SURFACE>"`, relationship constraints get a `rel_` prefix, so older
/// snapshots load unchanged.
fn encode_constraint_type(entity: ConstraintEntity, ct: &ConstraintType) -> String {
    let prefix = match entity {
        ConstraintEntity::Node => "",
        ConstraintEntity::Relationship => "rel_",
    };
    match ct {
        ConstraintType::Unique => format!("{prefix}unique"),
        ConstraintType::Exists => format!("{prefix}exists"),
        ConstraintType::Type(k) => format!("{prefix}type:{}", k.surface()),
        ConstraintType::NodeKey => format!("{prefix}node_key"),
    }
}

/// Inverse of `encode_constraint_type`. Unknown strings fall back to `Exists` (older snapshots).
fn decode_constraint_type(s: &str) -> (ConstraintEntity, ConstraintType) {
    let (entity, s) = match s.strip_prefix("rel_") {
        Some(rest) => (ConstraintEntity::Relationship, rest),
        None => (ConstraintEntity::Node, s),
    };
    if let Some(rest) = s.strip_prefix("type:")
        && let Some(k) = PropertyTypeKind::from_surface(rest)
    {
        return (entity, ConstraintType::Type(k));
    }
    let ct = match s {
        "unique" => ConstraintType::Unique,
        "node_key" => ConstraintType::NodeKey,
        _ => ConstraintType::Exists,
    };
    (entity, ct)
}

impl Graph {
    /// Everything a file holds besides the records: names, id counters, schema definitions and
    /// statistics. No node or relationship is copied, so it's cheap next to the records.
    pub(crate) fn catalog_parts(&self) -> CatalogParts {
        CatalogParts {
            stats: self.stats.as_deref().cloned(),
            ..self.catalog_definitions()
        }
    }

    /// [`catalog_parts`](Self::catalog_parts) without the statistics.
    fn catalog_definitions(&self) -> CatalogParts {
        let index_definitions = self
            .indexes
            .descriptors()
            .iter()
            .map(|d| {
                let label = self.strings.resolve(d.label_id.0).to_string();
                let prop_key = self.strings.resolve(d.property_key.0).to_string();
                (d.name.clone(), label, prop_key)
            })
            .collect();
        let constraint_definitions = self
            .constraints
            .descriptors()
            .iter()
            .map(|d| {
                let label = self.strings.resolve(d.label_id.0).to_string();
                // composite constraints join their keys with COMPOSITE_PROP_SEP
                let mut prop_key = self.strings.resolve(d.property_key.0).to_string();
                for pk in &d.extra_property_keys {
                    prop_key.push(COMPOSITE_PROP_SEP);
                    prop_key.push_str(self.strings.resolve(pk.0));
                }
                let ctype_str = encode_constraint_type(d.entity, &d.constraint_type);
                (d.name.clone(), label, prop_key, ctype_str)
            })
            .collect();
        let edge_index_definitions = self
            .edge_indexes
            .descriptors()
            .iter()
            .map(|d| {
                let edge_type = self.strings.resolve(d.edge_type_id.0).to_string();
                let prop_key = d
                    .property_key
                    .map(|pk| self.strings.resolve(pk.0).to_string());
                (d.name.clone(), edge_type, prop_key)
            })
            .collect();
        CatalogParts {
            strings: self.strings.to_strings().to_vec(),
            next_node_id: self.ids.next_node(),
            next_edge_id: self.ids.next_edge(),
            index_definitions,
            constraint_definitions,
            edge_index_definitions,
            vector_index_definitions: self.vector_index_definitions(),
            fulltext_index_definitions: self.fulltext_index_definitions(),
            point_index_definitions: self.point_index_definitions(),
            text_index_definitions: self.text_index_definitions(),
            stats: None,
        }
    }

    /// Splits the graph into raw parts for serialization: nodes, edges, strings, ID counters, index
    /// definitions.
    pub fn into_parts(self) -> GraphParts {
        let catalog = self.catalog_definitions();
        let stats = self
            .stats
            .map(|s| Arc::try_unwrap(s).unwrap_or_else(|s| (*s).clone()));
        let nodes = self.nodes.into_values().collect();
        let edges = self
            .edges
            .into_values()
            .chain(
                self.propless
                    .into_iter()
                    .map(|(id, pt)| Edge::new(id, pt.edge_type, pt.source, pt.target)),
            )
            .collect();
        GraphParts::assemble(CatalogParts { stats, ..catalog }, nodes, edges)
    }

    /// Copies the graph's raw parts for serialization.
    pub fn as_parts(&self) -> GraphParts {
        let nodes = self.nodes.values().cloned().collect();
        let edges = self
            .edges
            .values()
            .cloned()
            .chain(
                self.propless
                    .iter()
                    .map(|(id, pt)| Edge::new(*id, pt.edge_type, pt.source, pt.target)),
            )
            .collect();
        GraphParts::assemble(self.catalog_parts(), nodes, edges)
    }

    /// Calls `node` on every node and `edge` on every relationship, in storage order, without
    /// copying the graph. The caller keeps writers out while it runs.
    pub(crate) fn for_each_record(
        &self,
        node: &mut dyn FnMut(&Node) -> crate::error::Result<()>,
        edge: &mut dyn FnMut(&Edge) -> crate::error::Result<()>,
    ) -> crate::error::Result<()> {
        for n in self.nodes.values() {
            node(n)?;
        }
        for e in self.edges.values() {
            edge(e)?;
        }
        for (id, pt) in &self.propless {
            edge(&Edge::new(*id, pt.edge_type, pt.source, pt.target))?;
        }
        Ok(())
    }

    /// Rebuilds a graph from raw parts, including adjacency and the label, edge-type and property
    /// indexes.
    pub fn from_parts(parts: GraphParts) -> Self {
        let strings = StringTable::from_strings(parts.strings);
        let next_node_id = parts.next_node_id;
        let next_edge_id = parts.next_edge_id;
        let stats = parts.stats;
        let index_definitions = parts.index_definitions;
        let constraint_definitions = parts.constraint_definitions;
        let edge_index_definitions = parts.edge_index_definitions;
        let vector_index_definitions = parts.vector_index_definitions;
        let fulltext_index_definitions = parts.fulltext_index_definitions;
        let point_index_definitions = parts.point_index_definitions;
        let text_index_definitions = parts.text_index_definitions;
        let nodes_vec = parts.nodes;
        let edges_vec = parts.edges;

        // build node and edge structures in parallel
        type NodeResult = (
            FxHashMap<NodeId, Node>,
            FxHashMap<LabelId, FxHashSet<NodeId>>,
            FxHashSet<PropertyKeyId>,
        );
        type EdgeResult = (
            FxHashMap<EdgeId, Edge>,
            FxHashMap<EdgeId, ProplessEdge>,
            FxHashMap<EdgeTypeId, FxHashSet<EdgeId>>,
            FxHashSet<PropertyKeyId>,
        );

        let edge_count = edges_vec.len();
        let node_count = nodes_vec.len();

        // adjacency via counting sort, no intermediate map: temp memory is
        // 2 x Vec<u32>(node_count). The closure lets from_edges walk &edges_vec twice
        // without collecting.
        let (adj_out, adj_in) = rayon::join(
            || {
                AdjacencyStore::from_edges(
                    || {
                        edges_vec
                            .iter()
                            .map(|e| (e.source, e.id, e.edge_type, e.target))
                    },
                    next_node_id,
                )
            },
            || {
                AdjacencyStore::from_edges(
                    || {
                        edges_vec
                            .iter()
                            .map(|e| (e.target, e.id, e.edge_type, e.source))
                    },
                    next_node_id,
                )
            },
        );

        let (node_result, edge_result): (NodeResult, EdgeResult) = rayon::join(
            || {
                let mut nodes_map =
                    FxHashMap::with_capacity_and_hasher(node_count, Default::default());
                let mut label_index: FxHashMap<LabelId, FxHashSet<NodeId>> = FxHashMap::default();
                let mut prop_keys = FxHashSet::default();
                for node in nodes_vec {
                    let nid = node.id;
                    for &lid in &node.labels {
                        label_index.entry(lid).or_default().insert(nid);
                    }
                    for kid in node.properties.keys() {
                        prop_keys.insert(kid);
                    }
                    nodes_map.insert(nid, node);
                }
                (nodes_map, label_index, prop_keys)
            },
            || {
                // pre-count propless vs propped edges to size both maps exactly
                let mut n_propless = 0usize;
                for e in &edges_vec {
                    if e.properties.is_empty() {
                        n_propless += 1;
                    }
                }
                let n_propped = edge_count - n_propless;

                let mut edges_map =
                    FxHashMap::with_capacity_and_hasher(n_propped, Default::default());
                let mut propless_map =
                    FxHashMap::with_capacity_and_hasher(n_propless, Default::default());
                let mut edge_type_index: FxHashMap<EdgeTypeId, FxHashSet<EdgeId>> =
                    FxHashMap::default();
                let mut prop_keys = FxHashSet::default();
                for edge in edges_vec {
                    let eid = edge.id;
                    edge_type_index
                        .entry(edge.edge_type)
                        .or_default()
                        .insert(eid);
                    if edge.properties.is_empty() {
                        propless_map.insert(
                            eid,
                            ProplessEdge {
                                edge_type: edge.edge_type,
                                source: edge.source,
                                target: edge.target,
                            },
                        );
                    } else {
                        for kid in edge.properties.keys() {
                            prop_keys.insert(kid);
                        }
                        edges_map.insert(eid, edge);
                    }
                }
                (edges_map, propless_map, edge_type_index, prop_keys)
            },
        );

        let (nodes_map, label_index, node_prop_keys) = node_result;
        let (edges_map, propless_map, edge_type_index, edge_prop_keys) = edge_result;

        // Merge property keys from both loops
        let mut known_prop_keys = node_prop_keys;
        known_prop_keys.extend(edge_prop_keys);

        let mut graph = Self {
            nodes: nodes_map,
            edges: edges_map,
            propless: propless_map,
            adj_out,
            adj_in,
            label_index,
            edge_type_index,
            strings: strings.into(),
            indexes: IndexRegistry::new(),
            edge_indexes: EdgeIndexRegistry::new(),
            vector_indexes: Default::default(),
            fulltext_indexes: Default::default(),
            spatial_indexes: Default::default(),
            text_indexes: Default::default(),
            constraints: Default::default(),
            stats: stats.map(Arc::new),
            ids: IdCounters::shared(next_node_id, next_edge_id),
            dirty_nodes: FxHashSet::default(),
            dirty_edges: FxHashSet::default(),
            deleted_nodes: FxHashSet::default(),
            deleted_edges: FxHashSet::default(),
            strings_dirty: false,
            estimated_memory: 0,
            known_prop_keys,
            node_delta_heads: FxHashMap::default(),
            edge_delta_heads: FxHashMap::default(),
            versions: Default::default(),
        };

        // Shrink over-allocated secondary structures
        graph.nodes.shrink_to_fit();
        for set in graph.label_index.values_mut() {
            set.shrink_to_fit();
        }
        for set in graph.edge_type_index.values_mut() {
            set.shrink_to_fit();
        }

        // Shrink property Vec<u8> buffers to fit actual data
        for node in graph.nodes.values_mut() {
            node.properties.shrink_to_fit();
        }
        for edge in graph.edges.values_mut() {
            edge.properties.shrink_to_fit();
        }

        // Rebuild property indexes from definitions (scan + populate)
        for (name, label, prop_key) in &index_definitions {
            // ignore errors (e.g. duplicate index) while rebuilding
            let _ = graph.create_index(name, label, prop_key);
        }

        // Rebuild constraints from definitions
        for (name, label, prop_key, ctype_str) in &constraint_definitions {
            let (entity, ctype) = decode_constraint_type(ctype_str);
            if prop_key.contains(COMPOSITE_PROP_SEP) {
                let props: Vec<String> = prop_key
                    .split(COMPOSITE_PROP_SEP)
                    .map(str::to_string)
                    .collect();
                let _ = graph.create_composite_constraint(
                    name,
                    label,
                    &props,
                    ctype,
                    entity == ConstraintEntity::Relationship,
                );
                continue;
            }
            let _ = match entity {
                ConstraintEntity::Relationship => {
                    graph.create_relationship_constraint(name, label, prop_key, ctype)
                }
                ConstraintEntity::Node => graph.create_constraint(name, label, prop_key, ctype),
            };
        }

        // Rebuild edge indexes from definitions (scan + populate)
        for (name, edge_type, prop_key) in &edge_index_definitions {
            let _ = graph.create_edge_index(name, edge_type, prop_key.as_deref());
        }

        // rebuild vector (HNSW) indexes from persisted descriptors and the nodes just loaded
        graph.install_vector_index_definitions(&vector_index_definitions);
        graph.install_fulltext_index_definitions(&fulltext_index_definitions);
        graph.install_point_index_definitions(&point_index_definitions);
        graph.install_text_index_definitions(&text_index_definitions);

        // Clear dirty flags produced by create_index
        graph.strings_dirty = false;

        // Compute initial estimated_memory from loaded data
        {
            let mut mem: usize = 0;
            for node in graph.nodes.values() {
                mem += NODE_BASE_COST
                    + node.labels.len() * LABEL_INDEX_COST
                    + node.properties.data_len() * PROPERTY_BYTE;
            }
            for edge in graph.edges.values() {
                mem += EDGE_BASE_COST + ADJ_ENTRY_COST + edge.properties.data_len() * PROPERTY_BYTE;
            }
            mem += graph.propless.len() * (EDGE_BASE_COST + ADJ_ENTRY_COST);
            graph.estimated_memory = mem;
        }

        graph
    }

    /// Builds a graph straight from deserialized load results, skipping the
    /// `Vec<Node>` / `Vec<Edge>` copies of the `from_parts` path (about half the
    /// peak load memory). Returns `(Graph, LocationIndex)`, or `InvalidFormat` when the records
    /// don't fit together (a damaged or crafted file).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_loaded(
        node_results: Vec<(Node, u64, u16)>,
        edge_results: Vec<(LoadedEdge, u64, u16)>,
        strings: Vec<String>,
        next_node_id: u64,
        next_edge_id: u64,
        index_definitions: Vec<(String, String, String)>,
        constraint_definitions: Vec<(String, String, String, String)>,
        edge_index_definitions: Vec<(String, String, Option<String>)>,
        vector_index_definitions: Vec<crate::storage::catalog::VectorIndexDef>,
        stats: Option<GraphStatistics>,
    ) -> crate::error::Result<(Self, crate::storage::location::LocationIndex)> {
        use crate::storage::location::{LocationIndex, RecordLocation};

        load_check::check_loaded(
            &node_results,
            &edge_results,
            &strings,
            next_node_id,
            next_edge_id,
        )?;
        crate::memory::check_allocation(AdjacencyStore::build_bytes(next_node_id) * 2)?;
        let strings = StringTable::from_strings(strings);
        let edge_count = edge_results.len();
        let node_count = node_results.len();

        // location index by borrowing, no copies
        let (node_locs, edge_locs) = rayon::join(
            || {
                let mut locs = FxHashMap::with_capacity_and_hasher(node_count, Default::default());
                for (node, page_id, slot_idx) in &node_results {
                    locs.insert(
                        node.id,
                        RecordLocation {
                            page_id: *page_id,
                            slot_idx: *slot_idx,
                        },
                    );
                }
                locs
            },
            || {
                let mut locs = FxHashMap::with_capacity_and_hasher(edge_count, Default::default());
                for (edge, page_id, slot_idx) in &edge_results {
                    locs.insert(
                        edge.id(),
                        RecordLocation {
                            page_id: *page_id,
                            slot_idx: *slot_idx,
                        },
                    );
                }
                locs
            },
        );
        let locations = LocationIndex::from_maps(node_locs, edge_locs);

        // adjacency via counting sort over &edge_results twice; temp 2 x Vec<u32>(node_count)
        let (adj_out, adj_in) = rayon::join(
            || {
                AdjacencyStore::try_from_edges(
                    || {
                        edge_results
                            .iter()
                            .map(|(e, _, _)| (e.source(), e.id(), e.edge_type(), e.target()))
                    },
                    next_node_id,
                )
            },
            || {
                AdjacencyStore::try_from_edges(
                    || {
                        edge_results
                            .iter()
                            .map(|(e, _, _)| (e.target(), e.id(), e.edge_type(), e.source()))
                    },
                    next_node_id,
                )
            },
        );
        let (adj_out, adj_in) = (adj_out?, adj_in?);

        // node/edge ownership maps consume results in place, no extra Vec<Node>/Vec<Edge>
        type NodeResult = (
            FxHashMap<NodeId, Node>,
            FxHashMap<LabelId, FxHashSet<NodeId>>,
            FxHashSet<PropertyKeyId>,
        );
        type EdgeResult = (
            FxHashMap<EdgeId, Edge>,
            FxHashMap<EdgeId, ProplessEdge>,
            FxHashMap<EdgeTypeId, FxHashSet<EdgeId>>,
            FxHashSet<PropertyKeyId>,
        );

        let (node_result, edge_result): (NodeResult, EdgeResult) = rayon::join(
            || {
                let mut nodes_map =
                    FxHashMap::with_capacity_and_hasher(node_count, Default::default());
                let mut label_index: FxHashMap<LabelId, FxHashSet<NodeId>> = FxHashMap::default();
                let mut prop_keys = FxHashSet::default();
                for (node, _, _) in node_results {
                    let nid = node.id;
                    for &lid in &node.labels {
                        label_index.entry(lid).or_default().insert(nid);
                    }
                    for kid in node.properties.keys() {
                        prop_keys.insert(kid);
                    }
                    nodes_map.insert(nid, node);
                }
                (nodes_map, label_index, prop_keys)
            },
            || {
                let mut n_propless = 0usize;
                for (e, _, _) in &edge_results {
                    if matches!(e, LoadedEdge::Propless { .. }) {
                        n_propless += 1;
                    }
                }
                let n_propped = edge_count - n_propless;

                let mut edges_map =
                    FxHashMap::with_capacity_and_hasher(n_propped, Default::default());
                let mut propless_map =
                    FxHashMap::with_capacity_and_hasher(n_propless, Default::default());
                let mut edge_type_index: FxHashMap<EdgeTypeId, FxHashSet<EdgeId>> =
                    FxHashMap::default();
                let mut prop_keys = FxHashSet::default();
                for (loaded_edge, _, _) in edge_results {
                    let eid = loaded_edge.id();
                    edge_type_index
                        .entry(loaded_edge.edge_type())
                        .or_default()
                        .insert(eid);
                    match loaded_edge {
                        LoadedEdge::Propless {
                            id: _,
                            edge_type,
                            source,
                            target,
                        } => {
                            propless_map.insert(
                                eid,
                                ProplessEdge {
                                    edge_type,
                                    source,
                                    target,
                                },
                            );
                        }
                        LoadedEdge::Full(edge) => {
                            for kid in edge.properties.keys() {
                                prop_keys.insert(kid);
                            }
                            edges_map.insert(eid, edge);
                        }
                    }
                }
                (edges_map, propless_map, edge_type_index, prop_keys)
            },
        );

        let (nodes_map, label_index, node_prop_keys) = node_result;
        let (edges_map, propless_map, edge_type_index, edge_prop_keys) = edge_result;

        let mut known_prop_keys = node_prop_keys;
        known_prop_keys.extend(edge_prop_keys);

        let mut graph = Self {
            nodes: nodes_map,
            edges: edges_map,
            propless: propless_map,
            adj_out,
            adj_in,
            label_index,
            edge_type_index,
            strings: strings.into(),
            indexes: IndexRegistry::new(),
            edge_indexes: EdgeIndexRegistry::new(),
            vector_indexes: Default::default(),
            fulltext_indexes: Default::default(),
            spatial_indexes: Default::default(),
            text_indexes: Default::default(),
            constraints: Default::default(),
            stats: stats.map(Arc::new),
            ids: IdCounters::shared(next_node_id, next_edge_id),
            dirty_nodes: FxHashSet::default(),
            dirty_edges: FxHashSet::default(),
            deleted_nodes: FxHashSet::default(),
            deleted_edges: FxHashSet::default(),
            strings_dirty: false,
            estimated_memory: 0,
            known_prop_keys,
            node_delta_heads: FxHashMap::default(),
            edge_delta_heads: FxHashMap::default(),
            versions: Default::default(),
        };

        graph.nodes.shrink_to_fit();
        for set in graph.label_index.values_mut() {
            set.shrink_to_fit();
        }
        for set in graph.edge_type_index.values_mut() {
            set.shrink_to_fit();
        }
        for node in graph.nodes.values_mut() {
            node.properties.shrink_to_fit();
        }
        for edge in graph.edges.values_mut() {
            edge.properties.shrink_to_fit();
        }

        for (name, label, prop_key) in &index_definitions {
            let _ = graph.create_index(name, label, prop_key);
        }
        for (name, label, prop_key, ctype_str) in &constraint_definitions {
            let (entity, ctype) = decode_constraint_type(ctype_str);
            if prop_key.contains(COMPOSITE_PROP_SEP) {
                let props: Vec<String> = prop_key
                    .split(COMPOSITE_PROP_SEP)
                    .map(str::to_string)
                    .collect();
                let _ = graph.create_composite_constraint(
                    name,
                    label,
                    &props,
                    ctype,
                    entity == ConstraintEntity::Relationship,
                );
                continue;
            }
            let _ = match entity {
                ConstraintEntity::Relationship => {
                    graph.create_relationship_constraint(name, label, prop_key, ctype)
                }
                ConstraintEntity::Node => graph.create_constraint(name, label, prop_key, ctype),
            };
        }
        for (name, edge_type, prop_key) in &edge_index_definitions {
            let _ = graph.create_edge_index(name, edge_type, prop_key.as_deref());
        }
        graph.install_vector_index_definitions(&vector_index_definitions);

        graph.strings_dirty = false;

        {
            let mut mem: usize = 0;
            for node in graph.nodes.values() {
                mem += NODE_BASE_COST
                    + node.labels.len() * LABEL_INDEX_COST
                    + node.properties.data_len() * PROPERTY_BYTE;
            }
            for edge in graph.edges.values() {
                mem += EDGE_BASE_COST + ADJ_ENTRY_COST + edge.properties.data_len() * PROPERTY_BYTE;
            }
            mem += graph.propless.len() * (EDGE_BASE_COST + ADJ_ENTRY_COST);
            graph.estimated_memory = mem;
        }

        Ok((graph, locations))
    }
}

/// What a `.tvdb` holds besides the records: names, id counters, schema definitions, statistics.
/// Same meaning as the matching [`GraphParts`] fields.
pub(crate) struct CatalogParts {
    pub(crate) strings: Vec<String>,
    pub(crate) next_node_id: u64,
    pub(crate) next_edge_id: u64,
    pub(crate) index_definitions: Vec<(String, String, String)>,
    pub(crate) constraint_definitions: Vec<(String, String, String, String)>,
    pub(crate) edge_index_definitions: Vec<(String, String, Option<String>)>,
    pub(crate) vector_index_definitions: Vec<crate::storage::catalog::VectorIndexDef>,
    pub(crate) fulltext_index_definitions: Vec<crate::storage::catalog::FulltextIndexDef>,
    pub(crate) point_index_definitions: Vec<crate::storage::catalog::PointIndexDef>,
    pub(crate) text_index_definitions: Vec<crate::storage::catalog::TextIndexDef>,
    pub(crate) stats: Option<GraphStatistics>,
}

impl GraphParts {
    fn assemble(catalog: CatalogParts, nodes: Vec<Node>, edges: Vec<Edge>) -> Self {
        Self {
            nodes,
            edges,
            strings: catalog.strings,
            next_node_id: catalog.next_node_id,
            next_edge_id: catalog.next_edge_id,
            index_definitions: catalog.index_definitions,
            constraint_definitions: catalog.constraint_definitions,
            edge_index_definitions: catalog.edge_index_definitions,
            vector_index_definitions: catalog.vector_index_definitions,
            fulltext_index_definitions: catalog.fulltext_index_definitions,
            point_index_definitions: catalog.point_index_definitions,
            text_index_definitions: catalog.text_index_definitions,
            stats: catalog.stats,
        }
    }
}

/// The raw parts of a graph, suitable for serialization and reconstruction.
pub struct GraphParts {
    /// All nodes in the graph.
    pub nodes: Vec<Node>,
    /// All edges in the graph.
    pub edges: Vec<Edge>,
    /// All interned strings (labels and edge types), ordered by ID.
    pub strings: Vec<String>,
    /// The next node ID to allocate.
    pub next_node_id: u64,
    /// The next edge ID to allocate.
    pub next_edge_id: u64,
    /// Property index definitions: `(name, label, property_key)`.
    pub index_definitions: Vec<(String, String, String)>,
    /// Constraint definitions: `(name, label, property_key, type)`.
    pub constraint_definitions: Vec<(String, String, String, String)>,
    /// Edge index definitions: `(name, edge_type, property_key_opt)`.
    pub edge_index_definitions: Vec<(String, String, Option<String>)>,
    /// Vector-index descriptors (HNSW rebuilt from node vectors on load).
    pub vector_index_definitions: Vec<crate::storage::catalog::VectorIndexDef>,
    /// Persisted full-text-index descriptors (rebuilt on load).
    pub fulltext_index_definitions: Vec<crate::storage::catalog::FulltextIndexDef>,
    /// Persisted point-index descriptors (R-tree rebuilt on load).
    pub point_index_definitions: Vec<crate::storage::catalog::PointIndexDef>,
    /// Persisted text-index descriptors (trigram postings rebuilt on load).
    pub text_index_definitions: Vec<crate::storage::catalog::TextIndexDef>,
    /// Collected graph statistics for the optimizer.
    pub stats: Option<GraphStatistics>,
}
