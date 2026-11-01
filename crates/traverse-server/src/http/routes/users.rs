//! `/api/users`: user management.

use std::sync::Arc;

use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::auth::{Role, UserCommand, UserError};
use crate::sync::Unpoisoned as _;

use super::AppState;

// -- user management -------------------------------------------------

pub(super) async fn users_list(State(state): State<AppState>) -> impl IntoResponse {
    let catalog = state.auth_catalog.read_unpoisoned();
    let users: Vec<Value> = catalog
        .list_users()
        .into_iter()
        .map(|(name, role)| json!({ "username": name, "role": role.as_str().to_lowercase() }))
        .collect();
    axum::Json(json!(users))
}

/// Runs a user change off the async workers (bcrypt is slow on purpose; the save does I/O).
async fn change_user(state: &AppState, command: UserCommand) -> axum::response::Response {
    let users = Arc::clone(&state.auth_catalog);
    let registry = Arc::clone(&state.registry);
    let outcome = tokio::task::spawn_blocking(move || {
        crate::auth::run_user_command(command, &users, &registry, None)
    })
    .await;
    match outcome {
        Ok(Ok(_)) => axum::Json(json!({"ok": true})).into_response(),
        Ok(Err(e)) => {
            let status = match e {
                UserError::Exists(_) => StatusCode::CONFLICT,
                UserError::NotFound(_) => StatusCode::NOT_FOUND,
                UserError::DropSelf => StatusCode::FORBIDDEN,
                UserError::Hash(_) | UserError::Save(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, axum::Json(json!({"error": e.to_string()}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": format!("user change failed: {e}")})),
        )
            .into_response(),
    }
}

pub(super) async fn users_create(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let username = match body.get("username").and_then(|v| v.as_str()) {
        Some(u) => u.to_lowercase(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing 'username'"})),
            )
                .into_response();
        }
    };
    let password = match body.get("password").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "missing 'password'"})),
            )
                .into_response();
        }
    };
    let role = match body.get("role").and_then(|v| v.as_str()) {
        Some(r) => match Role::from_str_ci(r) {
            Some(role) => role,
            None => return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": format!("invalid role '{}': must be admin, editor, or reader", r)}))).into_response(),
        },
        None => Role::Reader,
    };
    change_user(
        &state,
        UserCommand::Create {
            username,
            password,
            role,
        },
    )
    .await
}

pub(super) async fn users_update(
    State(state): State<AppState>,
    Path(username): Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let username = username.to_lowercase();
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let role = match body.get("role").and_then(|v| v.as_str()) {
        Some(r) => match Role::from_str_ci(r) {
            Some(role) => Some(role),
            None => return (StatusCode::BAD_REQUEST, axum::Json(json!({"error": format!("invalid role '{}': must be admin, editor, or reader", r)}))).into_response(),
        },
        None => None,
    };
    change_user(
        &state,
        UserCommand::Alter {
            username,
            password,
            role,
        },
    )
    .await
}

pub(super) async fn users_delete(
    State(state): State<AppState>,
    Path(username): Path<String>,
) -> impl IntoResponse {
    let username = username.to_lowercase();
    change_user(&state, UserCommand::Drop { username }).await
}
