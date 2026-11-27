use crate::{
    core::{
        EngineEvent,
        config::Config,
        provider::open_line_stream,
        session::{ChatMessage, ThoughtPart},
    },
    tools::{ToolCall, ToolFunction},
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info};

fn normalize_model_name(model: &str) -> &str {
    model.trim().trim_start_matches("models/")
}

pub async fn run_preflight(model: &str, config: &Config) -> Result<()> {
    let Some(api_key) = config.gemini.resolve_api_key() else {
        bail!(
            "Gemini API key not found. Add GEMINI_API_KEY=... to your .env file or set `gemini.api_key` in config.toml."
        );
    };

    let base = config.gemini.base_url.trim_end_matches('/');
    let model_id = normalize_model_name(model);
    let resp = Client::builder()
        .timeout(Duration::from_secs(config.gemini.status_timeout.max(5)))
        .build()?
        .get(format!("{base}/models/{model_id}"))
        .header("x-goog-api-key", &api_key)
        .send()
        .await
        .with_context(|| format!("Failed to reach Gemini API at {base}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("Gemini preflight failed for model '{model_id}' (HTTP {status}): {body}");
    }
    Ok(())
}

const GEMINI_STRIP_KEYS: &[&str] = &[
    "$schema",
    "$defs",
    "definitions",
    "$ref",
    "title",
    "additionalProperties",
    "format",
    "default",
    "examples",
    "minimum",
    "maximum",
];

fn sanitize_schema_for_gemini(val: &mut Value) {
    super::flatten_tool_schema(val, GEMINI_STRIP_KEYS, true);
}

pub fn build_payload(
    messages: &[ChatMessage],
    system_prompt: &str,
    tools: Option<Vec<Value>>,
) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut pending_tool_names: Vec<String> = Vec::new();
    let mut tool_resp_idx = 0usize;

    for m in messages {
        match m.role.as_str() {
            "user" => {
                pending_tool_names.clear();
                let mut parts = Vec::new();
                if !m.content.is_empty() {
                    parts.push(json!({ "text": m.content }));
                }
                for b64 in m.images.iter().flatten().filter(|b| !b.is_empty()) {
                    parts.push(json!({ "inlineData": { "mimeType": "image/png", "data": b64 } }));
                }
                if !parts.is_empty() {
                    let can_merge = contents.last().is_some_and(|last| {
                        last["role"] == "user"
                            && last["parts"].as_array().is_some_and(|p| {
                                p.iter().all(|item| item.get("functionResponse").is_none())
                            })
                    });
                    if can_merge
                        && let Some(existing_parts) =
                            contents.last_mut().and_then(|l| l["parts"].as_array_mut())
                    {
                        existing_parts.extend(parts);
                    } else {
                        contents.push(json!({ "role": "user", "parts": parts }));
                    }
                }
            }
            "assistant" => {
                pending_tool_names.clear();
                tool_resp_idx = 0;
                let mut parts = Vec::new();

                for tp in m.thoughts.iter().flatten() {
                    let mut part = json!({});
                    if let Some(text) = &tp.text {
                        part["text"] = json!(text);
                        part["thought"] = json!(true);
                    }
                    if let Some(sig) = &tp.thought_signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    if part.as_object().is_some_and(|o| !o.is_empty()) {
                        parts.push(part);
                    }
                }

                if !m.content.trim().is_empty() {
                    parts.push(json!({ "text": m.content }));
                }
                for tc in m.tool_calls.iter().flatten() {
                    pending_tool_names.push(tc.function.name.clone());
                    let mut part = json!({
                        "functionCall": { "name": tc.function.name, "args": tc.function.arguments }
                    });
                    if let Some(sig) = &tc.thought_signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    parts.push(part);
                }
                if !parts.is_empty() {
                    if contents.last().is_some_and(|last| last["role"] == "model")
                        && let Some(existing_parts) =
                            contents.last_mut().and_then(|l| l["parts"].as_array_mut())
                    {
                        existing_parts.extend(parts);
                    } else {
                        contents.push(json!({ "role": "model", "parts": parts }));
                    }
                }
            }
            "tool" => {
                let positional_name = pending_tool_names.get(tool_resp_idx).cloned();
                tool_resp_idx += 1;

                let fn_name = m
                    .tool_name
                    .clone()
                    .or(positional_name)
                    .unwrap_or_else(|| "tool_execution".into());

                let part = json!({
                    "functionResponse": { "name": fn_name, "response": { "output": m.content } }
                });

                let last_is_fn_resp = contents.last().is_some_and(|last| {
                    last["role"] == "user" && last["parts"][0].get("functionResponse").is_some()
                });

                if last_is_fn_resp
                    && let Some(parts) = contents.last_mut().and_then(|l| l["parts"].as_array_mut())
                {
                    parts.push(part);
                } else {
                    contents.push(json!({ "role": "user", "parts": [part] }));
                }
            }
            _ => {}
        }
    }

    let mut payload = json!({
        "contents": contents,
        "generationConfig": {
            "thinkingConfig": {
                "includeThoughts": true
            }
        }
    });
    if !system_prompt.trim().is_empty() {
        payload["systemInstruction"] = json!({ "parts": [{ "text": system_prompt }] });
    }

    if let Some(raw_tools) = tools {
        let declarations: Vec<Value> = raw_tools
            .into_iter()
            .filter_map(|mut t| {
                let mut fn_obj = t.get_mut("function")?.take();
                if let Some(params) = fn_obj.get_mut("parameters") {
                    sanitize_schema_for_gemini(params);
                }
                Some(fn_obj)
            })
            .collect();

        if !declarations.is_empty() {
            payload["tools"] = json!([{ "functionDeclarations": declarations }]);
        }
    }

    payload
}

#[derive(Deserialize)]
struct GeminiStreamChunk {
    error: Option<GeminiErrorDetail>,
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(rename = "promptFeedback")]
    prompt_feedback: Option<GeminiPromptFeedback>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsage>,
}

#[derive(Deserialize)]
struct GeminiPromptFeedback {
    #[serde(rename = "blockReason")]
    block_reason: Option<String>,
    #[serde(rename = "blockReasonMessage")]
    block_reason_message: Option<String>,
}

#[derive(Deserialize)]
struct GeminiErrorDetail {
    code: Option<i64>,
    message: Option<String>,
    status: Option<String>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
    #[serde(rename = "finishMessage")]
    finish_message: Option<String>,
}

#[derive(Deserialize)]
struct GeminiContent {
    parts: Option<Vec<GeminiPart>>,
}

#[derive(Deserialize)]
struct GeminiPart {
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    #[serde(rename = "functionCall")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(rename = "thoughtSignature")]
    thought_signature: Option<String>,
}

#[derive(Deserialize)]
struct GeminiFunctionCall {
    name: String,
    args: Value,
}

#[derive(Deserialize)]
struct GeminiUsage {
    #[serde(rename = "promptTokenCount")]
    prompt_token_count: Option<u64>,
    #[serde(rename = "candidatesTokenCount")]
    candidates_token_count: Option<u64>,
    #[serde(rename = "thoughtsTokenCount")]
    thoughts_token_count: Option<u64>,
}

fn push_thought_part(collected: &mut Vec<ThoughtPart>, text: Option<String>, sig: Option<String>) {
    let text = text.filter(|t| !t.is_empty());
    let sig = sig.filter(|s| !s.is_empty());
    if text.is_none() && sig.is_none() {
        return;
    }

    if let Some(last) = collected
        .last_mut()
        .filter(|l| l.thought_signature.is_none())
    {
        if let Some(new_t) = text {
            last.text.get_or_insert_with(String::new).push_str(&new_t);
        }
        if sig.is_some() {
            last.thought_signature = sig;
        }
        return;
    }

    collected.push(ThoughtPart {
        text,
        thought_signature: sig,
    });
}

pub async fn stream_chat(
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    payload: Value,
    tx: UnboundedSender<EngineEvent>,
) {
    let model_id = normalize_model_name(&model);
    let url = format!(
        "{}/models/{model_id}:streamGenerateContent?alt=sse",
        base_url.trim_end_matches('/')
    );
    info!("Initiating Gemini stream request for model {model_id}");
    debug!(
        "Gemini Payload: {}",
        serde_json::to_string(&payload).unwrap_or_default()
    );

    let req = client
        .post(&url)
        .header("x-goog-api-key", &api_key)
        .json(&payload);
    let Some(mut lines) = open_line_stream(req, &tx).await else {
        return;
    };

    let mut collected_tools = Vec::new();
    let mut collected_thoughts = Vec::new();
    let mut prompt_tokens = 0u64;
    let mut eval_tokens = 0u64;
    let mut done_reason = None;
    let mut raw_finish_reason = None;
    let mut finish_message = None;
    let mut has_text = false;

    while let Some(line_res) = lines.next().await {
        let line = match line_res {
            Ok(l) => l,
            Err(e) => {
                error!("Gemini stream reading error: {e}");
                let _ = tx.send(EngineEvent::StreamError(format!("Stream interrupted: {e}")));
                return;
            }
        };

        let Some(data_str) = line.trim().strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data_str.is_empty() || data_str == "[DONE]" {
            continue;
        }

        let Ok(chunk) = serde_json::from_str::<GeminiStreamChunk>(data_str) else {
            error!("Failed to parse Gemini SSE chunk | Data: {data_str}");
            continue;
        };

        if let Some(err) = chunk.error {
            let status = err
                .status
                .or_else(|| err.code.map(|c| c.to_string()))
                .unwrap_or_else(|| "UNKNOWN".into());
            let message = err.message.unwrap_or_else(|| "Unknown error".into());
            let msg = format!("Gemini stream error ({status}): {message}");
            error!("{msg}");
            let _ = tx.send(EngineEvent::StreamError(msg));
            return;
        }

        if let Some(fb) = chunk.prompt_feedback
            && let Some(reason) = fb.block_reason.filter(|r| !r.is_empty())
        {
            let detail = fb
                .block_reason_message
                .filter(|m| !m.is_empty())
                .map(|m| format!(": {m}"))
                .unwrap_or_default();
            let msg = format!("Gemini blocked prompt ({reason}){detail}");
            error!("{msg}");
            let _ = tx.send(EngineEvent::StreamError(msg));
            return;
        }

        if let Some(usage) = chunk.usage_metadata {
            prompt_tokens = usage.prompt_token_count.unwrap_or(prompt_tokens);
            let total_eval =
                usage.candidates_token_count.unwrap_or(0) + usage.thoughts_token_count.unwrap_or(0);
            if total_eval > 0 {
                eval_tokens = total_eval;
            }
        }

        for cand in chunk.candidates.into_iter().flatten() {
            if let Some(msg) = cand.finish_message.filter(|m| !m.is_empty()) {
                finish_message = Some(msg);
            }
            if let Some(reason) = cand.finish_reason {
                done_reason = Some(if reason == "MAX_TOKENS" {
                    "length".into()
                } else {
                    reason.to_lowercase()
                });
                raw_finish_reason = Some(reason);
            }

            for part in cand.content.and_then(|c| c.parts).into_iter().flatten() {
                let text = part.text.filter(|t| !t.is_empty());
                if part.thought {
                    if let Some(ref t) = text {
                        let _ = tx.send(EngineEvent::StreamThoughtChunk(t.clone()));
                    }
                } else if let Some(ref t) = text {
                    has_text = true;
                    let _ = tx.send(EngineEvent::StreamChunk(t.clone()));
                }

                let thought_text = part.thought.then_some(text).flatten();
                if let Some(fc) = part.function_call {
                    push_thought_part(&mut collected_thoughts, thought_text, None);
                    collected_tools.push(ToolCall {
                        function: ToolFunction {
                            name: fc.name,
                            arguments: fc.args,
                        },
                        thought_signature: part.thought_signature,
                    });
                } else {
                    push_thought_part(
                        &mut collected_thoughts,
                        thought_text,
                        part.thought_signature,
                    );
                }
            }
        }
    }

    if let Some(ref reason) = raw_finish_reason
        && !matches!(reason.as_str(), "STOP" | "MAX_TOKENS")
        && ((!has_text && collected_tools.is_empty())
            || matches!(
                reason.as_str(),
                "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL"
            ))
    {
        let detail = finish_message
            .as_deref()
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        let msg = format!("Gemini generation stopped ({reason}){detail}");
        error!("{msg}");
        let _ = tx.send(EngineEvent::StreamError(msg));
        return;
    }

    info!(
        "Gemini stream completed. Prompt: {prompt_tokens}, Eval: {eval_tokens}, Reason: {done_reason:?}, Tools: {}, Thoughts: {}",
        collected_tools.len(),
        collected_thoughts.len()
    );

    let _ = tx.send(EngineEvent::StreamDone {
        prompt_tokens,
        eval_tokens,
        tool_calls: collected_tools,
        thoughts: collected_thoughts,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_preserves_title_property_name() {
        // Simulates the todo tool's `items` parameter with NewTodoItem having a `title` field.
        let mut schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": { "type": "string" },
                    "priority": { "type": "string", "enum": ["low", "medium", "high"] },
                    "notes": { "type": "string" }
                }
            }
        });

        sanitize_schema_for_gemini(&mut schema);

        // The `title` property must still exist in `properties`.
        let props = schema["items"]["properties"].as_object().unwrap();
        assert!(
            props.contains_key("title"),
            "expected 'title' property to survive sanitization, got: {props:?}"
        );
        assert!(
            props.contains_key("priority"),
            "expected 'priority' property to survive sanitization"
        );
        assert!(
            props.contains_key("notes"),
            "expected 'notes' property to survive sanitization"
        );
        // Required should still be intact.
        let required = schema["items"]["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], json!("title"));
    }

    #[test]
    fn sanitize_still_strips_schema_keywords() {
        let mut schema = json!({
            "type": "object",
            "title": "My Tool",
            "format": "date-time",
            "default": "x",
            "properties": {
                "name": {
                    "type": "string",
                    "title": "The name field",
                    "default": "foo"
                }
            }
        });

        sanitize_schema_for_gemini(&mut schema);

        // Top-level `title` (string value) should be stripped.
        assert!(schema.get("title").is_none());
        // Property-level `title` (string value) should also be stripped.
        let name_schema = schema["properties"]["name"].as_object().unwrap();
        assert!(name_schema.get("title").is_none(), "inner title should be stripped");
        assert!(name_schema.get("default").is_none(), "inner default should be stripped");
        // The `name` property must still exist.
        assert!(schema["properties"].as_object().unwrap().contains_key("name"));
    }

    #[test]
    fn sanitize_flattens_optional_documented_enum() {
        let mut schema = json!({
            "type": "object",
            "$defs": {
                "TaskStatus": {
                    "oneOf": [
                        { "type": "string", "const": "pending", "description": "Queued." },
                        { "type": "string", "const": "in_progress", "description": "Active." },
                        { "type": "string", "const": "blocked", "description": "Waiting." }
                    ]
                }
            },
            "properties": {
                "status": {
                    "anyOf": [
                        { "$ref": "#/$defs/TaskStatus" },
                        { "type": "null" }
                    ],
                    "default": null,
                    "description": "Task status"
                }
            }
        });

        sanitize_schema_for_gemini(&mut schema);

        let status = &schema["properties"]["status"];
        assert_eq!(status["type"], json!("string"));
        assert_eq!(status["enum"], json!(["pending", "in_progress", "blocked"]));
        assert_eq!(status["nullable"], json!(true));
        assert!(status.get("oneOf").is_none());
        assert!(status.get("anyOf").is_none());
    }
}
