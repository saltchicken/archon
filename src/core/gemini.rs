use crate::core::{
    config::Config,
    ollama::strip_thinking,
    session::ChatMessage,
    tools::{ToolCall, ToolFunction},
    EngineEvent,
};
use anyhow::{bail, Context, Result};
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{io, time::Duration};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    io::StreamReader,
};
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
    let url = format!("{base}/models/{model_id}");

    let client = Client::builder()
        .timeout(Duration::from_secs(config.gemini.status_timeout.max(5)))
        .build()?;

    let resp = client
        .get(&url)
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

/// Recursively strips JSON Schema fields that Gemini's OpenAPI schema subset rejects.
fn sanitize_schema_for_gemini(val: &mut Value) {
    if let Some(obj) = val.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
        obj.remove("additionalProperties");
        for v in obj.values_mut() {
            sanitize_schema_for_gemini(v);
        }
    } else if let Some(arr) = val.as_array_mut() {
        for v in arr {
            sanitize_schema_for_gemini(v);
        }
    }
}

pub fn build_payload(
    messages: &[ChatMessage],
    system_prompt: &str,
    tools: Option<Vec<Value>>,
    include_thoughts: bool,
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
                if let Some(imgs) = m.images.as_ref() {
                    for b64 in imgs {
                        if !b64.is_empty() {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": "image/png",
                                    "data": b64
                                }
                            }));
                        }
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            "assistant" => {
                pending_tool_names.clear();
                tool_resp_idx = 0;

                let mut parts = Vec::new();
                let clean_text = strip_thinking(&m.content).0;
                if !clean_text.trim().is_empty() {
                    parts.push(json!({ "text": clean_text }));
                }
                if let Some(tcs) = m.tool_calls.as_ref() {
                    for tc in tcs {
                        pending_tool_names.push(tc.function.name.clone());
                        let mut part = json!({
                            "functionCall": {
                                "name": tc.function.name,
                                "args": tc.function.arguments
                            }
                        });
                        if let Some(sig) = &tc.thought_signature {
                            part["thoughtSignature"] = json!(sig);
                        }
                        parts.push(part);
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            "tool" => {
                let fn_name = pending_tool_names
                    .get(tool_resp_idx)
                    .cloned()
                    .unwrap_or_else(|| "tool_execution".to_string());
                tool_resp_idx += 1;

                let part = json!({
                    "functionResponse": {
                        "name": fn_name,
                        "response": { "output": m.content }
                    }
                });

                // Group consecutive function responses into a single "user" turn as required by Gemini
                let append_to_last = contents
                    .last()
                    .and_then(|last| {
                        let is_user = last.get("role").and_then(Value::as_str) == Some("user");
                        let has_fn_resp = last
                            .get("parts")
                            .and_then(Value::as_array)
                            .and_then(|p| p.first())
                            .and_then(|first| first.get("functionResponse"))
                            .is_some();
                        Some(is_user && has_fn_resp)
                    })
                    .unwrap_or(false);

                if append_to_last {
                    if let Some(parts) = contents
                        .last_mut()
                        .and_then(|l| l.get_mut("parts"))
                        .and_then(Value::as_array_mut)
                    {
                        parts.push(part);
                    }
                } else {
                    contents.push(json!({
                        "role": "user",
                        "parts": [part]
                    }));
                }
            }
            _ => {}
        }
    }

    let mut payload = json!({ "contents": contents });

    if !system_prompt.trim().is_empty() {
        payload["systemInstruction"] = json!({
            "parts": [{ "text": system_prompt }]
        });
    }

    if include_thoughts {
        payload["generationConfig"] = json!({
            "thinkingConfig": {
                "includeThoughts": true
            }
        });
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
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsage>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
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

    info!("Initiating Gemini stream request for model {}", model_id);
    debug!(
        "Gemini Payload: {}",
        serde_json::to_string(&payload).unwrap_or_default()
    );

    let resp = match client
        .post(&url)
        .header("x-goog-api-key", &api_key)
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("Gemini HTTP Request failed: {}", e);
            let _ = tx.send(EngineEvent::StreamError(format!("Request failed: {e}")));
            return;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        error!("Gemini API returned error status {}: {}", status, text);
        let _ = tx.send(EngineEvent::StreamError(format!("HTTP {status}: {text}")));
        return;
    }

    info!("Gemini stream connection established successfully.");

    let byte_stream = resp
        .bytes_stream()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e));
    let mut lines = FramedRead::new(StreamReader::new(byte_stream), LinesCodec::new());

    let mut collected_tools = Vec::new();
    let mut prompt_tokens = 0u64;
    let mut eval_tokens = 0u64;
    let mut done_reason = None;
    let mut in_thinking = false;

    let close_thinking = |in_thinking: &mut bool| {
        if std::mem::replace(in_thinking, false) {
            let _ = tx.send(EngineEvent::StreamChunk("\n</think>\n\n".into()));
        }
    };

    while let Some(line_res) = lines.next().await {
        let line = match line_res {
            Ok(l) => l,
            Err(e) => {
                error!("Gemini stream reading error: {}", e);
                let _ = tx.send(EngineEvent::StreamError(format!(
                    "Stream interrupted: {e}"
                )));
                return;
            }
        };

        let trimmed = line.trim();
        let Some(data_str) = trimmed.strip_prefix("data:") else {
            continue;
        };
        let data_str = data_str.trim();
        if data_str.is_empty() || data_str == "[DONE]" {
            continue;
        }

        let chunk = match serde_json::from_str::<GeminiStreamChunk>(data_str) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to parse Gemini SSE chunk: {e} | Data: {data_str}");
                continue;
            }
        };

        if let Some(usage) = chunk.usage_metadata {
            if let Some(p) = usage.prompt_token_count {
                prompt_tokens = p;
            }
            let cand = usage.candidates_token_count.unwrap_or(0);
            let thoughts = usage.thoughts_token_count.unwrap_or(0);
            if cand + thoughts > 0 {
                eval_tokens = cand + thoughts;
            }
        }

        if let Some(candidates) = chunk.candidates {
            for cand in candidates {
                if let Some(reason) = cand.finish_reason {
                    done_reason = Some(if reason == "MAX_TOKENS" {
                        "length".to_string()
                    } else {
                        reason.to_lowercase()
                    });
                }

                if let Some(parts) = cand.content.and_then(|c| c.parts) {
                    for part in parts {
                        if part.thought {
                            if let Some(t) = part.text.filter(|t| !t.is_empty()) {
                                if !std::mem::replace(&mut in_thinking, true) {
                                    let _ = tx.send(EngineEvent::StreamChunk("<think>\n".into()));
                                }
                                let _ = tx.send(EngineEvent::StreamChunk(t));
                            }
                        } else if let Some(text) = part.text.filter(|t| !t.is_empty()) {
                            close_thinking(&mut in_thinking);
                            let _ = tx.send(EngineEvent::StreamChunk(text));
                        }

                        if let Some(fc) = part.function_call {
                            close_thinking(&mut in_thinking);
                            collected_tools.push(ToolCall {
                                function: ToolFunction {
                                    name: fc.name,
                                    arguments: fc.args,
                                },
                                thought_signature: part.thought_signature,
                            });
                        }
                    }
                }
            }
        }
    }

    if in_thinking {
        let _ = tx.send(EngineEvent::StreamChunk("\n</think>".into()));
    }

    info!(
        "Gemini stream completed. Prompt tokens: {}, Eval tokens: {}, Done reason: {:?}, Tool calls: {}",
        prompt_tokens,
        eval_tokens,
        done_reason,
        collected_tools.len()
    );

    let _ = tx.send(EngineEvent::StreamDone {
        prompt_tokens,
        eval_tokens,
        done_reason,
        tool_calls: collected_tools,
    });
}
