//! Databases: list, create, drop, unload, rename, load, aliases, save, quiesce and task
//! status.

use std::sync::Arc;

use axum::extract::Path;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use traverse_core::tx::view::GraphView;

use super::{AppState, AsyncParam, DB_LIFECYCLE_BUSY, blocking};

// -- Database management ---------------------------------------------

pub(super) async fn databases_list(State(state): State<AppState>) -> impl IntoResponse {
    let dbs: Vec<Value> = state
        .registry
        .list_databases()
        .iter()
        .map(|(name, is_default, read_only, aliases)| {
            json!({
                "name": name,
                "default": is_default,
                "readOnly": read_only,
                "aliases": aliases,
            })
        })
        .collect();
    axum::Json(json!(dbs)).into_response()
}

pub(super) async fn databases_create(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.trim().to_lowercase(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "name is required"})),
            )
                .into_response();
        }
    };

    let registry = Arc::clone(&state.registry);
    let db = name.clone();
    match blocking(move || registry.create_database(&db)).await {
        Ok(_) => axum::Json(json!({"ok": true, "name": name})).into_response(),
        Err(e) => (StatusCode::CONFLICT, axum::Json(json!({"error": e}))).into_response(),
    }
}

pub(super) async fn databases_drop(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<AsyncParam>,
) -> impl IntoResponse {
    if !state.allow_drop_database {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": "DROP DATABASE is disabled. Start server with TRAVERSE_ALLOW_DROP_DATABASE=true to enable."})),
        ).into_response();
    }

    let permit = match state.db_lifecycle_sem.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({"error": DB_LIFECYCLE_BUSY})),
            )
                .into_response();
        }
    };

    if params.async_mode {
        let registry = Arc::clone(&state.registry);
        let default_name = state.registry.default_name().to_string();
        let task_id = state.task_manager.spawn_blocking("drop", move || {
            let _permit = permit; // held until task completes
            registry
                .drop_database(&name, &default_name)
                .map(|()| json!({"ok": true}))
        });
        return (StatusCode::ACCEPTED, axum::Json(json!({"taskId": task_id}))).into_response();
    }

    let default_name = state.registry.default_name().to_string();
    let registry = Arc::clone(&state.registry);
    let result = blocking(move || registry.drop_database(&name, &default_name)).await;
    drop(permit);
    match result {
        Ok(()) => axum::Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response(),
    }
}

pub(super) async fn databases_unload(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<AsyncParam>,
) -> impl IntoResponse {
    let permit = match state.db_lifecycle_sem.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({"error": DB_LIFECYCLE_BUSY})),
            )
                .into_response();
        }
    };

    if params.async_mode {
        let registry = Arc::clone(&state.registry);
        let default_name = state.registry.default_name().to_string();
        let task_id = state.task_manager.spawn_blocking("unload", move || {
            let _permit = permit;
            registry
                .unload_database(&name, &default_name)
                .map(|()| json!({"ok": true}))
        });
        return (StatusCode::ACCEPTED, axum::Json(json!({"taskId": task_id}))).into_response();
    }

    let default_name = state.registry.default_name().to_string();
    let registry = Arc::clone(&state.registry);
    let result = blocking(move || registry.unload_database(&name, &default_name)).await;
    drop(permit);
    match result {
        Ok(()) => axum::Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response(),
    }
}

pub(super) async fn databases_rename(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let new_name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing or empty 'name' field"})),
            )
                .into_response();
        }
    };

    let registry = Arc::clone(&state.registry);
    let to = new_name.clone();
    match blocking(move || registry.rename_database(&name, &to)).await {
        Ok(()) => axum::Json(json!({"ok": true, "name": new_name})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e}))).into_response(),
    }
}

// -- POST /api/databases/load -----------------------------------------

pub(super) async fn databases_load(
    State(state): State<AppState>,
    Query(params): Query<AsyncParam>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.trim().to_lowercase(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "name is required"})),
            )
                .into_response();
        }
    };
    let file = match body.get("file").and_then(|v| v.as_str()) {
        Some(f) if !f.trim().is_empty() => f.trim().to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "file is required"})),
            )
                .into_response();
        }
    };

    let permit = match state.db_lifecycle_sem.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({"error": DB_LIFECYCLE_BUSY})),
            )
                .into_response();
        }
    };

    if params.async_mode {
        tracing::info!(
            "databases_load: async HTTP load requested name={} file={}",
            name,
            file
        );
        let registry = Arc::clone(&state.registry);
        let db_name = name.clone();
        let task_id = state.task_manager.spawn_blocking("load", move || {
            let _permit = permit;
            registry
                .load_database(&db_name, &file)
                .map(|_| json!({"ok": true, "name": db_name, "readOnly": true}))
        });
        return (StatusCode::ACCEPTED, axum::Json(json!({"taskId": task_id}))).into_response();
    }

    tracing::info!(
        "databases_load: sync HTTP load requested name={} file={}",
        name,
        file
    );
    let registry = Arc::clone(&state.registry);
    let db = name.clone();
    let result = blocking(move || registry.load_database(&db, &file)).await;
    drop(permit);
    match result {
        Ok(_) => axum::Json(json!({"ok": true, "name": name, "readOnly": true})).into_response(),
        Err(e) => {
            let status = if e.contains("already exists") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, axum::Json(json!({"error": e}))).into_response()
        }
    }
}

// -- aliases ---------------------------------------------------------

pub(super) async fn aliases_list(State(state): State<AppState>) -> impl IntoResponse {
    let aliases: Vec<Value> = state
        .registry
        .list_aliases()
        .into_iter()
        .map(|(alias, database)| json!({"alias": alias, "database": database}))
        .collect();
    axum::Json(json!(aliases)).into_response()
}

pub(super) async fn aliases_create(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let alias = match body.get("alias").and_then(|v| v.as_str()) {
        Some(a) if !a.trim().is_empty() => a.trim().to_lowercase(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "alias is required"})),
            )
                .into_response();
        }
    };

    let database = match body.get("database").and_then(|v| v.as_str()) {
        Some(d) if !d.trim().is_empty() => d.trim().to_lowercase(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "database is required"})),
            )
                .into_response();
        }
    };

    match state.registry.create_alias(&alias, &database) {
        Ok(()) => axum::Json(json!({"ok": true})).into_response(),
        Err(e) => {
            let status = if e.contains("already") {
                StatusCode::CONFLICT
            } else {
                StatusCode::NOT_FOUND
            };
            (status, axum::Json(json!({"error": e}))).into_response()
        }
    }
}

pub(super) async fn aliases_alter(
    State(state): State<AppState>,
    Path(name): Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let database = match body.get("database").and_then(|v| v.as_str()) {
        Some(d) if !d.trim().is_empty() => d.trim().to_lowercase(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "database is required"})),
            )
                .into_response();
        }
    };

    match state.registry.alter_alias(&name, &database) {
        Ok(()) => axum::Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, axum::Json(json!({"error": e}))).into_response(),
    }
}

pub(super) async fn aliases_drop(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.registry.drop_alias(&name) {
        Ok(()) => axum::Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, axum::Json(json!({"error": e}))).into_response(),
    }
}

// -- POST /api/save --------------------------------------------------

pub(super) async fn save_handler(
    State(state): State<AppState>,
    Query(params): Query<AsyncParam>,
) -> impl IntoResponse {
    if params.async_mode {
        let registry = Arc::clone(&state.registry);
        let task_id = state.task_manager.spawn_blocking("save", move || {
            registry
                .persist_all()
                .map(|()| json!({"status": "ok"}))
                .map_err(|e| format!("save failed: {e}"))
        });
        return (StatusCode::ACCEPTED, axum::Json(json!({"taskId": task_id}))).into_response();
    }

    let registry = Arc::clone(&state.registry);
    let saved =
        tokio::task::spawn_blocking(move || registry.persist_all().map_err(|e| e.to_string()))
            .await;
    match saved {
        Ok(Ok(())) => axum::Json(json!({ "status": "ok" })).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": format!("save failed: {}", e) })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({ "error": format!("save task failed: {}", e) })),
        )
            .into_response(),
    }
}

// -- POST /api/quiesce -----------------------------------------------

#[derive(serde::Deserialize)]
pub(super) struct QuiesceParams {
    timeout_ms: Option<u64>,
}

/// Barrier for host-driven stops (VM upgrades, planned kills): waits for in-flight background tasks
/// (async save/load), persists every dirty database synchronously and returns a node/edge inventory
/// for post-restart checks. After a 200, SIGKILL loses no acknowledged data *with `--durability
/// wal`*; in the default snapshot mode writes acked after this call are unprotected until the next
/// save, so callers fence writes first either way.
pub(super) async fn quiesce_handler(
    State(state): State<AppState>,
    Query(params): Query<QuiesceParams>,
) -> impl IntoResponse {
    let timeout = std::time::Duration::from_millis(params.timeout_ms.unwrap_or(120_000));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let running = state.task_manager.running_count();
        if running == 0 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({
                    "error": format!("quiesce timed out: {running} background task(s) still running")
                })),
            )
                .into_response();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let registry = Arc::clone(&state.registry);
    let saved =
        tokio::task::spawn_blocking(move || registry.persist_all().map_err(|e| e.to_string()))
            .await;
    match saved {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": format!("quiesce persist failed: {e}") })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": format!("quiesce persist task failed: {e}") })),
            )
                .into_response();
        }
    }

    let inventory: Vec<Value> = state
        .registry
        .list_databases()
        .into_iter()
        .map(|(name, is_default, read_only, _aliases)| {
            let entry = state.registry.get(&name);
            let (nodes, edges) = entry
                .as_ref()
                .and_then(|e| e.store.begin_read().ok())
                .map(|tx| {
                    (
                        <dyn GraphView>::node_count(&tx),
                        <dyn GraphView>::edge_count(&tx),
                    )
                })
                .unwrap_or((0, 0));
            json!({
                "name": name,
                "default": is_default,
                "read_only": read_only,
                "nodes": nodes,
                "edges": edges,
            })
        })
        .collect();

    axum::Json(json!({
        "success": true,
        "durability": if crate::registry::wal_durability_enabled() { "wal" } else { "snapshot" },
        "inventory": inventory,
    }))
    .into_response()
}

// -- GET /api/tasks/{id} ---------------------------------------------

pub(super) async fn task_status_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.task_manager.get(&id) {
        Some(task) => axum::Json(serde_json::json!({
            "id": task.id,
            "operation": task.operation,
            "status": task.status,
            "result": task.result,
            "error": task.error,
            "createdAt": task.created_at,
            "finishedAt": task.finished_at,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": format!("task not found: {id}")})),
        )
            .into_response(),
    }
}
