//! Bulk CSV importer: reads node/edge CSVs and writes a `.tvdb` file.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use traverse_core::graph::Graph;
use traverse_core::graph::property::{PropertyMap, PropertyValue};
use traverse_core::graph::types::NodeId;
use traverse_core::storage::Database;

use indicatif::{ProgressBar, ProgressStyle};

/// Infer a PropertyValue from a CSV string field.
fn infer_value(s: &str) -> PropertyValue {
    if s.is_empty() {
        return PropertyValue::Null;
    }
    match s {
        "true" | "TRUE" | "True" => return PropertyValue::Bool(true),
        "false" | "FALSE" | "False" => return PropertyValue::Bool(false),
        _ => {}
    }
    if let Ok(i) = s.parse::<i64>() {
        return PropertyValue::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return PropertyValue::Float(f);
    }
    PropertyValue::String(s.to_string())
}

/// Parse a `LABEL=path.csv` argument into (label, path).
fn parse_label_path(arg: &str) -> Result<(&str, &str), String> {
    let eq = arg
        .find('=')
        .ok_or_else(|| format!("expected LABEL=path.csv format, got: {arg}"))?;
    Ok((&arg[..eq], &arg[eq + 1..]))
}

pub fn run_import(
    nodes: &[String],
    edges: &[String],
    output: &str,
    _id_type: &str,
    delimiter: &str,
    skip_bad_rows: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let delim = delimiter.as_bytes().first().copied().unwrap_or(b',');
    let mut graph = Graph::new();

    // Map from CSV :ID value -> NodeId for edge resolution.
    let mut id_map: BTreeMap<String, NodeId> = BTreeMap::new();
    let mut total_nodes: u64 = 0;
    let mut total_edges: u64 = 0;
    let mut skipped: u64 = 0;

    // -- 1. nodes -------------------------------------------------------
    for arg in nodes {
        let (label, path) = parse_label_path(arg)?;
        eprintln!("Importing nodes :{label} from {path}");

        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .delimiter(delim)
            .from_path(path)?;

        let headers: Vec<String> = rdr.headers()?.iter().map(|h| h.to_string()).collect();
        let _id_col = headers.iter().position(|h| h == ":ID");

        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] {msg}")
                .unwrap(),
        );

        for result in rdr.records() {
            let record = match result {
                Ok(r) => r,
                Err(e) => {
                    if skip_bad_rows {
                        skipped += 1;
                        eprintln!("  skipping bad row: {e}");
                        continue;
                    }
                    return Err(e.into());
                }
            };

            let mut props: PropertyMap = BTreeMap::new();
            let mut csv_id: Option<String> = None;

            for (i, field) in record.iter().enumerate() {
                let header = &headers[i];
                if header == ":ID" {
                    csv_id = Some(field.to_string());
                    continue;
                }
                // Skip Neo4j-style meta columns
                if header.starts_with(':') {
                    continue;
                }
                let val = infer_value(field);
                if !matches!(val, PropertyValue::Null) {
                    props.insert(header.clone(), val);
                }
            }

            let node_id = graph.create_node(&[label], props);
            if let Some(cid) = csv_id {
                id_map.insert(cid, node_id);
            }
            total_nodes += 1;

            if total_nodes.is_multiple_of(10_000) {
                pb.set_message(format!("{total_nodes} nodes"));
            }
        }
        pb.finish_with_message(format!("{total_nodes} nodes loaded for :{label}"));
    }

    // -- 2. edges -------------------------------------------------------
    for arg in edges {
        let (edge_type, path) = parse_label_path(arg)?;
        eprintln!("Importing edges :{edge_type} from {path}");

        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .delimiter(delim)
            .from_path(path)?;

        let headers: Vec<String> = rdr.headers()?.iter().map(|h| h.to_string()).collect();
        let start_col = headers
            .iter()
            .position(|h| h == ":START_ID")
            .ok_or_else(|| format!("edge CSV {path} missing :START_ID column"))?;
        let end_col = headers
            .iter()
            .position(|h| h == ":END_ID")
            .ok_or_else(|| format!("edge CSV {path} missing :END_ID column"))?;

        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] {msg}")
                .unwrap(),
        );

        for result in rdr.records() {
            let record = match result {
                Ok(r) => r,
                Err(e) => {
                    if skip_bad_rows {
                        skipped += 1;
                        eprintln!("  skipping bad row: {e}");
                        continue;
                    }
                    return Err(e.into());
                }
            };

            let start_id_str = record.get(start_col).unwrap_or("");
            let end_id_str = record.get(end_col).unwrap_or("");

            let src = match id_map.get(start_id_str) {
                Some(id) => *id,
                None => {
                    if skip_bad_rows {
                        skipped += 1;
                        continue;
                    }
                    return Err(format!(
                        "edge :START_ID '{start_id_str}' not found in node ID map"
                    )
                    .into());
                }
            };
            let tgt = match id_map.get(end_id_str) {
                Some(id) => *id,
                None => {
                    if skip_bad_rows {
                        skipped += 1;
                        continue;
                    }
                    return Err(
                        format!("edge :END_ID '{end_id_str}' not found in node ID map").into(),
                    );
                }
            };

            let mut props: PropertyMap = BTreeMap::new();
            for (i, field) in record.iter().enumerate() {
                let header = &headers[i];
                if header.starts_with(':') {
                    continue;
                }
                let val = infer_value(field);
                if !matches!(val, PropertyValue::Null) {
                    props.insert(header.clone(), val);
                }
            }

            graph.create_edge(src, tgt, edge_type, props)?;
            total_edges += 1;

            if total_edges.is_multiple_of(10_000) {
                pb.set_message(format!("{total_edges} edges"));
            }
        }
        pb.finish_with_message(format!("{total_edges} edges loaded for :{edge_type}"));
    }

    // -- 3. save .tvdb --------------------------------------------------
    let out_path = Path::new(output);
    eprintln!("Saving to {output}...");
    Database::save(out_path, &graph, &[])?;

    let elapsed = start.elapsed();
    let total = total_nodes + total_edges;
    let rate = if elapsed.as_secs_f64() > 0.0 {
        total as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    eprintln!(
        "Done: {total_nodes} nodes, {total_edges} edges in {:.2}s ({:.0} rows/s)",
        elapsed.as_secs_f64(),
        rate
    );
    if skipped > 0 {
        eprintln!("  ({skipped} rows skipped)");
    }

    Ok(())
}
