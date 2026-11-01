use std::collections::BTreeMap;

use tonic::Status;
use traverse_core::graph::property::{PropertyMap, PropertyValue};
use traverse_core::graph::types::{EdgeId, NodeId};
use traverse_cypher::types::CypherValue;

use super::proto;

// -- CypherValue -> proto::Value --------------------------------------

fn property_value_to_proto(pv: &PropertyValue) -> proto::Value {
    use proto::value::Kind;
    let kind = match pv {
        PropertyValue::Null => Some(Kind::NullValue(true)),
        PropertyValue::Bool(b) => Some(Kind::BoolValue(*b)),
        PropertyValue::Int(n) => Some(Kind::IntValue(*n)),
        PropertyValue::Float(f) => Some(Kind::DoubleValue(*f)),
        PropertyValue::String(s) => Some(Kind::StringValue(s.clone())),
        PropertyValue::Bytes(b) => Some(Kind::BytesValue(b.clone())),
        PropertyValue::List(items) => Some(Kind::ListValue(proto::ListValue {
            values: items.iter().map(property_value_to_proto).collect(),
        })),
        PropertyValue::Map(m) => Some(Kind::MapValue(proto::MapValue {
            entries: m
                .iter()
                .map(|(k, v)| (k.clone(), property_value_to_proto(v)))
                .collect(),
        })),
        PropertyValue::DateTime(ms) => Some(Kind::TemporalValue(format!("epoch_ms:{ms}"))),
        PropertyValue::Duration(ms) => Some(Kind::TemporalValue(format!("duration_ms:{ms}"))),
        PropertyValue::Point2D { x, y, srid } => Some(Kind::MapValue(proto::MapValue {
            entries: [
                (
                    "srid".into(),
                    proto::Value {
                        kind: Some(Kind::IntValue(*srid as i64)),
                    },
                ),
                (
                    "x".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(*x)),
                    },
                ),
                (
                    "y".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(*y)),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        })),
        PropertyValue::Point3D { x, y, z, srid } => Some(Kind::MapValue(proto::MapValue {
            entries: [
                (
                    "srid".into(),
                    proto::Value {
                        kind: Some(Kind::IntValue(*srid as i64)),
                    },
                ),
                (
                    "x".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(*x)),
                    },
                ),
                (
                    "y".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(*y)),
                    },
                ),
                (
                    "z".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(*z)),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        })),
        PropertyValue::Vector(v) => Some(Kind::ListValue(proto::ListValue {
            values: v
                .iter()
                .map(|f| proto::Value {
                    kind: Some(Kind::DoubleValue(*f as f64)),
                })
                .collect(),
        })),
    };
    proto::Value { kind }
}

// -- CypherValue -> proto::Value (owned, zero-clone) -----------------

pub fn cypher_value_to_proto_owned(cv: CypherValue) -> proto::Value {
    use proto::value::Kind;
    let kind = match cv {
        CypherValue::Null => Some(Kind::NullValue(true)),
        CypherValue::Bool(b) => Some(Kind::BoolValue(b)),
        CypherValue::Integer(n) => Some(Kind::IntValue(n)),
        CypherValue::Float(f) => Some(Kind::DoubleValue(f)),
        CypherValue::String(s) => Some(Kind::StringValue(s)),
        CypherValue::Bytes(b) => Some(Kind::StringValue({
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&b)
        })),
        CypherValue::List(items) => Some(Kind::ListValue(proto::ListValue {
            values: items.into_iter().map(cypher_value_to_proto_owned).collect(),
        })),
        CypherValue::Map(m) => Some(Kind::MapValue(proto::MapValue {
            entries: m
                .into_iter()
                .map(|(k, v)| (k, cypher_value_to_proto_owned(v)))
                .collect(),
        })),
        CypherValue::Node {
            id,
            labels,
            properties,
        } => Some(Kind::NodeValue(proto::NodeValue {
            id: id.0,
            labels,
            properties: property_map_to_proto_map_owned(properties),
        })),
        CypherValue::Edge {
            id,
            edge_type,
            source,
            target,
            properties,
        } => Some(Kind::EdgeValue(proto::EdgeValue {
            id: id.0,
            edge_type,
            source: source.0,
            target: target.0,
            properties: property_map_to_proto_map_owned(properties),
        })),
        CypherValue::Path { nodes, edges } => {
            let proto_nodes: Vec<proto::NodeValue> = nodes
                .into_iter()
                .filter_map(|n| {
                    if let CypherValue::Node {
                        id,
                        labels,
                        properties,
                    } = n
                    {
                        Some(proto::NodeValue {
                            id: id.0,
                            labels,
                            properties: property_map_to_proto_map_owned(properties),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            let proto_edges: Vec<proto::EdgeValue> = edges
                .into_iter()
                .filter_map(|e| {
                    if let CypherValue::Edge {
                        id,
                        edge_type,
                        source,
                        target,
                        properties,
                    } = e
                    {
                        Some(proto::EdgeValue {
                            id: id.0,
                            edge_type,
                            source: source.0,
                            target: target.0,
                            properties: property_map_to_proto_map_owned(properties),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            Some(Kind::PathValue(proto::PathValue {
                nodes: proto_nodes,
                edges: proto_edges,
            }))
        }
        CypherValue::NodeRef(nid) => Some(Kind::NodeValue(proto::NodeValue {
            id: nid.0,
            labels: vec![],
            properties: Default::default(),
        })),
        CypherValue::EdgeRef {
            id,
            edge_type_id: _,
            source,
            target,
        } => Some(Kind::EdgeValue(proto::EdgeValue {
            id: id.0,
            edge_type: String::new(),
            source: source.0,
            target: target.0,
            properties: Default::default(),
        })),
        CypherValue::Date(d) => Some(Kind::TemporalValue(d.to_string())),
        CypherValue::LocalTime(lt) => Some(Kind::TemporalValue(format!(
            "{:02}:{:02}:{:02}.{:09}",
            lt.nanos / 3_600_000_000_000,
            (lt.nanos % 3_600_000_000_000) / 60_000_000_000,
            (lt.nanos % 60_000_000_000) / 1_000_000_000,
            lt.nanos % 1_000_000_000,
        ))),
        CypherValue::Time(t) => {
            let offset_h = t.offset_secs / 3600;
            let offset_m = (t.offset_secs.abs() % 3600) / 60;
            Some(Kind::TemporalValue(format!(
                "{:02}:{:02}:{:02}.{:09}{:+03}:{:02}",
                t.nanos / 3_600_000_000_000,
                (t.nanos % 3_600_000_000_000) / 60_000_000_000,
                (t.nanos % 60_000_000_000) / 1_000_000_000,
                t.nanos % 1_000_000_000,
                offset_h,
                offset_m,
            )))
        }
        CypherValue::LocalDateTime(ldt) => Some(Kind::TemporalValue(format!(
            "{}T{:02}:{:02}:{:02}.{:09}",
            traverse_cypher::temporal::CypherDate {
                date: ldt.date,
                cycles: ldt.cycles
            },
            ldt.nanos / 3_600_000_000_000,
            (ldt.nanos % 3_600_000_000_000) / 60_000_000_000,
            (ldt.nanos % 60_000_000_000) / 1_000_000_000,
            ldt.nanos % 1_000_000_000,
        ))),
        CypherValue::DateTime(dt) => {
            let offset_h = dt.offset_secs / 3600;
            let offset_m = (dt.offset_secs.abs() % 3600) / 60;
            Some(Kind::TemporalValue(format!(
                "{}T{:02}:{:02}:{:02}.{:09}{:+03}:{:02}",
                dt.date,
                dt.nanos / 3_600_000_000_000,
                (dt.nanos % 3_600_000_000_000) / 60_000_000_000,
                (dt.nanos % 60_000_000_000) / 1_000_000_000,
                dt.nanos % 1_000_000_000,
                offset_h,
                offset_m,
            )))
        }
        CypherValue::Duration(d) => Some(Kind::TemporalValue(format!(
            "P{}M{}DT{}S",
            d.months,
            d.days,
            d.nanos() as f64 / 1_000_000_000.0,
        ))),
    };
    proto::Value { kind }
}

fn property_map_to_proto_map_owned(
    pm: PropertyMap,
) -> std::collections::HashMap<String, proto::Value> {
    pm.into_iter()
        .map(|(k, v)| (k, property_value_to_proto_owned(v)))
        .collect()
}

fn property_value_to_proto_owned(pv: PropertyValue) -> proto::Value {
    use proto::value::Kind;
    let kind = match pv {
        PropertyValue::Null => Some(Kind::NullValue(true)),
        PropertyValue::Bool(b) => Some(Kind::BoolValue(b)),
        PropertyValue::Int(n) => Some(Kind::IntValue(n)),
        PropertyValue::Float(f) => Some(Kind::DoubleValue(f)),
        PropertyValue::String(s) => Some(Kind::StringValue(s)),
        PropertyValue::Bytes(b) => Some(Kind::BytesValue(b)),
        PropertyValue::List(items) => Some(Kind::ListValue(proto::ListValue {
            values: items
                .into_iter()
                .map(property_value_to_proto_owned)
                .collect(),
        })),
        PropertyValue::Map(m) => Some(Kind::MapValue(proto::MapValue {
            entries: m
                .into_iter()
                .map(|(k, v)| (k, property_value_to_proto_owned(v)))
                .collect(),
        })),
        PropertyValue::DateTime(ms) => Some(Kind::TemporalValue(format!("epoch_ms:{ms}"))),
        PropertyValue::Duration(ms) => Some(Kind::TemporalValue(format!("duration_ms:{ms}"))),
        PropertyValue::Point2D { x, y, srid } => Some(Kind::MapValue(proto::MapValue {
            entries: [
                (
                    "srid".into(),
                    proto::Value {
                        kind: Some(Kind::IntValue(srid as i64)),
                    },
                ),
                (
                    "x".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(x)),
                    },
                ),
                (
                    "y".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(y)),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        })),
        PropertyValue::Point3D { x, y, z, srid } => Some(Kind::MapValue(proto::MapValue {
            entries: [
                (
                    "srid".into(),
                    proto::Value {
                        kind: Some(Kind::IntValue(srid as i64)),
                    },
                ),
                (
                    "x".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(x)),
                    },
                ),
                (
                    "y".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(y)),
                    },
                ),
                (
                    "z".into(),
                    proto::Value {
                        kind: Some(Kind::DoubleValue(z)),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        })),
        PropertyValue::Vector(v) => Some(Kind::ListValue(proto::ListValue {
            values: v
                .iter()
                .map(|f| proto::Value {
                    kind: Some(Kind::DoubleValue(*f as f64)),
                })
                .collect(),
        })),
    };
    proto::Value { kind }
}

// -- proto::Value -> CypherValue --------------------------------------

pub fn proto_value_to_cypher(v: &proto::Value) -> Result<CypherValue, Status> {
    use proto::value::Kind;
    match &v.kind {
        None | Some(Kind::NullValue(_)) => Ok(CypherValue::Null),
        Some(Kind::BoolValue(b)) => Ok(CypherValue::Bool(*b)),
        Some(Kind::IntValue(n)) => Ok(CypherValue::Integer(*n)),
        Some(Kind::DoubleValue(f)) => Ok(CypherValue::Float(*f)),
        Some(Kind::StringValue(s)) => Ok(CypherValue::String(s.clone())),
        Some(Kind::BytesValue(_)) => Err(Status::invalid_argument(
            "bytes values are not supported as Cypher parameters; encode as base64 string",
        )),
        Some(Kind::ListValue(l)) => {
            let items: Result<Vec<_>, _> = l.values.iter().map(proto_value_to_cypher).collect();
            Ok(CypherValue::List(items?))
        }
        Some(Kind::MapValue(m)) => {
            let mut map = BTreeMap::new();
            for (k, v) in &m.entries {
                map.insert(k.clone(), proto_value_to_cypher(v)?);
            }
            Ok(CypherValue::Map(map))
        }
        Some(Kind::NodeValue(n)) => Ok(CypherValue::Node {
            id: NodeId(n.id),
            labels: n.labels.clone(),
            properties: proto_map_to_property_map(&n.properties)?,
        }),
        Some(Kind::EdgeValue(e)) => Ok(CypherValue::Edge {
            id: EdgeId(e.id),
            edge_type: e.edge_type.clone(),
            source: NodeId(e.source),
            target: NodeId(e.target),
            properties: proto_map_to_property_map(&e.properties)?,
        }),
        Some(Kind::PathValue(_)) => Err(Status::invalid_argument(
            "path values not supported as parameters",
        )),
        Some(Kind::TemporalValue(s)) => Ok(CypherValue::String(s.clone())),
    }
}

/// Proto map -> `PropertyMap`.
pub fn proto_map_to_property_map(
    m: &std::collections::HashMap<String, proto::Value>,
) -> Result<PropertyMap, Status> {
    let mut pm = BTreeMap::new();
    for (k, v) in m {
        pm.insert(k.clone(), proto_value_to_property(v)?);
    }
    Ok(pm)
}

fn proto_value_to_property(v: &proto::Value) -> Result<PropertyValue, Status> {
    use proto::value::Kind;
    match &v.kind {
        None | Some(Kind::NullValue(_)) => Ok(PropertyValue::Null),
        Some(Kind::BoolValue(b)) => Ok(PropertyValue::Bool(*b)),
        Some(Kind::IntValue(n)) => Ok(PropertyValue::Int(*n)),
        Some(Kind::DoubleValue(f)) => Ok(PropertyValue::Float(*f)),
        Some(Kind::StringValue(s)) => Ok(PropertyValue::String(s.clone())),
        Some(Kind::BytesValue(b)) => Ok(PropertyValue::Bytes(b.clone())),
        Some(Kind::ListValue(l)) => {
            let items: Result<Vec<_>, _> = l.values.iter().map(proto_value_to_property).collect();
            Ok(PropertyValue::List(items?))
        }
        Some(Kind::MapValue(m)) => {
            let mut map = BTreeMap::new();
            for (k, v) in &m.entries {
                map.insert(k.clone(), proto_value_to_property(v)?);
            }
            Ok(PropertyValue::Map(map))
        }
        Some(Kind::NodeValue(_) | Kind::EdgeValue(_) | Kind::PathValue(_)) => Err(
            Status::invalid_argument("graph types not supported as property values"),
        ),
        Some(Kind::TemporalValue(s)) => Ok(PropertyValue::String(s.clone())),
    }
}

/// Proto parameter map -> CypherValue HashMap.
pub fn proto_params_to_cypher(
    m: &std::collections::HashMap<String, proto::Value>,
) -> Result<std::collections::HashMap<String, CypherValue>, Status> {
    let mut map = std::collections::HashMap::new();
    for (k, v) in m {
        map.insert(k.clone(), proto_value_to_cypher(v)?);
    }
    Ok(map)
}

/// QueryStats -> proto.
pub fn stats_to_proto(s: &traverse_cypher::executor::QueryStats) -> proto::QueryStats {
    proto::QueryStats {
        nodes_created: s.nodes_created,
        nodes_deleted: s.nodes_deleted,
        relationships_created: s.relationships_created,
        relationships_deleted: s.relationships_deleted,
        properties_set: s.properties_set,
        labels_added: s.labels_added,
        labels_removed: s.labels_removed,
        indexes_added: s.indexes_added,
        indexes_removed: s.indexes_removed,
        constraints_added: s.constraints_added,
        constraints_removed: s.constraints_removed,
    }
}

/// proto::NodeValue read from a GraphView.
pub fn read_node_to_proto(
    view: &dyn traverse_core::tx::GraphView,
    id: NodeId,
) -> Result<proto::NodeValue, Status> {
    let node = traverse_core::tx::GraphView::get_node(view, id)
        .map_err(|e| Status::not_found(e.to_string()))?;
    let strings = traverse_core::tx::GraphView::strings(view);
    let labels: Vec<String> = node
        .labels
        .iter()
        .map(|lid| strings.resolve(lid.0).to_string())
        .collect();
    let properties = node
        .properties
        .iter()
        .map(|(k, v)| {
            (
                strings.resolve(k.0).to_string(),
                property_value_to_proto(&v),
            )
        })
        .collect();
    Ok(proto::NodeValue {
        id: id.0,
        labels,
        properties,
    })
}

/// proto::EdgeValue read from a GraphView.
pub fn read_edge_to_proto(
    view: &dyn traverse_core::tx::GraphView,
    id: EdgeId,
) -> Result<proto::EdgeValue, Status> {
    let edge = traverse_core::tx::GraphView::get_edge(view, id)
        .map_err(|e| Status::not_found(e.to_string()))?;
    let strings = traverse_core::tx::GraphView::strings(view);
    let edge_type = strings.resolve(edge.edge_type.0).to_string();
    let properties = edge
        .properties
        .iter()
        .map(|(k, v)| {
            (
                strings.resolve(k.0).to_string(),
                property_value_to_proto(&v),
            )
        })
        .collect();
    Ok(proto::EdgeValue {
        id: id.0,
        edge_type,
        source: edge.source.0,
        target: edge.target.0,
        properties,
    })
}
