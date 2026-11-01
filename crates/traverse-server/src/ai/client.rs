//! One chat turn against the provider, streamed: text as the model writes it, an error when the
//! turn can't finish. Blocking (ureq); the HTTP handler runs it on the blocking pool.

use std::io::{self, BufRead, BufReader};
use std::sync::LazyLock;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::prompt::SYSTEM_PROMPT;
use super::{AiConfig, Provider};

/// Claude API version the requests are written for.
const ANTHROPIC_VERSION: &str = "2026-10-21";
/// Beta that runs a declined request again on the model Anthropic recommends for its category.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Models that take `fallbacks: "default"`.
const FALLBACK_MODELS: [&str; 4] = [
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];
/// How much of an error response is read for its message.
const ERROR_BODY_LIMIT: u64 = 64 * 1024;

/// One message of the conversation, as Studio sends it.
#[derive(Clone, Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// What a turn produces.
#[derive(Debug, PartialEq)]
pub enum ChatEvent {
    /// Answer text, in order.
    Text(String),
    /// Why the turn stopped short; always the last event.
    Error(String),
}

/// One connection pool for every turn. No overall timeout: an answer streams for as long as the
/// model writes.
fn agent() -> ureq::Agent {
    static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(30)))
            // a 4xx/5xx carries the provider's reason in its body: read it
            .http_status_as_error(false)
            .build()
            .into()
    });
    AGENT.clone()
}

/// Runs one turn: `messages` is the conversation so far, ending with the user's question,
/// `context` what Studio knows about their graph. `emit` gets every event; returning false stops
/// the turn (the client went away), which closes the connection and so ends the generation.
pub fn stream_chat(
    cfg: &AiConfig,
    messages: &[ChatMessage],
    context: Option<&str>,
    emit: &mut dyn FnMut(ChatEvent) -> bool,
) {
    let response = match cfg.provider {
        Provider::Anthropic => send_anthropic(cfg, messages, context),
        Provider::OpenAi => send_openai(cfg, messages, context),
    };
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!("AI chat: {} unreachable: {e}", cfg.base_url);
            emit(ChatEvent::Error(format!(
                "Couldn't reach the AI provider at {}: {e}",
                cfg.base_url
            )));
            return;
        }
    };
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = response
            .into_body()
            .into_with_config()
            .limit(ERROR_BODY_LIMIT)
            .read_to_string()
            .unwrap_or_default();
        let message = provider_error(cfg.provider, status, &body);
        tracing::warn!("AI chat: {message}");
        emit(ChatEvent::Error(message));
        return;
    }
    let reader = BufReader::new(response.into_body().into_reader());
    let read = match cfg.provider {
        Provider::Anthropic => read_anthropic(reader, emit),
        Provider::OpenAi => read_openai(reader, emit),
    };
    if let Err(e) = read {
        emit(ChatEvent::Error(format!(
            "The AI provider's answer broke off: {e}"
        )));
    }
}

fn send_anthropic(
    cfg: &AiConfig,
    messages: &[ChatMessage],
    context: Option<&str>,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let fallbacks = cfg.is_claude_api() && FALLBACK_MODELS.contains(&cfg.model.as_str());
    let mut request = agent()
        .post(format!("{}/v1/messages", cfg.base_url))
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("accept", "text/event-stream");
    if let Some(key) = &cfg.api_key {
        request = request.header("x-api-key", key);
    }
    if fallbacks {
        request = request.header("anthropic-beta", FALLBACK_BETA);
    }
    request.send_json(anthropic_body(cfg, messages, context, fallbacks))
}

/// The Messages API request. The instructions come first and carry the cache breakpoint (the
/// same bytes every turn); the context changes with the user's query, so it follows them.
fn anthropic_body(
    cfg: &AiConfig,
    messages: &[ChatMessage],
    context: Option<&str>,
    fallbacks: bool,
) -> Value {
    let mut system = vec![json!({
        "type": "text",
        "text": SYSTEM_PROMPT,
        "cache_control": {"type": "ephemeral"},
    })];
    if let Some(context) = context {
        system.push(json!({"type": "text", "text": context}));
    }
    let mut body = json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens.unwrap_or(super::DEFAULT_ANTHROPIC_MAX_TOKENS),
        "stream": true,
        "system": system,
        "messages": conversation(messages),
    });
    if let Some(effort) = &cfg.effort {
        body["output_config"] = json!({"effort": effort});
    }
    if fallbacks {
        body["fallbacks"] = json!("default");
    }
    body
}

fn send_openai(
    cfg: &AiConfig,
    messages: &[ChatMessage],
    context: Option<&str>,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let mut request = agent()
        .post(format!("{}/chat/completions", cfg.base_url))
        .header("accept", "text/event-stream");
    if let Some(key) = &cfg.api_key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    request.send_json(openai_body(cfg, messages, context))
}

/// The Chat Completions request: instructions and context in one system message, instructions
/// first so endpoints that cache prompt prefixes reuse them.
fn openai_body(cfg: &AiConfig, messages: &[ChatMessage], context: Option<&str>) -> Value {
    let system = match context {
        Some(context) => format!("{SYSTEM_PROMPT}\n\n{context}"),
        None => SYSTEM_PROMPT.to_string(),
    };
    let mut all = vec![json!({"role": "system", "content": system})];
    all.extend(conversation(messages));
    let mut body = json!({"model": cfg.model, "stream": true, "messages": all});
    if let Some(max_tokens) = cfg.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    body
}

fn conversation(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| json!({"role": m.role, "content": m.content}))
        .collect()
}

const CUT_OFF: &str = "The answer reached the output limit (ai_max_tokens) and was cut off.";

/// Messages API stream: forwards `text_delta`s (thinking and other blocks stay out) and turns a
/// refusal, a cut-off or an `error` event into the closing error.
fn read_anthropic(reader: impl BufRead, emit: &mut dyn FnMut(ChatEvent) -> bool) -> io::Result<()> {
    read_sse(reader, |data| {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return true;
        };
        match event["type"].as_str() {
            Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                match event["delta"]["text"].as_str() {
                    Some(text) if !text.is_empty() => emit(ChatEvent::Text(text.to_string())),
                    _ => true,
                }
            }
            Some("message_delta") => match event["delta"]["stop_reason"].as_str() {
                Some("refusal") => {
                    let category = event["delta"]["stop_details"]["category"]
                        .as_str()
                        .map(|c| format!(" ({c})"))
                        .unwrap_or_default();
                    emit(ChatEvent::Error(format!(
                        "The model declined to answer this{category}."
                    )));
                    false
                }
                Some("max_tokens") => {
                    emit(ChatEvent::Error(CUT_OFF.into()));
                    false
                }
                _ => true,
            },
            Some("error") => {
                let message = error_message(&event).unwrap_or("the provider reported an error");
                emit(ChatEvent::Error(format!(
                    "The AI provider failed: {message}"
                )));
                false
            }
            Some("message_stop") => false,
            _ => true,
        }
    })
}

/// Chat Completions stream: forwards `delta.content` until `[DONE]`.
fn read_openai(reader: impl BufRead, emit: &mut dyn FnMut(ChatEvent) -> bool) -> io::Result<()> {
    read_sse(reader, |data| {
        if data.trim() == "[DONE]" {
            return false;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return true;
        };
        if let Some(message) = error_message(&event) {
            emit(ChatEvent::Error(format!(
                "The AI provider failed: {message}"
            )));
            return false;
        }
        let choice = &event["choices"][0];
        if let Some(text) = choice["delta"]["content"].as_str()
            && !text.is_empty()
            && !emit(ChatEvent::Text(text.to_string()))
        {
            return false;
        }
        match choice["finish_reason"].as_str() {
            Some("length") => {
                emit(ChatEvent::Error(CUT_OFF.into()));
                false
            }
            Some("content_filter") => {
                emit(ChatEvent::Error(
                    "The provider's content filter stopped the answer.".into(),
                ));
                false
            }
            // "stop": a usage chunk and [DONE] may follow
            _ => true,
        }
    })
}

/// Hands each server-sent event's data (its `data:` lines joined with `\n`) to `on_data` until
/// it returns false or the stream ends. `event:`, `id:` and comment lines are skipped: both
/// providers repeat the event type inside the data.
fn read_sse(reader: impl BufRead, mut on_data: impl FnMut(&str) -> bool) -> io::Result<()> {
    let mut data = String::new();
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            if !data.is_empty() {
                let more = on_data(&data);
                data.clear();
                if !more {
                    return Ok(());
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if !data.is_empty() {
        on_data(&data);
    }
    Ok(())
}

/// `error.message` (both providers), or a bare `error` string.
fn error_message(event: &Value) -> Option<&str> {
    event["error"]["message"]
        .as_str()
        .or_else(|| event["error"].as_str())
}

/// A failed request as a sentence for the user, with a hint at the setting to check.
fn provider_error(provider: Provider, status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| error_message(&v).map(str::to_string))
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    let hint = match status {
        401 | 403 => " Check ai_api_key.",
        404 => " Check ai_model and ai_base_url.",
        429 => " The provider is rate limiting; try again shortly.",
        _ => "",
    };
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    format!("The AI provider ({provider}) answered {status}{detail}.{hint}")
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;
    use crate::ai::{AiSettings, DEFAULT_ANTHROPIC_MODEL};

    fn anthropic(base_url: Option<String>) -> AiConfig {
        AiConfig::resolve(AiSettings {
            provider: Some("anthropic".into()),
            api_key: Some("sk-test".into()),
            base_url,
            ..AiSettings::default()
        })
        .unwrap()
        .unwrap()
    }

    fn question(text: &str) -> Vec<ChatMessage> {
        vec![ChatMessage {
            role: "user".into(),
            content: text.into(),
        }]
    }

    fn collect(read: impl FnOnce(&mut dyn FnMut(ChatEvent) -> bool)) -> Vec<ChatEvent> {
        let mut events = Vec::new();
        read(&mut |e| {
            events.push(e);
            true
        });
        events
    }

    #[test]
    fn sse_events_end_at_blank_lines_and_join_their_data_lines() {
        let stream = "event: a\r\ndata: one\r\n\r\n: comment\ndata: two\ndata:three\n\ndata: tail";
        let mut seen = Vec::new();
        read_sse(stream.as_bytes(), |d| {
            seen.push(d.to_string());
            true
        })
        .unwrap();
        assert_eq!(seen, ["one", "two\nthree", "tail"]);
    }

    #[test]
    fn anthropic_stream_forwards_text_only() {
        let stream = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5-5\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"MATCH \"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"(n)\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let events = collect(|emit| read_anthropic(stream.as_bytes(), emit).unwrap());
        assert_eq!(
            events,
            [
                ChatEvent::Text("MATCH ".into()),
                ChatEvent::Text("(n)".into())
            ]
        );
    }

    #[test]
    fn anthropic_refusals_cut_offs_and_errors_close_the_turn() {
        let refusal = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"category\":\"cyber\"}}}\n\n";
        assert_eq!(
            collect(|emit| read_anthropic(refusal.as_bytes(), emit).unwrap()),
            [ChatEvent::Error(
                "The model declined to answer this (cyber).".into()
            )]
        );
        let cut =
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n";
        assert_eq!(
            collect(|emit| read_anthropic(cut.as_bytes(), emit).unwrap()),
            [ChatEvent::Error(CUT_OFF.into())]
        );
        let overloaded = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        assert_eq!(
            collect(|emit| read_anthropic(overloaded.as_bytes(), emit).unwrap()),
            [ChatEvent::Error(
                "The AI provider failed: Overloaded".into()
            )]
        );
    }

    #[test]
    fn openai_stream_forwards_content_until_done() {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"RETURN \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"1\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"total_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"after done\"}}]}\n\n",
        );
        let events = collect(|emit| read_openai(stream.as_bytes(), emit).unwrap());
        assert_eq!(
            events,
            [
                ChatEvent::Text("RETURN ".into()),
                ChatEvent::Text("1".into())
            ]
        );
        let cut = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n";
        assert_eq!(
            collect(|emit| read_openai(cut.as_bytes(), emit).unwrap()),
            [ChatEvent::Error(CUT_OFF.into())]
        );
    }

    #[test]
    fn anthropic_body_caches_the_instructions_and_sends_no_sampling_settings() {
        let cfg = anthropic(None);
        let body = anthropic_body(&cfg, &question("hi"), Some("# The user's context"), true);
        assert_eq!(body["model"], DEFAULT_ANTHROPIC_MODEL);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"][0]["text"], SYSTEM_PROMPT);
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][1]["text"], "# The user's context");
        assert!(body["system"][1].get("cache_control").is_none());
        assert_eq!(body["messages"][0]["content"], "hi");
        assert_eq!(body["fallbacks"], "default");
        for removed in ["temperature", "top_p", "top_k", "thinking", "output_config"] {
            assert!(body.get(removed).is_none(), "{removed} sent");
        }
        let without_context = anthropic_body(&cfg, &question("hi"), None, false);
        assert_eq!(without_context["system"].as_array().unwrap().len(), 1);
        assert!(without_context.get("fallbacks").is_none());
    }

    #[test]
    fn openai_body_puts_the_context_after_the_instructions() {
        let cfg = AiConfig::resolve(AiSettings {
            provider: Some("openai".into()),
            model: Some("gpt-test".into()),
            max_tokens: Some(1000),
            ..AiSettings::default()
        })
        .unwrap()
        .unwrap();
        let body = openai_body(&cfg, &question("hi"), Some("ctx"));
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][0]["content"],
            format!("{SYSTEM_PROMPT}\n\nctx")
        );
        assert_eq!(body["messages"][1]["content"], "hi");
        assert_eq!(body["max_tokens"], 1000);
    }

    #[test]
    fn provider_errors_name_the_setting_to_check() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            provider_error(Provider::Anthropic, 401, body),
            "The AI provider (anthropic) answered 401: invalid x-api-key. Check ai_api_key."
        );
        assert_eq!(
            provider_error(Provider::OpenAi, 502, ""),
            "The AI provider (openai) answered 502."
        );
    }

    /// Serves one canned response on a local port; hands back the request it got.
    fn stub(response: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 8192];
            // headers, then the JSON body (Content-Length)
            loop {
                let n = socket.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).unwrap();
            tx.send(String::from_utf8_lossy(&request).into_owned())
                .unwrap();
        });
        (url, rx)
    }

    #[test]
    fn a_turn_streams_from_the_provider() {
        let (url, request) = stub(concat!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\r\n\r\n",
            "event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n",
        ));
        let cfg = anthropic(Some(url));
        let events = collect(|emit| stream_chat(&cfg, &question("hi"), Some("ctx"), emit));
        assert_eq!(events, [ChatEvent::Text("Hello".into())]);
        let request = request.recv().unwrap();
        let lower = request.to_ascii_lowercase();
        assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
        assert!(lower.contains("x-api-key: sk-test"));
        assert!(lower.contains(&format!("anthropic-version: {ANTHROPIC_VERSION}")));
        // a gateway in front of the API: no beta it might not know
        assert!(!lower.contains("anthropic-beta"));
        assert!(request.contains("\"cache_control\""));
    }

    #[test]
    fn a_rejected_request_reports_the_providers_reason() {
        let (url, _request) = stub(concat!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"invalid x-api-key\"}}",
        ));
        let cfg = anthropic(Some(url));
        let events = collect(|emit| stream_chat(&cfg, &question("hi"), None, emit));
        assert_eq!(
            events,
            [ChatEvent::Error(
                "The AI provider (anthropic) answered 401: invalid x-api-key. Check ai_api_key."
                    .into()
            )]
        );
    }

    #[test]
    fn an_unreachable_provider_is_an_error_not_a_hang() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = anthropic(Some(format!("http://127.0.0.1:{port}")));
        let events = collect(|emit| stream_chat(&cfg, &question("hi"), None, emit));
        assert!(
            matches!(&events[..], [ChatEvent::Error(m)] if m.starts_with("Couldn't reach the AI provider"))
        );
    }
}
