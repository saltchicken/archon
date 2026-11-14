use crate::{
    core::{
        EngineEvent, TurnConfig,
        config::{Config, EndpointSettings},
        gemini, ollama,
        session::ChatMessage,
    },
    tools,
};
use anyhow::Result;
use futures_util::{Stream, TryStreamExt};
use reqwest::{Client, RequestBuilder};
use std::{io, time::Duration};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::{
    codec::{FramedRead, LinesCodec, LinesCodecError},
    io::StreamReader,
};
use tracing::{error, info};

pub async fn open_line_stream(
    req: RequestBuilder,
    tx: &UnboundedSender<EngineEvent>,
) -> Option<impl Stream<Item = std::result::Result<String, LinesCodecError>> + Unpin> {
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            error!("HTTP Request failed: {e}");
            let _ = tx.send(EngineEvent::StreamError(format!("Request failed: {e}")));
            return None;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        error!("API returned error status {status}: {text}");
        let _ = tx.send(EngineEvent::StreamError(format!("HTTP {status}: {text}")));
        return None;
    }

    info!("Stream connection established successfully.");
    let byte_stream = resp.bytes_stream().map_err(io::Error::other);
    Some(FramedRead::new(
        StreamReader::new(byte_stream),
        LinesCodec::new(),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Ollama,
    Gemini,
}

impl Provider {
    pub fn from_model(model: &str) -> Self {
        let m = model.trim();
        if m.starts_with("gemini-") || m.starts_with("models/gemini-") {
            Self::Gemini
        } else {
            Self::Ollama
        }
    }

    pub fn endpoint<'a>(&self, config: &'a Config) -> &'a EndpointSettings {
        match self {
            Self::Ollama => &config.ollama,
            Self::Gemini => &config.gemini,
        }
    }

    pub fn build_client(&self, config: &Config) -> Result<Client> {
        let ep = self.endpoint(config);
        Ok(Client::builder()
            .connect_timeout(Duration::from_secs(ep.status_timeout.max(5)))
            .read_timeout(Duration::from_secs(ep.chat_timeout))
            .build()?)
    }

    pub fn fallback_ctx(&self) -> u32 {
        match self {
            Self::Ollama => 32_768,
            Self::Gemini => 1_048_576,
        }
    }

    pub fn fallback_max_tool_output(&self) -> usize {
        match self {
            Self::Ollama => 131_072,
            Self::Gemini => 524_288,
        }
    }

    pub async fn run_preflight(&self, model: &str, config: &Config) -> Result<()> {
        match self {
            Self::Ollama => ollama::run_preflight(model, config).await,
            Self::Gemini => gemini::run_preflight(model, config).await,
        }
    }

    pub async fn stream_turn(
        &self,
        client: Client,
        model: String,
        config: Config,
        messages: Vec<ChatMessage>,
        turn: TurnConfig,
        tx: UnboundedSender<EngineEvent>,
    ) {
        let active_skill = turn.active_skill.as_deref();

        let tools_schema = turn
            .enable_tools
            .then(|| tools::schemas_for_turn(&config, active_skill))
            .filter(|t| !t.is_empty());

        let effective_system_prompt =
            crate::core::skills::compose_system_prompt(&turn.system_prompt, active_skill, &config);

        // Strip any historical tool_calls for tools that are currently disabled
        let messages = sanitize_messages_for_turn(messages, &config, active_skill);

        match self {
            Self::Ollama => {
                let payload = ollama::build_payload(
                    &model,
                    &messages,
                    &effective_system_prompt,
                    turn.num_ctx,
                    tools_schema,
                );
                ollama::stream_chat(client, config.ollama.base_url, payload, tx).await;
            }
            Self::Gemini => {
                let Some(api_key) = config.gemini.resolve_api_key() else {
                    let _ = tx.send(EngineEvent::StreamError(
                        "Missing Gemini API key. Set GEMINI_API_KEY in .env or config.toml".into(),
                    ));
                    return;
                };
                let payload =
                    gemini::build_payload(&messages, &effective_system_prompt, tools_schema);
                gemini::stream_chat(client, config.gemini.base_url, api_key, model, payload, tx)
                    .await;
            }
        }
    }
}

pub fn sanitize_messages_for_turn(
    messages: Vec<ChatMessage>,
    config: &Config,
    active_skill: Option<&str>,
) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len());
    let mut active_call_mask: Vec<bool> = Vec::new();
    let mut tool_idx = 0usize;

    for mut m in messages {
        match m.role.as_str() {
            "assistant" => {
                active_call_mask.clear();
                tool_idx = 0;

                if let Some(calls) = m.tool_calls.take() {
                    let mut kept_calls = Vec::with_capacity(calls.len());
                    for tc in calls {
                        let enabled = tools::is_tool_enabled_for_turn(
                            &tc.function.name,
                            config,
                            active_skill,
                        );
                        active_call_mask.push(enabled);
                        if enabled {
                            kept_calls.push(tc);
                        }
                    }
                    m.tool_calls = (!kept_calls.is_empty()).then_some(kept_calls);
                }

                let has_content = !m.content.trim().is_empty();
                let has_tools = m.tool_calls.is_some();
                let has_thoughts = m.thoughts.as_ref().is_some_and(|t| !t.is_empty());

                if has_content || has_tools || has_thoughts {
                    out.push(m);
                }
            }
            "tool" => {
                let keep = if let Some(ref name) = m.tool_name {
                    let positional_ok = active_call_mask.get(tool_idx).copied().unwrap_or(true);
                    positional_ok && tools::is_tool_enabled_for_turn(name, config, active_skill)
                } else {
                    active_call_mask.get(tool_idx).copied().unwrap_or(false)
                };
                tool_idx += 1;

                if keep {
                    out.push(m);
                }
            }
            _ => {
                active_call_mask.clear();
                tool_idx = 0;
                out.push(m);
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolCall, ToolFunction};
    use serde_json::json;

    #[test]
    fn strips_disabled_tool_calls_and_matching_tool_responses_in_lockstep() {
        let mut cfg = Config::default();
        cfg.app.enabled_tools = vec!["read_file".into()];

        let mut assistant = ChatMessage::new("assistant", "");
        assistant.tool_calls = Some(vec![
            ToolCall {
                function: ToolFunction {
                    name: "cargo".into(),
                    arguments: json!({"action": "check"}),
                },
                thought_signature: None,
            },
            ToolCall {
                function: ToolFunction {
                    name: "read_file".into(),
                    arguments: json!({"file_path": "Cargo.toml"}),
                },
                thought_signature: None,
            },
        ]);

        let tool_1 = ChatMessage::new("tool", "cargo check output");
        let tool_2 = ChatMessage::new("tool", "[package]\nname = \"archon\"");

        let sanitized = sanitize_messages_for_turn(vec![assistant, tool_1, tool_2], &cfg, None);
        assert_eq!(sanitized.len(), 2);
        assert_eq!(
            sanitized[0].tool_calls.as_ref().unwrap()[0].function.name,
            "read_file"
        );
        assert_eq!(sanitized[1].role, "tool");
        assert_eq!(sanitized[1].content, "[package]\nname = \"archon\"");
    }
}
