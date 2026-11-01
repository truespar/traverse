//! HTTP API: the router with its auth layer and shared state; handlers live by area in the
//! submodules.

mod ai;
mod assets;
mod databases;
mod files;
mod import;
mod json;
mod query;
mod status;
mod studio;
mod users;
mod ws;

use std::sync::{Arc, RwLock};
use std::time::Instant;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use serde::Deserialize;
use serde_json::json;
use tower_http::cors::CorsLayer;

use crate::auth::AuthCatalog;
use crate::registry::DatabaseRegistry;

use super::tasks::TaskManager;

use ai::ai_chat_handler;
use assets::static_handler;
use databases::{
    aliases_alter, aliases_create, aliases_drop, aliases_list, databases_create, databases_drop,
    databases_list, databases_load, databases_rename, databases_unload, quiesce_handler,
    save_handler, task_status_handler,
};
use files::{files_delete, files_download, files_list, files_upload};
use import::{csv_edge_import_handler, csv_import_handler, import_handler};
use query::query_handler;
use status::{
    algorithms_list_handler, health_handler, metrics_handler, openapi_spec_handler, ready_handler,
    scalar_docs_handler, schema_handler,
};
use studio::{
    studio_queries_delete, studio_queries_list, studio_queries_save, studio_styles_get,
    studio_styles_put,
};
use users::{users_create, users_delete, users_list, users_update};
use ws::ws_handler;

#[cfg(feature = "mcp")]
pub(crate) use json::{cypher_value_to_json, json_to_cypher_params};

/// Shared state for all HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<DatabaseRegistry>,
    pub auth_catalog: Arc<RwLock<AuthCatalog>>,
    pub start_time: Instant,
    pub allow_drop_database: bool,
    pub api_key: Option<String>,
    /// Studio's Graph Assistant; `None` when no `ai_provider` is set.
    pub ai: ai::AiState,
    pub task_manager: Arc<TaskManager>,
    /// At most one load/unload/drop at a time.
    pub db_lifecycle_sem: Arc<tokio::sync::Semaphore>,
}

const DB_LIFECYCLE_BUSY: &str =
    "another database operation (load/unload/drop) is already in progress";

/// `?async=true`: run as an async task.
#[derive(Deserialize)]
struct AsyncParam {
    #[serde(rename = "async", default)]
    async_mode: bool,
}

/// API key from the request headers or query string.
fn extract_api_key(req: &axum::extract::Request) -> Option<String> {
    // Authorization: Bearer <key>
    if let Some(auth) = req.headers().get("authorization")
        && let Ok(val) = auth.to_str()
        && let Some(key) = val.strip_prefix("Bearer ")
    {
        return Some(key.to_string());
    }
    // X-API-Key: <key>
    if let Some(key) = req.headers().get("x-api-key")
        && let Ok(val) = key.to_str()
    {
        return Some(val.to_string());
    }
    // ?token=<key> (WebSocket)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("token=") {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Constant-time comparison for API keys.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Middleware enforcing API key auth on sensitive endpoints.
async fn api_key_auth(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path();

    // public endpoints and static files skip auth (the Studio SPA must load without it)
    let is_public = matches!(
        path,
        "/api/health" | "/api/ready" | "/api/docs" | "/api/openapi.json" | "/metrics"
    ) || (!path.starts_with("/api/") && path != "/ws");

    if is_public {
        return next.run(req).await;
    }

    let Some(ref expected) = state.api_key else {
        // no key configured = open access
        return next.run(req).await;
    };

    let provided = extract_api_key(&req);

    if provided
        .as_deref()
        .is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()))
    {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "Invalid or missing API key"})),
        )
            .into_response()
    }
}

/// Axum router with all routes and middleware.
pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers(tower_http::cors::Any);

    #[allow(unused_mut)]
    let mut router = Router::new()
        .route("/api/health", get(health_handler))
        .route("/api/ready", get(ready_handler))
        .route("/api/schema", get(schema_handler))
        .route("/api/query", post(query_handler))
        .route("/metrics", get(metrics_handler))
        .route("/ws", get(ws_handler))
        .route("/api/databases", get(databases_list).post(databases_create))
        .route("/api/databases/{name}", delete(databases_drop))
        .route("/api/databases/{name}/unload", post(databases_unload))
        .route("/api/databases/{name}/rename", post(databases_rename))
        .route("/api/databases/load", post(databases_load))
        .route("/api/files", get(files_list))
        .route(
            "/api/files/{name}",
            get(files_download)
                .put(files_upload)
                .delete(files_delete)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/api/aliases", get(aliases_list).post(aliases_create))
        .route(
            "/api/aliases/{name}",
            put(aliases_alter).delete(aliases_drop),
        )
        .route(
            "/api/import",
            post(import_handler).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
        )
        .route(
            "/api/import/csv",
            post(csv_import_handler).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
        )
        .route(
            "/api/import/csv/edges",
            post(csv_edge_import_handler).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
        )
        .route("/api/save", post(save_handler))
        .route("/api/quiesce", post(quiesce_handler))
        .route("/api/tasks/{id}", get(task_status_handler))
        .route("/api/users", get(users_list).post(users_create))
        .route(
            "/api/users/{username}",
            put(users_update).delete(users_delete),
        )
        .route(
            "/api/studio/queries",
            get(studio_queries_list).post(studio_queries_save),
        )
        .route("/api/studio/queries/{name}", delete(studio_queries_delete))
        .route(
            "/api/studio/styles",
            get(studio_styles_get).put(studio_styles_put),
        )
        .route("/api/algorithms", get(algorithms_list_handler))
        .route("/api/ai/chat", post(ai_chat_handler))
        .route(
            "/api",
            get(|| async { axum::response::Redirect::permanent("/api/docs") }),
        )
        .route("/api/docs", get(scalar_docs_handler))
        .route("/api/openapi.json", get(openapi_spec_handler));

    #[cfg(feature = "mcp")]
    {
        router = router.route(
            "/api/mcp",
            post(super::mcp::mcp_post_handler)
                .get(super::mcp::mcp_get_handler)
                .delete(super::mcp::mcp_delete_handler),
        );
    }

    router
        .fallback(static_handler)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api_key_auth,
        ))
        .layer(cors)
        .with_state(state)
}

/// 500 response for a commit failure (redo-log / disk I/O), so the import handlers return an error
/// instead of aborting the process.
fn commit_failed_response<E: std::fmt::Display>(e: E) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({ "error": format!("commit failed: {e}") })),
    )
        .into_response()
}

/// Runs registry work (file I/O, up to a whole database's worth) off the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| Err(format!("internal error: {e}")))
}
