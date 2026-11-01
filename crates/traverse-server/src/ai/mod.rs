//! Studio's Graph Assistant: chat with a model the operator configures, the Claude API or any
//! OpenAI-compatible endpoint, with the user's schema and query as context. Off unless
//! `ai_provider` is set; the API key never leaves the server.

mod client;
mod prompt;

use std::fmt;

pub use client::{ChatEvent, ChatMessage, stream_chat};
pub use prompt::context;

/// Model used with the Claude API when `ai_model` is unset.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5-5";
const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Output budget per answer on the Claude API: room for thinking plus the reply (a stream, so
/// no HTTP timeout to stay under).
const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 64_000;
const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Where the assistant's answers come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// The Claude API (`POST /v1/messages`).
    Anthropic,
    /// An OpenAI-compatible `POST /chat/completions`: OpenAI, Azure OpenAI, Ollama, LM Studio,
    /// vLLM, OpenRouter, ...
    OpenAi,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAi => "openai",
        })
    }
}

/// The chat's settings, resolved at startup.
#[derive(Clone)]
pub struct AiConfig {
    pub provider: Provider,
    pub api_key: Option<String>,
    pub model: String,
    /// Without the trailing slash.
    pub base_url: String,
    /// `max_tokens` per answer; on OpenAI-compatible endpoints sent only when set.
    pub max_tokens: Option<u32>,
    /// Claude API `output_config.effort`; the model's default when unset.
    pub effort: Option<String>,
}

impl fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiConfig")
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("max_tokens", &self.max_tokens)
            .field("effort", &self.effort)
            .finish()
    }
}

/// The `ai_*` settings as configured (TOML, environment, command line), before validation.
#[derive(Clone, Default)]
pub struct AiSettings {
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub effort: Option<String>,
}

impl AiConfig {
    /// The chat's config; `None` when no provider is set (the chat is off). Refuses settings that
    /// can't work, so a typo fails at startup rather than on the first question.
    pub fn resolve(settings: AiSettings) -> Result<Option<Self>, String> {
        let nonempty =
            |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let provider = match nonempty(settings.provider).map(|p| p.to_ascii_lowercase()) {
            None => return Ok(None),
            Some(p) => match p.as_str() {
                "none" | "off" | "false" | "disabled" => return Ok(None),
                "anthropic" | "claude" => Provider::Anthropic,
                "openai" | "openai-compatible" => Provider::OpenAi,
                other => {
                    return Err(format!(
                        "unknown ai_provider '{other}': expected anthropic or openai \
                         (any OpenAI-compatible endpoint)"
                    ));
                }
            },
        };
        let api_key = nonempty(settings.api_key);
        let model = nonempty(settings.model);
        let base_url = nonempty(settings.base_url).map(|u| u.trim_end_matches('/').to_string());
        let effort = nonempty(settings.effort).map(|e| e.to_ascii_lowercase());
        if settings.max_tokens == Some(0) {
            return Err("ai_max_tokens must be at least 1".into());
        }
        match provider {
            Provider::Anthropic => {
                if api_key.is_none() {
                    return Err(
                        "ai_provider anthropic needs ai_api_key (TRAVERSE_AI_API_KEY)".into(),
                    );
                }
                if let Some(e) = &effort
                    && !EFFORT_LEVELS.contains(&e.as_str())
                {
                    return Err(format!(
                        "invalid ai_effort '{e}': expected one of {}",
                        EFFORT_LEVELS.join(", ")
                    ));
                }
                Ok(Some(Self {
                    provider,
                    api_key,
                    model: model.unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.into()),
                    base_url: base_url.unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.into()),
                    max_tokens: Some(settings.max_tokens.unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS)),
                    effort,
                }))
            }
            Provider::OpenAi => {
                let Some(model) = model else {
                    return Err("ai_provider openai needs ai_model (TRAVERSE_AI_MODEL)".into());
                };
                if effort.is_some() {
                    return Err("ai_effort applies to ai_provider anthropic only".into());
                }
                Ok(Some(Self {
                    provider,
                    // local servers (Ollama, LM Studio) take none
                    api_key,
                    model,
                    base_url: base_url.unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.into()),
                    max_tokens: settings.max_tokens,
                    effort: None,
                }))
            }
        }
    }

    /// Whether requests go to the Claude API itself (not a gateway in front of it).
    fn is_claude_api(&self) -> bool {
        self.provider == Provider::Anthropic && self.base_url == DEFAULT_ANTHROPIC_BASE_URL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(provider: &str) -> AiSettings {
        AiSettings {
            provider: Some(provider.into()),
            ..AiSettings::default()
        }
    }

    #[test]
    fn no_provider_means_no_chat() {
        assert!(AiConfig::resolve(AiSettings::default()).unwrap().is_none());
        assert!(AiConfig::resolve(settings(" off ")).unwrap().is_none());
        assert!(AiConfig::resolve(settings("")).unwrap().is_none());
    }

    #[test]
    fn anthropic_needs_a_key_and_defaults_the_rest() {
        assert!(AiConfig::resolve(settings("anthropic")).is_err());
        let cfg = AiConfig::resolve(AiSettings {
            api_key: Some("sk-ant-test".into()),
            ..settings("Anthropic")
        })
        .unwrap()
        .unwrap();
        assert_eq!(cfg.provider, Provider::Anthropic);
        assert_eq!(cfg.model, DEFAULT_ANTHROPIC_MODEL);
        assert_eq!(cfg.base_url, DEFAULT_ANTHROPIC_BASE_URL);
        assert_eq!(cfg.max_tokens, Some(DEFAULT_ANTHROPIC_MAX_TOKENS));
        assert!(cfg.is_claude_api());
        assert!(!format!("{cfg:?}").contains("sk-ant-test"));
    }

    #[test]
    fn openai_needs_a_model_but_no_key() {
        assert!(AiConfig::resolve(settings("openai")).is_err());
        let cfg = AiConfig::resolve(AiSettings {
            model: Some("llama3.3".into()),
            base_url: Some("http://localhost:11434/v1/".into()),
            ..settings("openai")
        })
        .unwrap()
        .unwrap();
        assert_eq!(cfg.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.api_key, None);
        assert_eq!(cfg.max_tokens, None);
    }

    #[test]
    fn bad_settings_fail_at_startup() {
        assert!(AiConfig::resolve(settings("gemini")).is_err());
        let anthropic = AiSettings {
            api_key: Some("k".into()),
            ..settings("anthropic")
        };
        let bad_effort = AiSettings {
            effort: Some("extreme".into()),
            ..anthropic.clone()
        };
        assert!(AiConfig::resolve(bad_effort).is_err());
        let zero = AiSettings {
            max_tokens: Some(0),
            ..anthropic
        };
        assert!(AiConfig::resolve(zero).is_err());
        let effort_on_openai = AiSettings {
            model: Some("m".into()),
            effort: Some("low".into()),
            ..settings("openai")
        };
        assert!(AiConfig::resolve(effort_on_openai).is_err());
    }
}
