//! `apoc.create.*` functions: virtual nodes and relationships, UUIDs.

use base64::Engine;
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::{key_string, next_virtual_id, node_id_of, str_arg};

/// The pure functions of this module; `None` for other names.
pub(super) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.create.uuid" => crate::functions::call_function("randomuuid", &[]),
        "apoc.create.uuidbase64" => match crate::functions::call_function("randomuuid", &[]) {
            Ok(V::String(u)) => Ok(V::String(uuid_hex_to_base64(&u).unwrap_or(u))),
            Ok(_) => Ok(V::Null),
            Err(e) => Err(e),
        },
        "apoc.create.uuidbase64tohex" => match args.first() {
            Some(V::String(b)) => Ok(uuid_base64_to_hex(b).map(V::String).unwrap_or(V::Null)),
            _ => Ok(V::Null),
        },
        "apoc.create.uuidhextobase64" => match args.first() {
            Some(V::String(h)) => Ok(uuid_hex_to_base64(h).map(V::String).unwrap_or(V::Null)),
            _ => Ok(V::Null),
        },
        // virtual entity constructors (function forms)
        "apoc.create.vnode" => {
            let labels = match args.first() {
                Some(V::List(l)) => l.iter().map(key_string).collect(),
                Some(V::String(s)) => vec![s.clone()],
                _ => Vec::new(),
            };
            let props = match args.get(1) {
                Some(V::Map(m)) => m
                    .iter()
                    .filter_map(|(k, v)| v.to_property().map(|p| (k.clone(), p)))
                    .collect(),
                _ => Default::default(),
            };
            Ok(V::Node {
                id: next_virtual_id(),
                labels,
                properties: props,
            })
        }
        "apoc.create.vrelationship" => {
            let (Some(from), Some(to)) = (args.first(), args.get(3)) else {
                return Some(Ok(V::Null));
            };
            let (Some(s), Some(t)) = (node_id_of(from), node_id_of(to)) else {
                return Some(Ok(V::Null));
            };
            let rtype = str_arg(args, 1).unwrap_or_else(|| "REL".to_string());
            let props = match args.get(2) {
                Some(V::Map(m)) => m
                    .iter()
                    .filter_map(|(k, v)| v.to_property().map(|p| (k.clone(), p)))
                    .collect(),
                _ => Default::default(),
            };
            Ok(V::Edge {
                id: traverse_core::graph::types::EdgeId(next_virtual_id().0),
                edge_type: rtype,
                source: s,
                target: t,
                properties: props,
            })
        }
        _ => return None,
    };
    Some(r)
}

/// The functions of this module that read the graph; `None` for other names.
pub(super) fn eval_graph(
    name: &str,
    args: &[CypherValue],
    graph: &dyn GraphView,
) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.create.virtual.fromnode" => {
            let Some(id) = args.first().and_then(node_id_of) else {
                return Some(Ok(V::Null));
            };
            let keep: Vec<String> = match args.get(1) {
                Some(V::List(l)) => l.iter().map(key_string).collect(),
                _ => Vec::new(),
            };
            let Ok(node) = graph.get_node(id) else {
                return Some(Ok(V::Null));
            };
            let labels: Vec<String> = node
                .labels
                .iter()
                .map(|l| graph.strings().resolve(l.0).to_string())
                .collect();
            let props = node
                .properties
                .iter()
                .filter(|(k, _)| keep.contains(&graph.strings().resolve(k.0).to_string()))
                .map(|(k, v)| (graph.strings().resolve(k.0).to_string(), v.clone()))
                .collect();
            Ok(V::Node {
                id: next_virtual_id(),
                labels,
                properties: props,
            })
        }
        _ => return None,
    };
    Some(r)
}

fn uuid_hex_to_base64(hex: &str) -> Option<String> {
    let clean: String = hex.chars().filter(|c| *c != '-').collect();
    if clean.len() != 32 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..16)
        .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).ok())
        .collect();
    Some(base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes?))
}

fn uuid_base64_to_hex(b64: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(b64.trim_end_matches('='))
        .ok()?;
    if bytes.len() != 16 {
        return None;
    }
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}
