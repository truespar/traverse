//! `apoc.export.*` / `apoc.import.*`: file round-trips in APOC's four formats:
//! JSON (NDJSON), CSV, Cypher script and GraphML.
//!
//! Output is APOC-SHAPED (same structure, keys, conventions) but not
//! byte-identical; correctness is pinned by round-trip tests (export -> wipe ->
//! import -> identical graph). Local files only (LOAD CSV's trust model);
//! `{stream: true}` (or an empty file name) returns the payload in `data`
//! instead of writing.

use std::collections::BTreeMap;

use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{EdgeId, NodeId};
use traverse_core::tx::view::GraphView;

use crate::error::CypherError;
use crate::types::CypherValue;

/// Entities an export covers.
pub struct ExportSet {
    /// Node ids, in emission order.
    pub nodes: Vec<NodeId>,
    /// Edge ids, in emission order.
    pub edges: Vec<EdgeId>,
}

impl ExportSet {
    /// Everything in the graph.
    pub fn all(graph: &dyn GraphView) -> Self {
        let mut nodes = graph.nodes_vec();
        nodes.sort_unstable_by_key(|n| n.0);
        let mut edges: Vec<EdgeId> = graph
            .relationship_type_names()
            .iter()
            .flat_map(|t| graph.edges_by_type(t))
            .collect();
        edges.sort_unstable_by_key(|e| e.0);
        edges.dedup();
        Self { nodes, edges }
    }

    /// From explicit node/relationship value lists.
    pub fn from_values(node_vals: &[CypherValue], edge_vals: &[CypherValue]) -> Self {
        let nodes = node_vals
            .iter()
            .filter_map(|v| match v {
                CypherValue::NodeRef(id) | CypherValue::Node { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        let edges = edge_vals
            .iter()
            .filter_map(|v| match v {
                CypherValue::EdgeRef { id, .. } | CypherValue::Edge { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        Self { nodes, edges }
    }
}

/// Counters for the export/import summary row.
#[derive(Default)]
pub struct IoCounts {
    /// Nodes written / created.
    pub nodes: i64,
    /// Relationships written / created.
    pub relationships: i64,
    /// Properties written / created.
    pub properties: i64,
    /// Rows emitted (query exports / NDJSON lines).
    pub rows: i64,
}

fn node_parts(
    graph: &dyn GraphView,
    id: NodeId,
) -> Option<(Vec<String>, Vec<(String, PropertyValue)>)> {
    let n = graph.get_node(id).ok()?;
    let labels = n
        .labels
        .iter()
        .map(|l| graph.strings().resolve(l.0).to_string())
        .collect();
    let props = n
        .properties
        .iter()
        .map(|(k, v)| (graph.strings().resolve(k.0).to_string(), v.clone()))
        .collect();
    Some((labels, props))
}

fn edge_parts(
    graph: &dyn GraphView,
    id: EdgeId,
) -> Option<(String, NodeId, NodeId, Vec<(String, PropertyValue)>)> {
    let e = graph.get_edge(id).ok()?;
    let t = graph.strings().resolve(e.edge_type.0).to_string();
    let props = e
        .properties
        .iter()
        .map(|(k, v)| (graph.strings().resolve(k.0).to_string(), v.clone()))
        .collect();
    Some((t, e.source, e.target, props))
}

fn prop_to_json(v: &PropertyValue) -> serde_json::Value {
    crate::apoc::cypher_to_json_pure(&CypherValue::from_property(v))
}

// -- JSON (NDJSON, APOC node/relationship document shape) ------------

/// Serializes to APOC's NDJSON export format.
pub fn to_json(graph: &dyn GraphView, set: &ExportSet) -> (String, IoCounts) {
    let mut out = String::new();
    let mut c = IoCounts::default();
    for &id in &set.nodes {
        let Some((labels, props)) = node_parts(graph, id) else {
            continue;
        };
        let mut doc = serde_json::Map::new();
        doc.insert("type".into(), serde_json::Value::String("node".into()));
        doc.insert("id".into(), serde_json::Value::String(id.0.to_string()));
        if !labels.is_empty() {
            doc.insert(
                "labels".into(),
                serde_json::Value::Array(
                    labels
                        .iter()
                        .map(|l| serde_json::Value::String(l.clone()))
                        .collect(),
                ),
            );
        }
        if !props.is_empty() {
            c.properties += props.len() as i64;
            doc.insert(
                "properties".into(),
                serde_json::Value::Object(
                    props
                        .iter()
                        .map(|(k, v)| (k.clone(), prop_to_json(v)))
                        .collect(),
                ),
            );
        }
        out.push_str(&serde_json::Value::Object(doc).to_string());
        out.push('\n');
        c.nodes += 1;
        c.rows += 1;
    }
    for &id in &set.edges {
        let Some((t, src, dst, props)) = edge_parts(graph, id) else {
            continue;
        };
        let mut doc = serde_json::Map::new();
        doc.insert(
            "type".into(),
            serde_json::Value::String("relationship".into()),
        );
        doc.insert("id".into(), serde_json::Value::String(id.0.to_string()));
        doc.insert("label".into(), serde_json::Value::String(t));
        if !props.is_empty() {
            c.properties += props.len() as i64;
            doc.insert(
                "properties".into(),
                serde_json::Value::Object(
                    props
                        .iter()
                        .map(|(k, v)| (k.clone(), prop_to_json(v)))
                        .collect(),
                ),
            );
        }
        let endpoint = |nid: NodeId| {
            let mut m = serde_json::Map::new();
            m.insert("id".into(), serde_json::Value::String(nid.0.to_string()));
            if let Some((labels, _)) = node_parts(graph, nid)
                && !labels.is_empty()
            {
                m.insert(
                    "labels".into(),
                    serde_json::Value::Array(
                        labels.into_iter().map(serde_json::Value::String).collect(),
                    ),
                );
            }
            serde_json::Value::Object(m)
        };
        doc.insert("start".into(), endpoint(src));
        doc.insert("end".into(), endpoint(dst));
        out.push_str(&serde_json::Value::Object(doc).to_string());
        out.push('\n');
        c.relationships += 1;
        c.rows += 1;
    }
    (out, c)
}

/// Imports APOC NDJSON (from [`to_json`] or APOC itself).
pub fn import_json(graph: &mut dyn GraphView, text: &str) -> Result<IoCounts, CypherError> {
    let mut c = IoCounts::default();
    // exported id (string) -> created node id
    let mut id_map: BTreeMap<String, NodeId> = BTreeMap::new();
    let mut pending_rels: Vec<(
        String,
        String,
        String,
        traverse_core::graph::property::PropertyMap,
    )> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let doc: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| CypherError::runtime(format!("apoc.import.json: invalid JSON: {e}")))?;
        let ty = doc.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let props_of = |doc: &serde_json::Value| {
            let mut pm = traverse_core::graph::property::PropertyMap::new();
            if let Some(serde_json::Value::Object(o)) = doc.get("properties") {
                for (k, v) in o {
                    let cv = crate::apoc::json_to_cypher(v);
                    if let Some(pv) = cv.to_property() {
                        pm.insert(k.clone(), pv);
                    }
                }
            }
            pm
        };
        match ty {
            "node" => {
                let labels: Vec<String> = doc
                    .get("labels")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let props = props_of(&doc);
                c.properties += props.len() as i64;
                let nid = graph
                    .create_node(&label_refs, props)
                    .map_err(CypherError::from)?;
                crate::executor::inc_stat(|s| s.nodes_created += 1);
                if let Some(id) = doc.get("id").and_then(|v| v.as_str()) {
                    id_map.insert(id.to_string(), nid);
                }
                c.nodes += 1;
            }
            "relationship" => {
                let t = doc
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("REL")
                    .to_string();
                let start = doc
                    .pointer("/start/id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let end = doc
                    .pointer("/end/id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                pending_rels.push((start, end, t, props_of(&doc)));
            }
            other => {
                return Err(CypherError::runtime(format!(
                    "apoc.import.json: unknown document type `{other}`"
                )));
            }
        }
        c.rows += 1;
    }
    for (start, end, t, props) in pending_rels {
        let (Some(&s), Some(&e)) = (id_map.get(&start), id_map.get(&end)) else {
            return Err(CypherError::runtime(format!(
                "apoc.import.json: relationship references unknown node id {start} or {end}"
            )));
        };
        c.properties += props.len() as i64;
        graph
            .create_edge(s, e, &t, props)
            .map_err(CypherError::from)?;
        crate::executor::inc_stat(|s| s.relationships_created += 1);
        c.relationships += 1;
    }
    Ok(c)
}

// -- CSV (APOC csv.all single-file convention) ------------------------

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn prop_to_csv(v: &PropertyValue) -> String {
    match v {
        PropertyValue::String(s) => s.clone(),
        other => CypherValue::from_property(other).to_string(),
    }
}

/// APOC's `csv.all` single-file convention: `_id,_labels,<node props...>,_start,_end,_type,<rel
/// props...>`.
pub fn to_csv(graph: &dyn GraphView, set: &ExportSet) -> (String, IoCounts) {
    let mut c = IoCounts::default();
    // column discovery
    let mut node_props: Vec<String> = Vec::new();
    let mut rel_props: Vec<String> = Vec::new();
    for &id in &set.nodes {
        if let Some((_, props)) = node_parts(graph, id) {
            for (k, _) in props {
                if !node_props.contains(&k) {
                    node_props.push(k);
                }
            }
        }
    }
    for &id in &set.edges {
        if let Some((_, _, _, props)) = edge_parts(graph, id) {
            for (k, _) in props {
                if !rel_props.contains(&k) {
                    rel_props.push(k);
                }
            }
        }
    }
    node_props.sort();
    rel_props.sort();
    let mut out = String::new();
    out.push_str("_id,_labels");
    for p in &node_props {
        out.push(',');
        out.push_str(&csv_escape(p));
    }
    out.push_str(",_start,_end,_type");
    for p in &rel_props {
        out.push(',');
        out.push_str(&csv_escape(p));
    }
    out.push('\n');
    for &id in &set.nodes {
        let Some((labels, props)) = node_parts(graph, id) else {
            continue;
        };
        out.push_str(&id.0.to_string());
        out.push(',');
        out.push_str(&csv_escape(
            &labels.iter().map(|l| format!(":{l}")).collect::<String>(),
        ));
        for p in &node_props {
            out.push(',');
            if let Some((_, v)) = props.iter().find(|(k, _)| k == p) {
                out.push_str(&csv_escape(&prop_to_csv(v)));
                c.properties += 1;
            }
        }
        // empty rel columns
        out.push_str(",,,");
        for _ in &rel_props {
            out.push(',');
        }
        out.push('\n');
        c.nodes += 1;
        c.rows += 1;
    }
    for &id in &set.edges {
        let Some((t, src, dst, props)) = edge_parts(graph, id) else {
            continue;
        };
        out.push(',');
        out.push(',');
        for _ in &node_props {
            out.push(',');
        }
        out.push_str(&format!("{},{},{}", src.0, dst.0, csv_escape(&t)));
        for p in &rel_props {
            out.push(',');
            if let Some((_, v)) = props.iter().find(|(k, _)| k == p) {
                out.push_str(&csv_escape(&prop_to_csv(v)));
                c.properties += 1;
            }
        }
        out.push('\n');
        c.relationships += 1;
        c.rows += 1;
    }
    (out, c)
}

/// Query result as plain CSV.
pub fn result_to_csv(result: &crate::CypherResult) -> (String, IoCounts) {
    let mut out = String::new();
    let mut c = IoCounts::default();
    out.push_str(
        &result
            .columns
            .iter()
            .map(|s| csv_escape(s))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for row in &result.rows {
        out.push_str(
            &row.iter()
                .map(|v| match v {
                    CypherValue::Null => String::new(),
                    CypherValue::String(s) => csv_escape(s),
                    other => csv_escape(&other.to_string()),
                })
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
        c.rows += 1;
    }
    (out, c)
}

// -- Cypher script ----------------------------------------------------

fn cypher_literal(v: &PropertyValue) -> String {
    match v {
        PropertyValue::String(s) => {
            format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
        }
        PropertyValue::Bool(b) => b.to_string(),
        PropertyValue::Int(i) => i.to_string(),
        PropertyValue::Float(f) => format!("{f:?}"),
        PropertyValue::List(items) => format!(
            "[{}]",
            items
                .iter()
                .map(cypher_literal)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => format!(
            "'{}'",
            CypherValue::from_property(other)
                .to_string()
                .replace('\'', "\\'")
        ),
    }
}

/// Runnable Cypher script with APOC's import-id convention: nodes get `:_UNIQUE_IMPORT_LABEL
/// {_UNIQUE_IMPORT_ID}`, relationships MATCH on it, a cleanup statement removes it.
pub fn to_cypher(graph: &dyn GraphView, set: &ExportSet) -> (String, IoCounts) {
    let mut out = String::new();
    let mut c = IoCounts::default();
    for &id in &set.nodes {
        let Some((labels, props)) = node_parts(graph, id) else {
            continue;
        };
        let mut all = format!("`_UNIQUE_IMPORT_ID`: {}", id.0);
        for (k, v) in &props {
            all.push_str(&format!(", `{k}`: {}", cypher_literal(v)));
            c.properties += 1;
        }
        let label_str: String = labels.iter().map(|l| format!(":`{l}`")).collect();
        out.push_str(&format!(
            "CREATE ({label_str}:`_UNIQUE_IMPORT_LABEL` {{{all}}});\n"
        ));
        c.nodes += 1;
    }
    for &id in &set.edges {
        let Some((t, src, dst, props)) = edge_parts(graph, id) else {
            continue;
        };
        let prop_str = if props.is_empty() {
            String::new()
        } else {
            c.properties += props.len() as i64;
            format!(
                " {{{}}}",
                props
                    .iter()
                    .map(|(k, v)| format!("`{k}`: {}", cypher_literal(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        out.push_str(&format!(
            "MATCH (n1:`_UNIQUE_IMPORT_LABEL` {{`_UNIQUE_IMPORT_ID`: {}}}), \
             (n2:`_UNIQUE_IMPORT_LABEL` {{`_UNIQUE_IMPORT_ID`: {}}}) \
             CREATE (n1)-[:`{t}`{prop_str}]->(n2);\n",
            src.0, dst.0
        ));
        c.relationships += 1;
    }
    out.push_str(
        "MATCH (n:`_UNIQUE_IMPORT_LABEL`) REMOVE n:`_UNIQUE_IMPORT_LABEL` \
         REMOVE n.`_UNIQUE_IMPORT_ID`;\n",
    );
    c.rows = c.nodes + c.relationships;
    (out, c)
}

/// Schema-only export: index + constraint DDL.
pub fn schema_to_cypher(graph: &dyn GraphView) -> String {
    let strings = |id: u32| graph.strings().resolve(id).to_string();
    let mut out = String::new();
    for d in graph.index_catalog().descriptors() {
        let label = strings(d.label_id.0);
        let props: Vec<String> = strings(d.property_key.0)
            .split('\u{1F}')
            .map(|p| format!("n.`{p}`"))
            .collect();
        out.push_str(&format!(
            "CREATE INDEX `{}` FOR (n:`{label}`) ON ({});\n",
            d.name,
            props.join(", ")
        ));
    }
    for d in graph.constraint_registry().descriptors() {
        let label = strings(d.label_id.0);
        let mut props = vec![strings(d.property_key.0)];
        props.extend(d.extra_property_keys.iter().map(|k| strings(k.0)));
        let plist: Vec<String> = props.iter().map(|p| format!("n.`{p}`")).collect();
        use traverse_core::graph::constraint::ConstraintType as CT;
        let tail = match d.constraint_type {
            CT::Unique => "IS UNIQUE".to_string(),
            CT::NodeKey => "IS NODE KEY".to_string(),
            CT::Exists => "IS NOT NULL".to_string(),
            _ => continue,
        };
        out.push_str(&format!(
            "CREATE CONSTRAINT `{}` FOR (n:`{label}`) REQUIRE ({}) {tail};\n",
            d.name,
            plist.join(", ")
        ));
    }
    out
}

// -- GraphML ----------------------------------------------------------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// GraphML with APOC's keys: `labels` on nodes, `label` on edges.
pub fn to_graphml(graph: &dyn GraphView, set: &ExportSet) -> (String, IoCounts) {
    let mut c = IoCounts::default();
    let mut keys: Vec<(String, &'static str)> = Vec::new();
    for &id in &set.nodes {
        if let Some((_, props)) = node_parts(graph, id) {
            for (k, _) in props {
                if !keys.iter().any(|(n, f)| *n == k && *f == "node") {
                    keys.push((k, "node"));
                }
            }
        }
    }
    for &id in &set.edges {
        if let Some((_, _, _, props)) = edge_parts(graph, id) {
            for (k, _) in props {
                if !keys.iter().any(|(n, f)| *n == k && *f == "edge") {
                    keys.push((k, "edge"));
                }
            }
        }
    }
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">\n\
         <key id=\"labels\" for=\"node\" attr.name=\"labels\" attr.type=\"string\"/>\n\
         <key id=\"label\" for=\"edge\" attr.name=\"label\" attr.type=\"string\"/>\n",
    );
    for (k, f) in &keys {
        out.push_str(&format!(
            "<key id=\"{0}\" for=\"{1}\" attr.name=\"{0}\" attr.type=\"string\"/>\n",
            xml_escape(k),
            f
        ));
    }
    out.push_str("<graph id=\"G\" edgedefault=\"directed\">\n");
    for &id in &set.nodes {
        let Some((labels, props)) = node_parts(graph, id) else {
            continue;
        };
        out.push_str(&format!("<node id=\"n{}\">", id.0));
        out.push_str(&format!(
            "<data key=\"labels\">{}</data>",
            xml_escape(&labels.iter().map(|l| format!(":{l}")).collect::<String>())
        ));
        for (k, v) in &props {
            out.push_str(&format!(
                "<data key=\"{}\">{}</data>",
                xml_escape(k),
                xml_escape(&prop_to_csv(v))
            ));
            c.properties += 1;
        }
        out.push_str("</node>\n");
        c.nodes += 1;
    }
    for &id in &set.edges {
        let Some((t, src, dst, props)) = edge_parts(graph, id) else {
            continue;
        };
        out.push_str(&format!(
            "<edge id=\"e{}\" source=\"n{}\" target=\"n{}\">",
            id.0, src.0, dst.0
        ));
        out.push_str(&format!("<data key=\"label\">{}</data>", xml_escape(&t)));
        for (k, v) in &props {
            out.push_str(&format!(
                "<data key=\"{}\">{}</data>",
                xml_escape(k),
                xml_escape(&prop_to_csv(v))
            ));
            c.properties += 1;
        }
        out.push_str("</edge>\n");
        c.relationships += 1;
    }
    out.push_str("</graph>\n</graphml>\n");
    c.rows = c.nodes + c.relationships;
    (out, c)
}

/// Imports GraphML (from [`to_graphml`], APOC or Gephi-style exporters). Properties are strings;
/// `labels` / `label` keys map to node labels / edge type.
pub fn import_graphml(graph: &mut dyn GraphView, text: &str) -> Result<IoCounts, CypherError> {
    use crate::xml::XmlEvent;
    let mut events = crate::xml::XmlEvents::new(text, true);
    let mut c = IoCounts::default();
    let mut id_map: BTreeMap<String, NodeId> = BTreeMap::new();

    enum Cur {
        None,
        Node {
            xml_id: String,
            labels: Vec<String>,
            props: Vec<(String, String)>,
        },
        Edge {
            src: String,
            dst: String,
            etype: String,
            props: Vec<(String, String)>,
        },
    }
    let mut cur = Cur::None;
    let mut cur_key = String::new();
    loop {
        match events.next() {
            Ok(XmlEvent::Start(e)) | Ok(XmlEvent::Empty(e)) => {
                let name = e.name().into_inner().to_string();
                let attr = |key: &str| -> Option<String> {
                    e.attributes()
                        .flatten()
                        .find_map(|a| (a.key.into_inner() == key).then(|| a.value.to_string()))
                };
                match name.as_str() {
                    "node" => {
                        cur = Cur::Node {
                            xml_id: attr("id").unwrap_or_default(),
                            labels: Vec::new(),
                            props: Vec::new(),
                        };
                    }
                    "edge" => {
                        cur = Cur::Edge {
                            src: attr("source").unwrap_or_default(),
                            dst: attr("target").unwrap_or_default(),
                            etype: attr("label").unwrap_or_default(),
                            props: Vec::new(),
                        };
                    }
                    "data" => {
                        cur_key = attr("key").unwrap_or_default();
                    }
                    _ => {}
                }
            }
            Ok(XmlEvent::Text(text)) => match &mut cur {
                Cur::Node { labels, props, .. } => {
                    if cur_key == "labels" {
                        labels.extend(
                            text.split(':')
                                .filter(|s| !s.is_empty())
                                .map(str::to_string),
                        );
                    } else if !cur_key.is_empty() {
                        props.push((cur_key.clone(), text));
                    }
                }
                Cur::Edge { etype, props, .. } => {
                    if cur_key == "label" {
                        *etype = text;
                    } else if !cur_key.is_empty() {
                        props.push((cur_key.clone(), text));
                    }
                }
                Cur::None => {}
            },
            Ok(XmlEvent::End(e)) => {
                let name = e.name().into_inner().to_string();
                match (name.as_str(), std::mem::replace(&mut cur, Cur::None)) {
                    (
                        "node",
                        Cur::Node {
                            xml_id,
                            labels,
                            props,
                        },
                    ) => {
                        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                        let mut pm = traverse_core::graph::property::PropertyMap::new();
                        for (k, v) in props {
                            c.properties += 1;
                            pm.insert(k, PropertyValue::String(v));
                        }
                        let nid = graph
                            .create_node(&label_refs, pm)
                            .map_err(CypherError::from)?;
                        crate::executor::inc_stat(|s| s.nodes_created += 1);
                        id_map.insert(xml_id, nid);
                        c.nodes += 1;
                    }
                    (
                        "edge",
                        Cur::Edge {
                            src,
                            dst,
                            etype,
                            props,
                        },
                    ) => {
                        let (Some(&s), Some(&d)) = (id_map.get(&src), id_map.get(&dst)) else {
                            return Err(CypherError::runtime(format!(
                                "apoc.import.graphml: edge references unknown node {src} or {dst}"
                            )));
                        };
                        let mut pm = traverse_core::graph::property::PropertyMap::new();
                        for (k, v) in props {
                            c.properties += 1;
                            pm.insert(k, PropertyValue::String(v));
                        }
                        let t = if etype.is_empty() {
                            "RELATED".to_string()
                        } else {
                            etype
                        };
                        graph.create_edge(s, d, &t, pm).map_err(CypherError::from)?;
                        crate::executor::inc_stat(|s| s.relationships_created += 1);
                        c.relationships += 1;
                    }
                    (_, prev) => cur = prev,
                }
            }
            Ok(XmlEvent::Eof) => break,
            Err(e) => {
                return Err(CypherError::runtime(format!(
                    "apoc.import.graphml: XML parse error: {e}"
                )));
            }
        }
    }
    c.rows = c.nodes + c.relationships;
    Ok(c)
}

// -- Shared file / stream plumbing ------------------------------------

/// Writes or streams a payload; returns the (file, data) columns.
pub fn deliver(
    file: &str,
    stream: bool,
    payload: String,
) -> Result<(CypherValue, CypherValue), CypherError> {
    if stream || file.is_empty() {
        return Ok((CypherValue::Null, CypherValue::String(payload)));
    }
    if file.contains("://") && !file.starts_with("file://") {
        return Err(CypherError::semantic(
            "apoc.export.*: local files only (same policy as LOAD CSV)".to_string(),
        ));
    }
    let path = file.strip_prefix("file://").unwrap_or(file);
    std::fs::write(path, payload)
        .map_err(|e| CypherError::runtime(format!("apoc.export: cannot write '{path}': {e}")))?;
    Ok((CypherValue::String(file.to_string()), CypherValue::Null))
}

/// Reads a local file for the importers (same policy).
pub fn read_local(file: &str) -> Result<String, CypherError> {
    if file.contains("://") && !file.starts_with("file://") {
        return Err(CypherError::semantic(
            "apoc.import.*: local files only (same policy as LOAD CSV)".to_string(),
        ));
    }
    let path = file.strip_prefix("file://").unwrap_or(file);
    std::fs::read_to_string(path)
        .map_err(|e| CypherError::runtime(format!("apoc.import: cannot read '{path}': {e}")))
}

// -- Arrow IPC (apoc.export.arrow.* / apoc.load.arrow) ---------------

use std::sync::Arc;

/// Arrow type per column, inferred from the values.
#[derive(Clone, Copy, PartialEq)]
enum ArrowColType {
    Int,
    Float,
    Bool,
    Utf8,
}

fn infer_col_type(values: &[Option<PropertyValue>]) -> ArrowColType {
    let mut ty: Option<ArrowColType> = None;
    for v in values.iter().flatten() {
        let t = match v {
            PropertyValue::Int(_) => ArrowColType::Int,
            PropertyValue::Float(_) => ArrowColType::Float,
            PropertyValue::Bool(_) => ArrowColType::Bool,
            _ => ArrowColType::Utf8,
        };
        match ty {
            None => ty = Some(t),
            Some(prev) if prev == t => {}
            Some(ArrowColType::Int) if t == ArrowColType::Float => {
                ty = Some(ArrowColType::Float);
            }
            Some(ArrowColType::Float) if t == ArrowColType::Int => {}
            _ => return ArrowColType::Utf8,
        }
    }
    ty.unwrap_or(ArrowColType::Utf8)
}

fn prop_to_arrow_string(v: &PropertyValue) -> String {
    prop_to_csv(v)
}

fn build_arrow_column(
    name: &str,
    values: Vec<Option<PropertyValue>>,
) -> (arrow_schema::Field, Arc<dyn arrow_array::Array>) {
    use arrow_array::{ArrayRef, BooleanArray, Float64Array, Int64Array, StringArray};
    let ty = infer_col_type(&values);
    let (dt, arr): (arrow_schema::DataType, ArrayRef) = match ty {
        ArrowColType::Int => (
            arrow_schema::DataType::Int64,
            Arc::new(Int64Array::from(
                values
                    .iter()
                    .map(|v| match v {
                        Some(PropertyValue::Int(i)) => Some(*i),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )),
        ),
        ArrowColType::Float => (
            arrow_schema::DataType::Float64,
            Arc::new(Float64Array::from(
                values
                    .iter()
                    .map(|v| match v {
                        Some(PropertyValue::Float(f)) => Some(*f),
                        Some(PropertyValue::Int(i)) => Some(*i as f64),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )),
        ),
        ArrowColType::Bool => (
            arrow_schema::DataType::Boolean,
            Arc::new(BooleanArray::from(
                values
                    .iter()
                    .map(|v| match v {
                        Some(PropertyValue::Bool(b)) => Some(*b),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )),
        ),
        ArrowColType::Utf8 => (
            arrow_schema::DataType::Utf8,
            Arc::new(StringArray::from(
                values
                    .iter()
                    .map(|v| v.as_ref().map(prop_to_arrow_string))
                    .collect::<Vec<_>>(),
            )),
        ),
    };
    (arrow_schema::Field::new(name, dt, true), arr)
}

/// Export set as an Arrow IPC file: the `csv.all`-style table `<id>`, `labels`, `<source.id>`,
/// `<target.id>`, `<type>`, plus one column per property key.
pub fn to_arrow(
    graph: &dyn GraphView,
    set: &ExportSet,
) -> Result<(Vec<u8>, IoCounts), CypherError> {
    use arrow_array::{ArrayRef, Int64Array, StringArray};
    let mut c = IoCounts::default();
    let total = set.nodes.len() + set.edges.len();
    // fixed columns
    let mut ids: Vec<Option<i64>> = Vec::with_capacity(total);
    let mut labels_col: Vec<Option<String>> = Vec::with_capacity(total);
    let mut src_col: Vec<Option<i64>> = Vec::with_capacity(total);
    let mut dst_col: Vec<Option<i64>> = Vec::with_capacity(total);
    let mut type_col: Vec<Option<String>> = Vec::with_capacity(total);
    // property columns (discovered)
    let mut prop_names: Vec<String> = Vec::new();
    let mut prop_rows: Vec<Vec<(usize, PropertyValue)>> = Vec::with_capacity(total);

    let push_props = |props: &[(String, PropertyValue)],
                      prop_names: &mut Vec<String>|
     -> Vec<(usize, PropertyValue)> {
        props
            .iter()
            .map(|(k, v)| {
                let idx = match prop_names.iter().position(|n| n == k) {
                    Some(i) => i,
                    None => {
                        prop_names.push(k.clone());
                        prop_names.len() - 1
                    }
                };
                (idx, v.clone())
            })
            .collect()
    };
    for &id in &set.nodes {
        let Some((labels, props)) = node_parts(graph, id) else {
            continue;
        };
        ids.push(Some(id.0 as i64));
        labels_col.push(Some(labels.iter().map(|l| format!(":{l}")).collect()));
        src_col.push(None);
        dst_col.push(None);
        type_col.push(None);
        c.properties += props.len() as i64;
        prop_rows.push(push_props(&props, &mut prop_names));
        c.nodes += 1;
        c.rows += 1;
    }
    for &id in &set.edges {
        let Some((t, s, d, props)) = edge_parts(graph, id) else {
            continue;
        };
        ids.push(Some(id.0 as i64));
        labels_col.push(None);
        src_col.push(Some(s.0 as i64));
        dst_col.push(Some(d.0 as i64));
        type_col.push(Some(t));
        c.properties += props.len() as i64;
        prop_rows.push(push_props(&props, &mut prop_names));
        c.relationships += 1;
        c.rows += 1;
    }

    let mut fields = vec![
        arrow_schema::Field::new("<id>", arrow_schema::DataType::Int64, true),
        arrow_schema::Field::new("labels", arrow_schema::DataType::Utf8, true),
        arrow_schema::Field::new("<source.id>", arrow_schema::DataType::Int64, true),
        arrow_schema::Field::new("<target.id>", arrow_schema::DataType::Int64, true),
        arrow_schema::Field::new("<type>", arrow_schema::DataType::Utf8, true),
    ];
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(ids)),
        Arc::new(StringArray::from(labels_col)),
        Arc::new(Int64Array::from(src_col)),
        Arc::new(Int64Array::from(dst_col)),
        Arc::new(StringArray::from(type_col)),
    ];
    let row_count = prop_rows.len();
    for (pi, pname) in prop_names.iter().enumerate() {
        let mut vals: Vec<Option<PropertyValue>> = vec![None; row_count];
        for (ri, row) in prop_rows.iter().enumerate() {
            if let Some((_, v)) = row.iter().find(|(i, _)| *i == pi) {
                vals[ri] = Some(v.clone());
            }
        }
        let (field, arr) = build_arrow_column(pname, vals);
        fields.push(field);
        columns.push(arr);
    }

    let schema = Arc::new(arrow_schema::Schema::new(fields));
    let batch = arrow_array::RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| CypherError::runtime(format!("arrow: {e}")))?;
    let mut buf = Vec::new();
    {
        let mut writer = arrow_ipc::writer::FileWriter::try_new(&mut buf, &schema)
            .map_err(|e| CypherError::runtime(format!("arrow: {e}")))?;
        writer
            .write(&batch)
            .and_then(|()| writer.finish())
            .map_err(|e| CypherError::runtime(format!("arrow: {e}")))?;
    }
    Ok((buf, c))
}

/// Query result as an Arrow IPC file: one column per result column, entities/collections as JSON
/// strings.
pub fn result_to_arrow(result: &crate::CypherResult) -> Result<(Vec<u8>, IoCounts), CypherError> {
    let c = IoCounts {
        rows: result.rows.len() as i64,
        ..Default::default()
    };
    let mut fields = Vec::new();
    let mut columns: Vec<Arc<dyn arrow_array::Array>> = Vec::new();
    for (ci, name) in result.columns.iter().enumerate() {
        let vals: Vec<Option<PropertyValue>> = result
            .rows
            .iter()
            .map(|row| match row.get(ci) {
                Some(CypherValue::Null) | None => None,
                Some(v) => v.to_property().or_else(|| {
                    Some(PropertyValue::String(
                        serde_json::to_string(&crate::apoc::cypher_to_json_pure(v))
                            .unwrap_or_default(),
                    ))
                }),
            })
            .collect();
        let (field, arr) = build_arrow_column(name, vals);
        fields.push(field);
        columns.push(arr);
    }
    let schema = Arc::new(arrow_schema::Schema::new(fields));
    let batch = arrow_array::RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| CypherError::runtime(format!("arrow: {e}")))?;
    let mut buf = Vec::new();
    {
        let mut writer = arrow_ipc::writer::FileWriter::try_new(&mut buf, &schema)
            .map_err(|e| CypherError::runtime(format!("arrow: {e}")))?;
        writer
            .write(&batch)
            .and_then(|()| writer.finish())
            .map_err(|e| CypherError::runtime(format!("arrow: {e}")))?;
    }
    Ok((buf, c))
}

/// Arrow IPC file -> row maps (apoc.load.arrow, APOC extended).
pub fn arrow_to_rows(bytes: &[u8]) -> Result<Vec<CypherValue>, CypherError> {
    use arrow_array::Array;
    let reader = arrow_ipc::reader::FileReader::try_new(std::io::Cursor::new(bytes), None)
        .map_err(|e| CypherError::runtime(format!("arrow: {e}")))?;
    let schema = reader.schema();
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| CypherError::runtime(format!("arrow: {e}")))?;
        for ri in 0..batch.num_rows() {
            let mut m = BTreeMap::new();
            for (ci, field) in schema.fields().iter().enumerate() {
                let col = batch.column(ci);
                if col.is_null(ri) {
                    continue;
                }
                let v = if let Some(a) = col.as_any().downcast_ref::<arrow_array::Int64Array>() {
                    CypherValue::Integer(a.value(ri))
                } else if let Some(a) = col.as_any().downcast_ref::<arrow_array::Float64Array>() {
                    CypherValue::Float(a.value(ri))
                } else if let Some(a) = col.as_any().downcast_ref::<arrow_array::BooleanArray>() {
                    CypherValue::Bool(a.value(ri))
                } else if let Some(a) = col.as_any().downcast_ref::<arrow_array::StringArray>() {
                    CypherValue::String(a.value(ri).to_string())
                } else {
                    continue;
                };
                m.insert(field.name().clone(), v);
            }
            out.push(CypherValue::Map(m));
        }
    }
    Ok(out)
}

/// Binary [`deliver`] for Arrow payloads.
pub fn deliver_bytes(file: &str, payload: Vec<u8>) -> Result<CypherValue, CypherError> {
    if file.contains("://") && !file.starts_with("file://") {
        return Err(CypherError::semantic(
            "apoc.export.*: local files only (same policy as LOAD CSV)".to_string(),
        ));
    }
    let path = file.strip_prefix("file://").unwrap_or(file);
    std::fs::write(path, payload)
        .map_err(|e| CypherError::runtime(format!("apoc.export: cannot write '{path}': {e}")))?;
    Ok(CypherValue::String(file.to_string()))
}
