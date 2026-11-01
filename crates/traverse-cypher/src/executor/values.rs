//! Value plumbing: entity values, property maps, index and join keys, frame dedup.

use std::collections::BTreeMap;

use rustc_hash::FxHashSet;
use traverse_core::graph::property::{IndexKey, PropertyMap, PropertyValue};
use traverse_core::tx::view::GraphView;

use crate::ast::Expr;
use crate::error::CypherError;
use crate::frame::{Frame, SlotValue, SymbolTable};
use crate::types::CypherValue;

use super::Record;
use super::eval::eval_expr_rec;

// -- HashJoin key -------------------------------------------------------

/// Hashable join key. Whole floats normalize so Integer(1) and Float(1.0) share a bucket (Cypher
/// equality).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum JoinKey {
    Null,
    Bool(bool),
    Integer(i64),
    /// Non-integer float bits.
    Float(u64),
    Str(String),
    /// Fallback: Debug repr for complex types.
    Other(String),
}

pub(crate) fn cypher_to_join_key(cv: &CypherValue) -> Result<JoinKey, CypherError> {
    Ok(match cv {
        CypherValue::Null => JoinKey::Null,
        CypherValue::Bool(b) => JoinKey::Bool(*b),
        CypherValue::Integer(i) => JoinKey::Integer(*i),
        CypherValue::Float(f) => {
            // whole floats -> Integer for cross-type joins
            if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                JoinKey::Integer(*f as i64)
            } else {
                JoinKey::Float(f.to_bits())
            }
        }
        CypherValue::String(s) => JoinKey::Str(s.clone()),
        // fast paths for node/edge refs (common in GROUP BY)
        CypherValue::NodeRef(id) | CypherValue::Node { id, .. } => JoinKey::Integer(id.0 as i64),
        CypherValue::EdgeRef { id, .. } | CypherValue::Edge { id, .. } => {
            JoinKey::Integer(id.0 as i64)
        }
        other => JoinKey::Other(format!("{other:?}")),
    })
}

/// CypherValue -> IndexKey for index lookups.
pub(crate) fn cypher_value_to_index_key(val: &CypherValue) -> Option<IndexKey> {
    match val {
        CypherValue::Bool(b) => Some(IndexKey::Bool(*b)),
        CypherValue::Integer(i) => Some(IndexKey::Int(*i)),
        CypherValue::Float(f) => Some(IndexKey::Float(*f)),
        CypherValue::String(s) => Some(IndexKey::String(s.clone())),
        _ => None,
    }
}

/// Graph Node -> CypherValue.
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

/// Graph Edge -> CypherValue.
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

/// One NodeRef/EdgeRef -> fully materialized value.
pub(super) fn resolve_ref(cv: CypherValue, graph: &dyn GraphView) -> CypherValue {
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
                // edge deleted: shell with the type, no properties
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

pub(crate) fn eval_property_map(
    expr: &Expr,
    rec: &Record,
    graph: &dyn GraphView,
) -> Result<PropertyMap, CypherError> {
    let val = eval_expr_rec(expr, rec, graph)?;
    cypher_value_to_property_map(&val)
}

pub(crate) fn cypher_value_to_property_map(val: &CypherValue) -> Result<PropertyMap, CypherError> {
    match val {
        CypherValue::Map(m) => {
            let mut props = BTreeMap::new();
            for (k, v) in m {
                // nulls aren't stored as properties
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

/// SlotValue -> JoinKey for dedup.
fn slot_to_join_key(sv: &SlotValue) -> JoinKey {
    match sv {
        SlotValue::Null => JoinKey::Null,
        SlotValue::Bool(b) => JoinKey::Bool(*b),
        SlotValue::Integer(i) => JoinKey::Integer(*i),
        SlotValue::Float(f) => {
            if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                JoinKey::Integer(*f as i64)
            } else {
                JoinKey::Float(f.to_bits())
            }
        }
        SlotValue::NodeRef(id) => JoinKey::Integer(id.0 as i64),
        SlotValue::EdgeRef { id, .. } => JoinKey::Integer(id.0 as i64),
        SlotValue::SingleHopPath {
            start,
            end,
            edge_id,
            ..
        } => {
            // combine start, end and edge_id
            JoinKey::Integer((start.0 ^ end.0 ^ edge_id.0) as i64)
        }
        SlotValue::Heap(cv) => cypher_to_join_key(cv).unwrap_or(JoinKey::Null),
    }
}

/// Dedups frames by their non-Null values as JoinKeys.
pub(crate) fn dedup_frames(frames: Vec<Frame>, _symtab: &SymbolTable) -> Vec<Frame> {
    if frames.is_empty() {
        return frames;
    }

    // fast path: all non-null values in one slot -> flat FxHashSet<JoinKey>, no Vec key per frame
    let first = &frames[0];
    let mut non_null_slot = None;
    let mut multi = false;
    for (i, v) in first.iter().enumerate() {
        if !v.is_null() {
            if non_null_slot.is_some() {
                multi = true;
                break;
            }
            non_null_slot = Some(i);
        }
    }
    if !multi {
        if let Some(slot) = non_null_slot {
            let mut seen: FxHashSet<JoinKey> = FxHashSet::default();
            let mut result = Vec::new();
            for frame in frames {
                if seen.insert(slot_to_join_key(&frame[slot])) {
                    result.push(frame);
                }
            }
            return result;
        } else {
            // all slots null: at most one unique frame
            return vec![frames.into_iter().next().unwrap()];
        }
    }

    // general path: Vec key per frame
    let mut seen: FxHashSet<Vec<(usize, JoinKey)>> = FxHashSet::default();
    let mut result = Vec::new();
    for frame in frames {
        let key: Vec<(usize, JoinKey)> = frame
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.is_null())
            .map(|(i, v)| (i, slot_to_join_key(v)))
            .collect();
        if seen.insert(key) {
            result.push(frame);
        }
    }
    result
}
