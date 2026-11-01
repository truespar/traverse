//! `apoc.export.*` and `apoc.import.*`: files in and out (CSV, JSON, GraphML, Cypher, Arrow, XML),
//! plus `apoc.load.arrow`.

use traverse_core::tx::view::GraphView;

use crate::apoc::key_string;
use crate::error::CypherError;
use crate::executor::eval_expr;
use crate::frame::Frame;
use crate::operators::call::{ProcCall, run_inner_cypher};
use crate::types::CypherValue;

/// `apoc.export.*`, `apoc.import.*` and `apoc.load.arrow`.
pub(super) fn call(cx: &ProcCall, graph: &mut dyn GraphView) -> Result<Vec<Frame>, CypherError> {
    let ProcCall {
        args,
        frames,
        symtab,
        ..
    } = *cx;
    match cx.name_lower.as_str() {
        // -- apoc.import.xml - XML document as a graph ---------
        "apoc.import.xml" => {
            let mut out = Vec::new();
            for frame in frames {
                let file = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                    CypherValue::String(s) => s,
                    _ => continue,
                };
                let config = match cx.opt_arg(1, frame, &*graph) {
                    Some(CypherValue::Map(m)) => m,
                    _ => Default::default(),
                };
                let word_rels = matches!(
                    config.get("createNextWordRelationships"),
                    Some(CypherValue::Bool(true))
                );
                let text = crate::apoc_export::read_local(&file)?;
                let root = import_xml_graph(graph, &text, &file, word_rels)?;
                let mut nf = frame.clone();
                if let Some(sl) = cx.slot_for("node") {
                    nf[sl] = CypherValue::NodeRef(root).into();
                }
                out.push(nf);
            }
            Ok(out)
        }
        // -- apoc.export.arrow.* / apoc.load.arrow ------------
        n if n.starts_with("apoc.export.arrow") || n == "apoc.load.arrow" => {
            let mut out = Vec::new();
            for frame in frames {
                let t0 = web_time::Instant::now();
                if n == "apoc.load.arrow" {
                    let file = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                        CypherValue::String(s) => s,
                        _ => continue,
                    };
                    if file.contains("://") && !file.starts_with("file://") {
                        return Err(CypherError::semantic(
                            "apoc.load.arrow: local files only".to_string(),
                        ));
                    }
                    let path = file.strip_prefix("file://").unwrap_or(&file);
                    let bytes = std::fs::read(path)
                        .map_err(|e| CypherError::runtime(format!("cannot open '{path}': {e}")))?;
                    for row in crate::apoc_export::arrow_to_rows(&bytes)? {
                        let mut nf = frame.clone();
                        if let Some(sl) = cx.slot_for("value") {
                            nf[sl] = row.into();
                        }
                        out.push(nf);
                    }
                    continue;
                }
                let stream = n.contains(".stream.");
                let fam = n.rsplit('.').next().unwrap_or_default();
                // File variants: file FIRST (unlike the other export
                // families); stream variants have no file argument.
                let data_arg = if stream { 0 } else { 1 };
                let (payload, counts) = match fam {
                    "all" => {
                        let set = crate::apoc_export::ExportSet::all(&*graph);
                        crate::apoc_export::to_arrow(&*graph, &set)?
                    }
                    "graph" => {
                        let g = eval_expr(cx.arg(data_arg)?, frame, &*graph, symtab)?;
                        let (ns, rs) = match g {
                            CypherValue::Map(m) => (
                                match m.get("nodes") {
                                    Some(CypherValue::List(l)) => l.clone(),
                                    _ => Vec::new(),
                                },
                                match m.get("relationships") {
                                    Some(CypherValue::List(l)) => l.clone(),
                                    _ => Vec::new(),
                                },
                            ),
                            _ => (Vec::new(), Vec::new()),
                        };
                        let set = crate::apoc_export::ExportSet::from_values(&ns, &rs);
                        crate::apoc_export::to_arrow(&*graph, &set)?
                    }
                    _ => {
                        let q = match eval_expr(cx.arg(data_arg)?, frame, &*graph, symtab)? {
                            CypherValue::String(s) => s,
                            _ => continue,
                        };
                        let result = run_inner_cypher(graph, &q, Default::default())?;
                        crate::apoc_export::result_to_arrow(&result)?
                    }
                };
                let mut nf = frame.clone();
                if stream {
                    if let Some(sl) = cx.slot_for("value") {
                        nf[sl] = CypherValue::Bytes(payload).into();
                    }
                } else {
                    let file = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                        CypherValue::String(s) => s,
                        _ => continue,
                    };
                    let file_v = crate::apoc_export::deliver_bytes(&file, payload)?;
                    for (col, val) in [
                        ("file", file_v),
                        ("source", CypherValue::String("database".to_string())),
                        ("format", CypherValue::String("arrow".to_string())),
                        ("nodes", CypherValue::Integer(counts.nodes)),
                        ("relationships", CypherValue::Integer(counts.relationships)),
                        ("properties", CypherValue::Integer(counts.properties)),
                        (
                            "time",
                            CypherValue::Integer(t0.elapsed().as_millis() as i64),
                        ),
                        ("rows", CypherValue::Integer(counts.rows)),
                        ("batchSize", CypherValue::Integer(-1)),
                        ("batches", CypherValue::Integer(0)),
                        ("done", CypherValue::Bool(true)),
                        ("data", CypherValue::Null),
                    ] {
                        if let Some(sl) = cx.slot_for(col) {
                            nf[sl] = val.into();
                        }
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        // -- apoc.export.* / apoc.import.* --------------------
        n if n.starts_with("apoc.export.") || n.starts_with("apoc.import.") => {
            let mut out = Vec::new();
            for frame in frames {
                let t0 = web_time::Instant::now();
                // Argument layout per family:
                //   .all(file, config)          .data(nodes, rels, file, config)
                //   .graph(graph, file, config) .query(query, file, config)
                //   cypher.schema(file, config) import.*(file, config)
                //   import.csv(nodes, rels, config)
                let fam = n.rsplit('.').next().unwrap_or_default();
                let (format, source_desc);
                let mut counts = crate::apoc_export::IoCounts::default();
                let mut file_arg_idx = 0usize;
                let payload: Option<String> = if n.starts_with("apoc.export.") {
                    let fmt = n.split('.').nth(2).unwrap_or_default(); // json/csv/cypher/graphml
                    format = fmt.to_string();
                    // Build the export set / result.
                    enum Src {
                        Set(crate::apoc_export::ExportSet),
                        Query(crate::CypherResult),
                    }
                    let src_data = match fam {
                        "all" => {
                            source_desc = format!(
                                "database: nodes({}), rels({})",
                                graph.node_count(),
                                graph.edge_count()
                            );
                            Src::Set(crate::apoc_export::ExportSet::all(&*graph))
                        }
                        "schema" => {
                            source_desc = "database: schema".to_string();
                            Src::Set(crate::apoc_export::ExportSet {
                                nodes: Vec::new(),
                                edges: Vec::new(),
                            })
                        }
                        "data" => {
                            file_arg_idx = 2;
                            let ns = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                                CypherValue::List(l) => l,
                                _ => Vec::new(),
                            };
                            let rs = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                                CypherValue::List(l) => l,
                                _ => Vec::new(),
                            };
                            source_desc = format!("data: nodes({}), rels({})", ns.len(), rs.len());
                            Src::Set(crate::apoc_export::ExportSet::from_values(&ns, &rs))
                        }
                        "graph" => {
                            file_arg_idx = 1;
                            let g = eval_expr(cx.arg(0)?, frame, &*graph, symtab)?;
                            let (ns, rs) = match g {
                                CypherValue::Map(m) => (
                                    match m.get("nodes") {
                                        Some(CypherValue::List(l)) => l.clone(),
                                        _ => Vec::new(),
                                    },
                                    match m.get("relationships") {
                                        Some(CypherValue::List(l)) => l.clone(),
                                        _ => Vec::new(),
                                    },
                                ),
                                _ => (Vec::new(), Vec::new()),
                            };
                            source_desc = format!("graph: nodes({}), rels({})", ns.len(), rs.len());
                            Src::Set(crate::apoc_export::ExportSet::from_values(&ns, &rs))
                        }
                        _ => {
                            // query
                            file_arg_idx = 1;
                            let q = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                                CypherValue::String(s) => s,
                                _ => continue,
                            };
                            let result = run_inner_cypher(graph, &q, Default::default())?;
                            source_desc = format!("statement: cols({})", result.columns.len());
                            Src::Query(result)
                        }
                    };
                    let (payload, c) = match (&src_data, fmt) {
                        (Src::Set(set), "json") => crate::apoc_export::to_json(&*graph, set),
                        (Src::Set(set), "csv") => crate::apoc_export::to_csv(&*graph, set),
                        (Src::Set(set), "graphml") => crate::apoc_export::to_graphml(&*graph, set),
                        (Src::Set(set), _) => {
                            if fam == "schema" {
                                (
                                    crate::apoc_export::schema_to_cypher(&*graph),
                                    crate::apoc_export::IoCounts::default(),
                                )
                            } else {
                                crate::apoc_export::to_cypher(&*graph, set)
                            }
                        }
                        (Src::Query(res), "csv") => crate::apoc_export::result_to_csv(res),
                        (Src::Query(res), _) => {
                            // json/cypher/graphml query exports: entities
                            // found in the result rows.
                            let mut ns = Vec::new();
                            let mut rs = Vec::new();
                            for row in &res.rows {
                                for v in row {
                                    crate::executor::collect_graph_parts(v, &mut ns, &mut rs);
                                }
                            }
                            let set = crate::apoc_export::ExportSet::from_values(&ns, &rs);
                            match fmt {
                                "json" => crate::apoc_export::to_json(&*graph, &set),
                                "graphml" => crate::apoc_export::to_graphml(&*graph, &set),
                                _ => crate::apoc_export::to_cypher(&*graph, &set),
                            }
                        }
                    };
                    counts = c;
                    Some(payload)
                } else {
                    // import.*
                    format = n.rsplit('.').next().unwrap_or_default().to_string();
                    source_desc = "file".to_string();
                    None
                };

                // config map is always the LAST argument.
                let total_args = args.map(|a| a.len()).unwrap_or(0);
                let config = if total_args > 0 {
                    match cx.opt_arg(total_args - 1, frame, &*graph) {
                        Some(CypherValue::Map(m)) => m,
                        _ => Default::default(),
                    }
                } else {
                    Default::default()
                };
                let stream = matches!(config.get("stream"), Some(CypherValue::Bool(true)));

                let (file_v, data_v) = if let Some(payload) = payload {
                    let file = match cx.opt_arg(file_arg_idx, frame, &*graph) {
                        Some(CypherValue::String(s)) => s,
                        _ => String::new(),
                    };
                    crate::apoc_export::deliver(&file, stream, payload)?
                } else {
                    // Importers.
                    match n {
                        "apoc.import.json" | "apoc.import.graphml" => {
                            let file = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                                CypherValue::String(s) => s,
                                _ => continue,
                            };
                            let text = crate::apoc_export::read_local(&file)?;
                            counts = if n == "apoc.import.json" {
                                crate::apoc_export::import_json(graph, &text)?
                            } else {
                                crate::apoc_export::import_graphml(graph, &text)?
                            };
                            (CypherValue::String(file), CypherValue::Null)
                        }
                        _ => {
                            // apoc.import.csv(nodes, rels, config): each
                            // descriptor {fileName, labels}/{fileName, type}
                            // in the neo4j-admin header convention.
                            let node_descs = match eval_expr(cx.arg(0)?, frame, &*graph, symtab)? {
                                CypherValue::List(l) => l,
                                _ => Vec::new(),
                            };
                            let rel_descs = match eval_expr(cx.arg(1)?, frame, &*graph, symtab)? {
                                CypherValue::List(l) => l,
                                _ => Vec::new(),
                            };
                            counts = import_csv_files(graph, &node_descs, &rel_descs)?;
                            (CypherValue::String("csv".to_string()), CypherValue::Null)
                        }
                    }
                };

                let mut nf = frame.clone();
                for (col, val) in [
                    ("file", file_v),
                    ("source", CypherValue::String(source_desc.clone())),
                    ("format", CypherValue::String(format.clone())),
                    ("nodes", CypherValue::Integer(counts.nodes)),
                    ("relationships", CypherValue::Integer(counts.relationships)),
                    ("properties", CypherValue::Integer(counts.properties)),
                    (
                        "time",
                        CypherValue::Integer(t0.elapsed().as_millis() as i64),
                    ),
                    ("rows", CypherValue::Integer(counts.rows)),
                    ("batchSize", CypherValue::Integer(-1)),
                    ("batches", CypherValue::Integer(0)),
                    ("done", CypherValue::Bool(true)),
                    ("data", data_v),
                ] {
                    if let Some(sl) = cx.slot_for(col) {
                        nf[sl] = val.into();
                    }
                }
                out.push(nf);
            }
            Ok(out)
        }
        other => Err(super::unknown(other)),
    }
}

/// apoc.import.csv: node/relationship CSVs in the neo4j-admin header convention (`id:ID`, `:LABEL`,
/// `:START_ID`, `:END_ID`, `:TYPE`, `name:type` suffixes for int/long/float/double/boolean).
fn import_csv_files(
    graph: &mut dyn GraphView,
    node_descs: &[CypherValue],
    rel_descs: &[CypherValue],
) -> Result<crate::apoc_export::IoCounts, CypherError> {
    use traverse_core::graph::property::PropertyValue;
    let mut c = crate::apoc_export::IoCounts::default();
    let mut id_map: std::collections::HashMap<String, traverse_core::graph::types::NodeId> =
        Default::default();
    let desc_str = |d: &CypherValue, key: &str| -> Option<String> {
        match d {
            CypherValue::Map(m) => match m.get(key) {
                Some(CypherValue::String(s)) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        }
    };
    // minimal CSV split honoring quotes
    fn split_csv(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_q = false;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '"' if in_q && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => in_q = !in_q,
                ',' if !in_q => out.push(std::mem::take(&mut cur)),
                _ => cur.push(ch),
            }
        }
        out.push(cur);
        out
    }
    let typed = |raw: &str, ty: &str| -> Option<PropertyValue> {
        if raw.is_empty() {
            return None;
        }
        Some(match ty {
            "int" | "long" | "integer" => PropertyValue::Int(raw.parse().ok()?),
            "float" | "double" => PropertyValue::Float(raw.parse().ok()?),
            "boolean" | "bool" => PropertyValue::Bool(raw.eq_ignore_ascii_case("true")),
            _ => PropertyValue::String(raw.to_string()),
        })
    };
    for d in node_descs {
        let Some(file) = desc_str(d, "fileName") else {
            continue;
        };
        let extra_labels: Vec<String> = match d {
            CypherValue::Map(m) => match m.get("labels") {
                Some(CypherValue::List(l)) => l.iter().map(key_string).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let text = crate::apoc_export::read_local(&file)?;
        let mut lines = text.lines();
        let Some(header) = lines.next() else { continue };
        let cols = split_csv(header);
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let vals = split_csv(line);
            let mut labels = extra_labels.clone();
            let mut props = traverse_core::graph::property::PropertyMap::new();
            let mut ext_id = String::new();
            for (i, col) in cols.iter().enumerate() {
                let raw = vals.get(i).map(String::as_str).unwrap_or("");
                if let Some(name) = col.strip_suffix(":ID") {
                    ext_id = raw.to_string();
                    let name = name.trim();
                    if !name.is_empty() && !raw.is_empty() {
                        props.insert(name.to_string(), PropertyValue::String(raw.to_string()));
                        c.properties += 1;
                    }
                } else if col.trim() == ":LABEL" {
                    labels.extend(raw.split(';').filter(|s| !s.is_empty()).map(str::to_string));
                } else {
                    let (name, ty) = match col.split_once(':') {
                        Some((n, t)) => (n.trim(), t.trim().to_lowercase()),
                        None => (col.trim(), "string".to_string()),
                    };
                    if name.is_empty() {
                        continue;
                    }
                    if let Some(pv) = typed(raw, &ty) {
                        props.insert(name.to_string(), pv);
                        c.properties += 1;
                    }
                }
            }
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let nid = graph
                .create_node(&label_refs, props)
                .map_err(CypherError::from)?;
            crate::executor::inc_stat(|s| s.nodes_created += 1);
            if !ext_id.is_empty() {
                id_map.insert(ext_id, nid);
            }
            c.nodes += 1;
            c.rows += 1;
        }
    }
    for d in rel_descs {
        let Some(file) = desc_str(d, "fileName") else {
            continue;
        };
        let default_type = desc_str(d, "type").unwrap_or_else(|| "RELATED".to_string());
        let text = crate::apoc_export::read_local(&file)?;
        let mut lines = text.lines();
        let Some(header) = lines.next() else { continue };
        let cols = split_csv(header);
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let vals = split_csv(line);
            let mut start = String::new();
            let mut end = String::new();
            let mut rtype = default_type.clone();
            let mut props = traverse_core::graph::property::PropertyMap::new();
            for (i, col) in cols.iter().enumerate() {
                let raw = vals.get(i).map(String::as_str).unwrap_or("");
                match col.trim() {
                    ":START_ID" => start = raw.to_string(),
                    ":END_ID" => end = raw.to_string(),
                    ":TYPE" => {
                        if !raw.is_empty() {
                            rtype = raw.to_string();
                        }
                    }
                    other => {
                        let (name, ty) = match other.split_once(':') {
                            Some((n, t)) => (n.trim(), t.trim().to_lowercase()),
                            None => (other, "string".to_string()),
                        };
                        if name.is_empty() {
                            continue;
                        }
                        if let Some(pv) = typed(raw, &ty) {
                            props.insert(name.to_string(), pv);
                            c.properties += 1;
                        }
                    }
                }
            }
            let (Some(&s), Some(&e)) = (id_map.get(&start), id_map.get(&end)) else {
                return Err(CypherError::runtime(format!(
                    "apoc.import.csv: relationship references unknown :ID `{start}` or `{end}`"
                )));
            };
            graph
                .create_edge(s, e, &rtype, props)
                .map_err(CypherError::from)?;
            crate::executor::inc_stat(|st| st.relationships_created += 1);
            c.relationships += 1;
            c.rows += 1;
        }
    }
    Ok(c)
}

/// apoc.import.xml: an XML document as a graph, APOC's model: `(:XmlDocument)`
/// root, `(:XmlTag {_name})` per element with attributes as properties,
/// `(:XmlWord {text})` per whitespace-split word; `IS_CHILD_OF` (child -> parent),
/// `FIRST_CHILD_OF` (first child -> parent), `NEXT_SIBLING` between consecutive
/// children, a `NEXT` chain over all nodes in document order, optionally
/// `NEXT_WORD` between words. Returns the document node.
fn import_xml_graph(
    graph: &mut dyn GraphView,
    text: &str,
    url: &str,
    word_rels: bool,
) -> Result<traverse_core::graph::types::NodeId, CypherError> {
    use crate::xml::XmlEvent;
    use traverse_core::graph::property::{PropertyMap, PropertyValue};
    use traverse_core::graph::types::NodeId;

    let mk_node = |graph: &mut dyn GraphView,
                   label: &str,
                   props: PropertyMap|
     -> Result<NodeId, CypherError> {
        let id = graph
            .create_node(&[label], props)
            .map_err(CypherError::from)?;
        crate::executor::inc_stat(|s| s.nodes_created += 1);
        Ok(id)
    };
    let link =
        |graph: &mut dyn GraphView, a: NodeId, b: NodeId, t: &str| -> Result<(), CypherError> {
            graph
                .create_edge(a, b, t, PropertyMap::new())
                .map_err(CypherError::from)?;
            crate::executor::inc_stat(|s| s.relationships_created += 1);
            Ok(())
        };

    // document node properties as live APOC sets them: {_xmlEncoding: 'UTF-8', url: <original
    // argument>}
    let mut doc_props = PropertyMap::new();
    doc_props.insert(
        "_xmlEncoding".to_string(),
        PropertyValue::String("UTF-8".to_string()),
    );
    doc_props.insert("url".to_string(), PropertyValue::String(url.to_string()));
    let doc = mk_node(graph, "XmlDocument", doc_props)?;

    let mut events = crate::xml::XmlEvents::new(text, false);
    // per open element: (node id, children so far)
    let mut stack: Vec<(NodeId, Vec<NodeId>)> = vec![(doc, Vec::new())];
    let mut doc_order: Vec<NodeId> = vec![doc];
    let mut last_word: Option<NodeId> = None;
    // character offset across ALL text (APOC's startIndex/endIndex, whitespace tokens included)
    let mut char_cursor: usize = 0;

    // wire a finished child into its parent (containment + sibling)
    let attach = |graph: &mut dyn GraphView,
                  parent: &mut (NodeId, Vec<NodeId>),
                  child: NodeId|
     -> Result<(), CypherError> {
        link(graph, child, parent.0, "IS_CHILD_OF")?;
        match parent.1.last() {
            None => link(graph, child, parent.0, "FIRST_CHILD_OF")?,
            Some(prev) => link(graph, *prev, child, "NEXT_SIBLING")?,
        }
        parent.1.push(child);
        Ok(())
    };

    loop {
        match events.next() {
            Ok(ev @ (XmlEvent::Start(_) | XmlEvent::Empty(_))) => {
                let is_empty = matches!(ev, XmlEvent::Empty(_));
                let (XmlEvent::Start(e) | XmlEvent::Empty(e)) = ev else {
                    unreachable!()
                };
                let mut props = PropertyMap::new();
                props.insert(
                    "_name".to_string(),
                    PropertyValue::String(e.name().into_inner().to_string()),
                );
                for a in e.attributes().flatten() {
                    props.insert(
                        a.key.into_inner().to_string(),
                        PropertyValue::String(a.value.to_string()),
                    );
                }
                let tag = mk_node(graph, "XmlTag", props)?;
                doc_order.push(tag);
                let parent = stack.last_mut().expect("document root always present");
                attach(graph, parent, tag)?;
                if !is_empty {
                    stack.push((tag, Vec::new()));
                }
            }
            Ok(XmlEvent::Text(s)) => {
                // text outside the root element isn't materialized (as in APOC)
                if stack.len() == 1 {
                    continue;
                }
                // alternating word / whitespace runs, one XmlCharacters node each (APOC's model)
                let mut chars = s.chars().peekable();
                while chars.peek().is_some() {
                    let ws = chars.peek().is_some_and(|c| c.is_whitespace());
                    let mut token = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_whitespace() != ws {
                            break;
                        }
                        token.push(c);
                        chars.next();
                    }
                    let len = token.chars().count();
                    let mut props = PropertyMap::new();
                    props.insert("text".to_string(), PropertyValue::String(token));
                    props.insert(
                        "startIndex".to_string(),
                        PropertyValue::Int(char_cursor as i64),
                    );
                    props.insert(
                        "endIndex".to_string(),
                        PropertyValue::Int((char_cursor + len - 1) as i64),
                    );
                    char_cursor += len;
                    let w = mk_node(graph, "XmlCharacters", props)?;
                    doc_order.push(w);
                    let parent = stack.last_mut().expect("document root always present");
                    attach(graph, parent, w)?;
                    if !ws {
                        if word_rels && let Some(prev) = last_word {
                            link(graph, prev, w, "NEXT_WORD")?;
                        }
                        last_word = Some(w);
                    }
                }
            }
            Ok(XmlEvent::End(_)) => {
                if stack.len() > 1 {
                    let (tag, children) = stack.pop().expect("checked non-root");
                    // last child -> tag (XmlTag parents only, like APOC)
                    if let Some(last) = children.last() {
                        link(graph, *last, tag, "LAST_CHILD_OF")?;
                    }
                }
            }
            Ok(XmlEvent::Eof) => break,
            Err(e) => {
                return Err(CypherError::runtime(format!(
                    "apoc.import.xml: XML parse error: {e}"
                )));
            }
        }
    }
    // document-order NEXT chain
    for w in doc_order.windows(2) {
        link(graph, w[0], w[1], "NEXT")?;
    }
    Ok(doc)
}
