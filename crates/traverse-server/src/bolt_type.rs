use std::collections::{BTreeMap, HashMap};

use traverse_core::graph::property::{PropertyMap, PropertyValue};
use traverse_cypher::temporal::{
    CypherDate, CypherDateTime, CypherDuration, CypherLocalDateTime, CypherLocalTime, CypherTime,
    wide_date_from_epoch_days,
};
use traverse_cypher::types::CypherValue;

use crate::error::{Result, ServerError};
use crate::packstream::Value;

// -- PackStream structure tags for typed values ----------------------

const TAG_NODE: u8 = 0x4E;
const TAG_RELATIONSHIP: u8 = 0x52;
const TAG_UNBOUND_RELATIONSHIP: u8 = 0x72;
const TAG_PATH: u8 = 0x50;
const TAG_DATE: u8 = 0x44;
const TAG_LOCAL_TIME: u8 = 0x74;
const TAG_TIME: u8 = 0x54;
const TAG_LOCAL_DATETIME: u8 = 0x64;
const TAG_DATETIME_OFFSET: u8 = 0x49;
const TAG_DATETIME_ZONE: u8 = 0x69;
const TAG_DURATION: u8 = 0x45;
const TAG_POINT_2D: u8 = 0x58;
const TAG_POINT_3D: u8 = 0x59;
const TAG_VECTOR: u8 = 0x56;
const TAG_UNSUPPORTED: u8 = 0x00;

// -- CypherValue -> PackStream Value ---------------------------------

/// CypherValue -> PackStream Value for the wire, Bolt 5.x+ encoding: Node has 4 fields (with
/// element_id), Relationship 8 (with element_ids), UnboundRel 4. `bolt_major` selects
/// version-specific encodings (e.g. the Vector type on Bolt 6.0+).
pub fn cypher_value_to_packstream(cv: &CypherValue, bolt_major: u8) -> Value {
    match cv {
        CypherValue::Null => Value::Null,
        CypherValue::Bool(b) => Value::Bool(*b),
        CypherValue::Integer(n) => Value::Integer(*n),
        CypherValue::Float(f) => Value::Float(*f),
        CypherValue::String(s) => Value::String(s.clone()),
        CypherValue::Bytes(b) => Value::Bytes(b.clone()),

        CypherValue::List(items) => Value::List(
            items
                .iter()
                .map(|v| cypher_value_to_packstream(v, bolt_major))
                .collect(),
        ),

        CypherValue::Map(m) => {
            // canonical point maps go out as Bolt Point structures, like Neo4j's `RETURN
            // point({...})`
            if let (
                Some(CypherValue::Float(x)),
                Some(CypherValue::Float(y)),
                Some(CypherValue::Integer(srid)),
            ) = (m.get("x"), m.get("y"), m.get("srid"))
            {
                let point_keys = m
                    .keys()
                    .all(|k| matches!(k.as_str(), "x" | "y" | "z" | "srid" | "crs"));
                if point_keys {
                    return match m.get("z") {
                        Some(CypherValue::Float(z)) => Value::Structure {
                            tag: TAG_POINT_3D,
                            fields: vec![
                                Value::Integer(*srid),
                                Value::Float(*x),
                                Value::Float(*y),
                                Value::Float(*z),
                            ],
                        },
                        _ => Value::Structure {
                            tag: TAG_POINT_2D,
                            fields: vec![Value::Integer(*srid), Value::Float(*x), Value::Float(*y)],
                        },
                    };
                }
            }
            Value::Map(
                m.iter()
                    .map(|(k, v)| (k.clone(), cypher_value_to_packstream(v, bolt_major)))
                    .collect(),
            )
        }

        CypherValue::Node {
            id,
            labels,
            properties,
        } => {
            let labels_val = Value::List(labels.iter().map(|l| Value::String(l.clone())).collect());
            let props_val = property_map_to_packstream(properties, bolt_major);
            let element_id = Value::String(format!("4:{}", id.0));

            if bolt_major >= 6 {
                // Bolt 6.0+: 3 fields (no integer ID)
                Value::Structure {
                    tag: TAG_NODE,
                    fields: vec![labels_val, props_val, element_id],
                }
            } else {
                // Bolt 5.x: 4 fields
                Value::Structure {
                    tag: TAG_NODE,
                    fields: vec![
                        Value::Integer(id.0 as i64),
                        labels_val,
                        props_val,
                        element_id,
                    ],
                }
            }
        }

        CypherValue::Edge {
            id,
            edge_type,
            source,
            target,
            properties,
        } => {
            let type_val = Value::String(edge_type.clone());
            let props_val = property_map_to_packstream(properties, bolt_major);
            let element_id = Value::String(format!("5:{}", id.0));
            let start_element_id = Value::String(format!("4:{}", source.0));
            let end_element_id = Value::String(format!("4:{}", target.0));

            if bolt_major >= 6 {
                // Bolt 6.0+: 5 fields (no integer IDs)
                Value::Structure {
                    tag: TAG_RELATIONSHIP,
                    fields: vec![
                        type_val,
                        props_val,
                        element_id,
                        start_element_id,
                        end_element_id,
                    ],
                }
            } else {
                // Bolt 5.x: 8 fields
                Value::Structure {
                    tag: TAG_RELATIONSHIP,
                    fields: vec![
                        Value::Integer(id.0 as i64),
                        Value::Integer(source.0 as i64),
                        Value::Integer(target.0 as i64),
                        type_val,
                        props_val,
                        element_id,
                        start_element_id,
                        end_element_id,
                    ],
                }
            }
        }

        CypherValue::Path { nodes, edges } => encode_path(nodes, edges, bolt_major),

        // NodeRef/EdgeRef must be materialized before Bolt serialization
        CypherValue::NodeRef(_) | CypherValue::EdgeRef { .. } => {
            unreachable!("NodeRef/EdgeRef should be materialized before bolt serialization")
        }

        CypherValue::Date(d) => {
            let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let days = d.date.signed_duration_since(epoch).num_days() + 146_097 * d.cycles; // 400-year Gregorian cycles
            Value::Structure {
                tag: TAG_DATE,
                fields: vec![Value::Integer(days)],
            }
        }

        CypherValue::LocalTime(lt) => Value::Structure {
            tag: TAG_LOCAL_TIME,
            fields: vec![Value::Integer(lt.nanos as i64)],
        },

        CypherValue::Time(t) => Value::Structure {
            tag: TAG_TIME,
            fields: vec![
                Value::Integer(t.nanos as i64),
                Value::Integer(t.offset_secs as i64),
            ],
        },

        CypherValue::LocalDateTime(ldt) => {
            let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let days = ldt.date.signed_duration_since(epoch).num_days() + 146_097 * ldt.cycles; // 400-year Gregorian cycles
            let seconds = days * 86400 + (ldt.nanos / 1_000_000_000) as i64;
            let nanos = (ldt.nanos % 1_000_000_000) as i64;
            Value::Structure {
                tag: TAG_LOCAL_DATETIME,
                fields: vec![Value::Integer(seconds), Value::Integer(nanos)],
            }
        }

        CypherValue::DateTime(dt) => {
            let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let days = dt.date.signed_duration_since(epoch).num_days();
            let seconds = days * 86400 + (dt.nanos / 1_000_000_000) as i64;
            let nanos = (dt.nanos % 1_000_000_000) as i64;

            if let Some(tz_name) = &dt.tz_name {
                Value::Structure {
                    tag: TAG_DATETIME_ZONE,
                    fields: vec![
                        Value::Integer(seconds),
                        Value::Integer(nanos),
                        Value::String(tz_name.clone()),
                    ],
                }
            } else {
                Value::Structure {
                    tag: TAG_DATETIME_OFFSET,
                    fields: vec![
                        Value::Integer(seconds),
                        Value::Integer(nanos),
                        Value::Integer(dt.offset_secs as i64),
                    ],
                }
            }
        }

        CypherValue::Duration(d) => Value::Structure {
            tag: TAG_DURATION,
            fields: vec![
                Value::Integer(d.months),
                Value::Integer(d.days),
                // Bolt canonical form: nanoseconds in [0, 1e9), seconds carry the borrow for
                // negative durations
                Value::Integer(d.seconds - i64::from(d.subsec_nanos < 0)),
                Value::Integer(
                    d.subsec_nanos as i64 + if d.subsec_nanos < 0 { 1_000_000_000 } else { 0 },
                ),
            ],
        },
    }
}

fn encode_path(nodes: &[CypherValue], edges: &[CypherValue], bolt_major: u8) -> Value {
    let mut unique_nodes: Vec<Value> = Vec::new();
    let mut node_index_map: HashMap<u64, usize> = HashMap::new();
    let mut unbound_rels: Vec<Value> = Vec::new();
    let mut indices: Vec<Value> = Vec::new();

    if let Some(first_node) = nodes.first() {
        let node_ps = cypher_value_to_packstream(first_node, bolt_major);
        if let CypherValue::Node { id, .. } = first_node {
            node_index_map.insert(id.0, 0);
        }
        unique_nodes.push(node_ps);
    }

    for (i, edge) in edges.iter().enumerate() {
        if let CypherValue::Edge {
            id,
            edge_type,
            source: _,
            target,
            properties,
        } = edge
        {
            let type_val = Value::String(edge_type.clone());
            let props_val = property_map_to_packstream(properties, bolt_major);
            let element_id = Value::String(format!("5:{}", id.0));

            if bolt_major >= 6 {
                // Bolt 6.0+: 3 fields (no integer ID)
                unbound_rels.push(Value::Structure {
                    tag: TAG_UNBOUND_RELATIONSHIP,
                    fields: vec![type_val, props_val, element_id],
                });
            } else {
                // Bolt 5.x: 4 fields
                unbound_rels.push(Value::Structure {
                    tag: TAG_UNBOUND_RELATIONSHIP,
                    fields: vec![Value::Integer(id.0 as i64), type_val, props_val, element_id],
                });
            }

            let rel_idx = (i + 1) as i64;

            let next_node = &nodes[i + 1];
            let (signed_rel_idx, next_node_id) =
                if let CypherValue::Node { id: nid, .. } = next_node {
                    if target.0 == nid.0 {
                        (rel_idx, nid.0)
                    } else {
                        (-rel_idx, nid.0)
                    }
                } else {
                    (rel_idx, 0)
                };

            indices.push(Value::Integer(signed_rel_idx));

            let node_idx = if let Some(&idx) = node_index_map.get(&next_node_id) {
                idx
            } else {
                let idx = unique_nodes.len();
                node_index_map.insert(next_node_id, idx);
                unique_nodes.push(cypher_value_to_packstream(next_node, bolt_major));
                idx
            };
            indices.push(Value::Integer(node_idx as i64));
        }
    }

    Value::Structure {
        tag: TAG_PATH,
        fields: vec![
            Value::List(unique_nodes),
            Value::List(unbound_rels),
            Value::List(indices),
        ],
    }
}

/// PropertyMap -> PackStream Map.
pub fn property_map_to_packstream(pm: &PropertyMap, bolt_major: u8) -> Value {
    Value::Map(
        pm.iter()
            .map(|(k, v)| (k.clone(), property_value_to_packstream(v, bolt_major)))
            .collect(),
    )
}

fn property_value_to_packstream(pv: &PropertyValue, bolt_major: u8) -> Value {
    match pv {
        PropertyValue::Null => Value::Null,
        PropertyValue::Bool(b) => Value::Bool(*b),
        PropertyValue::Int(n) => Value::Integer(*n),
        PropertyValue::Float(f) => Value::Float(*f),
        PropertyValue::String(s) => Value::String(s.clone()),
        PropertyValue::Bytes(b) => Value::Bytes(b.clone()),
        PropertyValue::List(items) => Value::List(
            items
                .iter()
                .map(|v| property_value_to_packstream(v, bolt_major))
                .collect(),
        ),
        PropertyValue::Map(m) => Value::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), property_value_to_packstream(v, bolt_major)))
                .collect(),
        ),
        PropertyValue::DateTime(ms) => {
            let seconds = ms.div_euclid(1000);
            let nanos = ms.rem_euclid(1000) * 1_000_000;
            Value::Structure {
                tag: TAG_DATETIME_OFFSET,
                fields: vec![
                    Value::Integer(seconds),
                    Value::Integer(nanos),
                    Value::Integer(0),
                ],
            }
        }
        PropertyValue::Duration(ms) => {
            let seconds = ms.div_euclid(1000);
            let nanos = ms.rem_euclid(1000) * 1_000_000;
            Value::Structure {
                tag: TAG_DURATION,
                fields: vec![
                    Value::Integer(0),
                    Value::Integer(0),
                    Value::Integer(seconds),
                    Value::Integer(nanos),
                ],
            }
        }
        PropertyValue::Point2D { x, y, srid } => Value::Structure {
            tag: TAG_POINT_2D,
            fields: vec![
                Value::Integer(*srid as i64),
                Value::Float(*x),
                Value::Float(*y),
            ],
        },
        PropertyValue::Point3D { x, y, z, srid } => Value::Structure {
            tag: TAG_POINT_3D,
            fields: vec![
                Value::Integer(*srid as i64),
                Value::Float(*x),
                Value::Float(*y),
                Value::Float(*z),
            ],
        },
        PropertyValue::Vector(v) => {
            if bolt_major >= 6 {
                // Bolt 6.0+: native Vector structure (tag 0x56)
                Value::Structure {
                    tag: TAG_VECTOR,
                    fields: vec![Value::List(
                        v.iter().map(|f| Value::Float(*f as f64)).collect(),
                    )],
                }
            } else {
                // Bolt 5.x: plain list of floats
                Value::List(v.iter().map(|f| Value::Float(*f as f64)).collect())
            }
        }
    }
}

// -- PackStream Value -> CypherValue ---------------------------------

/// PackStream Value (query parameters) -> CypherValue.
pub fn packstream_to_cypher_value(v: &Value) -> Result<CypherValue> {
    match v {
        Value::Null => Ok(CypherValue::Null),
        Value::Bool(b) => Ok(CypherValue::Bool(*b)),
        Value::Integer(n) => Ok(CypherValue::Integer(*n)),
        Value::Float(f) => Ok(CypherValue::Float(*f)),
        Value::String(s) => Ok(CypherValue::String(s.clone())),
        Value::Bytes(b) => {
            // bytes -> list of integer byte values
            Ok(CypherValue::List(
                b.iter()
                    .map(|byte| CypherValue::Integer(*byte as i64))
                    .collect(),
            ))
        }
        Value::List(items) => {
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                result.push(packstream_to_cypher_value(item)?);
            }
            Ok(CypherValue::List(result))
        }
        Value::Map(entries) => {
            let mut map = BTreeMap::new();
            for (k, v) in entries {
                map.insert(k.clone(), packstream_to_cypher_value(v)?);
            }
            Ok(CypherValue::Map(map))
        }
        Value::Structure { tag, fields } => match *tag {
            TAG_DATE => {
                let days = fields
                    .first()
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid Date structure".into()))?;
                let (date, cycles) = wide_date_from_epoch_days(days)
                    .ok_or_else(|| ServerError::Protocol("Date out of range".into()))?;
                Ok(CypherValue::Date(CypherDate { date, cycles }))
            }
            TAG_LOCAL_TIME => {
                let nanos = fields
                    .first()
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid LocalTime".into()))?;
                Ok(CypherValue::LocalTime(CypherLocalTime {
                    nanos: nanos as u64,
                }))
            }
            TAG_TIME => {
                let nanos = fields
                    .first()
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid Time".into()))?;
                let offset = fields
                    .get(1)
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid Time offset".into()))?;
                Ok(CypherValue::Time(CypherTime {
                    nanos: nanos as u64,
                    offset_secs: offset as i32,
                }))
            }
            TAG_LOCAL_DATETIME => {
                let seconds = fields
                    .first()
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid LocalDateTime".into()))?;
                let nanos = fields.get(1).and_then(Value::as_i64).unwrap_or(0);
                let days = seconds.div_euclid(86400);
                let day_seconds = seconds.rem_euclid(86400);
                let (date, cycles) = wide_date_from_epoch_days(days)
                    .ok_or_else(|| ServerError::Protocol("LocalDateTime out of range".into()))?;
                let total_nanos = (day_seconds as u64) * 1_000_000_000 + nanos as u64;
                Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
                    date,
                    nanos: total_nanos,
                    cycles,
                }))
            }
            TAG_DATETIME_OFFSET => {
                let seconds = fields
                    .first()
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid DateTime".into()))?;
                let nanos = fields.get(1).and_then(Value::as_i64).unwrap_or(0);
                let offset = fields
                    .get(2)
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid DateTime offset".into()))?;
                let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let days = seconds.div_euclid(86400);
                let day_seconds = seconds.rem_euclid(86400);
                let date = epoch + chrono::Duration::days(days);
                let total_nanos = (day_seconds as u64) * 1_000_000_000 + nanos as u64;
                Ok(CypherValue::DateTime(CypherDateTime {
                    date,
                    nanos: total_nanos,
                    offset_secs: offset as i32,
                    tz_name: None,
                }))
            }
            TAG_DATETIME_ZONE => {
                let seconds = fields
                    .first()
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid DateTime".into()))?;
                let nanos = fields.get(1).and_then(Value::as_i64).unwrap_or(0);
                let tz_name = fields
                    .get(2)
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or_else(|| ServerError::Protocol("invalid DateTime tz name".into()))?;
                let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let days = seconds.div_euclid(86400);
                let day_seconds = seconds.rem_euclid(86400);
                let date = epoch + chrono::Duration::days(days);
                let total_nanos = (day_seconds as u64) * 1_000_000_000 + nanos as u64;
                Ok(CypherValue::DateTime(CypherDateTime {
                    date,
                    nanos: total_nanos,
                    offset_secs: 0,
                    tz_name: Some(tz_name),
                }))
            }
            TAG_DURATION => {
                let months = fields.first().and_then(Value::as_i64).unwrap_or(0);
                let days = fields.get(1).and_then(Value::as_i64).unwrap_or(0);
                let seconds = fields.get(2).and_then(Value::as_i64).unwrap_or(0);
                let nanos = fields.get(3).and_then(Value::as_i64).unwrap_or(0);
                // canonical seconds/subsec split: grouping keys use the Debug form, so equal
                // durations must be field-identical
                Ok(CypherValue::Duration(CypherDuration::from_nanos(
                    months,
                    days,
                    seconds as i128 * 1_000_000_000 + nanos as i128,
                )))
            }
            TAG_POINT_2D => {
                let srid = fields
                    .first()
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid Point2D srid".into()))?;
                let x = fields
                    .get(1)
                    .and_then(|v| match v {
                        Value::Float(f) => Some(*f),
                        Value::Integer(n) => Some(*n as f64),
                        _ => None,
                    })
                    .ok_or_else(|| ServerError::Protocol("invalid Point2D x".into()))?;
                let y = fields
                    .get(2)
                    .and_then(|v| match v {
                        Value::Float(f) => Some(*f),
                        Value::Integer(n) => Some(*n as f64),
                        _ => None,
                    })
                    .ok_or_else(|| ServerError::Protocol("invalid Point2D y".into()))?;
                Ok(CypherValue::Map(BTreeMap::from([
                    ("srid".into(), CypherValue::Integer(srid)),
                    ("x".into(), CypherValue::Float(x)),
                    ("y".into(), CypherValue::Float(y)),
                ])))
            }
            TAG_POINT_3D => {
                let srid = fields
                    .first()
                    .and_then(Value::as_i64)
                    .ok_or_else(|| ServerError::Protocol("invalid Point3D srid".into()))?;
                let x = fields
                    .get(1)
                    .and_then(|v| match v {
                        Value::Float(f) => Some(*f),
                        Value::Integer(n) => Some(*n as f64),
                        _ => None,
                    })
                    .ok_or_else(|| ServerError::Protocol("invalid Point3D x".into()))?;
                let y = fields
                    .get(2)
                    .and_then(|v| match v {
                        Value::Float(f) => Some(*f),
                        Value::Integer(n) => Some(*n as f64),
                        _ => None,
                    })
                    .ok_or_else(|| ServerError::Protocol("invalid Point3D y".into()))?;
                let z = fields
                    .get(3)
                    .and_then(|v| match v {
                        Value::Float(f) => Some(*f),
                        Value::Integer(n) => Some(*n as f64),
                        _ => None,
                    })
                    .ok_or_else(|| ServerError::Protocol("invalid Point3D z".into()))?;
                Ok(CypherValue::Map(BTreeMap::from([
                    ("srid".into(), CypherValue::Integer(srid)),
                    ("x".into(), CypherValue::Float(x)),
                    ("y".into(), CypherValue::Float(y)),
                    ("z".into(), CypherValue::Float(z)),
                ])))
            }
            TAG_VECTOR => {
                // Bolt 6.0 Vector structure -> list of floats
                let items = fields
                    .first()
                    .and_then(|v| {
                        if let Value::List(l) = v {
                            Some(l)
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| ServerError::Protocol("invalid Vector structure".into()))?;
                let floats: Vec<CypherValue> = items
                    .iter()
                    .map(|v| match v {
                        Value::Float(f) => CypherValue::Float(*f),
                        Value::Integer(n) => CypherValue::Float(*n as f64),
                        _ => CypherValue::Null,
                    })
                    .collect();
                Ok(CypherValue::List(floats))
            }
            TAG_UNSUPPORTED => {
                // Bolt 6.0 UnsupportedType (tag 0x00) -> Null
                Ok(CypherValue::Null)
            }
            TAG_NODE => {
                // Bolt 5.x: [id, [labels], {props}, element_id]
                if fields.len() < 4 {
                    return Err(ServerError::Protocol("invalid Node structure".into()));
                }
                let id = fields[0]
                    .as_i64()
                    .ok_or_else(|| ServerError::Protocol("invalid Node id".into()))?;
                let labels = match &fields[1] {
                    Value::List(items) => items
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                    _ => vec![],
                };
                let properties = packstream_map_to_property_map(&fields[2])?;
                Ok(CypherValue::Node {
                    id: traverse_core::graph::types::NodeId(id as u64),
                    labels,
                    properties,
                })
            }
            TAG_RELATIONSHIP => {
                // Bolt 5.x: [id, start_id, end_id, type, {props}, element_id,
                // start_element_id, end_element_id]
                if fields.len() < 8 {
                    return Err(ServerError::Protocol(
                        "invalid Relationship structure".into(),
                    ));
                }
                let id = fields[0]
                    .as_i64()
                    .ok_or_else(|| ServerError::Protocol("invalid Relationship id".into()))?;
                let start_id = fields[1]
                    .as_i64()
                    .ok_or_else(|| ServerError::Protocol("invalid Relationship start_id".into()))?;
                let end_id = fields[2]
                    .as_i64()
                    .ok_or_else(|| ServerError::Protocol("invalid Relationship end_id".into()))?;
                let edge_type = fields[3]
                    .as_str()
                    .ok_or_else(|| ServerError::Protocol("invalid Relationship type".into()))?
                    .to_string();
                let properties = packstream_map_to_property_map(&fields[4])?;
                Ok(CypherValue::Edge {
                    id: traverse_core::graph::types::EdgeId(id as u64),
                    edge_type,
                    source: traverse_core::graph::types::NodeId(start_id as u64),
                    target: traverse_core::graph::types::NodeId(end_id as u64),
                    properties,
                })
            }
            TAG_PATH => {
                // Bolt: [[unique_nodes], [unique_rels], [indices]]
                if fields.len() < 3 {
                    return Err(ServerError::Protocol("invalid Path structure".into()));
                }
                let unique_nodes: Vec<CypherValue> = match &fields[0] {
                    Value::List(items) => items
                        .iter()
                        .map(packstream_to_cypher_value)
                        .collect::<Result<Vec<_>>>()?,
                    _ => return Err(ServerError::Protocol("invalid Path nodes list".into())),
                };
                let unique_rels: Vec<CypherValue> = match &fields[1] {
                    Value::List(items) => items
                        .iter()
                        .map(decode_unbound_rel)
                        .collect::<Result<Vec<_>>>()?,
                    _ => return Err(ServerError::Protocol("invalid Path rels list".into())),
                };
                let indices: Vec<i64> = match &fields[2] {
                    Value::List(items) => items.iter().filter_map(|v| v.as_i64()).collect(),
                    _ => return Err(ServerError::Protocol("invalid Path indices list".into())),
                };

                // rebuild the path from unique nodes/rels + indices
                let mut nodes = Vec::new();
                let mut edges = Vec::new();

                if !unique_nodes.is_empty() {
                    nodes.push(unique_nodes[0].clone());
                }

                for chunk in indices.chunks(2) {
                    if chunk.len() < 2 {
                        break;
                    }
                    let rel_idx = chunk[0];
                    let node_idx = chunk[1] as usize;

                    let abs_rel = rel_idx.unsigned_abs() as usize;
                    if abs_rel == 0 || abs_rel > unique_rels.len() {
                        return Err(ServerError::Protocol("invalid Path rel index".into()));
                    }
                    let rel = unique_rels[abs_rel - 1].clone();

                    // a negative rel_idx means the relationship is traversed in reverse;
                    // source/target are set by direction
                    if rel_idx > 0 {
                        edges.push(rel);
                    } else if let CypherValue::Edge {
                        id,
                        edge_type,
                        source,
                        target,
                        properties,
                    } = rel
                    {
                        // reversed
                        edges.push(CypherValue::Edge {
                            id,
                            edge_type,
                            source: target,
                            target: source,
                            properties,
                        });
                    } else {
                        edges.push(rel);
                    }

                    if node_idx < unique_nodes.len() {
                        nodes.push(unique_nodes[node_idx].clone());
                    }
                }

                Ok(CypherValue::Path { nodes, edges })
            }
            TAG_UNBOUND_RELATIONSHIP => {
                // Bolt 5.x: [id, type, {props}, element_id]
                if fields.len() < 4 {
                    return Err(ServerError::Protocol(
                        "invalid UnboundRelationship structure".into(),
                    ));
                }
                let id = fields[0].as_i64().ok_or_else(|| {
                    ServerError::Protocol("invalid UnboundRelationship id".into())
                })?;
                let edge_type = fields[1]
                    .as_str()
                    .ok_or_else(|| {
                        ServerError::Protocol("invalid UnboundRelationship type".into())
                    })?
                    .to_string();
                let properties = packstream_map_to_property_map(&fields[2])?;
                Ok(CypherValue::Edge {
                    id: traverse_core::graph::types::EdgeId(id as u64),
                    edge_type,
                    source: traverse_core::graph::types::NodeId(0),
                    target: traverse_core::graph::types::NodeId(0),
                    properties,
                })
            }
            _ => Err(ServerError::Protocol(format!(
                "unsupported structure tag 0x{tag:02X} in parameter"
            ))),
        },
    }
}

/// Unbound relationship from a PackStream structure. Bolt 5.x: [id, type, {props}, element_id]
fn decode_unbound_rel(v: &Value) -> Result<CypherValue> {
    match v {
        Value::Structure {
            tag: TAG_UNBOUND_RELATIONSHIP,
            fields,
        } => {
            if fields.len() < 4 {
                return Err(ServerError::Protocol(
                    "invalid UnboundRelationship structure".into(),
                ));
            }
            let id = fields[0]
                .as_i64()
                .ok_or_else(|| ServerError::Protocol("invalid UnboundRelationship id".into()))?;
            let edge_type = fields[1]
                .as_str()
                .ok_or_else(|| ServerError::Protocol("invalid UnboundRelationship type".into()))?
                .to_string();
            let properties = packstream_map_to_property_map(&fields[2])?;
            Ok(CypherValue::Edge {
                id: traverse_core::graph::types::EdgeId(id as u64),
                edge_type,
                source: traverse_core::graph::types::NodeId(0),
                target: traverse_core::graph::types::NodeId(0),
                properties,
            })
        }
        _ => Err(ServerError::Protocol(
            "expected UnboundRelationship structure".into(),
        )),
    }
}

/// PackStream Value::Map -> PropertyMap.
fn packstream_map_to_property_map(v: &Value) -> Result<PropertyMap> {
    match v {
        Value::Map(entries) => {
            let mut map = PropertyMap::new();
            for (k, val) in entries {
                map.insert(k.clone(), packstream_to_property_value(val)?);
            }
            Ok(map)
        }
        _ => Ok(PropertyMap::new()),
    }
}

/// PackStream Value -> PropertyValue.
fn packstream_to_property_value(v: &Value) -> Result<PropertyValue> {
    match v {
        Value::Null => Ok(PropertyValue::Null),
        Value::Bool(b) => Ok(PropertyValue::Bool(*b)),
        Value::Integer(n) => Ok(PropertyValue::Int(*n)),
        Value::Float(f) => Ok(PropertyValue::Float(*f)),
        Value::String(s) => Ok(PropertyValue::String(s.clone())),
        Value::Bytes(b) => Ok(PropertyValue::Bytes(b.clone())),
        Value::List(items) => {
            let result: Result<Vec<PropertyValue>> =
                items.iter().map(packstream_to_property_value).collect();
            Ok(PropertyValue::List(result?))
        }
        Value::Map(entries) => {
            let mut map = BTreeMap::new();
            for (k, val) in entries {
                map.insert(k.clone(), packstream_to_property_value(val)?);
            }
            Ok(PropertyValue::Map(map))
        }
        Value::Structure { .. } => {
            // temporal/spatial structures in properties fall back to the CypherValue string form
            Ok(PropertyValue::String(format!("{v:?}")))
        }
    }
}

/// PackStream parameter map -> HashMap<String, CypherValue>.
pub fn packstream_params_to_cypher(
    params: &[(String, Value)],
) -> Result<HashMap<String, CypherValue>> {
    let mut map = HashMap::new();
    for (k, v) in params {
        map.insert(k.clone(), packstream_to_cypher_value(v)?);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use traverse_core::graph::types::{EdgeId, NodeId};

    #[test]
    fn scalar_roundtrip() {
        let values = vec![
            CypherValue::Null,
            CypherValue::Bool(true),
            CypherValue::Integer(42),
            CypherValue::Float(1.25),
            CypherValue::String("hello".into()),
        ];
        for cv in values {
            let ps = cypher_value_to_packstream(&cv, 5);
            let back = packstream_to_cypher_value(&ps).unwrap();
            assert_eq!(back, cv);
        }
    }

    #[test]
    fn list_roundtrip() {
        let cv = CypherValue::List(vec![
            CypherValue::Integer(1),
            CypherValue::String("two".into()),
        ]);
        let ps = cypher_value_to_packstream(&cv, 5);
        let back = packstream_to_cypher_value(&ps).unwrap();
        assert_eq!(back, cv);
    }

    #[test]
    fn node_encoding() {
        let cv = CypherValue::Node {
            id: NodeId(42),
            labels: vec!["Person".into()],
            properties: BTreeMap::from([("name".into(), PropertyValue::String("Alice".into()))]),
        };
        let ps = cypher_value_to_packstream(&cv, 5);
        match &ps {
            Value::Structure { tag, fields } => {
                assert_eq!(*tag, TAG_NODE);
                assert_eq!(fields.len(), 4); // id, labels, props, element_id
                assert_eq!(fields[3], Value::String("4:42".into()));
            }
            _ => panic!("expected structure"),
        }
    }

    #[test]
    fn edge_encoding() {
        let cv = CypherValue::Edge {
            id: EdgeId(7),
            edge_type: "KNOWS".into(),
            source: NodeId(1),
            target: NodeId(2),
            properties: BTreeMap::new(),
        };
        let ps = cypher_value_to_packstream(&cv, 5);
        match &ps {
            Value::Structure { tag, fields } => {
                assert_eq!(*tag, TAG_RELATIONSHIP);
                assert_eq!(fields.len(), 8); // id, start, end, type, props, element_id, start_element_id, end_element_id
            }
            _ => panic!("expected structure"),
        }
    }

    #[test]
    fn duration_roundtrip() {
        let cv = CypherValue::Duration(CypherDuration::from_nanos(2, 10, 3_500_000_000));
        let ps = cypher_value_to_packstream(&cv, 5);
        let back = packstream_to_cypher_value(&ps).unwrap();
        assert_eq!(back, cv);
    }

    #[test]
    fn vector_bolt5_as_list() {
        let pv = PropertyValue::Vector(vec![1.0, 2.0, 3.0]);
        let ps = property_value_to_packstream(&pv, 5);
        match &ps {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::Float(1.0));
            }
            _ => panic!("expected list for Bolt 5"),
        }
    }

    #[test]
    fn vector_bolt6_as_structure() {
        let pv = PropertyValue::Vector(vec![1.0, 2.0, 3.0]);
        let ps = property_value_to_packstream(&pv, 6);
        match &ps {
            Value::Structure { tag, fields } => {
                assert_eq!(*tag, TAG_VECTOR);
                assert_eq!(fields.len(), 1);
                if let Value::List(items) = &fields[0] {
                    assert_eq!(items.len(), 3);
                } else {
                    panic!("expected list inside Vector structure");
                }
            }
            _ => panic!("expected structure for Bolt 6"),
        }
    }

    #[test]
    fn node_roundtrip() {
        let cv = CypherValue::Node {
            id: NodeId(42),
            labels: vec!["Person".into()],
            properties: BTreeMap::from([("name".into(), PropertyValue::String("Alice".into()))]),
        };
        let ps = cypher_value_to_packstream(&cv, 5);
        let back = packstream_to_cypher_value(&ps).unwrap();
        match &back {
            CypherValue::Node {
                id,
                labels,
                properties,
            } => {
                assert_eq!(id.0, 42);
                assert_eq!(labels, &["Person"]);
                assert_eq!(
                    properties.get("name"),
                    Some(&PropertyValue::String("Alice".into()))
                );
            }
            _ => panic!("expected Node, got {back:?}"),
        }
    }

    #[test]
    fn edge_roundtrip() {
        let cv = CypherValue::Edge {
            id: EdgeId(7),
            edge_type: "KNOWS".into(),
            source: NodeId(1),
            target: NodeId(2),
            properties: BTreeMap::new(),
        };
        let ps = cypher_value_to_packstream(&cv, 5);
        let back = packstream_to_cypher_value(&ps).unwrap();
        match &back {
            CypherValue::Edge {
                id,
                edge_type,
                source,
                target,
                ..
            } => {
                assert_eq!(id.0, 7);
                assert_eq!(edge_type, "KNOWS");
                assert_eq!(source.0, 1);
                assert_eq!(target.0, 2);
            }
            _ => panic!("expected Edge, got {back:?}"),
        }
    }

    #[test]
    fn path_roundtrip() {
        let node_a = CypherValue::Node {
            id: NodeId(1),
            labels: vec!["A".into()],
            properties: BTreeMap::new(),
        };
        let node_b = CypherValue::Node {
            id: NodeId(2),
            labels: vec!["B".into()],
            properties: BTreeMap::new(),
        };
        let edge = CypherValue::Edge {
            id: EdgeId(10),
            edge_type: "KNOWS".into(),
            source: NodeId(1),
            target: NodeId(2),
            properties: BTreeMap::new(),
        };
        let cv = CypherValue::Path {
            nodes: vec![node_a, node_b],
            edges: vec![edge],
        };
        let ps = cypher_value_to_packstream(&cv, 5);
        let back = packstream_to_cypher_value(&ps).unwrap();
        match &back {
            CypherValue::Path { nodes, edges } => {
                assert_eq!(nodes.len(), 2);
                assert_eq!(edges.len(), 1);
            }
            _ => panic!("expected Path, got {back:?}"),
        }
    }

    #[test]
    fn decode_unsupported_type() {
        let v = Value::Structure {
            tag: 0x00, // TAG_UNSUPPORTED
            fields: vec![],
        };
        let cv = packstream_to_cypher_value(&v).unwrap();
        assert_eq!(cv, CypherValue::Null);
    }

    #[test]
    fn decode_vector_structure() {
        let v = Value::Structure {
            tag: TAG_VECTOR,
            fields: vec![Value::List(vec![
                Value::Float(1.0),
                Value::Float(2.0),
                Value::Float(3.0),
            ])],
        };
        let cv = packstream_to_cypher_value(&v).unwrap();
        match cv {
            CypherValue::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], CypherValue::Float(1.0));
            }
            _ => panic!("expected list"),
        }
    }
}
