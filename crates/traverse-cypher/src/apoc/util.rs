//! `apoc.util.*`, `apoc.hashing.*`, `apoc.static.*` and `apoc.version`.

use md5::Digest;
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::{key_string, node_id_of, str_arg};

/// The pure functions of this module; `None` for other names.
pub(super) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.version" => Ok(V::String(format!("traverse-{}", env!("CARGO_PKG_VERSION")))),
        "apoc.util.md5" => util_hash(args, "md5"),
        "apoc.util.sha1" => util_hash(args, "sha1"),
        "apoc.util.sha256" => util_hash(args, "sha256"),
        "apoc.util.sha512" => util_hash(args, "sha512"),
        "apoc.util.validatepredicate" => util_validate_predicate(args),
        "apoc.util.sha384" => util_hash(args, "sha384"),
        "apoc.util.compress" => util_compress(args),
        "apoc.util.decompress" => util_decompress(args),
        "apoc.static.get" => {
            let Some(key) = str_arg(args, 0) else {
                return Some(Ok(V::Null));
            };
            Ok(static_store()
                .read()
                .ok()
                .and_then(|m| m.get(&key).cloned())
                .unwrap_or(V::Null))
        }
        "apoc.static.getall" => {
            let prefix = str_arg(args, 0).unwrap_or_default();
            let m = static_store()
                .read()
                .map(|m| {
                    m.iter()
                        .filter(|(k, _)| k.starts_with(&prefix))
                        .map(|(k, v)| {
                            let short = k
                                .strip_prefix(&prefix)
                                .unwrap_or(k)
                                .trim_start_matches('.')
                                .to_string();
                            (short, v.clone())
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(V::Map(m))
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
        "apoc.hashing.fingerprint" | "apoc.hashing.fingerprinting" => {
            let excluded: Vec<String> = match args.get(1) {
                Some(V::List(l)) => l.iter().map(key_string).collect(),
                _ => Vec::new(),
            };
            Ok(V::String(fingerprint_value(
                graph,
                args.first().unwrap_or(&V::Null),
                &excluded,
            )))
        }
        "apoc.hashing.fingerprintgraph" => {
            let excluded: Vec<String> = match args.first() {
                Some(V::List(l)) => l.iter().map(key_string).collect(),
                _ => Vec::new(),
            };
            use md5::Digest;
            let mut h = md5::Md5::new();
            let mut node_prints: Vec<String> = graph
                .nodes_vec()
                .into_iter()
                .map(|nid| fingerprint_value(graph, &V::NodeRef(nid), &excluded))
                .collect();
            node_prints.sort();
            for p in node_prints {
                h.update(p.as_bytes());
            }
            for t in graph.relationship_type_names() {
                let mut edge_prints: Vec<String> = graph
                    .edges_by_type(&t)
                    .into_iter()
                    .filter_map(|eid| {
                        let e = graph.get_edge(eid).ok()?;
                        Some(fingerprint_value(
                            graph,
                            &V::EdgeRef {
                                id: eid,
                                edge_type_id: e.edge_type,
                                source: e.source,
                                target: e.target,
                            },
                            &excluded,
                        ))
                    })
                    .collect();
                edge_prints.sort();
                for p in edge_prints {
                    h.update(p.as_bytes());
                }
            }
            Ok(V::String(hex_string(&h.finalize())))
        }
        _ => return None,
    };
    Some(r)
}

fn hash_input(args: &[CypherValue]) -> String {
    // APOC hashes the concatenated string forms of a LIST argument (or the single value)
    match args.first() {
        Some(CypherValue::List(l)) => l
            .iter()
            .map(|v| match v {
                CypherValue::String(s) => s.clone(),
                CypherValue::Null => String::new(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(CypherValue::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn util_hash(args: &[CypherValue], algo: &str) -> Result<CypherValue, CypherError> {
    let input = hash_input(args);
    let hex = match algo {
        "md5" => {
            let mut h = md5::Md5::new();
            h.update(input.as_bytes());
            hex_string(&h.finalize())
        }
        "sha1" => {
            let mut h = sha1::Sha1::new();
            h.update(input.as_bytes());
            hex_string(&h.finalize())
        }
        "sha256" => {
            let mut h = sha2::Sha256::new();
            h.update(input.as_bytes());
            hex_string(&h.finalize())
        }
        _ => {
            let mut h = sha2::Sha512::new();
            h.update(input.as_bytes());
            hex_string(&h.finalize())
        }
    };
    Ok(CypherValue::String(hex))
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn util_validate_predicate(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let fail = matches!(args.first(), Some(CypherValue::Bool(true)));
    if fail {
        let msg = str_arg(args, 1).unwrap_or_else(|| "validation failed".to_string());
        return Err(CypherError::semantic(msg));
    }
    Ok(CypherValue::Bool(true))
}

/// Deterministic MD5 fingerprint of a value / entity (props sorted, listed keys excluded),
/// apoc.hashing.fingerprint semantics with our own canonicalization (not APOC's exact bytes).
#[recursive::recursive]
fn fingerprint_value(graph: &dyn GraphView, v: &CypherValue, excluded: &[String]) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    match v {
        CypherValue::NodeRef(_) | CypherValue::Node { .. } => {
            if let Some(id) = node_id_of(v)
                && let Ok(n) = graph.get_node(id)
            {
                let mut labels: Vec<String> = n
                    .labels
                    .iter()
                    .map(|l| graph.strings().resolve(l.0).to_string())
                    .collect();
                labels.sort();
                for l in labels {
                    h.update(l.as_bytes());
                }
                let mut props: Vec<(String, String)> = n
                    .properties
                    .iter()
                    .map(|(k, pv)| {
                        (
                            graph.strings().resolve(k.0).to_string(),
                            CypherValue::from_property(&pv).to_string(),
                        )
                    })
                    .filter(|(k, _)| !excluded.contains(k))
                    .collect();
                props.sort();
                for (k, val) in props {
                    h.update(k.as_bytes());
                    h.update(val.as_bytes());
                }
            }
        }
        CypherValue::EdgeRef { id, .. } | CypherValue::Edge { id, .. } => {
            if let Ok(e) = graph.get_edge(*id) {
                h.update(graph.strings().resolve(e.edge_type.0).as_bytes());
                let mut props: Vec<(String, String)> = e
                    .properties
                    .iter()
                    .map(|(k, pv)| {
                        (
                            graph.strings().resolve(k.0).to_string(),
                            CypherValue::from_property(&pv).to_string(),
                        )
                    })
                    .filter(|(k, _)| !excluded.contains(k))
                    .collect();
                props.sort();
                for (k, val) in props {
                    h.update(k.as_bytes());
                    h.update(val.as_bytes());
                }
            }
        }
        CypherValue::Map(m) => {
            for (k, val) in m {
                if !excluded.contains(k) {
                    h.update(k.as_bytes());
                    h.update(fingerprint_value(graph, val, excluded).as_bytes());
                }
            }
        }
        other => h.update(other.to_string().as_bytes()),
    }
    hex_string(&h.finalize())
}

/// Process-global value store for `apoc.static.*` (APOC extended's server-lifetime storage):
/// survives across queries, not persisted.
pub(crate) fn static_store()
-> &'static std::sync::RwLock<std::collections::HashMap<String, CypherValue>> {
    use std::sync::OnceLock;
    static STORE: OnceLock<std::sync::RwLock<std::collections::HashMap<String, CypherValue>>> =
        OnceLock::new();
    STORE.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

fn compression_algo(args: &[CypherValue], idx: usize) -> String {
    match args.get(idx) {
        Some(CypherValue::Map(m)) => match m.get("compression") {
            Some(CypherValue::String(s)) => s.to_uppercase(),
            _ => "GZIP".to_string(),
        },
        _ => "GZIP".to_string(),
    }
}

fn util_compress(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    use std::io::Write;
    let Some(CypherValue::String(text)) = args.first() else {
        return Ok(CypherValue::Null);
    };
    let algo = compression_algo(args, 1);
    let bytes = match algo.as_str() {
        "NONE" => text.as_bytes().to_vec(),
        "DEFLATE" => {
            let mut enc =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(text.as_bytes())
                .and_then(|()| enc.finish())
                .map_err(|e| CypherError::runtime(format!("compress: {e}")))?
        }
        "GZIP" => {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(text.as_bytes())
                .and_then(|()| enc.finish())
                .map_err(|e| CypherError::runtime(format!("compress: {e}")))?
        }
        other => {
            return Err(CypherError::semantic(format!(
                "unsupported compression `{other}` (GZIP, DEFLATE, NONE)"
            )));
        }
    };
    Ok(CypherValue::Bytes(bytes))
}

fn util_decompress(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    use std::io::Read;
    let Some(CypherValue::Bytes(data)) = args.first() else {
        return Ok(CypherValue::Null);
    };
    let algo = compression_algo(args, 1);
    let mut out = String::new();
    match algo.as_str() {
        "NONE" => out = String::from_utf8_lossy(data).to_string(),
        "DEFLATE" => {
            flate2::read::DeflateDecoder::new(&data[..])
                .read_to_string(&mut out)
                .map_err(|e| CypherError::runtime(format!("decompress: {e}")))?;
        }
        "GZIP" => {
            flate2::read::GzDecoder::new(&data[..])
                .read_to_string(&mut out)
                .map_err(|e| CypherError::runtime(format!("decompress: {e}")))?;
        }
        other => {
            return Err(CypherError::semantic(format!(
                "unsupported compression `{other}` (GZIP, DEFLATE, NONE)"
            )));
        }
    }
    Ok(CypherValue::String(out))
}
