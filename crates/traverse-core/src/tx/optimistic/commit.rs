//! Commit: validation (first committer wins), constraint re-checks against the latest
//! state, and the schema changes applied before the data.

use std::collections::BTreeMap;

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::graph::constraint::{ConstraintDescriptor, ConstraintEntity, ConstraintType};
use crate::graph::property::{IndexKey, InternedPropertyMap, PropertyMap, PropertyValue};
use crate::graph::types::{Direction, EdgeId, LabelId, NodeId, PropertyKeyId};

use super::super::mvcc::{TXN_ID_SENTINEL, changed_since};
use super::super::timestamp::HybridTimestamp;
use super::super::view::GraphView;
use super::super::write::WriteTx;
use super::{OptimisticTx, SchemaKind, SchemaOp};
use crate::error::ViolationKind;

/// A constraint the committed state would break.
struct Violation {
    kind: ViolationKind,
    message: String,
    /// The constraint's name.
    constraint: String,
    /// The other object sharing a unique value, if that's the problem.
    other: Option<Entity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entity {
    Node(NodeId),
    Edge(EdgeId),
}

/// Value tuples of a constraint's keys, by owner, built by one scan when no index serves it.
type TupleMap<T> = BTreeMap<Vec<IndexKey>, SmallVec<[T; 1]>>;

/// The values of `keys` in `props` when all are present and indexable; `None` exempts the object
/// from uniqueness.
fn tuple_of(props: &InternedPropertyMap, keys: &[PropertyKeyId]) -> Option<Vec<IndexKey>> {
    keys.iter()
        .map(|k| match props.get(*k) {
            None | Some(PropertyValue::Null) => None,
            Some(v) => IndexKey::from_property_value(&v),
        })
        .collect()
}

fn keys_of(desc: &ConstraintDescriptor) -> Vec<PropertyKeyId> {
    std::iter::once(desc.property_key)
        .chain(desc.extra_property_keys.iter().copied())
        .collect()
}

/// Constraint checks against the writer's view after applying, scans cached per constraint.
struct Checker<'v> {
    view: &'v dyn GraphView,
    node_scans: FxHashMap<String, TupleMap<NodeId>>,
    edge_scans: FxHashMap<String, TupleMap<EdgeId>>,
}

impl<'v> Checker<'v> {
    fn new(view: &'v dyn GraphView) -> Self {
        Self {
            view,
            node_scans: FxHashMap::default(),
            edge_scans: FxHashMap::default(),
        }
    }

    fn name(&self, id: u32) -> &str {
        self.view.strings().resolve(id)
    }

    /// Existence and type checks shared by nodes and edges.
    fn presence(
        &self,
        desc: &ConstraintDescriptor,
        props: &InternedPropertyMap,
        what: &str,
    ) -> Option<Violation> {
        let keys = keys_of(desc);
        let must_exist = matches!(
            desc.constraint_type,
            ConstraintType::Exists | ConstraintType::NodeKey
        );
        for k in &keys {
            match props.get(*k) {
                None | Some(PropertyValue::Null) if must_exist => {
                    return Some(Violation {
                        kind: ViolationKind::Existence,
                        message: format!(
                            "{what} with `{}` must have property `{}`",
                            self.name(desc.label_id.0),
                            self.name(k.0)
                        ),
                        constraint: desc.name.clone(),
                        other: None,
                    });
                }
                Some(v) => {
                    if let ConstraintType::Type(kind) = &desc.constraint_type
                        && !kind.matches(&v)
                    {
                        return Some(Violation {
                            kind: ViolationKind::Type,
                            message: format!(
                                "{what} with `{}` has property `{}` of a type other than {}",
                                self.name(desc.label_id.0),
                                self.name(k.0),
                                kind.surface()
                            ),
                            constraint: desc.name.clone(),
                            other: None,
                        });
                    }
                }
                None => {}
            }
        }
        None
    }

    fn check_node(&mut self, id: NodeId) -> Option<Violation> {
        let node = self.view.get_node(id).ok()?.into_owned();
        let registry: Vec<ConstraintDescriptor> = self
            .view
            .constraint_registry()
            .descriptors()
            .iter()
            .filter(|d| d.entity == ConstraintEntity::Node && node.labels.contains(&d.label_id))
            .cloned()
            .collect();
        for desc in &registry {
            let what = format!("node({})", id.0);
            if let Some(v) = self.presence(desc, &node.properties, &what) {
                return Some(v);
            }
            if !matches!(
                desc.constraint_type,
                ConstraintType::Unique | ConstraintType::NodeKey
            ) {
                continue;
            }
            let keys = keys_of(desc);
            let Some(tuple) = tuple_of(&node.properties, &keys) else {
                continue;
            };
            if let Some(other) = self.other_node(desc, &keys, &tuple, id) {
                let props: Vec<&str> = keys.iter().map(|k| self.name(k.0)).collect();
                return Some(Violation {
                    kind: ViolationKind::Uniqueness,
                    message: format!(
                        "node({}) already exists with label `{}` and ({}) = this node's",
                        other.0,
                        self.name(desc.label_id.0),
                        props.join(", ")
                    ),
                    constraint: desc.name.clone(),
                    other: Some(Entity::Node(other)),
                });
            }
        }
        None
    }

    /// Another node with the label and the same key tuple: index seek when one serves it, else
    /// one cached label scan.
    fn other_node(
        &mut self,
        desc: &ConstraintDescriptor,
        keys: &[PropertyKeyId],
        tuple: &[IndexKey],
        me: NodeId,
    ) -> Option<NodeId> {
        let label = self.name(desc.label_id.0).to_string();
        let (key_name, value) = if keys.len() == 1 {
            (self.name(keys[0].0).to_string(), tuple[0].clone())
        } else {
            let joined: Vec<&str> = keys.iter().map(|k| self.name(k.0)).collect();
            (
                joined.join(&crate::graph::COMPOSITE_PROP_SEP.to_string()),
                IndexKey::composite_encode(tuple),
            )
        };
        if let Some(ids) = self.view.index_lookup(&label, &key_name, &value) {
            return ids.into_iter().find(|&o| o != me);
        }
        let view = self.view;
        let map = self.node_scans.entry(desc.name.clone()).or_insert_with(|| {
            let mut map = TupleMap::new();
            for n in view.nodes_by_label(&label) {
                if let Ok(node) = view.get_node(n)
                    && let Some(t) = tuple_of(&node.properties, keys)
                {
                    map.entry(t).or_default().push(n);
                }
            }
            map
        });
        map.get(tuple)
            .and_then(|ids| ids.iter().copied().find(|&o| o != me))
    }

    fn check_edge(&mut self, id: EdgeId) -> Option<Violation> {
        let edge = self.view.get_edge(id).ok()?.into_owned();
        let registry: Vec<ConstraintDescriptor> = self
            .view
            .constraint_registry()
            .descriptors()
            .iter()
            .filter(|d| {
                d.entity == ConstraintEntity::Relationship && d.label_id.0 == edge.edge_type.0
            })
            .cloned()
            .collect();
        for desc in &registry {
            let what = format!("relationship({})", id.0);
            if let Some(v) = self.presence(desc, &edge.properties, &what) {
                return Some(v);
            }
            if !matches!(
                desc.constraint_type,
                ConstraintType::Unique | ConstraintType::NodeKey
            ) {
                continue;
            }
            let keys = keys_of(desc);
            let Some(tuple) = tuple_of(&edge.properties, &keys) else {
                continue;
            };
            if let Some(other) = self.other_edge(desc, &keys, &tuple, id) {
                let props: Vec<&str> = keys.iter().map(|k| self.name(k.0)).collect();
                return Some(Violation {
                    kind: ViolationKind::Uniqueness,
                    message: format!(
                        "relationship({}) already exists with type `{}` and ({}) = this one's",
                        other.0,
                        self.name(desc.label_id.0),
                        props.join(", ")
                    ),
                    constraint: desc.name.clone(),
                    other: Some(Entity::Edge(other)),
                });
            }
        }
        None
    }

    fn other_edge(
        &mut self,
        desc: &ConstraintDescriptor,
        keys: &[PropertyKeyId],
        tuple: &[IndexKey],
        me: EdgeId,
    ) -> Option<EdgeId> {
        let edge_type = self.name(desc.label_id.0).to_string();
        if keys.len() == 1 {
            let key_name = self.name(keys[0].0).to_string();
            if let Some(ids) = self
                .view
                .edge_index_lookup(&edge_type, &key_name, &tuple[0])
            {
                return ids.into_iter().find(|&o| o != me);
            }
        }
        let view = self.view;
        let map = self.edge_scans.entry(desc.name.clone()).or_insert_with(|| {
            let mut map = TupleMap::new();
            for e in view.edges_by_type(&edge_type) {
                if let Ok(edge) = view.get_edge(e)
                    && let Some(t) = tuple_of(&edge.properties, keys)
                {
                    map.entry(t).or_default().push(e);
                }
            }
            map
        });
        map.get(tuple)
            .and_then(|ids| ids.iter().copied().find(|&o| o != me))
    }
}

/// Applies one schema change through the writer.
fn apply_schema(w: &mut WriteTx<'_>, op: &SchemaOp) -> Result<()> {
    match op {
        SchemaOp::CreateIndex {
            name,
            label,
            property,
        } => w.create_index(name, label, property),
        SchemaOp::CreateCompositeIndex {
            name,
            label,
            properties,
            is_relationship,
        } => w.create_composite_index(name, label, properties, *is_relationship),
        SchemaOp::DropIndex(name) => w.drop_index(name),
        SchemaOp::CreateVectorIndex {
            name,
            label,
            property,
            dim,
            sim,
            params,
        } => w.create_vector_index(name, label, property, *dim, *sim, *params),
        SchemaOp::DropVectorIndex(name) => w.drop_vector_index(name),
        SchemaOp::CreateFulltextIndex {
            name,
            labels,
            properties,
            analyzer,
        } => {
            let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            let props: Vec<&str> = properties.iter().map(String::as_str).collect();
            w.create_fulltext_index(name, &labels, &props, *analyzer)
        }
        SchemaOp::DropFulltextIndex(name) => w.drop_fulltext_index(name),
        SchemaOp::CreatePointIndex {
            name,
            label,
            property,
        } => w.create_point_index(name, label, property),
        SchemaOp::DropPointIndex(name) => w.drop_point_index(name),
        SchemaOp::CreateTextIndex {
            name,
            label,
            property,
        } => w.create_text_index(name, label, property),
        SchemaOp::DropTextIndex(name) => w.drop_text_index(name),
        SchemaOp::CreateConstraint {
            name,
            label,
            property,
            ctype,
        } => w.create_constraint(name, label, property, ctype.clone()),
        SchemaOp::CreateRelationshipConstraint {
            name,
            edge_type,
            property,
            ctype,
        } => w.create_relationship_constraint(name, edge_type, property, ctype.clone()),
        SchemaOp::CreateCompositeConstraint {
            name,
            label,
            properties,
            ctype,
            is_relationship,
        } => {
            w.create_composite_constraint(name, label, properties, ctype.clone(), *is_relationship)
        }
        SchemaOp::DropConstraint(name) => w.drop_constraint(name),
        SchemaOp::CreateEdgeIndex {
            name,
            edge_type,
            property,
        } => w.create_edge_index(name, edge_type, property.as_deref()),
        SchemaOp::DropEdgeIndex(name) => w.drop_edge_index(name),
        SchemaOp::Analyze(labels) => {
            w.analyze_statistics(labels);
            Ok(())
        }
    }
}

impl<'a> OptimisticTx<'a> {
    /// Commits: validate, apply, re-check constraints, log, publish. Returns the commit
    /// timestamp (the snapshot's when there is nothing to write).
    ///
    /// Errors with [`Error::TransactionConflict`] when a concurrent commit got there first
    /// (retry the transaction), or the usual constraint and memory errors.
    pub fn commit(self) -> Result<HybridTimestamp> {
        if !self.has_writes() {
            return Ok(self.base.snapshot_ts());
        }
        let mut w = self.store.begin_write();
        self.validate(&w)?;
        self.apply(&mut w)?;
        self.check_constraints(&w)?;
        w.commit()
    }

    /// First committer wins: every snapshot object this transaction changed or deleted must be
    /// unchanged since the snapshot; created edges' endpoints must still exist; a deleted node must
    /// not have gained edges.
    fn validate(&self, w: &WriteTx<'_>) -> Result<()> {
        let g = w.graph_ref();
        let snap = self.base.snap();
        for (&id, state) in &self.local.nodes {
            if self.local.created_nodes.contains(&id) {
                continue;
            }
            // SAFETY: writer lock held, and our snapshot registration keeps every delta newer
            // than it alive
            if unsafe { changed_since(snap, g.node_delta_head(id)) } {
                return Err(Error::TransactionConflict(format!(
                    "node {} was changed by a concurrent transaction",
                    id.0
                )));
            }
            if state.is_none() {
                for e in g
                    .edges_of_ids(id, Direction::Both, None)
                    .unwrap_or_default()
                {
                    if !matches!(self.local.edges.get(&e), Some(None)) {
                        return Err(Error::TransactionConflict(format!(
                            "node {} gained a relationship in a concurrent transaction",
                            id.0
                        )));
                    }
                }
            }
        }
        for &id in self.local.edges.keys() {
            if self.local.created_edges.contains(&id) {
                continue;
            }
            // SAFETY: as above
            if unsafe { changed_since(snap, g.edge_delta_head(id)) } {
                return Err(Error::TransactionConflict(format!(
                    "relationship {} was changed by a concurrent transaction",
                    id.0
                )));
            }
        }
        for id in &self.local.created_edges {
            if let Some(Some(edge)) = self.local.edges.get(id) {
                for end in [edge.source, edge.target] {
                    if !self.local.created_nodes.contains(&end) && g.get_node(end).is_err() {
                        return Err(Error::TransactionConflict(format!(
                            "node {} was deleted by a concurrent transaction",
                            end.0
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn labels_of(&self, labels: &[LabelId]) -> Vec<&str> {
        labels.iter().map(|l| self.strings.resolve(l.0)).collect()
    }

    fn props_of(&self, props: &InternedPropertyMap) -> PropertyMap {
        props
            .iter()
            .map(|(k, v)| (self.strings.resolve(k.0).to_string(), v))
            .collect()
    }

    /// Replays the net changes through the writer: schema, created nodes, changed nodes,
    /// created edges, changed edges, deleted edges, deleted nodes, then statistics.
    fn apply(&self, w: &mut WriteTx<'_>) -> Result<()> {
        let mut analyze = Vec::new();
        for op in &self.schema {
            if let SchemaOp::Analyze(labels) = op {
                analyze.push(labels.clone());
            } else {
                apply_schema(w, op)?;
            }
        }
        let mut nodes: Vec<NodeId> = self.local.nodes.keys().copied().collect();
        nodes.sort_unstable();
        let mut edges: Vec<EdgeId> = self.local.edges.keys().copied().collect();
        edges.sort_unstable();

        for &id in nodes
            .iter()
            .filter(|id| self.local.created_nodes.contains(id))
        {
            if let Some(Some(n)) = self.local.nodes.get(&id) {
                w.create_node_with_id(
                    id,
                    &self.labels_of(&n.labels),
                    self.props_of(&n.properties),
                )?;
            }
        }
        for &id in nodes
            .iter()
            .filter(|id| !self.local.created_nodes.contains(id))
        {
            let Some(Some(n)) = self.local.nodes.get(&id) else {
                continue;
            };
            // validated: the live node is the snapshot's
            let before = self.base.get_node(id)?;
            for l in before.labels.iter().filter(|l| !n.labels.contains(l)) {
                w.remove_label(id, self.strings.resolve(l.0))?;
            }
            for l in n.labels.iter().filter(|l| !before.labels.contains(l)) {
                w.add_label(id, self.strings.resolve(l.0))?;
            }
            if before.properties != n.properties {
                w.update_node_properties(id, self.props_of(&n.properties))?;
            }
        }
        for &id in edges
            .iter()
            .filter(|id| self.local.created_edges.contains(id))
        {
            if let Some(Some(e)) = self.local.edges.get(&id) {
                w.create_edge_with_id(
                    id,
                    e.source,
                    e.target,
                    self.strings.resolve(e.edge_type.0),
                    self.props_of(&e.properties),
                )?;
            }
        }
        for &id in edges
            .iter()
            .filter(|id| !self.local.created_edges.contains(id))
        {
            match self.local.edges.get(&id) {
                Some(Some(e)) => {
                    let before = self.base.get_edge(id)?;
                    if before.properties != e.properties {
                        w.update_edge_properties(id, self.props_of(&e.properties))?;
                    }
                }
                Some(None) => w.delete_edge(id)?,
                None => {}
            }
        }
        for &id in &nodes {
            if matches!(self.local.nodes.get(&id), Some(None)) {
                w.delete_node(id)?;
            }
        }
        for labels in &analyze {
            w.analyze_statistics(labels);
        }
        Ok(())
    }

    /// Created or changed by a transaction this snapshot doesn't see (so a violation it causes is
    /// a race, not this transaction's mistake).
    fn concurrent(&self, g: &Graph, who: Entity) -> bool {
        let snap = self.base.snap();
        let hidden = |ci: *const crate::tx::mvcc::CommitInfo| {
            // SAFETY: CommitInfos outlive the runs pointing at them
            let ts = unsafe { (*ci).timestamp.load(std::sync::atomic::Ordering::Acquire) };
            ts == TXN_ID_SENTINEL || ts > snap
        };
        match who {
            Entity::Node(id) => {
                // SAFETY: writer lock held, snapshot registered
                let changed = unsafe { changed_since(snap, g.node_delta_head(id)) };
                changed || g.versions().node_runs.creator(id.0).is_some_and(hidden)
            }
            Entity::Edge(id) => {
                // SAFETY: as above
                let changed = unsafe { changed_since(snap, g.edge_delta_head(id)) };
                changed || g.versions().edge_runs.creator(id.0).is_some_and(hidden)
            }
        }
    }

    /// Constraints of everything created or changed, on the state about to commit. A violation
    /// caused by a concurrent commit (the other object, or a constraint this snapshot didn't
    /// have) is a conflict; any other is this transaction's.
    fn check_constraints(&self, w: &WriteTx<'_>) -> Result<()> {
        if w.constraint_registry().is_empty() {
            return Ok(());
        }
        let mut checker = Checker::new(w);
        let mut found = None;
        for (&id, state) in &self.local.nodes {
            if state.is_some()
                && let Some(v) = checker.check_node(id)
            {
                found = Some(v);
                break;
            }
        }
        if found.is_none() {
            for (&id, state) in &self.local.edges {
                if state.is_some()
                    && let Some(v) = checker.check_edge(id)
                {
                    found = Some(v);
                    break;
                }
            }
        }
        let Some(v) = found else {
            return Ok(());
        };
        let ours = |who: Entity| match who {
            Entity::Node(id) => self.local.nodes.contains_key(&id),
            Entity::Edge(id) => self.local.edges.contains_key(&id),
        };
        let raced = match v.other {
            Some(other) => !ours(other) && self.concurrent(w.graph_ref(), other),
            // a constraint created after our snapshot, by someone else
            None => {
                self.base
                    .constraint_registry()
                    .get_by_name(&v.constraint)
                    .is_none()
                    && !self
                        .schema_names
                        .contains_key(&(SchemaKind::Constraint, v.constraint.clone()))
            }
        };
        if raced {
            Err(Error::TransactionConflict(format!(
                "{} (written by a concurrent transaction)",
                v.message
            )))
        } else {
            Err(Error::constraint_violation(v.kind, v.message))
        }
    }
}
