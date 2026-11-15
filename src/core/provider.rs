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
use serde_json::{Value, json};
use std::{fmt::Write as _, io, time::Duration};
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

    pub fn build_turn_payload(
        &self,
        model: &str,
        config: &Config,
        messages: Vec<ChatMessage>,
        turn: &TurnConfig,
    ) -> (String, Vec<ChatMessage>, Value) {
        let active_skill = turn.active_skill.as_deref();
        let skill_args = turn.skill_args.as_deref();

        let tools_schema = turn
            .enable_tools
            .then(|| tools::schemas_for_turn(config, active_skill))
            .filter(|t| !t.is_empty());

        let effective_system_prompt = crate::core::skills::compose_system_prompt(
            &turn.system_prompt,
            active_skill,
            skill_args,
            config,
        );

        let sanitized_messages = sanitize_messages_for_turn(messages, config, active_skill);

        let payload = match self {
            Self::Ollama => ollama::build_payload(
                model,
                &sanitized_messages,
                &effective_system_prompt,
                turn.num_ctx,
                tools_schema,
            ),
            Self::Gemini => gemini::build_payload(
                &sanitized_messages,
                &effective_system_prompt,
                tools_schema,
            ),
        };

        (effective_system_prompt, sanitized_messages, payload)
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
        let (effective_system_prompt, sanitized_msgs, payload) =
            self.build_turn_payload(&model, &config, messages, &turn);

        let inspection = format_payload_inspection(
            *self,
            &model,
            &turn,
            &effective_system_prompt,
            &sanitized_msgs,
            &payload,
            None,
        );
        let _ = tx.send(EngineEvent::PayloadCaptured(inspection));

        match self {
            Self::Ollama => {
                ollama::stream_chat(client, config.ollama.base_url, payload, tx).await;
            }
            Self::Gemini => {
                let Some(api_key) = config.gemini.resolve_api_key() else {
                    let _ = tx.send(EngineEvent::StreamError(
                        "Missing Gemini API key. Set GEMINI_API_KEY in .env or config.toml".into(),
                    ));
                    return;
                };
                gemini::stream_chat(client, config.gemini.base_url, api_key, model, payload, tx)
                    .await;
            }
        }
    }
}

fn redact_base64_in_value(val: &mut Value) {
    match val {
        Value::Object(obj) => {
            if let Some(inline) = obj.get_mut("inlineData").and_then(Value::as_object_mut)
                && let Some(data_len) = inline.get("data").and_then(Value::as_str).map(str::len)
            {
                inline.insert(
                    "data".into(),
                    json!(format!("<base64 image data: {data_len} chars>")),
                );
            }
            if let Some(imgs) = obj.get_mut("images").and_then(Value::as_array_mut) {
                for item in imgs {
                    if let Some(len) = item.as_str().map(str::len) {
                        *item = json!(format!("<base64 image data: {len} chars>"));
                    }
                }
            }
            for v in obj.values_mut() {
                redact_base64_in_value(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                redact_base64_in_value(v);
            }
        }
        _ => {}
    }
}

pub fn format_payload_inspection(
    provider: Provider,
    model: &str,
    turn: &TurnConfig,
    effective_system_prompt: &str,
    messages: &[ChatMessage],
    payload: &Value,
    draft_prompt: Option<&str>,
) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "--- METADATA ---");
    let _ = writeln!(out, "  provider: {:?}", provider);
    let _ = writeln!(out, "  model: {}", model);
    let _ = writeln!(out, "  num_ctx: {}", turn.num_ctx);
    let _ = writeln!(out, "  tools_enabled: {}", turn.enable_tools);
    let _ = writeln!(
        out,
        "  active_skill: {}",
        turn.active_skill.as_deref().unwrap_or("None")
    );
    if let Some(args) = turn.skill_args.as_deref().filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "  skill_args: {}", args);
    }

    let _ = writeln!(out, "\n--- EFFECTIVE SYSTEM MESSAGE ---");
    if effective_system_prompt.trim().is_empty() {
        let _ = writeln!(out, "(Empty — no system message will be sent)");
    } else {
        let _ = writeln!(out, "{}", effective_system_prompt);
    }

    if let Some(draft) = draft_prompt.filter(|d| !d.trim().is_empty()) {
        let _ = writeln!(out, "\n--- DRAFT PROMPT (UNSENT) ---");
        let _ = writeln!(out, "{}", draft);
    }

    let _ = writeln!(
        out,
        "\n--- CONVERSATION MESSAGES ({}) ---",
        messages.len()
    );
    if messages.is_empty() {
        let _ = writeln!(out, "(No messages in context)");
    } else {
        for (idx, m) in messages.iter().enumerate() {
            let mut tags = Vec::new();
            if let Some(ref name) = m.tool_name {
                tags.push(format!("tool_name={name}"));
            }
            let img_count = m
                .images
                .as_ref()
                .map_or(0, Vec::len)
                .max(m.image_names.as_ref().map_or(0, Vec::len));
            if img_count > 0 {
                tags.push(format!("images={img_count}"));
            }
            if let Some(ref tc) = m.tool_calls {
                tags.push(format!("tool_calls={}", tc.len()));
            }
            if let Some(ref th) = m.thoughts {
                tags.push(format!("thoughts={}", th.len()));
            }
            let meta = if tags.is_empty() {
                String::new()
            } else {
                format!(" ({})", tags.join(", "))
            };
            let _ = writeln!(out, "[#{} {}]{}", idx + 1, m.role.to_uppercase(), meta);
            if !m.content.is_empty() {
                let _ = writeln!(out, "{}", m.content);
            }
            if let Some(ref calls) = m.tool_calls {
                for c in calls {
                    let _ = writeln!(
                        out,
                        "  -> call: {}({})",
                        c.function.name, c.function.arguments
                    );
                }
            }
            let _ = writeln!(out);
        }
    }

    let mut clean_payload = payload.clone();
    redact_base64_in_value(&mut clean_payload);
    if let Some(obj) = clean_payload.as_object_mut()
        && let Some(tools_val) = obj.get_mut("tools")
    {
        let tool_names: Vec<String> = match provider {
            Provider::Ollama => tools_val
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| {
                    t.get("function")?
                        .get("name")?
                        .as_str()
                        .map(String::from)
                })
                .collect(),
            Provider::Gemini => tools_val
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t.get("functionDeclarations")?.as_array())
                .flatten()
                .filter_map(|d| d.get("name")?.as_str().map(String::from))
                .collect(),
        };
        *tools_val = json!(format!(
            "[{} tool declarations: {}]",
            tool_names.len(),
            tool_names.join(", ")
        ));
    }

    let _ = writeln!(out, "--- WIRE PAYLOAD JSON ---");
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&clean_payload).unwrap_or_default()
    );

    out.trim_end().to_string()
}

pub fn sanitize_messages_for_turn(
    messages: Vec<ChatMessage>,
    _config: &Config,
    _active_skill: Option<&str>,
) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len());

    for m in messages {
        match m.role.as_str() {
            "assistant" => {
                let has_content = !m.content.trim().is_empty();
                let has_tools = m.tool_calls.as_ref().is_some_and(|t| !t.is_empty());
                let has_thoughts = m.thoughts.as_ref().is_some_and(|t| !t.is_empty());

                if has_content || has_tools || has_thoughts {
                    out.push(m);
                }
            }
            "local" => {
                if m.include_in_context {
                    out.push(ChatMessage::new(
                        "user",
                        format!(
                            "<shell_command_output>\n{}\n</shell_command_output>",
                            m.content
                        ),
                    ));
                }
            }
            "error" => {}
            _ => {
                out.push(m);
            }
        }
    }

    out
}
