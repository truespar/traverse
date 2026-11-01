//! Studio's saved queries and styles, kept in a JSON sidecar next to the `.tvdb`.

use std::collections::HashMap;

use axum::extract::Path;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::sync::Unpoisoned as _;

use super::AppState;

// -- Studio sidecar (JSON file next to the .tvdb) -------------------
//
// Saved queries and visual styles in an envelope:
//   { "queries": [...], "styles": { "nodes": {...}, "edges": {...} } }
// A bare JSON array is read as { "queries": [...] }.

/// Database entry for sidecar operations.
fn sidecar_entry(
    state: &AppState,
    db_name: Option<&str>,
) -> Option<std::sync::Arc<crate::registry::DatabaseEntry>> {
    match db_name {
        Some(name) => state.registry.get(name),
        None => Some(state.registry.default_entry()),
    }
}

fn empty_sidecar() -> Value {
    json!({ "queries": [], "styles": { "nodes": {}, "edges": {} } })
}

/// `<stem>.studio.json` next to the database file, for a database that has one.
fn sidecar_path(entry: &crate::registry::DatabaseEntry) -> Option<std::path::PathBuf> {
    let db_path = entry.db_path()?;
    let stem = db_path.file_stem()?.to_str()?;
    Some(db_path.with_file_name(format!("{stem}.studio.json")))
}

/// The sidecar as stored, `{queries: [..], styles: {..}}` whatever the file holds: the
/// `.studio.json` file if the database has one, else `mem`. Under the entry's sidecar lock.
fn load_sidecar(entry: &crate::registry::DatabaseEntry, mem: &Value) -> Value {
    let stored = match sidecar_path(entry) {
        Some(path) => std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null),
        None => mem.clone(),
    };
    let mut sidecar = empty_sidecar();
    match stored {
        // the old layout: just the saved queries
        Value::Array(queries) => sidecar["queries"] = Value::Array(queries),
        Value::Object(mut obj) => {
            if let Some(queries @ Value::Array(_)) = obj.remove("queries") {
                sidecar["queries"] = queries;
            }
            if let Some(styles @ Value::Object(_)) = obj.remove("styles") {
                sidecar["styles"] = styles;
            }
            // anything else a newer Studio stored stays
            for (k, v) in obj {
                sidecar[k] = v;
            }
        }
        _ => {}
    }
    sidecar
}

/// Reads the sidecar.
fn read_sidecar(entry: &crate::registry::DatabaseEntry) -> Value {
    let mem = entry.sidecar.read_unpoisoned();
    load_sidecar(entry, &mem)
}

/// Read-modify-write of the sidecar under the entry's sidecar lock, so concurrent edits don't lose
/// each other. `change` returns whether it changed anything; the file is replaced crash-safely.
fn update_sidecar(
    entry: &crate::registry::DatabaseEntry,
    change: impl FnOnce(&mut Value) -> bool,
) -> Result<bool, String> {
    let mut mem = entry.sidecar.write_unpoisoned();
    let mut sidecar = load_sidecar(entry, &mem);
    if !change(&mut sidecar) {
        return Ok(false);
    }
    match sidecar_path(entry) {
        Some(path) => {
            let json = serde_json::to_vec_pretty(&sidecar).map_err(|e| e.to_string())?;
            traverse_core::storage::write_durable(&path, &json).map_err(|e| e.to_string())?;
        }
        None => *mem = sidecar,
    }
    Ok(true)
}

pub(super) async fn studio_queries_list(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let db = params.get("database").map(|s| s.as_str());
    let Some(entry) = sidecar_entry(&state, db) else {
        return axum::Json(json!([])).into_response();
    };
    let sidecar = read_sidecar(&entry);
    let queries = sidecar.get("queries").cloned().unwrap_or(json!([]));
    axum::Json(queries).into_response()
}

pub(super) async fn studio_queries_save(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let db = params.get("database").map(|s| s.as_str());
    let Some(entry) = sidecar_entry(&state, db) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "database not found"})),
        )
            .into_response();
    };

    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let query = body
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    if name.is_empty() || query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "name and query are required"})),
        )
            .into_response();
    }

    let saved = update_sidecar(&entry, |sidecar| {
        let Some(queries) = sidecar["queries"].as_array_mut() else {
            return false;
        };
        // drop an existing entry with the same name (upsert), then insert at the front
        queries.retain(|q| q.get("name").and_then(|v| v.as_str()) != Some(name));
        queries.insert(
            0,
            json!({
                "name": name,
                "query": query,
                "createdAt": chrono::Utc::now().to_rfc3339(),
            }),
        );
        true
    });
    match saved {
        Ok(_) => axum::Json(json!({"ok": true})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": e})),
        )
            .into_response(),
    }
}

pub(super) async fn studio_queries_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let db = params.get("database").map(|s| s.as_str());
    let Some(entry) = sidecar_entry(&state, db) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "database not found"})),
        )
            .into_response();
    };

    let removed = update_sidecar(&entry, |sidecar| {
        let Some(queries) = sidecar["queries"].as_array_mut() else {
            return false;
        };
        let before = queries.len();
        queries.retain(|q| q.get("name").and_then(|v| v.as_str()) != Some(name.as_str()));
        queries.len() != before
    });
    match removed {
        Ok(true) => axum::Json(json!({"ok": true})).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "query not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": e})),
        )
            .into_response(),
    }
}

// -- Studio styles ---------------------------------------------------

pub(super) async fn studio_styles_get(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let db = params.get("database").map(|s| s.as_str());
    let Some(entry) = sidecar_entry(&state, db) else {
        return axum::Json(json!({ "nodes": {}, "edges": {} })).into_response();
    };
    let sidecar = read_sidecar(&entry);
    let styles = sidecar
        .get("styles")
        .cloned()
        .unwrap_or(json!({ "nodes": {}, "edges": {} }));
    axum::Json(styles).into_response()
}

pub(super) async fn studio_styles_put(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let db = params.get("database").map(|s| s.as_str());
    let Some(entry) = sidecar_entry(&state, db) else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "database not found"})),
        )
            .into_response();
    };

    let saved = update_sidecar(&entry, |sidecar| {
        sidecar["styles"] = body;
        true
    });
    match saved {
        Ok(_) => axum::Json(json!({"ok": true})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": e})),
        )
            .into_response(),
    }
}
