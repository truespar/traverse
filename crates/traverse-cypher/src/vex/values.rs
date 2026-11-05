//! Value plumbing: entity materialization, hashable keys for joins and grouping, index keys and
//! property maps.

use std::collections::BTreeMap;

use traverse_core::graph::property::{IndexKey, PropertyMap, PropertyValue};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::vector::ValueRef;

/// Ids at or above this are VIRTUAL entities (APOC vNode / vRelationship): pure values that never
/// resolve against the graph.
pub const VIRTUAL_ID_BASE: u64 = 1 << 62;

/// Hashable key with Cypher equality: whole floats share a bucket with the equal integer, entities
/// hash by id, everything else by its debug form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JoinKey {
    /// Null.
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer (also whole floats and entity ids).
    Integer(i64),
    /// Non-integer float bits.
    Float(u64),
    /// String.
    Str(String),
    /// Everything else, by its debug form.
    Other(String),
}

/// A hash of a row key, for partitioning rows across workers.
pub fn hash_key(key: &[JoinKey]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    key.hash(&mut h);
    h.finish()
}

/// The key of an owned value.
pub fn join_key(cv: &CypherValue) -> JoinKey {
    match cv {
        CypherValue::Null => JoinKey::Null,
        CypherValue::Bool(b) => JoinKey::Bool(*b),
        CypherValue::Integer(i) => JoinKey::Integer(*i),
        CypherValue::Float(f) => float_key(*f),
        CypherValue::String(s) => JoinKey::Str(s.clone()),
        CypherValue::NodeRef(id) | CypherValue::Node { id, .. } => JoinKey::Integer(id.0 as i64),
        CypherValue::EdgeRef { id, .. } | CypherValue::Edge { id, .. } => {
            JoinKey::Integer(id.0 as i64)
        }
        other => JoinKey::Other(format!("{other:?}")),
    }
}

/// The key of a lane.
pub fn join_key_ref(v: ValueRef<'_>) -> JoinKey {
    match v {
        ValueRef::Null => JoinKey::Null,
        ValueRef::Bool(b) => JoinKey::Bool(b),
        ValueRef::Int(i) => JoinKey::Integer(i),
        ValueRef::Float(f) => float_key(f),
        ValueRef::Str(s) => JoinKey::Str(s.to_string()),
        ValueRef::Node(id) => JoinKey::Integer(id.0 as i64),
        ValueRef::Edge { id, .. } => JoinKey::Integer(id.0 as i64),
        ValueRef::Any(cv) => join_key(cv),
    }
}

#[inline]
fn float_key(f: f64) -> JoinKey {
    if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        JoinKey::Integer(f as i64)
    } else {
        JoinKey::Float(f.to_bits())
    }
}

/// Index key of a value, for seeks; `None` for unindexable types.
pub fn index_key(val: &CypherValue) -> Option<IndexKey> {
    match val {
        CypherValue::Bool(b) => Some(IndexKey::Bool(*b)),
        CypherValue::Integer(i) => Some(IndexKey::Int(*i)),
        CypherValue::Float(f) => Some(IndexKey::Float(*f)),
        CypherValue::String(s) => Some(IndexKey::String(s.clone())),
        _ => None,
    }
}

/// A node as a result value.
pub fn node_to_cypher_value(
    node: &traverse_core::graph::node::Node,
    graph: &dyn GraphView,
) -> CypherValue {
    let labels: Vec<String> = node
        .labels
        .iter()
        .map(|lid| graph.strings().resolve(lid.0).to_string())
        .collect();
    let properties: BTreeMap<String, PropertyValue> = node
        .properties
        .iter()
        .map(|(kid, pv)| (graph.strings().resolve(kid.0).to_string(), pv))
        .collect();
    CypherValue::Node {
        id: node.id,
        labels,
        properties,
    }
}

/// An edge as a result value.
pub fn edge_to_cypher_value(
    edge: &traverse_core::graph::edge::Edge,
    graph: &dyn GraphView,
) -> CypherValue {
    let edge_type = graph.strings().resolve(edge.edge_type.0).to_string();
    let properties: BTreeMap<String, PropertyValue> = edge
        .properties
        .iter()
        .map(|(kid, pv)| (graph.strings().resolve(kid.0).to_string(), pv))
        .collect();
    CypherValue::Edge {
        id: edge.id,
        edge_type,
        source: edge.source,
        target: edge.target,
        properties,
    }
}

/// One NodeRef/EdgeRef -> fully materialized value. A deleted node is null; a deleted edge keeps
/// its topology and type without properties.
pub fn resolve_ref(cv: CypherValue, graph: &dyn GraphView) -> CypherValue {
    match cv {
        CypherValue::NodeRef(id) => {
            if let Ok(node) = graph.get_node(id) {
                node_to_cypher_value(&node, graph)
            } else {
                CypherValue::Null
            }
        }
        CypherValue::EdgeRef {
            id,
            edge_type_id,
            source,
            target,
        } => {
            if let Ok(edge) = graph.get_edge(id) {
                edge_to_cypher_value(&edge, graph)
            } else {
                let edge_type = graph.strings().resolve(edge_type_id.0).to_string();
                CypherValue::Edge {
                    id,
                    edge_type,
                    source,
                    target,
                    properties: BTreeMap::new(),
                }
            }
        }
        other => other,
    }
}

/// Deep-resolves every NodeRef/EdgeRef (inside lists, maps and paths too).
pub fn materialize_value(cv: CypherValue, graph: &dyn GraphView) -> CypherValue {
    match cv {
        CypherValue::NodeRef(_) | CypherValue::EdgeRef { .. } => resolve_ref(cv, graph),
        CypherValue::List(items) => CypherValue::List(
            items
                .into_iter()
                .map(|v| materialize_value(v, graph))
                .collect(),
        ),
        CypherValue::Map(entries) => CypherValue::Map(
            entries
                .into_iter()
                .map(|(k, v)| (k, materialize_value(v, graph)))
                .collect(),
        ),
        CypherValue::Path { nodes, edges } => CypherValue::Path {
            nodes: nodes
                .into_iter()
                .map(|v| materialize_value(v, graph))
                .collect(),
            edges: edges
                .into_iter()
                .map(|v| materialize_value(v, graph))
                .collect(),
        },
        other => other,
    }
}

/// A map value as stored properties: nulls dropped, structural values rejected.
pub fn cypher_value_to_property_map(val: &CypherValue) -> Result<PropertyMap, CypherError> {
    match val {
        CypherValue::Map(m) => {
            let mut props = BTreeMap::new();
            for (k, v) in m {
                if matches!(v, CypherValue::Null) {
                    continue;
                }
                let pv = v.to_property().unwrap_or(PropertyValue::Null);
                props.insert(k.clone(), pv);
            }
            Ok(props)
        }
        CypherValue::Node { properties, .. } | CypherValue::Edge { properties, .. } => {
            Ok(properties.clone())
        }
        _ => Err(CypherError::type_error(format!(
            "expected map for properties, got {}",
            val.type_name()
        ))),
    }
}

/// Storable as a property (no maps, nodes, edges or paths)?
pub fn is_storable_property(val: &CypherValue) -> bool {
    match val {
        CypherValue::Null
        | CypherValue::Bytes(_)
        | CypherValue::Bool(_)
        | CypherValue::Integer(_)
        | CypherValue::Float(_)
        | CypherValue::String(_)
        | CypherValue::Date(_)
        | CypherValue::LocalTime(_)
        | CypherValue::Time(_)
        | CypherValue::LocalDateTime(_)
        | CypherValue::DateTime(_)
        | CypherValue::Duration(_) => true,
        CypherValue::List(items) => items.iter().all(is_storable_property),
        // points arrive as `{x, y, z?, srid, crs}` maps and are storable; other maps aren't
        CypherValue::Map(m) => {
            m.contains_key("x")
                && m.contains_key("y")
                && m.contains_key("srid")
                && m.contains_key("crs")
        }
        CypherValue::Node { .. }
        | CypherValue::Edge { .. }
        | CypherValue::NodeRef(_)
        | CypherValue::EdgeRef { .. }
        | CypherValue::Path { .. } => false,
    }
}
