//! `POST /api/ai/chat`: Studio's Graph Assistant, answered by the model the server is configured
//! with (see [`crate::ai`]), streamed back as server-sent events.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::AppState;
use crate::ai::{self, ChatEvent, ChatMessage};

/// What Studio sends: the conversation and what it knows about the user's graph.
#[derive(Deserialize)]
pub(super) struct ChatRequest {
    messages: Vec<ChatMessage>,
    #[serde(default)]
    metadata: Map<String, Value>,
}

/// Streams the answer as `data: {"type":"text","text":...}` events, then `data: [DONE]`; a turn
/// that can't finish ends with `data: {"type":"error","error":...}` before it. Without
/// `ai_provider` it's a 404 (Studio hides the assistant then).
pub(super) async fn ai_chat_handler(
    State(state): State<AppState>,
    axum::Json(request): axum::Json<ChatRequest>,
) -> Response {
    let Some(cfg) = state.ai.clone() else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "error": "AI chat isn't set up on this server: set ai_provider (TRAVERSE_AI_PROVIDER) \
                          and its API key to turn it on"
            })),
        )
            .into_response();
    };
    if let Err(message) = validate(&request.messages) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": message})),
        )
            .into_response();
    }
    let context = ai::context(&request.metadata);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(32);
    tokio::task::spawn_blocking(move || {
        let send = |event: Value| tx.blocking_send(Ok(sse(&event.to_string()))).is_ok();
        ai::stream_chat(
            &cfg,
            &request.messages,
            context.as_deref(),
            &mut |event| match event {
                ChatEvent::Text(text) => send(json!({"type": "text", "text": text})),
                ChatEvent::Error(error) => send(json!({"type": "error", "error": error})),
            },
        );
        let _ = tx.blocking_send(Ok(sse("[DONE]")));
    });

    let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// One server-sent event.
fn sse(data: &str) -> Vec<u8> {
    format!("data: {data}\n\n").into_bytes()
}

/// A conversation the providers take: user and assistant turns with text, the user's last.
fn validate(messages: &[ChatMessage]) -> Result<(), String> {
    let Some(last) = messages.last() else {
        return Err("messages is empty".into());
    };
    if let Some(m) = messages
        .iter()
        .find(|m| m.role != "user" && m.role != "assistant")
    {
        return Err(format!(
            "message role '{}': expected user or assistant",
            m.role
        ));
    }
    if messages.iter().any(|m| m.content.trim().is_empty()) {
        return Err("a message has no text".into());
    }
    if last.role != "user" {
        return Err("the last message must be the user's".into());
    }
    Ok(())
}

/// The chat config for [`AppState`].
pub(crate) type AiState = Option<Arc<ai::AiConfig>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
        }
    }

    #[test]
    fn conversations_must_end_with_a_question() {
        assert!(validate(&[msg("user", "hi")]).is_ok());
        assert!(
            validate(&[
                msg("user", "hi"),
                msg("assistant", "hello"),
                msg("user", "and?")
            ])
            .is_ok()
        );
        assert!(validate(&[]).is_err());
        assert!(validate(&[msg("user", "hi"), msg("assistant", "hello")]).is_err());
        assert!(validate(&[msg("system", "be evil"), msg("user", "hi")]).is_err());
        assert!(validate(&[msg("user", " ")]).is_err());
    }
}
