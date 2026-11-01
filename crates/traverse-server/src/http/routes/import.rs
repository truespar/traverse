//! Imports: JSON graphs, CSV nodes and CSV relationships.

use std::collections::HashMap;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use traverse_core::graph::property::PropertyValue;
use traverse_core::tx::view::GraphView;

use super::{AppState, commit_failed_response};
use crate::statement::ClientAddress;

// -- POST /api/import ------------------------------------------------

pub(super) async fn import_handler(
    State(state): State<AppState>,
    ClientAddress(client_address): ClientAddress,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let cypher = match body.get("cypher").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing 'cypher' field"})),
            )
                .into_response();
        }
    };

    let db_name = body
        .get("database")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| state.registry.default_name());

    let entry = match state.registry.get(db_name) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": format!("database '{}' not found", db_name)})),
            )
                .into_response();
        }
    };

    // read-only databases
    if entry.read_only {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": format!("database '{}' is read-only", db_name)})),
        )
            .into_response();
    }

    let start = Instant::now();
    let user = state.api_key.is_some().then(|| "api-user".to_string());
    let database = db_name.to_string();
    // the statements run on a blocking thread, each an auto-commit transaction of its own; the
    // client going away stops the import at its next check
    let outcome = crate::statement::run_request("http", client_address, user, move |request| {
        // split on semicolons outside string literals and line comments (a bare `split(';')` would
        // cut `{name: 'Smith; Jones'}` in half)
        let statements: Vec<&str> = traverse_cypher::statements::split_statements(&cypher);
        let mut executed = 0;
        let mut errors: Vec<Value> = Vec::new();
        let mut total_stats = ImportStats::default();
        for stmt in &statements {
            // the client went away: the rest is moot
            if request.ctx().interrupt().is_some() {
                break;
            }
            let statement = request.statement(&database, stmt, None);
            let _query = statement.ctx.enter();
            let result = run_import_statement(&entry, stmt, &statement.ctx);
            match result {
                Ok(r) => {
                    executed += 1;
                    total_stats.nodes_created += r.stats.nodes_created;
                    total_stats.nodes_deleted += r.stats.nodes_deleted;
                    total_stats.relationships_created += r.stats.relationships_created;
                    total_stats.relationships_deleted += r.stats.relationships_deleted;
                    total_stats.properties_set += r.stats.properties_set;
                    total_stats.labels_added += r.stats.labels_added;
                    total_stats.labels_removed += r.stats.labels_removed;
                }
                Err(e) => {
                    errors.push(json!({
                        "statement": crate::truncate_str(stmt, 200),
                        "error": e,
                    }));
                    // continue with the remaining statements
                }
            }
        }
        (statements.len(), executed, errors, total_stats)
    })
    .await;
    let (total, executed, errors, total_stats) = match outcome {
        Ok(done) => done,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": e })),
            )
                .into_response();
        }
    };

    // reclaim write-path temporaries across all threads
    crate::registry::purge_allocator();

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    axum::Json(json!({
        "total_statements": total,
        "executed": executed,
        "errors": errors,
        "stats": {
            "nodes_created": total_stats.nodes_created,
            "nodes_deleted": total_stats.nodes_deleted,
            "relationships_created": total_stats.relationships_created,
            "relationships_deleted": total_stats.relationships_deleted,
            "properties_set": total_stats.properties_set,
            "labels_added": total_stats.labels_added,
            "labels_removed": total_stats.labels_removed,
        },
        "time_ms": elapsed_ms,
    }))
    .into_response()
}

/// One import statement as an auto-commit transaction, under its entered context.
fn run_import_statement(
    entry: &crate::registry::DatabaseEntry,
    stmt: &str,
    ctx: &traverse_cypher::query::QueryContext,
) -> Result<traverse_cypher::executor::CypherResult, String> {
    if crate::is_write_query(stmt) {
        let mut tx = crate::statement::begin_write(&entry.store, ctx).map_err(|e| e.to_string())?;
        match traverse_cypher::execute_with_params(&mut tx, stmt, &HashMap::new()) {
            Ok(r) => match tx.commit() {
                Ok(_) => {
                    entry.mark_dirty();
                    Ok(r)
                }
                Err(e) => Err(format!("commit failed: {e}")),
            },
            Err(e) => {
                tx.abort();
                Err(e.to_string())
            }
        }
    } else {
        let mut tx = entry.store.begin_read().map_err(|e| e.to_string())?;
        traverse_cypher::execute_with_params(&mut tx, stmt, &HashMap::new())
            .map_err(|e| e.to_string())
    }
}

#[derive(Default)]
struct ImportStats {
    nodes_created: i64,
    nodes_deleted: i64,
    relationships_created: i64,
    relationships_deleted: i64,
    properties_set: i64,
    labels_added: i64,
    labels_removed: i64,
}

// -- POST /api/import/csv ---------------------------------------------

pub(super) async fn csv_import_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let csv_data = match body.get("csv").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing 'csv' field"})),
            )
                .into_response();
        }
    };

    let label = match body.get("label").and_then(|v| v.as_str()) {
        Some(l) if !l.trim().is_empty() => l.trim(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing or empty 'label' field"})),
            )
                .into_response();
        }
    };

    let columns = match body.get("columns").and_then(|v| v.as_array()) {
        Some(c) if !c.is_empty() => c,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing or empty 'columns' array"})),
            )
                .into_response();
        }
    };

    // column mappings
    struct ColMapping {
        index: usize,
        property: String,
        col_type: String,
    }
    let mut mappings: Vec<ColMapping> = Vec::new();

    // header -> index map, if there are headers
    let delimiter = body
        .get("delimiter")
        .and_then(|v| v.as_str())
        .unwrap_or(",");
    let delim_byte = match delimiter {
        "\t" | "tab" => b'\t',
        ";" => b';',
        "|" => b'|',
        _ => b',',
    };
    let skip_header = body
        .get("skip_header")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // read headers to map column names to indices
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim_byte)
        .has_headers(skip_header)
        .from_reader(csv_data.as_bytes());

    let header_map: HashMap<String, usize> = if skip_header {
        match rdr.headers() {
            Ok(hdrs) => hdrs
                .iter()
                .enumerate()
                .map(|(i, h)| (h.to_string(), i))
                .collect(),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": format!("CSV header error: {e}")})),
                )
                    .into_response();
            }
        }
    } else {
        HashMap::new()
    };

    for col_def in columns {
        let col_name = col_def.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let property = col_def
            .get("property")
            .and_then(|v| v.as_str())
            .unwrap_or(col_name);
        let col_type = col_def
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("string");

        if property.is_empty() {
            continue;
        }

        // column index: header map first, then positional
        let index = if let Some(&idx) = header_map.get(col_name) {
            idx
        } else if let Ok(idx) = col_name.parse::<usize>() {
            idx
        } else {
            continue; // skip unmapped columns
        };

        mappings.push(ColMapping {
            index,
            property: property.to_string(),
            col_type: col_type.to_string(),
        });
    }

    if mappings.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "no valid column mappings"})),
        )
            .into_response();
    }

    let db_name = body
        .get("database")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| state.registry.default_name());

    let entry = match state.registry.get(db_name) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": format!("database '{}' not found", db_name)})),
            )
                .into_response();
        }
    };

    // read-only databases
    if entry.read_only {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": format!("database '{}' is read-only", db_name)})),
        )
            .into_response();
    }

    let start = Instant::now();
    let mut nodes_created: i64 = 0;
    let mut properties_set: i64 = 0;
    let mut errors: Vec<Value> = Vec::new();
    let batch_size = 10_000;
    let label_str = label.to_string();

    // rows in batches
    let mut batch_count = 0;
    let mut tx = entry.store.begin_write();

    for result in rdr.records() {
        let record = match result {
            Ok(r) => r,
            Err(e) => {
                errors.push(
                    json!({"row": nodes_created + errors.len() as i64 + 1, "error": e.to_string()}),
                );
                if errors.len() > 100 {
                    break;
                }
                continue;
            }
        };

        let mut props = std::collections::BTreeMap::<String, PropertyValue>::new();
        let mut prop_count: i64 = 0;

        for mapping in &mappings {
            let val = match record.get(mapping.index) {
                Some(v) if !v.is_empty() => v,
                _ => continue,
            };

            let pv = match mapping.col_type.as_str() {
                "integer" => match val.parse::<i64>() {
                    Ok(i) => PropertyValue::Int(i),
                    Err(_) => continue,
                },
                "float" => match val.parse::<f64>() {
                    Ok(f) => PropertyValue::Float(f),
                    Err(_) => continue,
                },
                "boolean" => match val.to_lowercase().as_str() {
                    "true" | "1" | "yes" => PropertyValue::Bool(true),
                    "false" | "0" | "no" => PropertyValue::Bool(false),
                    _ => continue,
                },
                _ => PropertyValue::String(val.to_string()),
            };

            props.insert(mapping.property.clone(), pv);
            prop_count += 1;
        }

        match <dyn GraphView>::create_node(&mut tx, &[&label_str], props) {
            Ok(_) => {
                nodes_created += 1;
                properties_set += prop_count;
            }
            Err(e) => {
                errors.push(
                    json!({"row": nodes_created + errors.len() as i64 + 1, "error": e.to_string()}),
                );
                if errors.len() > 100 {
                    break;
                }
            }
        }

        batch_count += 1;
        if batch_count >= batch_size {
            if let Err(e) = tx.commit() {
                return commit_failed_response(e);
            }
            tx = entry.store.begin_write();
            batch_count = 0;
        }
    }

    // commit the remaining batch
    if batch_count > 0 {
        if let Err(e) = tx.commit() {
            return commit_failed_response(e);
        }
    } else {
        tx.abort();
    }

    if nodes_created > 0 {
        entry.mark_dirty();
    }

    // reclaim write-path temporaries across all threads
    crate::registry::purge_allocator();

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    axum::Json(json!({
        "nodes_created": nodes_created,
        "properties_set": properties_set,
        "labels_added": nodes_created,
        "errors": errors,
        "time_ms": elapsed_ms,
    }))
    .into_response()
}

// -- POST /api/import/csv/edges ------------------------------------

pub(super) async fn csv_edge_import_handler(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let csv_data = match body.get("csv").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing 'csv' field"})),
            )
                .into_response();
        }
    };

    let edge_type = match body.get("edge_type").and_then(|v| v.as_str()) {
        Some(t) if !t.trim().is_empty() => t.trim(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing or empty 'edge_type' field"})),
            )
                .into_response();
        }
    };

    // source config
    let source_cfg = match body.get("source") {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing 'source' field"})),
            )
                .into_response();
        }
    };
    let src_label = match source_cfg.get("label").and_then(|v| v.as_str()) {
        Some(l) if !l.trim().is_empty() => l.trim(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "source.label is required"})),
            )
                .into_response();
        }
    };
    let src_column = match source_cfg.get("column").and_then(|v| v.as_str()) {
        Some(c) if !c.trim().is_empty() => c.trim(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "source.column is required"})),
            )
                .into_response();
        }
    };
    let src_property = match source_cfg.get("property").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "source.property is required"})),
            )
                .into_response();
        }
    };

    // target config
    let target_cfg = match body.get("target") {
        Some(t) => t,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing 'target' field"})),
            )
                .into_response();
        }
    };
    let tgt_label = match target_cfg.get("label").and_then(|v| v.as_str()) {
        Some(l) if !l.trim().is_empty() => l.trim(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "target.label is required"})),
            )
                .into_response();
        }
    };
    let tgt_column = match target_cfg.get("column").and_then(|v| v.as_str()) {
        Some(c) if !c.trim().is_empty() => c.trim(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "target.column is required"})),
            )
                .into_response();
        }
    };
    let tgt_property = match target_cfg.get("property").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "target.property is required"})),
            )
                .into_response();
        }
    };

    // edge property column mappings
    struct ColMapping {
        index: usize,
        property: String,
        col_type: String,
    }
    let mut mappings: Vec<ColMapping> = Vec::new();

    let delimiter = body
        .get("delimiter")
        .and_then(|v| v.as_str())
        .unwrap_or(",");
    let delim_byte = match delimiter {
        "\t" | "tab" => b'\t',
        ";" => b';',
        "|" => b'|',
        _ => b',',
    };
    let skip_header = body
        .get("skip_header")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim_byte)
        .has_headers(skip_header)
        .from_reader(csv_data.as_bytes());

    let header_map: HashMap<String, usize> = if skip_header {
        match rdr.headers() {
            Ok(hdrs) => hdrs
                .iter()
                .enumerate()
                .map(|(i, h)| (h.to_string(), i))
                .collect(),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": format!("CSV header error: {e}")})),
                )
                    .into_response();
            }
        }
    } else {
        HashMap::new()
    };

    // source/target column indices
    let src_col_idx = if let Some(&idx) = header_map.get(src_column) {
        idx
    } else if let Ok(idx) = src_column.parse::<usize>() {
        idx
    } else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": format!("source column '{}' not found in CSV headers", src_column)}))).into_response();
    };

    let tgt_col_idx = if let Some(&idx) = header_map.get(tgt_column) {
        idx
    } else if let Ok(idx) = tgt_column.parse::<usize>() {
        idx
    } else {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": format!("target column '{}' not found in CSV headers", tgt_column)}))).into_response();
    };

    // extra edge property columns
    if let Some(columns) = body.get("columns").and_then(|v| v.as_array()) {
        for col_def in columns {
            let col_name = col_def.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let property = col_def
                .get("property")
                .and_then(|v| v.as_str())
                .unwrap_or(col_name);
            let col_type = col_def
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("string");

            if property.is_empty() {
                continue;
            }

            let index = if let Some(&idx) = header_map.get(col_name) {
                idx
            } else if let Ok(idx) = col_name.parse::<usize>() {
                idx
            } else {
                continue;
            };

            // skip the source/target key columns
            if index == src_col_idx || index == tgt_col_idx {
                continue;
            }

            mappings.push(ColMapping {
                index,
                property: property.to_string(),
                col_type: col_type.to_string(),
            });
        }
    }

    let db_name = body
        .get("database")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| state.registry.default_name());

    let entry = match state.registry.get(db_name) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": format!("database '{}' not found", db_name)})),
            )
                .into_response();
        }
    };

    // read-only databases
    if entry.read_only {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": format!("database '{}' is read-only", db_name)})),
        )
            .into_response();
    }

    // lookup maps property value -> NodeId, built from a read transaction over existing nodes
    use traverse_core::graph::types::{NodeId, PropertyKeyId};

    let (src_lookup, tgt_lookup) = {
        let read_tx = match entry.store.begin_read() {
            Ok(tx) => tx,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({"error": format!("failed to read database: {e}")})),
                )
                    .into_response();
            }
        };

        let strings = <dyn GraphView>::strings(&read_tx);

        let build_lookup = |tx: &dyn GraphView,
                            label: &str,
                            prop_key_id: PropertyKeyId|
         -> Result<HashMap<String, NodeId>, String> {
            let node_ids = tx.nodes_by_label(label);
            let mut map = HashMap::with_capacity(node_ids.len());
            for nid in node_ids {
                let node = tx.get_node(nid).map_err(|e| e.to_string())?;
                if let Some(pv) = node.properties.get(prop_key_id) {
                    let key = match &pv {
                        PropertyValue::String(s) => s.clone(),
                        PropertyValue::Int(i) => i.to_string(),
                        PropertyValue::Float(f) => f.to_string(),
                        PropertyValue::Bool(b) => b.to_string(),
                        _ => continue,
                    };
                    if map.contains_key(&key) {
                        return Err(format!(
                            "duplicate value '{}' for {}.{}",
                            key,
                            label,
                            strings.try_resolve(prop_key_id.0).unwrap_or("?")
                        ));
                    }
                    map.insert(key, nid);
                }
            }
            Ok(map)
        };

        let src_prop_id = match strings.get(src_property) {
            Some(id) => PropertyKeyId(id),
            None => {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": format!("property '{}' not found in database", src_property)}))).into_response();
            }
        };

        let src_map = match build_lookup(&read_tx, src_label, src_prop_id) {
            Ok(m) => m,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response();
            }
        };

        // same label+property: reuse the map
        let tgt_map = if tgt_label == src_label && tgt_property == src_property {
            src_map.clone()
        } else {
            let tgt_prop_id = match strings.get(tgt_property) {
                Some(id) => PropertyKeyId(id),
                None => {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": format!("property '{}' not found in database", tgt_property)}))).into_response();
                }
            };
            match build_lookup(&read_tx, tgt_label, tgt_prop_id) {
                Ok(m) => m,
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e})))
                        .into_response();
                }
            }
        };

        (src_map, tgt_map)
    };

    let start = Instant::now();
    let mut edges_created: i64 = 0;
    let mut properties_set: i64 = 0;
    let mut errors: Vec<Value> = Vec::new();
    let batch_size = 10_000;
    let edge_type_str = edge_type.to_string();

    let mut batch_count = 0;
    let mut tx = entry.store.begin_write();
    let mut row_num: i64 = 1;

    for result in rdr.records() {
        let record = match result {
            Ok(r) => r,
            Err(e) => {
                errors.push(json!({"row": row_num, "error": e.to_string()}));
                row_num += 1;
                if errors.len() > 100 {
                    break;
                }
                continue;
            }
        };

        // source node
        let src_key = match record.get(src_col_idx) {
            Some(v) if !v.is_empty() => v,
            _ => {
                errors.push(json!({"row": row_num, "error": "empty source key"}));
                row_num += 1;
                if errors.len() > 100 {
                    break;
                }
                continue;
            }
        };

        let src_id = match src_lookup.get(src_key) {
            Some(&id) => id,
            None => {
                errors.push(json!({"row": row_num, "error": format!("source node not found: '{}'", src_key)}));
                row_num += 1;
                if errors.len() > 100 {
                    break;
                }
                continue;
            }
        };

        // target node
        let tgt_key = match record.get(tgt_col_idx) {
            Some(v) if !v.is_empty() => v,
            _ => {
                errors.push(json!({"row": row_num, "error": "empty target key"}));
                row_num += 1;
                if errors.len() > 100 {
                    break;
                }
                continue;
            }
        };

        let tgt_id = match tgt_lookup.get(tgt_key) {
            Some(&id) => id,
            None => {
                errors.push(json!({"row": row_num, "error": format!("target node not found: '{}'", tgt_key)}));
                row_num += 1;
                if errors.len() > 100 {
                    break;
                }
                continue;
            }
        };

        // edge properties
        let mut props = std::collections::BTreeMap::<String, PropertyValue>::new();
        let mut prop_count: i64 = 0;

        for mapping in &mappings {
            let val = match record.get(mapping.index) {
                Some(v) if !v.is_empty() => v,
                _ => continue,
            };

            let pv = match mapping.col_type.as_str() {
                "integer" => match val.parse::<i64>() {
                    Ok(i) => PropertyValue::Int(i),
                    Err(_) => continue,
                },
                "float" => match val.parse::<f64>() {
                    Ok(f) => PropertyValue::Float(f),
                    Err(_) => continue,
                },
                "boolean" => match val.to_lowercase().as_str() {
                    "true" | "1" | "yes" => PropertyValue::Bool(true),
                    "false" | "0" | "no" => PropertyValue::Bool(false),
                    _ => continue,
                },
                _ => PropertyValue::String(val.to_string()),
            };

            props.insert(mapping.property.clone(), pv);
            prop_count += 1;
        }

        match <dyn GraphView>::create_edge(&mut tx, src_id, tgt_id, &edge_type_str, props) {
            Ok(_) => {
                edges_created += 1;
                properties_set += prop_count;
            }
            Err(e) => {
                errors.push(json!({"row": row_num, "error": e.to_string()}));
                if errors.len() > 100 {
                    break;
                }
            }
        }

        row_num += 1;
        batch_count += 1;
        if batch_count >= batch_size {
            if let Err(e) = tx.commit() {
                return commit_failed_response(e);
            }
            tx = entry.store.begin_write();
            batch_count = 0;
        }
    }

    // commit the remaining batch
    if batch_count > 0 {
        if let Err(e) = tx.commit() {
            return commit_failed_response(e);
        }
    } else {
        tx.abort();
    }

    if edges_created > 0 {
        entry.mark_dirty();
    }

    // lookup maps and CSV buffers are dropped: reclaim memory
    crate::registry::purge_allocator();

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

    axum::Json(json!({
        "edges_created": edges_created,
        "properties_set": properties_set,
        "errors": errors,
        "time_ms": elapsed_ms,
    }))
    .into_response()
}
