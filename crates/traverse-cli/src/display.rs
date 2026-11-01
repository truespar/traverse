use std::io::Write;

use comfy_table::{ContentArrangement, Table};
use traverse_cypher::types::CypherValue;
use traverse_server::packstream::Value;

/// Output format for query results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "table" => Some(OutputFormat::Table),
            "json" => Some(OutputFormat::Json),
            "csv" => Some(OutputFormat::Csv),
            _ => None,
        }
    }
}

/// Display query results in the given format.
pub fn display_results(
    columns: &[String],
    rows: &[Vec<CypherValue>],
    stats: &[(String, Value)],
    plan: Option<&CypherValue>,
    format: OutputFormat,
) {
    // Display query plan if present (EXPLAIN/PROFILE)
    if let Some(plan) = plan {
        display_plan(plan);
    }

    if columns.is_empty() && rows.is_empty() {
        display_stats(stats);
        return;
    }

    match format {
        OutputFormat::Table => display_table(columns, rows, stats),
        OutputFormat::Json => display_json(columns, rows),
        OutputFormat::Csv => display_csv(columns, rows),
    }
}

fn display_table(columns: &[String], rows: &[Vec<CypherValue>], stats: &[(String, Value)]) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(columns);

    for row in rows {
        let cells: Vec<String> = row.iter().map(|v| format!("{v}")).collect();
        table.add_row(cells);
    }

    println!("{table}");

    let count = rows.len();
    if count == 1 {
        print!("1 row");
    } else {
        print!("{count} rows");
    }
    display_stats(stats);
    println!();
}

fn display_stats(stats: &[(String, Value)]) {
    let stat_parts: Vec<String> = stats
        .iter()
        .filter_map(|(k, v)| match (k.as_str(), v) {
            (
                "nodes_created"
                | "nodes_deleted"
                | "relationships_created"
                | "relationships_deleted"
                | "properties_set"
                | "labels_added"
                | "labels_removed"
                | "indexes_added"
                | "indexes_removed",
                Value::Integer(n),
            ) if *n > 0 => {
                let label = k.replace('_', " ");
                Some(format!("{n} {label}"))
            }
            _ => None,
        })
        .collect();

    if !stat_parts.is_empty() {
        print!(", {}", stat_parts.join(", "));
    }
}

fn display_json(columns: &[String], rows: &[Vec<CypherValue>]) {
    for row in rows {
        let mut map = serde_json::Map::new();
        for (col, val) in columns.iter().zip(row.iter()) {
            map.insert(col.clone(), cypher_to_json(val));
        }
        println!("{}", serde_json::Value::Object(map));
    }
}

fn display_csv(columns: &[String], rows: &[Vec<CypherValue>]) {
    let stdout = std::io::stdout();
    let mut wtr = csv::Writer::from_writer(stdout.lock());
    wtr.write_record(columns).ok();
    for row in rows {
        let fields: Vec<String> = row.iter().map(cypher_to_csv_field).collect();
        wtr.write_record(&fields).ok();
    }
    wtr.flush().ok();
}

/// Convert a CypherValue to a raw string for CSV (no wrapping quotes on strings).
pub fn cypher_to_csv_field(cv: &CypherValue) -> String {
    match cv {
        CypherValue::Null => String::new(),
        CypherValue::String(s) => s.clone(),
        _ => format!("{cv}"),
    }
}

pub fn cypher_to_json(cv: &CypherValue) -> serde_json::Value {
    match cv {
        CypherValue::Null => serde_json::Value::Null,
        CypherValue::Bool(b) => serde_json::Value::Bool(*b),
        CypherValue::Integer(n) => serde_json::json!(*n),
        CypherValue::Float(f) => serde_json::json!(*f),
        CypherValue::String(s) => serde_json::Value::String(s.clone()),
        CypherValue::List(items) => {
            serde_json::Value::Array(items.iter().map(cypher_to_json).collect())
        }
        CypherValue::Map(m) => {
            let obj: serde_json::Map<String, serde_json::Value> = m
                .iter()
                .map(|(k, v)| (k.clone(), cypher_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        CypherValue::Node {
            id,
            labels,
            properties,
        } => {
            serde_json::json!({
                "_id": id.0,
                "_labels": labels,
                "_properties": properties.iter()
                    .map(|(k, v)| (k.clone(), format!("{v:?}")))
                    .collect::<std::collections::BTreeMap<_, _>>()
            })
        }
        CypherValue::Edge {
            id,
            edge_type,
            source,
            target,
            properties,
        } => {
            serde_json::json!({
                "_id": id.0,
                "_type": edge_type,
                "_start": source.0,
                "_end": target.0,
                "_properties": properties.iter()
                    .map(|(k, v)| (k.clone(), format!("{v:?}")))
                    .collect::<std::collections::BTreeMap<_, _>>()
            })
        }
        CypherValue::Path { nodes, edges } => {
            serde_json::json!({
                "_nodes": nodes.iter().map(cypher_to_json).collect::<Vec<_>>(),
                "_edges": edges.iter().map(cypher_to_json).collect::<Vec<_>>()
            })
        }
        // Temporal types - use Display representation
        _ => serde_json::Value::String(format!("{cv}")),
    }
}

// -- Query plan rendering ---------------------------------------------

/// Display a query plan tree (from EXPLAIN/PROFILE).
fn display_plan(plan: &CypherValue) {
    println!("Query Plan:");
    render_plan_node(plan, "", true);
    println!();
}

fn render_plan_node(node: &CypherValue, prefix: &str, is_last: bool) {
    let CypherValue::Map(map) = node else {
        return;
    };

    let operator = map
        .get("operatorType")
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    let args = map.get("args");
    let args_str = format_plan_args(args);

    let connector = if prefix.is_empty() {
        ""
    } else if is_last {
        "`-- "
    } else {
        "|-- "
    };

    if args_str.is_empty() {
        println!("{prefix}{connector}{operator}");
    } else {
        println!("{prefix}{connector}{operator} ({args_str})");
    }

    if let Some(CypherValue::List(children)) = map.get("children") {
        let child_prefix = if prefix.is_empty() {
            "".to_string()
        } else if is_last {
            format!("{prefix}    ")
        } else {
            format!("{prefix}|   ")
        };
        for (i, child) in children.iter().enumerate() {
            let last = i == children.len() - 1;
            render_plan_node(child, &child_prefix, last);
        }
    }
}

pub fn format_plan_args(args: Option<&CypherValue>) -> String {
    let Some(CypherValue::Map(map)) = args else {
        return String::new();
    };
    let parts: Vec<String> = map
        .iter()
        .map(|(k, v)| {
            let val = match v {
                CypherValue::String(s) => s.clone(),
                CypherValue::Bool(b) => b.to_string(),
                CypherValue::Integer(n) => n.to_string(),
                other => format!("{other}"),
            };
            format!("{k}: {val}")
        })
        .collect();
    parts.join(", ")
}

// -- Error display ----------------------------------------------------

/// Display an error message to stderr.
pub fn display_error(msg: &str) {
    let _ = writeln!(std::io::stderr(), "ERROR: {msg}");
}

/// Display a syntax error with a position indicator pointing to the error in the query.
pub fn display_error_with_position(query: &str, error_msg: &str) {
    // Try to extract position from error messages like "at position N: ..."
    let position = extract_error_position(error_msg);

    let _ = writeln!(std::io::stderr(), "ERROR: {error_msg}");

    if let Some(pos) = position {
        // Show the query with a caret pointing to the error position
        let _ = writeln!(std::io::stderr(), "  {query}");
        let caret_offset = pos.min(query.len());
        let _ = writeln!(std::io::stderr(), "  {}^", " ".repeat(caret_offset));
    }
}

/// Try to extract a byte position from an error message.
/// Looks for patterns like "at position N:" or "position N:"
pub fn extract_error_position(msg: &str) -> Option<usize> {
    // Pattern: "at position N:"
    if let Some(idx) = msg.find("at position ") {
        let after = &msg[idx + 12..];
        let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        return num_str.parse().ok();
    }
    // Pattern: "position N:"
    if let Some(idx) = msg.find("position ") {
        let after = &msg[idx + 9..];
        let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        return num_str.parse().ok();
    }
    None
}
