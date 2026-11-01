//! Conversions and types: `apoc.convert.*`, `apoc.json.*`, `apoc.data.*`, `apoc.xml.*` and the
//! `apoc.meta.*` type functions, plus the JSON and XML codecs the procedures share.

use std::collections::BTreeMap;

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::{as_map, dedup_keep_order, key_string, node_id_of, rel_id_of, str_arg, with_str};

/// The pure functions of this module; `None` for other names.
pub(super) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.convert.fromjsonmap" => convert_from_json(args, true),
        "apoc.convert.fromjsonlist" => convert_from_json(args, false),
        "apoc.convert.tolist" => convert_to_list(args),
        "apoc.convert.toset" => match convert_to_list(args) {
            Ok(V::List(l)) => Ok(V::List(dedup_keep_order(&l))),
            other => other,
        },
        "apoc.convert.tostring" => Ok(match args.first() {
            None | Some(V::Null) => V::Null,
            Some(v) => V::String(v.to_string()),
        }),
        "apoc.convert.toboolean" => convert_to_boolean(args),
        "apoc.convert.tointeger" => convert_to_integer(args),
        "apoc.convert.tofloat" => convert_to_float(args),
        "apoc.xml.parse" => {
            let Some(CypherValue::String(data)) = args.first() else {
                return Some(Ok(V::Null));
            };
            // args: (data, path = '/', config = {}, simple = false)
            if let Some(V::String(p)) = args.get(1)
                && !p.is_empty()
                && p != "/"
            {
                return Some(Err(CypherError::semantic(
                    "apoc.xml.parse: only the root path '/' is supported".to_string(),
                )));
            }
            let simple = matches!(args.get(3), Some(V::Bool(true)));
            xml_parse_to_map(data, simple)
        }
        "apoc.data.url" => data_url(args),
        "apoc.json.path" => json_path_fn(args),
        "apoc.meta.cypher.type" => Ok(V::String(meta_type_name(args.first().unwrap_or(&V::Null)))),
        "apoc.meta.cypher.istype" => {
            let t = str_arg(args, 1).unwrap_or_default();
            Ok(V::Bool(
                meta_type_name(args.first().unwrap_or(&V::Null)).eq_ignore_ascii_case(&t),
            ))
        }
        "apoc.meta.cypher.types" => {
            let Some(m) = as_map(args.first()) else {
                return Some(Ok(V::Null));
            };
            Ok(V::Map(
                m.iter()
                    .map(|(k, v)| (k.clone(), V::String(meta_type_name(v))))
                    .collect(),
            ))
        }
        "apoc.convert.tomap" => Ok(match args.first() {
            Some(V::Map(m)) => V::Map(m.clone()),
            _ => V::Null,
        }),
        "apoc.convert.tosortedjsonmap" => {
            // BTreeMap is sorted already: deterministic output
            let ignore_case = !matches!(args.get(1), Some(V::Bool(false)));
            match args.first() {
                Some(V::Map(m)) => {
                    let mut entries: Vec<(&String, &V)> = m.iter().collect();
                    if ignore_case {
                        entries.sort_by_key(|(k, _)| k.to_lowercase());
                    }
                    let obj: serde_json::Map<String, serde_json::Value> = entries
                        .into_iter()
                        .map(|(k, v)| (k.clone(), pure_to_json(v)))
                        .collect();
                    Ok(V::String(
                        serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or_default(),
                    ))
                }
                _ => Ok(V::Null),
            }
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
        "apoc.convert.tojson" => Ok(V::String(
            serde_json::to_string(&to_json(args.first().unwrap_or(&V::Null), graph))
                .unwrap_or_else(|_| "null".to_string()),
        )),
        "apoc.convert.tonode" => Ok(match args.first().and_then(node_id_of) {
            Some(id) => V::NodeRef(id),
            None => V::Null,
        }),
        "apoc.convert.torelationship" => match rel_id_of(args.first()) {
            Some(id) => match graph.get_edge(id) {
                Ok(e) => Ok(V::EdgeRef {
                    id,
                    edge_type_id: e.edge_type,
                    source: e.source,
                    target: e.target,
                }),
                Err(_) => Ok(V::Null),
            },
            None => Ok(V::Null),
        },
        "apoc.convert.tonodelist" | "apoc.convert.torelationshiplist" => match args.first() {
            Some(V::List(l)) => Ok(V::List(l.clone())),
            Some(V::Null) | None => Ok(V::Null),
            Some(other) => Ok(V::List(vec![other.clone()])),
        },
        "apoc.convert.getjsonproperty" | "apoc.convert.getjsonpropertymap" => {
            let (Some(id), Some(key)) = (args.first().and_then(node_id_of), str_arg(args, 1))
            else {
                return Some(Ok(V::Null));
            };
            let Ok(Some(pv)) = graph.get_node_property(id, &key) else {
                return Some(Ok(V::Null));
            };
            let traverse_core::graph::property::PropertyValue::String(s) = pv else {
                return Some(Ok(V::Null));
            };
            let path = str_arg(args, 2).unwrap_or_default();
            match serde_json::from_str::<serde_json::Value>(&s) {
                Ok(j) => {
                    let v = from_json(&j);
                    Ok(if path.is_empty() {
                        v
                    } else {
                        json_path_eval(&v, &path)
                    })
                }
                Err(e) => Err(CypherError::semantic(format!("invalid JSON: {e}"))),
            }
        }
        "apoc.meta.nodes.count" => {
            let labels: Vec<String> = match args.first() {
                Some(V::List(l)) if !l.is_empty() => l.iter().map(key_string).collect(),
                _ => graph.label_names(),
            };
            let mut seen: std::collections::HashSet<u64> = Default::default();
            for l in labels {
                for nid in graph.nodes_by_label(&l) {
                    seen.insert(nid.0);
                }
            }
            Ok(V::Integer(seen.len() as i64))
        }
        _ => return None,
    };
    Some(r)
}

#[recursive::recursive]
fn to_json(v: &CypherValue, graph: &dyn GraphView) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        CypherValue::Null => J::Null,
        CypherValue::Bool(b) => J::Bool(*b),
        CypherValue::Integer(i) => J::from(*i),
        CypherValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(J::Number)
            .unwrap_or(J::Null),
        CypherValue::String(s) => J::String(s.clone()),
        CypherValue::List(l) => J::Array(l.iter().map(|x| to_json(x, graph)).collect()),
        CypherValue::Map(m) => J::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), to_json(x, graph)))
                .collect(),
        ),
        CypherValue::NodeRef(id) => match graph.get_node(*id) {
            Ok(n) => to_json(&crate::executor::node_to_cypher_value(&n, graph), graph),
            Err(_) => J::Null,
        },
        CypherValue::Bytes(b) => {
            use base64::Engine;
            J::String(base64::engine::general_purpose::STANDARD.encode(b))
        }
        other => J::String(other.to_string()),
    }
}

/// JSON -> CypherValue; the apoc.load.json* procedures use it too.
pub(crate) fn json_to_cypher(j: &serde_json::Value) -> CypherValue {
    from_json(j)
}

#[recursive::recursive]
fn from_json(j: &serde_json::Value) -> CypherValue {
    use serde_json::Value as J;
    match j {
        J::Null => CypherValue::Null,
        J::Bool(b) => CypherValue::Bool(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                CypherValue::Integer(i)
            } else {
                CypherValue::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        J::String(s) => CypherValue::String(s.clone()),
        J::Array(a) => CypherValue::List(a.iter().map(from_json).collect()),
        J::Object(o) => {
            CypherValue::Map(o.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
        }
    }
}

fn convert_from_json(args: &[CypherValue], map: bool) -> Result<CypherValue, CypherError> {
    match args.first() {
        Some(CypherValue::String(s)) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(j) => {
                let v = from_json(&j);
                match (&v, map) {
                    (CypherValue::Map(_), true) | (CypherValue::List(_), false) => Ok(v),
                    _ => Err(CypherError::type_error(format!(
                        "JSON is not a {}",
                        if map { "map" } else { "list" }
                    ))),
                }
            }
            Err(e) => Err(CypherError::semantic(format!("invalid JSON: {e}"))),
        },
        Some(CypherValue::Null) | None => Ok(CypherValue::Null),
        Some(other) => Err(CypherError::type_error(format!(
            "expected a STRING of JSON, got {}",
            other.type_name()
        ))),
    }
}

fn convert_to_list(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    Ok(match args.first() {
        None | Some(CypherValue::Null) => CypherValue::List(Vec::new()),
        Some(CypherValue::List(l)) => CypherValue::List(l.clone()),
        Some(CypherValue::Map(m)) => CypherValue::List(m.values().cloned().collect()),
        Some(other) => CypherValue::List(vec![other.clone()]),
    })
}

fn convert_to_boolean(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    Ok(match args.first() {
        Some(CypherValue::Bool(b)) => CypherValue::Bool(*b),
        Some(CypherValue::Integer(i)) => CypherValue::Bool(*i != 0),
        Some(CypherValue::Float(f)) => CypherValue::Bool(*f != 0.0),
        Some(CypherValue::String(s)) => CypherValue::Bool(s.eq_ignore_ascii_case("true")),
        _ => CypherValue::Null,
    })
}

fn convert_to_integer(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    Ok(match args.first() {
        Some(CypherValue::Integer(i)) => CypherValue::Integer(*i),
        Some(CypherValue::Float(f)) => CypherValue::Integer(*f as i64),
        Some(CypherValue::Bool(b)) => CypherValue::Integer(i64::from(*b)),
        Some(CypherValue::String(s)) => s
            .trim()
            .parse::<i64>()
            .ok()
            .or_else(|| s.trim().parse::<f64>().ok().map(|f| f as i64))
            .map(CypherValue::Integer)
            .unwrap_or(CypherValue::Null),
        _ => CypherValue::Null,
    })
}

fn convert_to_float(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    Ok(match args.first() {
        Some(CypherValue::Integer(i)) => CypherValue::Float(*i as f64),
        Some(CypherValue::Float(f)) => CypherValue::Float(*f),
        Some(CypherValue::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .map(CypherValue::Float)
            .unwrap_or(CypherValue::Null),
        _ => CypherValue::Null,
    })
}

fn data_url(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    with_str(args, |s| {
        let mut m = BTreeMap::new();
        let put = |m: &mut BTreeMap<String, CypherValue>, k: &str, v: Option<String>| {
            m.insert(
                k.to_string(),
                v.map(CypherValue::String).unwrap_or(CypherValue::Null),
            );
        };
        // protocol://user@host:port/path?query
        let (proto, rest) = match s.split_once("://") {
            Some((p, r)) => (Some(p.to_string()), r),
            None => (None, s),
        };
        let (authority, path_q) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (None, authority),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                (Some(h.to_string()), p.parse::<i64>().ok())
            }
            _ => ((!hostport.is_empty()).then(|| hostport.to_string()), None),
        };
        let (path_q, anchor) = match path_q.split_once('#') {
            Some((p, a)) => (p, Some(a.to_string())),
            None => (path_q, None),
        };
        let (path, query) = match path_q.split_once('?') {
            Some((p, q)) => ((!p.is_empty()).then(|| p.to_string()), Some(q.to_string())),
            None => ((!path_q.is_empty()).then(|| path_q.to_string()), None),
        };
        let file = path.clone().map(|p| match query.clone() {
            Some(q) => format!("{p}?{q}"),
            None => p,
        });
        put(&mut m, "protocol", proto);
        put(&mut m, "user", user);
        put(&mut m, "host", host);
        m.insert(
            "port".to_string(),
            port.map(CypherValue::Integer).unwrap_or(CypherValue::Null),
        );
        put(&mut m, "path", path);
        put(&mut m, "query", query);
        put(&mut m, "file", file);
        put(&mut m, "anchor", anchor);
        CypherValue::Map(m)
    })
}

/// JSONPath subset for apoc.json.path / convert.getJsonProperty: `$`, `.name`, `[index]`; Null on a
/// miss.
fn json_path_eval(v: &CypherValue, path: &str) -> CypherValue {
    let mut cur = v.clone();
    let p = path.trim().trim_start_matches('$');
    let mut rest = p;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            let key = &r[..end];
            cur = match &cur {
                CypherValue::Map(m) => m.get(key).cloned().unwrap_or(CypherValue::Null),
                _ => CypherValue::Null,
            };
            rest = &r[end..];
        } else if let Some(r) = rest.strip_prefix('[') {
            let Some(close) = r.find(']') else {
                return CypherValue::Null;
            };
            let idx: usize = match r[..close].trim().parse() {
                Ok(i) => i,
                Err(_) => return CypherValue::Null,
            };
            cur = match &cur {
                CypherValue::List(l) => l.get(idx).cloned().unwrap_or(CypherValue::Null),
                _ => CypherValue::Null,
            };
            rest = &r[close + 1..];
        } else {
            return CypherValue::Null;
        }
    }
    cur
}

fn json_path_fn(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(CypherValue::String(json)) = args.first() else {
        return Ok(CypherValue::Null);
    };
    let path = str_arg(args, 1).unwrap_or_else(|| "$".to_string());
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(j) => Ok(json_path_eval(&from_json(&j), &path)),
        Err(e) => Err(CypherError::semantic(format!("invalid JSON: {e}"))),
    }
}

/// Neo4j-style type name for apoc.meta.cypher.type.
fn meta_type_name(v: &CypherValue) -> String {
    match v {
        CypherValue::Null => "NULL".to_string(),
        CypherValue::Bool(_) => "BOOLEAN".to_string(),
        CypherValue::Integer(_) => "INTEGER".to_string(),
        CypherValue::Float(_) => "FLOAT".to_string(),
        CypherValue::String(_) => "STRING".to_string(),
        CypherValue::List(_) => "LIST OF ANY".to_string(),
        CypherValue::Map(_) => "MAP".to_string(),
        CypherValue::Node { .. } | CypherValue::NodeRef(_) => "NODE".to_string(),
        CypherValue::Edge { .. } | CypherValue::EdgeRef { .. } => "RELATIONSHIP".to_string(),
        CypherValue::Path { .. } => "PATH".to_string(),
        other => other.type_name().to_string(),
    }
}

/// JSON for PURE values (no graph: entities render as strings; the graph-aware toJson handles
/// refs).
#[recursive::recursive]
fn pure_to_json(v: &CypherValue) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        CypherValue::Null => J::Null,
        CypherValue::Bool(b) => J::Bool(*b),
        CypherValue::Integer(i) => J::from(*i),
        CypherValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(J::Number)
            .unwrap_or(J::Null),
        CypherValue::String(s) => J::String(s.clone()),
        CypherValue::List(l) => J::Array(l.iter().map(pure_to_json).collect()),
        CypherValue::Map(m) => J::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), pure_to_json(x)))
                .collect(),
        ),
        CypherValue::Bytes(b) => {
            use base64::Engine;
            J::String(base64::engine::general_purpose::STANDARD.encode(b))
        }
        other => J::String(other.to_string()),
    }
}

/// Pure JSON encoder (no graph access).
pub(crate) fn cypher_to_json_pure(v: &CypherValue) -> serde_json::Value {
    pure_to_json(v)
}

/// XML -> APOC's map shape: `_type` = element name, attributes as keys, child elements in
/// `_children` (document order; mixed-content text chunks as strings), text-only elements get
/// `_text`. Simple mode groups children under `_<tag>` keys (always lists) instead of `_children`.
fn xml_parse_to_map(data: &str, simple: bool) -> Result<CypherValue, CypherError> {
    use crate::xml::XmlEvent;
    struct El {
        map: std::collections::BTreeMap<String, CypherValue>,
        children: Vec<CypherValue>,
        text: String,
        has_element_children: bool,
    }
    fn finish(mut el: El, simple: bool) -> CypherValue {
        if el.has_element_children {
            if simple {
                let mut grouped: std::collections::BTreeMap<String, Vec<CypherValue>> =
                    Default::default();
                for c in el.children {
                    let tag = match &c {
                        CypherValue::Map(m) => match m.get("_type") {
                            Some(CypherValue::String(t)) => t.clone(),
                            _ => continue,
                        },
                        _ => continue, // simple mode drops text chunks
                    };
                    grouped.entry(format!("_{tag}")).or_default().push(c);
                }
                for (k, v) in grouped {
                    el.map.insert(k, CypherValue::List(v));
                }
            } else {
                el.map
                    .insert("_children".to_string(), CypherValue::List(el.children));
            }
            let t = el.text.trim();
            if !t.is_empty() {
                el.map
                    .insert("_text".to_string(), CypherValue::String(t.to_string()));
            }
        } else {
            let t = el.text.trim();
            if !t.is_empty() {
                el.map
                    .insert("_text".to_string(), CypherValue::String(t.to_string()));
            }
        }
        CypherValue::Map(el.map)
    }
    let mut events = crate::xml::XmlEvents::new(data, false);
    let mut stack: Vec<El> = Vec::new();
    let mut root: Option<CypherValue> = None;
    loop {
        match events.next() {
            Ok(ev @ (XmlEvent::Start(_) | XmlEvent::Empty(_))) => {
                let is_empty = matches!(ev, XmlEvent::Empty(_));
                let (XmlEvent::Start(e) | XmlEvent::Empty(e)) = ev else {
                    unreachable!()
                };
                let name = e.name().into_inner().to_string();
                let mut map = std::collections::BTreeMap::new();
                map.insert("_type".to_string(), CypherValue::String(name));
                for a in e.attributes().flatten() {
                    map.insert(
                        a.key.into_inner().to_string(),
                        CypherValue::String(a.value.to_string()),
                    );
                }
                let el = El {
                    map,
                    children: Vec::new(),
                    text: String::new(),
                    has_element_children: false,
                };
                if is_empty {
                    // self-closing: no End event follows
                    let v = finish(el, simple);
                    match stack.last_mut() {
                        Some(parent) => {
                            parent.has_element_children = true;
                            parent.children.push(v);
                        }
                        None => root = Some(v),
                    }
                } else {
                    stack.push(el);
                }
            }
            Ok(XmlEvent::Text(text)) => {
                if let Some(top) = stack.last_mut() {
                    if top.has_element_children && !text.trim().is_empty() {
                        // mixed content: keep the chunk in document order
                        top.children
                            .push(CypherValue::String(text.trim().to_string()));
                    }
                    top.text.push_str(&text);
                }
            }
            Ok(XmlEvent::End(_)) => {
                let Some(el) = stack.pop() else { continue };
                let v = finish(el, simple);
                match stack.last_mut() {
                    Some(parent) => {
                        parent.has_element_children = true;
                        parent.children.push(v);
                    }
                    None => root = Some(v),
                }
            }
            Ok(XmlEvent::Eof) => break,
            Err(e) => {
                return Err(CypherError::runtime(format!(
                    "apoc.xml.parse: XML parse error: {e}"
                )));
            }
        }
    }
    root.ok_or_else(|| CypherError::runtime("apoc.xml.parse: empty document".to_string()))
}

/// Shared with the apoc.load.xml procedure.
pub(crate) fn xml_to_map(data: &str, simple: bool) -> Result<CypherValue, CypherError> {
    xml_parse_to_map(data, simple)
}
