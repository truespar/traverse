//! `apoc.search.*` and `apoc.spatial.*`: multi-label property search, geocoding, distance sort.

use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::operators::call::ProcCall;
use crate::types::CypherValue;

use super::node_ref_of;

/// `apoc.search.*` and `apoc.spatial.*`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall { frames, symtab, .. } = *cx;
    let name_lower = &cx.name_lower;
    match name_lower.as_str() {
        "apoc.search.node"
        | "apoc.search.nodeall"
        | "apoc.search.nodereduced"
        | "apoc.search.nodeallreduced"
        | "apoc.search.multisearchreduced" => {
            let reduced = name_lower.contains("reduced");
            let require_all = name_lower.contains("nodeall");
            cx.emit_rows(&|frame| {
                let spec = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                let op = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s.to_lowercase(),
                    _ => "exact".to_string(),
                };
                let value = eval_expr(cx.arg(2)?, frame, &*graph, symtab)?;
                let CypherValue::Map(spec) = spec else {
                    return Ok(Vec::new());
                };
                let matches_op = |pv: &CypherValue| -> bool {
                    match (&op[..], pv, &value) {
                        ("exact" | "=", a, b) => a == b,
                        ("contains", CypherValue::String(a), CypherValue::String(b)) => {
                            a.contains(b.as_str())
                        }
                        ("starts with", CypherValue::String(a), CypherValue::String(b)) => {
                            a.starts_with(b.as_str())
                        }
                        ("ends with", CypherValue::String(a), CypherValue::String(b)) => {
                            a.ends_with(b.as_str())
                        }
                        ("<", a, b) => {
                            crate::executor::compare_values(a, b) == std::cmp::Ordering::Less
                        }
                        (">", a, b) => {
                            crate::executor::compare_values(a, b) == std::cmp::Ordering::Greater
                        }
                        _ => false,
                    }
                };
                let mut rows = Vec::new();
                let mut seen: std::collections::HashSet<u64> = Default::default();
                for (label, props_v) in &spec {
                    let props: Vec<String> = match props_v {
                        CypherValue::List(l) => l
                            .iter()
                            .map(|v| match v {
                                CypherValue::String(s) => s.clone(),
                                o => o.to_string(),
                            })
                            .collect(),
                        CypherValue::String(s) => vec![s.clone()],
                        _ => continue,
                    };
                    for nid in graph.nodes_by_label(label) {
                        if seen.contains(&nid.0) {
                            continue;
                        }
                        let mut hits: Vec<(String, CypherValue)> = Vec::new();
                        for p in &props {
                            if let Ok(Some(pv)) = graph.get_node_property(nid, p) {
                                let cv = CypherValue::from_property(&pv);
                                if matches_op(&cv) {
                                    hits.push((p.clone(), cv));
                                }
                            }
                        }
                        let ok = if require_all {
                            hits.len() == props.len()
                        } else {
                            !hits.is_empty()
                        };
                        if !ok {
                            continue;
                        }
                        seen.insert(nid.0);
                        if reduced {
                            let labels: Vec<CypherValue> = graph
                                .get_node(nid)
                                .map(|n| {
                                    n.labels
                                        .iter()
                                        .map(|l| {
                                            CypherValue::String(
                                                graph.strings().resolve(l.0).to_string(),
                                            )
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let values: std::collections::BTreeMap<String, CypherValue> =
                                hits.into_iter().collect();
                            rows.push(vec![
                                ("id", CypherValue::Integer(nid.0 as i64)),
                                ("labels", CypherValue::List(labels)),
                                ("values", CypherValue::Map(values)),
                            ]);
                        } else {
                            rows.push(vec![("node", CypherValue::NodeRef(nid))]);
                        }
                    }
                }
                Ok(rows)
            })
        }
        "apoc.spatial.sortbydistance" => {
            let mut out = Vec::new();
            for frame in frames {
                let CypherValue::List(paths) = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?
                else {
                    continue;
                };
                let mut scored: Vec<(f64, CypherValue)> = paths
                    .into_iter()
                    .map(|p| (path_point_distance(&*graph, &p), p))
                    .collect();
                scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                for (d, p) in scored {
                    let mut nf = frame.clone();
                    if let Some(sl) = cx.slot_for("path") {
                        nf[sl] = p.into();
                    }
                    if let Some(sl) = cx.slot_for("distance") {
                        nf[sl] = CypherValue::Float(d).into();
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        // -- apoc.spatial.geocode / geocodeOnce / reverseGeocode --
        "apoc.spatial.geocode" | "apoc.spatial.geocodeonce" | "apoc.spatial.reversegeocode" => {
            let reverse = name_lower.ends_with("reversegeocode");
            let once = name_lower.ends_with("geocodeonce");
            let mut out = Vec::new();
            for frame in frames {
                let config_idx = if reverse {
                    3
                } else if once {
                    1
                } else {
                    3
                };
                let config = match cx.opt_arg(config_idx, frame, &*graph) {
                    Some(CypherValue::Map(m)) => m,
                    _ => Default::default(),
                };
                let url_template = match config.get(if reverse { "reverseUrl" } else { "url" }) {
                    Some(CypherValue::String(s)) => s.clone(),
                    // OSM Nominatim, APOC's default provider.
                    _ if reverse => {
                        "https://nominatim.openstreetmap.org/reverse?format=json&lat=LAT&lon=LNG"
                            .to_string()
                    }
                    _ => {
                        "https://nominatim.openstreetmap.org/search?format=json&q=PLACE".to_string()
                    }
                };
                let url = if reverse {
                    let lat = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                        CypherValue::Float(f) => f,
                        CypherValue::Integer(i) => i as f64,
                        _ => continue,
                    };
                    let lng = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                        CypherValue::Float(f) => f,
                        CypherValue::Integer(i) => i as f64,
                        _ => continue,
                    };
                    url_template
                        .replace("LAT", &lat.to_string())
                        .replace("LNG", &lng.to_string())
                } else {
                    let place = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                        CypherValue::String(s) => s,
                        _ => continue,
                    };
                    url_template.replace("PLACE", &urlencode(&place))
                };
                let max_results = if once || reverse {
                    1
                } else {
                    match cx.opt_arg(1, frame, &*graph) {
                        Some(CypherValue::Integer(n)) => n.max(1) as usize,
                        _ => 100,
                    }
                };
                let body = http_get_json(&url)?;
                let results: Vec<serde_json::Value> = match body {
                    serde_json::Value::Array(a) => a,
                    other => vec![other],
                };
                for item in results.into_iter().take(max_results) {
                    let lat = item.get("lat").and_then(json_number).unwrap_or(f64::NAN);
                    let lon = item.get("lon").and_then(json_number).unwrap_or(f64::NAN);
                    let desc = item
                        .get("display_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let mut loc = std::collections::BTreeMap::new();
                    loc.insert("latitude".to_string(), CypherValue::Float(lat));
                    loc.insert("longitude".to_string(), CypherValue::Float(lon));
                    loc.insert("description".to_string(), CypherValue::String(desc.clone()));
                    let mut nf = frame.clone();
                    for (col, val) in [
                        ("location", CypherValue::Map(loc)),
                        ("data", crate::apoc::json_to_cypher(&item)),
                        ("latitude", CypherValue::Float(lat)),
                        ("longitude", CypherValue::Float(lon)),
                        ("description", CypherValue::String(desc)),
                    ] {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.into();
                        }
                    }
                    out.push(nf);
                }
            }
            Ok(out)
        }
        other => Err(super::unknown(other)),
    }
}

/// Total point-to-point distance along a path (spatial.sortByDistance): consecutive nodes'
/// `latitude`/`longitude` or `location` point properties; falls back to hop count.
fn path_point_distance(graph: &dyn GraphView, p: &CypherValue) -> f64 {
    use traverse_core::graph::property::PropertyValue;
    let CypherValue::Path { nodes, .. } = p else {
        return f64::MAX;
    };
    let coords: Vec<(f64, f64)> = nodes
        .iter()
        .filter_map(|n| {
            let id = node_ref_of(n)?;
            let node = graph.get_node(id).ok()?;
            for (kid, pv) in node.properties.iter() {
                let _ = kid;
                if let PropertyValue::Point2D { x, y, .. } = pv {
                    return Some((x, y));
                }
            }
            None
        })
        .collect();
    if coords.len() < 2 {
        return nodes.len() as f64;
    }
    coords
        .windows(2)
        .map(|w| {
            let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            (dx * dx + dy * dy).sqrt()
        })
        .sum()
}

/// Minimal percent-encoding for geocode queries.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// GETs a JSON document (geocoding providers), 10 s timeout.
#[cfg(not(target_arch = "wasm32"))]
fn http_get_json(url: &str) -> Result<serde_json::Value, CypherError> {
    ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .user_agent("traverse-apoc-compat")
        .build()
        .new_agent()
        .get(url)
        .call()
        .map_err(|e| CypherError::runtime(format!("apoc.spatial: {e}")))?
        .body_mut()
        .read_json()
        .map_err(|e| CypherError::runtime(format!("apoc.spatial: {e}")))
}

/// wasm32 has no outbound HTTP; geocoding needs a server.
#[cfg(target_arch = "wasm32")]
fn http_get_json(_url: &str) -> Result<serde_json::Value, CypherError> {
    Err(CypherError::runtime(
        "apoc.spatial: outbound HTTP is not available in WASM builds",
    ))
}

fn json_number(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}
