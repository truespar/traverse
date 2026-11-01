//! [`OptimisticTx`]: a transaction that runs on a snapshot and applies its writes at
//! commit, after checking that nothing it changed was changed by someone else first.
//!
//! Reads go through a local delta over a [`ReadTx`]: objects this transaction created,
//! changed or deleted shadow the snapshot, and local label / type / index / adjacency
//! listings make its writes visible to its later statements. Writes never take the
//! writer lock, so a long or idle transaction blocks nobody; ids come from the store's
//! shared counters, so the ids a client sees are the ids after commit.
//!
//! Commit takes the writer lock, validates (first committer wins: any object this
//! transaction changed that a later commit also changed is a conflict), replays the net
//! changes through a [`WriteTx`] (logged and published like any write), and re-checks
//! constraints against the latest state, so two concurrent `MERGE`s of the same key
//! can't both commit. Conflicts are [`Error::TransactionConflict`]: transient, retry
//! the transaction. Isolation is snapshot isolation.
//!
//! Schema changes (indexes, constraints, statistics) are validated when issued and take
//! effect at commit, before the data changes; reads in the transaction see the schema as
//! of its snapshot.

mod commit;
mod view;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::graph::constraint::ConstraintType;
use crate::graph::edge::Edge;
use crate::graph::node::Node;
use crate::graph::property::{IndexKey, InternedPropertyMap, PropertyMap};
use crate::graph::types::{Direction, EdgeId, EdgeTypeId, LabelId, NodeId, PropertyKeyId};
use crate::interner::SharedStrings;

use super::MvccStore;
use super::read::ReadTx;
use super::timestamp::HybridTimestamp;
use super::view::GraphView;

/// Where a local node is listed: its labels and B-tree index keys.
#[derive(Debug, Default)]
struct NodeListing {
    labels: SmallVec<[LabelId; 2]>,
    keys: Vec<(LabelId, PropertyKeyId, IndexKey)>,
}

/// Where a local edge is listed: its type and edge-index keys.
#[derive(Debug)]
struct EdgeListing {
    edge_type: EdgeTypeId,
    keys: Vec<(PropertyKeyId, IndexKey)>,
}

type KeyedIds<T> = FxHashMap<(u32, PropertyKeyId), BTreeMap<IndexKey, FxHashSet<T>>>;

/// This transaction's writes, shadowing the snapshot.
#[derive(Debug, Default)]
struct Local {
    /// Current state of every node this transaction touched: `Some` (created or changed) or
    /// `None` (a snapshot node it deleted).
    nodes: FxHashMap<NodeId, Option<Node>>,
    edges: FxHashMap<EdgeId, Option<Edge>>,
    created_nodes: FxHashSet<NodeId>,
    created_edges: FxHashSet<EdgeId>,
    /// Created edges by endpoint.
    out_added: FxHashMap<NodeId, Vec<EdgeId>>,
    in_added: FxHashMap<NodeId, Vec<EdgeId>>,
    /// Snapshot nodes whose adjacency changed (an edge created or deleted on them).
    adjacency_changed: FxHashSet<NodeId>,
    /// Listings of live local objects.
    node_listing: FxHashMap<NodeId, NodeListing>,
    edge_listing: FxHashMap<EdgeId, EdgeListing>,
    labels: FxHashMap<LabelId, FxHashSet<NodeId>>,
    types: FxHashMap<EdgeTypeId, FxHashSet<EdgeId>>,
    node_index: KeyedIds<NodeId>,
    edge_index: KeyedIds<EdgeId>,
    /// Handed out by `get_node_mut` / `get_edge_mut` since their last listing: their listings
    /// may be out of date, so reads check them by their current state.
    stale_nodes: FxHashSet<NodeId>,
    stale_edges: FxHashSet<EdgeId>,
    node_delta: i64,
    edge_delta: i64,
}

fn unset<K: std::hash::Hash + Eq, T: std::hash::Hash + Eq>(
    map: &mut FxHashMap<K, FxHashSet<T>>,
    key: &K,
    id: &T,
) {
    if let Some(set) = map.get_mut(key) {
        set.remove(id);
        if set.is_empty() {
            map.remove(key);
        }
    }
}

fn unset_keyed<T: std::hash::Hash + Eq>(
    map: &mut KeyedIds<T>,
    key: (u32, PropertyKeyId),
    value: &IndexKey,
    id: &T,
) {
    if let Some(tree) = map.get_mut(&key) {
        if let Some(set) = tree.get_mut(value) {
            set.remove(id);
            if set.is_empty() {
                tree.remove(value);
            }
        }
        if tree.is_empty() {
            map.remove(&key);
        }
    }
}

impl Local {
    fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }

    fn unlist_node(&mut self, id: NodeId) {
        if let Some(l) = self.node_listing.remove(&id) {
            for lab in &l.labels {
                unset(&mut self.labels, lab, &id);
            }
            for (lab, key, value) in &l.keys {
                unset_keyed(&mut self.node_index, (lab.0, *key), value, &id);
            }
        }
    }

    fn list_node(&mut self, id: NodeId, listing: NodeListing) {
        for lab in &listing.labels {
            self.labels.entry(*lab).or_default().insert(id);
        }
        for (lab, key, value) in &listing.keys {
            self.node_index
                .entry((lab.0, *key))
                .or_default()
                .entry(value.clone())
                .or_default()
                .insert(id);
        }
        self.node_listing.insert(id, listing);
    }

    fn unlist_edge(&mut self, id: EdgeId) {
        if let Some(l) = self.edge_listing.remove(&id) {
            unset(&mut self.types, &l.edge_type, &id);
            for (key, value) in &l.keys {
                unset_keyed(&mut self.edge_index, (l.edge_type.0, *key), value, &id);
            }
        }
    }

    fn list_edge(&mut self, id: EdgeId, listing: EdgeListing) {
        self.types.entry(listing.edge_type).or_default().insert(id);
        for (key, value) in &listing.keys {
            self.edge_index
                .entry((listing.edge_type.0, *key))
                .or_default()
                .entry(value.clone())
                .or_default()
                .insert(id);
        }
        self.edge_listing.insert(id, listing);
    }
}

/// A schema change, applied at commit.
#[derive(Debug, Clone)]
enum SchemaOp {
    CreateIndex {
        name: String,
        label: String,
        property: String,
    },
    CreateCompositeIndex {
        name: String,
        label: String,
        properties: Vec<String>,
        is_relationship: bool,
    },
    DropIndex(String),
    CreateVectorIndex {
        name: String,
        label: String,
        property: String,
        dim: usize,
        sim: crate::graph::vector::Similarity,
        params: crate::graph::vector::HnswParams,
    },
    DropVectorIndex(String),
    CreateFulltextIndex {
        name: String,
        labels: Vec<String>,
        properties: Vec<String>,
        analyzer: crate::graph::fulltext::Analyzer,
    },
    DropFulltextIndex(String),
    CreatePointIndex {
        name: String,
        label: String,
        property: String,
    },
    DropPointIndex(String),
    CreateTextIndex {
        name: String,
        label: String,
        property: String,
    },
    DropTextIndex(String),
    CreateConstraint {
        name: String,
        label: String,
        property: String,
        ctype: ConstraintType,
    },
    CreateRelationshipConstraint {
        name: String,
        edge_type: String,
        property: String,
        ctype: ConstraintType,
    },
    CreateCompositeConstraint {
        name: String,
        label: String,
        properties: Vec<String>,
        ctype: ConstraintType,
        is_relationship: bool,
    },
    DropConstraint(String),
    CreateEdgeIndex {
        name: String,
        edge_type: String,
        property: Option<String>,
    },
    DropEdgeIndex(String),
    Analyze(Vec<String>),
}

/// Namespaces schema object names live in (an index and a constraint may share a name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SchemaKind {
    Index,
    EdgeIndex,
    Constraint,
    Vector,
    Fulltext,
    Point,
    Text,
}

/// Optimistic (snapshot + validate-at-commit) transaction; see the module docs.
pub struct OptimisticTx<'a> {
    store: &'a MvccStore,
    base: ReadTx<'a>,
    read_only: bool,
    /// The snapshot's string table, copied on the first new string.
    strings: SharedStrings,
    local: Local,
    schema: Vec<SchemaOp>,
    /// Schema names created (`true`) or dropped (`false`) by `schema`, over the snapshot's.
    schema_names: FxHashMap<(SchemaKind, String), bool>,
    /// Changes so far (see `GraphView::write_count`).
    mutations: u64,
}

impl<'a> OptimisticTx<'a> {
    pub(super) fn new(store: &'a MvccStore, base: ReadTx<'a>, read_only: bool) -> Self {
        let strings = SharedStrings::from(Arc::clone(&base.meta().strings));
        Self {
            store,
            base,
            read_only,
            strings,
            local: Local::default(),
            schema: Vec::new(),
            schema_names: FxHashMap::default(),
            mutations: 0,
        }
    }

    /// Changes so far: every write counts, refused or not.
    pub fn write_count(&self) -> u64 {
        self.mutations
    }

    /// Snapshot timestamp: what this transaction reads, besides its own writes.
    pub fn snapshot_ts(&self) -> HybridTimestamp {
        self.base.snapshot_ts()
    }

    /// Read-only (`BEGIN ... mode "r"`): every write is refused.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Anything to commit?
    pub fn has_writes(&self) -> bool {
        !self.local.is_empty() || !self.schema.is_empty()
    }

    /// Aborts: nothing was applied, so dropping the transaction is the whole job.
    pub fn rollback(self) {}

    /// Gate of every write: refuses them in a read-only transaction, counts them otherwise.
    fn writable(&mut self) -> Result<()> {
        if self.read_only {
            Err(Error::ReadOnlyTransaction)
        } else {
            self.mutations += 1;
            Ok(())
        }
    }

    /// The B-tree index value `props` has under index key `key` (composite keys included).
    fn index_key_of(&self, props: &InternedPropertyMap, key: PropertyKeyId) -> Option<IndexKey> {
        match self.strings.try_resolve(key.0) {
            Some(name) if name.contains(crate::graph::COMPOSITE_PROP_SEP) => {
                let parts: Option<Vec<PropertyKeyId>> = name
                    .split(crate::graph::COMPOSITE_PROP_SEP)
                    .map(|p| self.strings.get(p).map(PropertyKeyId))
                    .collect();
                Graph::composite_key_of(props, &parts?)
            }
            _ => IndexKey::from_property_value(&props.get(key)?),
        }
    }

    /// Listing of a node state under the snapshot's index catalog.
    fn node_listing(&self, node: &Node) -> NodeListing {
        let keys = self
            .base
            .index_catalog()
            .descriptors()
            .iter()
            .filter(|d| node.labels.contains(&d.label_id))
            .filter_map(|d| {
                self.index_key_of(&node.properties, d.property_key)
                    .map(|v| (d.label_id, d.property_key, v))
            })
            .collect();
        NodeListing {
            labels: node.labels.clone(),
            keys,
        }
    }

    fn edge_listing(&self, edge: &Edge) -> EdgeListing {
        let keys = self
            .base
            .edge_index_catalog()
            .descriptors()
            .iter()
            .filter(|d| d.edge_type_id == edge.edge_type)
            .filter_map(|d| {
                let key = d.property_key?;
                self.index_key_of(&edge.properties, key).map(|v| (key, v))
            })
            .collect();
        EdgeListing {
            edge_type: edge.edge_type,
            keys,
        }
    }

    /// Re-lists a local node from its current state.
    fn relist_node(&mut self, id: NodeId) {
        self.local.unlist_node(id);
        self.local.stale_nodes.remove(&id);
        let listing = match self.local.nodes.get(&id) {
            Some(Some(n)) => self.node_listing(n),
            _ => return,
        };
        self.local.list_node(id, listing);
    }

    fn relist_edge(&mut self, id: EdgeId) {
        self.local.unlist_edge(id);
        self.local.stale_edges.remove(&id);
        let listing = match self.local.edges.get(&id) {
            Some(Some(e)) => self.edge_listing(e),
            _ => return,
        };
        self.local.list_edge(id, listing);
    }

    /// Our copy of a node to change (copied from the snapshot on first write).
    fn own_node(&mut self, id: NodeId) -> Result<&mut Node> {
        if !self.local.nodes.contains_key(&id) {
            let node = self.base.get_node(id)?.into_owned();
            self.local.nodes.insert(id, Some(node));
            self.relist_node(id);
        }
        match self.local.nodes.get_mut(&id) {
            Some(Some(n)) => Ok(n),
            _ => Err(Error::NodeNotFound(id)),
        }
    }

    fn own_edge(&mut self, id: EdgeId) -> Result<&mut Edge> {
        if !self.local.edges.contains_key(&id) {
            let edge = self.base.get_edge(id)?.into_owned();
            self.local.edges.insert(id, Some(edge));
            self.relist_edge(id);
        }
        match self.local.edges.get_mut(&id) {
            Some(Some(e)) => Ok(e),
            _ => Err(Error::EdgeNotFound(id)),
        }
    }

    fn intern_map(&mut self, props: PropertyMap) -> InternedPropertyMap {
        props
            .into_iter()
            .map(|(k, v)| (PropertyKeyId(self.strings.intern(&k)), v))
            .collect()
    }

    /// The node at our view, or `None` if it doesn't exist here.
    fn node_ref(&self, id: NodeId) -> Option<Cow<'_, Node>> {
        match self.local.nodes.get(&id) {
            Some(Some(n)) => Some(Cow::Borrowed(n)),
            Some(None) => None,
            None => self.base.get_node(id).ok(),
        }
    }

    fn edge_ref(&self, id: EdgeId) -> Option<Cow<'_, Edge>> {
        match self.local.edges.get(&id) {
            Some(Some(e)) => Some(Cow::Borrowed(e)),
            Some(None) => None,
            None => self.base.get_edge(id).ok(),
        }
    }

    /// Adjacency unchanged here: the snapshot's answers stand.
    #[inline]
    fn adjacency_is_base(&self, node: NodeId) -> bool {
        !self.local.adjacency_changed.contains(&node)
            && !self.local.created_nodes.contains(&node)
            && !matches!(self.local.nodes.get(&node), Some(None))
    }

    /// Edges of `node` at our view: `(edge, type, neighbor, outgoing)`.
    fn edges_at(
        &self,
        node: NodeId,
        dir: Direction,
    ) -> Result<Vec<(EdgeId, EdgeTypeId, NodeId, bool)>> {
        if self.node_ref(node).is_none() {
            return Err(Error::NodeNotFound(node));
        }
        let mut out = Vec::new();
        let scans: &[(Direction, bool)] = match dir {
            Direction::Outgoing => &[(Direction::Outgoing, true)],
            Direction::Incoming => &[(Direction::Incoming, false)],
            Direction::Both => &[(Direction::Outgoing, true), (Direction::Incoming, false)],
        };
        for &(d, outgoing) in scans {
            if !self.local.created_nodes.contains(&node) {
                for ep in self.base.edges_with_neighbor(node, d, None)? {
                    if matches!(self.local.edges.get(&ep.edge_id), Some(None)) {
                        continue;
                    }
                    out.push((ep.edge_id, ep.edge_type, ep.neighbor, outgoing));
                }
            }
            let added = if outgoing {
                self.local.out_added.get(&node)
            } else {
                self.local.in_added.get(&node)
            };
            for e in added.into_iter().flatten() {
                if let Some(Some(edge)) = self.local.edges.get(e) {
                    let nb = if outgoing { edge.target } else { edge.source };
                    out.push((*e, edge.edge_type, nb, outgoing));
                }
            }
        }
        Ok(out)
    }

    /// Local nodes with `label` (current state), stale ones re-checked.
    fn local_with_label(&self, label: LabelId) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = self
            .local
            .labels
            .get(&label)
            .into_iter()
            .flatten()
            .copied()
            .filter(|id| !self.local.stale_nodes.contains(id))
            .collect();
        for &id in &self.local.stale_nodes {
            if let Some(Some(n)) = self.local.nodes.get(&id)
                && n.labels.contains(&label)
            {
                out.push(id);
            }
        }
        out
    }

    /// Local nodes whose `(label, key)` value satisfies `matches` (stale ones re-checked).
    fn local_index_hits(
        &self,
        label: LabelId,
        key: PropertyKeyId,
        listed: impl Iterator<Item = NodeId>,
        matches: &dyn Fn(&IndexKey) -> bool,
    ) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = listed
            .filter(|id| !self.local.stale_nodes.contains(id))
            .collect();
        for &id in &self.local.stale_nodes {
            if let Some(Some(n)) = self.local.nodes.get(&id)
                && n.labels.contains(&label)
                && self
                    .index_key_of(&n.properties, key)
                    .is_some_and(|k| matches(&k))
            {
                out.push(id);
            }
        }
        out
    }

    /// Records a schema change, checking its name against the snapshot plus earlier changes.
    fn schema_change(
        &mut self,
        kind: SchemaKind,
        name: &str,
        create: bool,
        op: SchemaOp,
    ) -> Result<()> {
        self.writable()?;
        let exists = match self.schema_names.get(&(kind, name.to_string())) {
            Some(&e) => e,
            None => match kind {
                SchemaKind::Index => self.base.index_catalog().has_index_by_name(name),
                SchemaKind::EdgeIndex => self
                    .base
                    .edge_index_catalog()
                    .get_descriptor_by_name(name)
                    .is_some(),
                SchemaKind::Constraint => {
                    self.base.constraint_registry().get_by_name(name).is_some()
                }
                SchemaKind::Vector => self.base.has_vector_index(name),
                SchemaKind::Fulltext => self.base.has_fulltext_index(name),
                SchemaKind::Point => self.base.has_point_index(name),
                SchemaKind::Text => self.base.has_text_index(name),
            },
        };
        if create && exists {
            return Err(match kind {
                SchemaKind::Constraint => {
                    Error::ConstraintError(format!("constraint '{name}' already exists"))
                }
                _ => Error::IndexError(format!("index '{name}' already exists")),
            });
        }
        if !create && !exists {
            return Err(match kind {
                SchemaKind::Constraint => {
                    Error::ConstraintError(format!("constraint `{name}` not found"))
                }
                _ => Error::IndexError(format!("index '{name}' not found")),
            });
        }
        self.schema_names.insert((kind, name.to_string()), create);
        self.schema.push(op);
        Ok(())
    }
}

impl MvccStore {
    /// Optimistic transaction at the latest commit; see [`OptimisticTx`].
    pub fn begin_optimistic(&self, read_only: bool) -> Result<OptimisticTx<'_>> {
        let base = self.begin_read()?;
        Ok(OptimisticTx::new(self, base, read_only))
    }
}
