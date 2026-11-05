//! Constraint checks for writes: uniqueness (single and composite), existence and type.

use traverse_core::graph::property::{IndexKey, PropertyValue};
use traverse_core::graph::types::{EdgeId, LabelId, NodeId, PropertyKeyId};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;

/// Validates a unique constraint on one (label, property) for a node.
fn validate_unique_via_view(
    graph: &dyn GraphView,
    nid: NodeId,
    lid: LabelId,
    pkid: PropertyKeyId,
    value: &PropertyValue,
) -> Result<(), CypherError> {
    if matches!(value, PropertyValue::Null) {
        return Ok(());
    }
    if let Some(idx_key) = IndexKey::from_property_value(value) {
        let strings = graph.strings();
        let label_str = strings.resolve(lid.0);
        let node_ids = graph.nodes_by_label(label_str);
        let prop_str = strings.resolve(pkid.0);
        for other_nid in node_ids {
            if other_nid == nid {
                continue;
            }
            if let Ok(other_node) = graph.get_node(other_nid)
                && let Some(other_pv) = other_node.properties.get(pkid)
                && !matches!(other_pv, PropertyValue::Null)
                && let Some(other_key) = IndexKey::from_property_value(&other_pv)
                && other_key == idx_key
            {
                return Err(CypherError::from(
                    traverse_core::error::Error::constraint_violation(
                        traverse_core::error::ViolationKind::Uniqueness,
                        format!(
                            "node({}) already exists with label `{label_str}` and property `{prop_str}`",
                            other_nid.0
                        ),
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Composite (tuple) uniqueness for a node: no OTHER node with the label may share the full tuple.
/// Rows with a missing / null / unindexable component are exempt (KEY's existence side rejects them
/// separately).
fn validate_composite_unique_via_view(
    graph: &dyn GraphView,
    nid: NodeId,
    lid: LabelId,
    desc: &traverse_core::graph::constraint::ConstraintDescriptor,
) -> Result<(), CypherError> {
    let node = graph.get_node(nid).map_err(CypherError::from)?;
    let keys: Vec<PropertyKeyId> = std::iter::once(desc.property_key)
        .chain(desc.extra_property_keys.iter().copied())
        .collect();
    let tuple: Option<Vec<IndexKey>> = keys
        .iter()
        .map(|pk| {
            node.properties.get(*pk).and_then(|pv| {
                if matches!(pv, PropertyValue::Null) {
                    None
                } else {
                    IndexKey::from_property_value(&pv)
                }
            })
        })
        .collect();
    let Some(tuple) = tuple else { return Ok(()) };
    drop(node);

    let strings = graph.strings();
    let label_str = strings.resolve(lid.0);
    for other_nid in graph.nodes_by_label(label_str) {
        if other_nid == nid {
            continue;
        }
        let Ok(other) = graph.get_node(other_nid) else {
            continue;
        };
        let other_tuple: Option<Vec<IndexKey>> = keys
            .iter()
            .map(|pk| {
                other.properties.get(*pk).and_then(|pv| {
                    if matches!(pv, PropertyValue::Null) {
                        None
                    } else {
                        IndexKey::from_property_value(&pv)
                    }
                })
            })
            .collect();
        if other_tuple.as_ref() == Some(&tuple) {
            let props: Vec<&str> = keys.iter().map(|pk| strings.resolve(pk.0)).collect();
            return Err(CypherError::from(
                traverse_core::error::Error::constraint_violation(
                    traverse_core::error::ViolationKind::Uniqueness,
                    format!(
                        "node({}) already exists with label `{label_str}` and the same ({}) tuple",
                        other_nid.0,
                        props.join(", ")
                    ),
                ),
            ));
        }
    }
    Ok(())
}

/// Edge version of [`validate_composite_unique_via_view`].
fn validate_composite_edge_unique_via_view(
    graph: &dyn GraphView,
    eid: EdgeId,
    tid: LabelId,
    desc: &traverse_core::graph::constraint::ConstraintDescriptor,
) -> Result<(), CypherError> {
    let edge = graph.get_edge(eid).map_err(CypherError::from)?;
    let keys: Vec<PropertyKeyId> = std::iter::once(desc.property_key)
        .chain(desc.extra_property_keys.iter().copied())
        .collect();
    let tuple: Option<Vec<IndexKey>> = keys
        .iter()
        .map(|pk| {
            edge.properties.get(*pk).and_then(|pv| {
                if matches!(pv, PropertyValue::Null) {
                    None
                } else {
                    IndexKey::from_property_value(&pv)
                }
            })
        })
        .collect();
    let Some(tuple) = tuple else { return Ok(()) };
    drop(edge);

    let strings = graph.strings();
    let type_str = strings.resolve(tid.0);
    for other_eid in graph.edges_by_type(type_str) {
        if other_eid == eid {
            continue;
        }
        let Ok(other) = graph.get_edge(other_eid) else {
            continue;
        };
        let other_tuple: Option<Vec<IndexKey>> = keys
            .iter()
            .map(|pk| {
                other.properties.get(*pk).and_then(|pv| {
                    if matches!(pv, PropertyValue::Null) {
                        None
                    } else {
                        IndexKey::from_property_value(&pv)
                    }
                })
            })
            .collect();
        if other_tuple.as_ref() == Some(&tuple) {
            let props: Vec<&str> = keys.iter().map(|pk| strings.resolve(pk.0)).collect();
            return Err(CypherError::from(
                traverse_core::error::Error::constraint_violation(
                    traverse_core::error::ViolationKind::Uniqueness,
                    format!(
                        "relationship({}) already exists with type `{type_str}` and the same ({}) tuple",
                        other_eid.0,
                        props.join(", ")
                    ),
                ),
            ));
        }
    }
    Ok(())
}

/// Constraints for one property on a node.
pub(crate) fn check_node_property_constraint(
    graph: &dyn GraphView,
    nid: NodeId,
    prop_key: &str,
) -> Result<(), CypherError> {
    let registry = graph.constraint_registry();
    if registry.is_empty() {
        return Ok(());
    }
    let node = graph.get_node(nid).map_err(CypherError::from)?;
    let strings = graph.strings();
    // only constraints on this property
    if let Some(pkid) = strings.get(prop_key).map(PropertyKeyId) {
        for lid in &node.labels {
            if registry.has_unique_constraint(*lid, pkid) {
                let pv = node.properties.get(pkid).unwrap_or(PropertyValue::Null);
                validate_unique_via_view(graph, nid, *lid, pkid, &pv)?;
            }
            if registry.has_existence_constraint(*lid, pkid) {
                let has_value = node
                    .properties
                    .get(pkid)
                    .map(|v| !matches!(v, PropertyValue::Null))
                    .unwrap_or(false);
                if !has_value {
                    let label_name = strings.resolve(lid.0);
                    return Err(CypherError::from(
                        traverse_core::error::Error::constraint_violation(
                            traverse_core::error::ViolationKind::Existence,
                            format!(
                                "node({}) with label `{}` must have property `{}`",
                                nid.0, label_name, prop_key
                            ),
                        ),
                    ));
                }
            }
            // composite constraints including this property: tuple uniqueness + (KEY, via the
            // existence iterator) every key present
            for desc in registry.unique_constraints_for_label(*lid) {
                if !desc.extra_property_keys.is_empty()
                    && (desc.property_key == pkid || desc.extra_property_keys.contains(&pkid))
                {
                    validate_composite_unique_via_view(graph, nid, *lid, desc)?;
                }
            }
            for desc in registry.existence_constraints_for_label(*lid) {
                if !desc.extra_property_keys.is_empty()
                    && (desc.property_key == pkid || desc.extra_property_keys.contains(&pkid))
                {
                    let has_value = node
                        .properties
                        .get(pkid)
                        .map(|v| !matches!(v, PropertyValue::Null))
                        .unwrap_or(false);
                    if !has_value {
                        let label_name = strings.resolve(lid.0);
                        return Err(CypherError::from(
                            traverse_core::error::Error::constraint_violation(
                                traverse_core::error::ViolationKind::Existence,
                                format!(
                                    "node({}) with label `{}` must have property `{}`",
                                    nid.0, label_name, prop_key
                                ),
                            ),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// All constraints on a node (every label x every constraint type).
pub(crate) fn check_all_node_constraints(
    graph: &dyn GraphView,
    nid: NodeId,
) -> Result<(), CypherError> {
    let registry = graph.constraint_registry();
    if registry.is_empty() {
        return Ok(());
    }
    let node = graph.get_node(nid).map_err(CypherError::from)?;
    let strings = graph.strings();
    for lid in &node.labels {
        // unique (composite = tuple uniqueness)
        for desc in registry.unique_constraints_for_label(*lid) {
            if desc.extra_property_keys.is_empty() {
                let pv = node
                    .properties
                    .get(desc.property_key)
                    .unwrap_or(PropertyValue::Null);
                validate_unique_via_view(graph, nid, *lid, desc.property_key, &pv)?;
            } else {
                validate_composite_unique_via_view(graph, nid, *lid, desc)?;
            }
        }
        // existence (composite KEY = every key present)
        for desc in registry.existence_constraints_for_label(*lid) {
            for pk in std::iter::once(&desc.property_key).chain(desc.extra_property_keys.iter()) {
                let has_value = node
                    .properties
                    .get(*pk)
                    .map(|v| !matches!(v, PropertyValue::Null))
                    .unwrap_or(false);
                if !has_value {
                    let label_name = strings.resolve(lid.0);
                    let prop_name = strings.resolve(pk.0);
                    return Err(CypherError::from(
                        traverse_core::error::Error::constraint_violation(
                            traverse_core::error::ViolationKind::Existence,
                            format!(
                                "node({}) with label `{}` must have property `{}`",
                                nid.0, label_name, prop_name
                            ),
                        ),
                    ));
                }
            }
        }
        // property type; NULL always passes (add `Exists` for non-null)
        for desc in registry.type_constraints_for_label(*lid) {
            let kind = match desc.constraint_type {
                traverse_core::graph::constraint::ConstraintType::Type(k) => k,
                _ => continue,
            };
            if let Some(pv) = node.properties.get(desc.property_key)
                && !kind.matches(&pv)
            {
                let label_name = strings.resolve(lid.0);
                let prop_name = strings.resolve(desc.property_key.0);
                return Err(CypherError::from(
                    traverse_core::error::Error::constraint_violation(
                        traverse_core::error::ViolationKind::Type,
                        format!(
                            "node({}) property `{label_name}.{prop_name}` is not of type `{}`",
                            nid.0,
                            kind.surface(),
                        ),
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// All relationship constraints on an edge after a write (create or property change). Like
/// [`check_all_node_constraints`], but an edge has one type, not a label set.
pub(crate) fn check_all_edge_constraints(
    graph: &dyn GraphView,
    eid: EdgeId,
) -> Result<(), CypherError> {
    let registry = graph.constraint_registry();
    if registry.is_empty() {
        return Ok(());
    }
    let edge = graph.get_edge(eid).map_err(CypherError::from)?;
    let tid = LabelId(edge.edge_type.0);
    let strings = graph.strings();
    // uniqueness (relationship KEY implies it; composite = tuple)
    for desc in registry.unique_constraints_for_edge_type(tid) {
        if desc.extra_property_keys.is_empty() {
            let pv = edge
                .properties
                .get(desc.property_key)
                .unwrap_or(PropertyValue::Null);
            validate_edge_unique_via_view(graph, eid, tid, desc.property_key, &pv)?;
        } else {
            validate_composite_edge_unique_via_view(graph, eid, tid, desc)?;
        }
    }
    // existence (relationship KEY implies it; composite = every key)
    for desc in registry.existence_constraints_for_edge_type(tid) {
        for pk in std::iter::once(&desc.property_key).chain(desc.extra_property_keys.iter()) {
            let has_value = edge
                .properties
                .get(*pk)
                .map(|v| !matches!(v, PropertyValue::Null))
                .unwrap_or(false);
            if !has_value {
                let type_name = strings.resolve(tid.0);
                let prop_name = strings.resolve(pk.0);
                return Err(CypherError::from(
                    traverse_core::error::Error::constraint_violation(
                        traverse_core::error::ViolationKind::Existence,
                        format!(
                            "relationship({}) of type `{}` must have property `{}`",
                            eid.0, type_name, prop_name
                        ),
                    ),
                ));
            }
        }
    }
    // property type (NULL always passes)
    for desc in registry.type_constraints_for_edge_type(tid) {
        let kind = match desc.constraint_type {
            traverse_core::graph::constraint::ConstraintType::Type(k) => k,
            _ => continue,
        };
        if let Some(pv) = edge.properties.get(desc.property_key)
            && !kind.matches(&pv)
        {
            let type_name = strings.resolve(tid.0);
            let prop_name = strings.resolve(desc.property_key.0);
            return Err(CypherError::from(
                traverse_core::error::Error::constraint_violation(
                    traverse_core::error::ViolationKind::Type,
                    format!(
                        "relationship({}) property `{type_name}.{prop_name}` is not of type `{}`",
                        eid.0,
                        kind.surface()
                    ),
                ),
            ));
        }
    }
    Ok(())
}

/// Edge version of [`validate_unique_via_view`]: scans same-type edges for a duplicate value.
fn validate_edge_unique_via_view(
    graph: &dyn GraphView,
    eid: EdgeId,
    tid: LabelId,
    pkid: PropertyKeyId,
    value: &PropertyValue,
) -> Result<(), CypherError> {
    if matches!(value, PropertyValue::Null) {
        return Ok(());
    }
    if let Some(idx_key) = IndexKey::from_property_value(value) {
        let strings = graph.strings();
        let type_str = strings.resolve(tid.0);
        let prop_str = strings.resolve(pkid.0);
        for other_eid in graph.edges_by_type(type_str) {
            if other_eid == eid {
                continue;
            }
            if let Ok(other) = graph.get_edge(other_eid)
                && let Some(other_pv) = other.properties.get(pkid)
                && !matches!(other_pv, PropertyValue::Null)
                && let Some(other_key) = IndexKey::from_property_value(&other_pv)
                && other_key == idx_key
            {
                return Err(CypherError::from(
                    traverse_core::error::Error::constraint_violation(
                        traverse_core::error::ViolationKind::Uniqueness,
                        format!(
                            "relationship({}) already exists with type `{type_str}` and property `{prop_str}`",
                            other_eid.0
                        ),
                    ),
                ));
            }
        }
    }
    Ok(())
}
