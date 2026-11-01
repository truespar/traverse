//! Cypher values <-> JSON for the HTTP API.

use std::collections::HashMap;

use serde_json::{Value, json};
use traverse_core::graph::property::PropertyValue;
use traverse_cypher::types::CypherValue;

/// CypherValue -> serde_json Value for HTTP responses.
pub(crate) fn cypher_value_to_json(cv: &CypherValue) -> Value {
    match cv {
        CypherValue::Null => Value::Null,
        CypherValue::Bool(b) => Value::Bool(*b),
        CypherValue::Integer(i) => json!(i),
        CypherValue::Float(f) => json!(f),
        CypherValue::String(s) => Value::String(s.clone()),
        CypherValue::List(items) => Value::Array(items.iter().map(cypher_value_to_json).collect()),
        CypherValue::Map(m) => {
            let obj: serde_json::Map<String, Value> = m
                .iter()
                .map(|(k, v)| (k.clone(), cypher_value_to_json(v)))
                .collect();
            Value::Object(obj)
        }
        CypherValue::Node {
            id,
            labels,
            properties,
        } => {
            json!({
                "_type": "node",
                "id": id.0,
                "labels": labels,
                "properties": property_map_to_json(properties),
            })
        }
        CypherValue::Edge {
            id,
            edge_type,
            source,
            target,
            properties,
        } => {
            json!({
                "_type": "edge",
                "id": id.0,
                "type": edge_type,
                "source": source.0,
                "target": target.0,
                "properties": property_map_to_json(properties),
            })
        }
        CypherValue::NodeRef(nid) => json!({ "_type": "node_ref", "id": nid.0 }),
        CypherValue::EdgeRef { id, .. } => json!({ "_type": "edge_ref", "id": id.0 }),
        CypherValue::Path { nodes, edges } => {
            json!({
                "_type": "path",
                "nodes": nodes.iter().map(cypher_value_to_json).collect::<Vec<_>>(),
                "edges": edges.iter().map(cypher_value_to_json).collect::<Vec<_>>(),
            })
        }
        // temporal types as their string form
        other => Value::String(other.to_string()),
    }
}

/// PropertyMap (BTreeMap<String, PropertyValue>) -> JSON.
fn property_map_to_json(props: &std::collections::BTreeMap<String, PropertyValue>) -> Value {
    let obj: serde_json::Map<String, Value> = props
        .iter()
        .map(|(k, v)| (k.clone(), property_value_to_json(v)))
        .collect();
    Value::Object(obj)
}

/// PropertyValue -> serde_json Value.
fn property_value_to_json(pv: &PropertyValue) -> Value {
    match pv {
        PropertyValue::Null => Value::Null,
        PropertyValue::Bool(b) => Value::Bool(*b),
        PropertyValue::Int(i) => json!(i),
        PropertyValue::Float(f) => json!(f),
        PropertyValue::String(s) => Value::String(s.clone()),
        PropertyValue::Bytes(b) => json!(b),
        PropertyValue::List(items) => {
            Value::Array(items.iter().map(property_value_to_json).collect())
        }
        PropertyValue::Map(m) => {
            let obj: serde_json::Map<String, Value> = m
                .iter()
                .map(|(k, v)| (k.clone(), property_value_to_json(v)))
                .collect();
            Value::Object(obj)
        }
        PropertyValue::DateTime(ms) => json!({ "datetime_ms": ms }),
        PropertyValue::Duration(ms) => json!({ "duration_ms": ms }),
        PropertyValue::Point2D { x, y, srid } => json!({ "x": x, "y": y, "srid": srid }),
        PropertyValue::Point3D { x, y, z, srid } => json!({ "x": x, "y": y, "z": z, "srid": srid }),
        PropertyValue::Vector(v) => json!(v),
    }
}

/// JSON parameters -> CypherValue parameters.
pub(crate) fn json_to_cypher_params(val: &Value) -> HashMap<String, CypherValue> {
    let mut params = HashMap::new();
    if let Value::Object(obj) = val {
        for (k, v) in obj {
            params.insert(k.clone(), json_to_cypher_value(v));
        }
    }
    params
}

fn json_to_cypher_value(val: &Value) -> CypherValue {
    match val {
        Value::Null => CypherValue::Null,
        Value::Bool(b) => CypherValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CypherValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                CypherValue::Float(f)
            } else {
                CypherValue::Null
            }
        }
        Value::String(s) => CypherValue::String(s.clone()),
        Value::Array(arr) => CypherValue::List(arr.iter().map(json_to_cypher_value).collect()),
        Value::Object(obj) => {
            let m: std::collections::BTreeMap<String, CypherValue> = obj
                .iter()
                .map(|(k, v)| (k.clone(), json_to_cypher_value(v)))
                .collect();
            CypherValue::Map(m)
        }
    }
}
