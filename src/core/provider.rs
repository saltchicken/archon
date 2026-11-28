pub mod gemini;
pub mod ollama;

use crate::{
    core::{
        EngineEvent, TurnConfig,
        config::{Config, EndpointSettings},
        schema_utils,
        session::ChatMessage,
    },
    tools,
};
use anyhow::Result;
use futures_util::{Stream, TryStreamExt};
use reqwest::{Client, RequestBuilder};
use serde_json::{Map, Value, json};
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
        LinesCodec::new_with_max_length(8 * 1024 * 1024),
    ))
}

fn inline_schema_refs(val: &mut Value, defs: &Map<String, Value>, depth: usize) {
    if depth > 16 {
        return;
    }
    match val {
        Value::Object(obj) => {
            if let Some(ref_str) = obj
                .remove("$ref")
                .and_then(|v| v.as_str().map(String::from))
                && let Some(name) = schema_utils::resolve_ref_name(&ref_str)
                && let Some(Value::Object(target_obj)) = defs.get(name).cloned()
            {
                for (k, v) in target_obj {
                    obj.entry(k).or_insert(v);
                }
            }
            for (k, v) in obj.iter_mut() {
                if (k == "properties" || k == "patternProperties") && v.is_object() {
                    if let Some(props) = v.as_object_mut() {
                        for prop_schema in props.values_mut() {
                            inline_schema_refs(prop_schema, defs, depth + 1);
                        }
                    }
                } else {
                    inline_schema_refs(v, defs, depth + 1);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr {
                inline_schema_refs(v, defs, depth + 1);
            }
        }
        _ => {}
    }
}

fn flatten_schema_node(val: &mut Value, strip_keys: &[&str], mark_nullable: bool) {
    match val {
        Value::Object(obj) => {
            for k in strip_keys {
                obj.remove(*k);
            }

            if let Some(s) = obj
                .remove("const")
                .and_then(|v| v.as_str().map(String::from))
            {
                obj.insert("type".into(), json!("string"));
                obj.insert("enum".into(), json!([s]));
            }

            if let Some(Value::Array(types)) = obj.get("type").cloned() {
                let orig_len = types.len();
                let mut non_null: Vec<Value> = types
                    .into_iter()
                    .filter(|t| t.as_str() != Some("null"))
                    .collect();
                let had_null = non_null.len() < orig_len;
                if non_null.len() == 1
                    && let Some(first) = non_null.pop()
                {
                    obj.insert("type".into(), first);
                    if had_null {
                        if mark_nullable {
                            obj.insert("nullable".into(), Value::Bool(true));
                        }
                        if obj.get("default") == Some(&Value::Null) {
                            obj.remove("default");
                        }
                    }
                }
            }

            // Loop so nested wrappers like `anyOf: [{ oneOf: [...] }, { type: "null" }]`
            // (emitted by schemars for `Option<Enum>` with variant doc comments)
            // unwrap `anyOf` and then immediately flatten the exposed `oneOf`.
            for _ in 0..4 {
                let mut changed = false;
                for key in ["anyOf", "oneOf"] {
                    let Some(Value::Array(variants)) = obj.get(key).cloned() else {
                        continue;
                    };
                    let orig_len = variants.len();
                    let non_null: Vec<Value> = variants
                        .into_iter()
                        .filter(|i| {
                            i.get("type").and_then(Value::as_str) != Some("null")
                                && i.get("const") != Some(&Value::Null)
                        })
                        .collect();
                    let had_null = non_null.len() < orig_len;

                    let replaced =
                        if let Some(enum_vals) = schema_utils::extract_enum_strings(&non_null) {
                            obj.insert("type".into(), json!("string"));
                            obj.insert(
                                "enum".into(),
                                Value::Array(enum_vals.into_iter().map(Value::String).collect()),
                            );
                            true
                        } else if non_null.len() == 1
                            && let Some(Value::Object(inner)) = non_null.into_iter().next()
                        {
                            for (k, v) in inner {
                                if !strip_keys.contains(&k.as_str()) {
                                    obj.entry(k).or_insert(v);
                                }
                            }
                            true
                        } else {
                            false
                        };

                    if replaced {
                        obj.remove(key);
                        if had_null {
                            if mark_nullable {
                                obj.insert("nullable".into(), Value::Bool(true));
                            }
                            if obj.get("default") == Some(&Value::Null) {
                                obj.remove("default");
                            }
                        }
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }

            for (k, v) in obj.iter_mut() {
                if (k == "properties" || k == "patternProperties") && v.is_object() {
                    // Keys here are property names, not schema keywords.
                    if let Some(props) = v.as_object_mut() {
                        for prop_schema in props.values_mut() {
                            flatten_schema_node(prop_schema, strip_keys, mark_nullable);
                        }
                    }
                } else {
                    flatten_schema_node(v, strip_keys, mark_nullable);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr {
                flatten_schema_node(v, strip_keys, mark_nullable);
            }
        }
        _ => {}
    }
}

pub(crate) fn flatten_tool_schema(val: &mut Value, strip_keys: &[&str], mark_nullable: bool) {
    let root_defs = schema_utils::take_root_defs(val);
    if !root_defs.is_empty() {
        inline_schema_refs(val, &root_defs, 0);
    }
    flatten_schema_node(val, strip_keys, mark_nullable);
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

    #[allow(clippy::too_many_arguments)]
    pub fn build_turn_payload(
        &self,
        model: &str,
        config: &Config,
        messages: Vec<ChatMessage>,
        turn: &TurnConfig,
        session_id: Option<&str>,
        session_tools: &tools::SessionToolState,
    ) -> Value {
        let active_skill = turn.active_skill.as_deref();
        let skill_args = turn.skill_args.as_deref();
        let raw_skill_args = turn.raw_skill_args.as_deref();

        let tools_schema = turn
            .enable_tools
            .then(|| tools::schemas_for_turn(config, session_tools, active_skill))
            .filter(|t| !t.is_empty());

        let effective_system_prompt = crate::core::skills::compose_system_prompt_for_turn(
            &turn.system_prompt,
            active_skill,
            skill_args,
            raw_skill_args,
            &turn.skill_inputs,
            session_id,
            config,
            turn.enable_tools,
            session_tools,
        );

        let sanitized_messages = sanitize_messages_for_turn(messages);

        match self {
            Self::Ollama => ollama::build_payload(
                model,
                &sanitized_messages,
                &effective_system_prompt,
                turn.num_ctx,
                config.find_model(model),
                tools_schema,
            ),
            Self::Gemini => {
                gemini::build_payload(&sanitized_messages, &effective_system_prompt, tools_schema)
            }
        }
    }

    async fn stream_payload(
        &self,
        client: Client,
        model: String,
        config: Config,
        payload: Value,
        tx: UnboundedSender<EngineEvent>,
    ) {
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

    pub async fn complete_oneshot(
        &self,
        client: Client,
        model: String,
        config: Config,
        num_ctx: u32,
        system_prompt: &str,
        user_prompt: String,
    ) -> std::result::Result<(String, u64, u64), String> {
        let messages = vec![ChatMessage::new("user", user_prompt)];
        let payload = match self {
            Self::Ollama => ollama::build_payload(
                &model,
                &messages,
                system_prompt,
                num_ctx,
                config.find_model(&model),
                None,
            ),
            Self::Gemini => gemini::build_payload(&messages, system_prompt, None),
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        self.stream_payload(client, model, config, payload, tx)
            .await;

        let mut text = String::new();
        let mut prompt_tokens = 0u64;
        let mut eval_tokens = 0u64;
        while let Some(ev) = rx.recv().await {
            match ev {
                EngineEvent::StreamChunk(c) => text.push_str(&c),
                EngineEvent::StreamError(e) => return Err(e),
                EngineEvent::StreamDone {
                    prompt_tokens: pt,
                    eval_tokens: et,
                    ..
                } => {
                    prompt_tokens = pt;
                    eval_tokens = et;
                    break;
                }
                _ => {}
            }
        }
        Ok((text, prompt_tokens, eval_tokens))
    }

    /// Runs a multi-turn subagent conversation in an isolated context.
    /// The subagent can use tools across up to `MAX_SUBAGENT_TURNS` model rounds
    /// and returns the final assistant text.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_subagent(
        &self,
        client: Client,
        model: String,
        config: Config,
        num_ctx: u32,
        system_prompt: &str,
        task: &str,
        allowed_tools: Vec<String>,
        active_skill: Option<&str>,
        session_tools: &tools::SessionToolState,
    ) -> std::result::Result<String, String> {
        const MAX_SUBAGENT_TURNS: usize = 10;

        // Build the effective tool list (expanding `@group` entries and excluding
        // `"subagent"` to prevent recursion and `"load_tools"`).
        let expanded_allowed = tools::expand_tool_list(&allowed_tools, &config.tool_groups);
        let effective_tools: Vec<String> = if expanded_allowed.iter().any(|t| t == "*") {
            tools::ALL_TOOL_NAMES
                .iter()
                .filter(|&&t| {
                    t != "subagent"
                        && tools::is_tool_enabled_for_turn(t, &config, session_tools, active_skill)
                })
                .map(|&t| t.to_string())
                .collect()
        } else {
            expanded_allowed
                .into_iter()
                .filter(|t| {
                    t != "subagent"
                        && t != tools::LOAD_TOOLS_NAME
                        && tools::is_tool_enabled_for_turn(t, &config, session_tools, active_skill)
                })
                .collect()
        };

        let tools_schema =
            (!effective_tools.is_empty()).then(|| tools::schemas_for_tool_names(&effective_tools));

        let mut messages = vec![ChatMessage::new("user", task)];
        let mut final_text = String::new();

        let max_out = config.max_tool_output_for_model(&model);
        let full_cap = (num_ctx as usize).saturating_mul(4).max(4096);
        let active_skill_owned: Option<String> = active_skill.map(String::from);

        for turn_idx in 0..MAX_SUBAGENT_TURNS {
            // Emit progress so the UI can display live subagent activity.
            let progress = tools::progress::TOOL_PROGRESS_TX
                .try_with(|tx| tx.clone())
                .ok();
            if let Some(ref ptx) = progress {
                let _ = ptx.send(EngineEvent::ToolStarted {
                    summary: format!("subagent turn {} — requesting model", turn_idx + 1),
                    index: turn_idx + 1,
                    total: MAX_SUBAGENT_TURNS,
                });
            }

            let payload = match self {
                Self::Ollama => ollama::build_payload(
                    &model,
                    &messages,
                    system_prompt,
                    num_ctx,
                    config.find_model(&model),
                    tools_schema.clone(),
                ),
                Self::Gemini => {
                    gemini::build_payload(&messages, system_prompt, tools_schema.clone())
                }
            };

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            info!("Subagent turn {turn_idx}: streaming model response");
            self.stream_payload(client.clone(), model.clone(), config.clone(), payload, tx)
                .await;

            let mut text = String::new();
            let mut tool_calls = Vec::new();
            while let Some(ev) = rx.recv().await {
                match ev {
                    EngineEvent::StreamChunk(c) => text.push_str(&c),
                    EngineEvent::StreamDone { tool_calls: tc, .. } => tool_calls = tc,
                    EngineEvent::StreamError(e) => return Err(e),
                    _ => {}
                }
            }

            if tool_calls.is_empty() {
                final_text = text;
                if let Some(ref ptx) = progress {
                    let _ = ptx.send(EngineEvent::ToolStarted {
                        summary: format!("subagent turn {} — final response", turn_idx + 1),
                        index: turn_idx + 1,
                        total: MAX_SUBAGENT_TURNS,
                    });
                }
                break;
            }

            // Report which tools the subagent is calling.
            if let Some(ref ptx) = progress {
                let tool_names: Vec<&str> = tool_calls
                    .iter()
                    .map(|tc| tc.function.name.as_str())
                    .collect();
                let _ = ptx.send(EngineEvent::ToolStarted {
                    summary: format!(
                        "subagent turn {} — calling {}",
                        turn_idx + 1,
                        tool_names.join(", ")
                    ),
                    index: turn_idx + 1,
                    total: MAX_SUBAGENT_TURNS,
                });
            }

            // Record the assistant's tool-call message.
            messages.push(ChatMessage {
                content: text,
                tool_calls: Some(tool_calls.clone()),
                ..ChatMessage::new("assistant", "")
            });

            // Execute each tool call on a dedicated blocking thread so the
            // wait doesn't stall a tokio worker. The sub-tool gets a fresh
            // single-thread runtime for its own async I/O.
            for tc in &tool_calls {
                let call = tc.clone();
                let config_clone = config.clone();
                let session_tools_clone = session_tools.clone();

                let skill_owned = active_skill_owned.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to build runtime for subagent tool execution");
                    let skill_ref = skill_owned.as_deref();
                    rt.block_on(tools::execute(
                        &call,
                        &config_clone,
                        skill_ref,
                        None,
                        &session_tools_clone,
                        max_out,
                        full_cap,
                    ))
                })
                .await
                .unwrap_or_else(|e| {
                    tools::ToolExecutionOutput::error(format!(
                        "Subagent tool execution panicked: {e}"
                    ))
                });
                messages.push(ChatMessage::new_tool(
                    &tc.function.name,
                    result.truncated_output,
                    result.truncated,
                    result.is_error,
                ));
            }
        }

        if final_text.trim().is_empty() {
            Err("Subagent returned an empty response after all turns.".into())
        } else {
            Ok(final_text)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn condense_tool_output(
        &self,
        client: Client,
        model: String,
        config: Config,
        num_ctx: u32,
        max_size: usize,
        tool_name: &str,
        tool_summary: &str,
        user_query: &str,
        guide_prompt: Option<&str>,
        truncated_output: &str,
    ) -> std::result::Result<String, String> {
        let system_prompt = "You are a precise technical output condenser. Summarize and extract the most relevant information, key findings, file paths, code signatures, error messages, and data from the following tool output so the assistant can continue its task without losing critical details. Be concise, well-structured, and omit repetitive noise.";
        let query_ctx = if user_query.trim().is_empty() {
            String::new()
        } else {
            format!("User's current goal/prompt:\n{user_query}\n\n")
        };
        let guide_ctx = match guide_prompt.map(str::trim).filter(|g| !g.is_empty()) {
            Some(g) => format!("Specific user instructions for condensing this output:\n{g}\n\n"),
            None => String::new(),
        };
        let prompt = format!(
            "{query_ctx}{guide_ctx}Tool executed: {tool_summary} (tool: {tool_name})\nOutput limit applied: {max_size} bytes.\n\nPlease condense the following output into a focused summary preserving all essential technical details:\n\n<truncated_tool_output>\n{truncated_output}\n</truncated_tool_output>"
        );

        let (condensed, _, _) = self
            .complete_oneshot(client, model, config, num_ctx, system_prompt, prompt)
            .await?;
        if condensed.trim().is_empty() {
            return Err("Model returned an empty summary while condensing tool output.".into());
        }
        Ok(condensed)
    }

    pub async fn compact_conversation(
        &self,
        client: Client,
        model: String,
        config: Config,
        num_ctx: u32,
        messages: &[ChatMessage],
        custom_instructions: Option<&str>,
    ) -> std::result::Result<(String, u64, u64), String> {
        let system_prompt = "You are a precise conversation context compactor for an AI coding assistant. Summarize the conversation history so far into a structured, comprehensive technical checkpoint that can replace the prior messages without losing critical context. Preserve:\n1. User goals, requirements, and preferences.\n2. Key architectural decisions, findings, and root causes.\n3. Exact file paths, function/type signatures, and modifications made or planned.\n4. Unresolved issues, current state, and immediate next steps.\nBe concise, factual, and well-structured.";

        let mut transcript = String::new();
        for (idx, m) in sanitize_messages_for_turn(messages.to_vec())
            .iter()
            .enumerate()
        {
            let _ = writeln!(transcript, "--- Message #{} ({}) ---", idx + 1, m.role);
            if !m.content.trim().is_empty() {
                let _ = writeln!(transcript, "{}", m.content);
            }
            if let Some(ref calls) = m.tool_calls {
                for c in calls {
                    let _ = writeln!(
                        transcript,
                        "Tool Call: {}({})",
                        c.function.name, c.function.arguments
                    );
                }
            }
            let _ = writeln!(transcript);
        }

        let guide_ctx = match custom_instructions.map(str::trim).filter(|g| !g.is_empty()) {
            Some(g) => format!("Additional user instructions for this compaction:\n{g}\n\n"),
            None => String::new(),
        };

        let prompt = format!(
            "{guide_ctx}Please compact the following conversation transcript into a dense context checkpoint:\n\n<conversation_transcript>\n{transcript}</conversation_transcript>"
        );

        let (summary, prompt_tokens, eval_tokens) = self
            .complete_oneshot(client, model, config, num_ctx, system_prompt, prompt)
            .await?;
        if summary.trim().is_empty() {
            return Err("Model returned an empty summary while compacting conversation.".into());
        }
        Ok((summary, prompt_tokens, eval_tokens))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn stream_turn(
        &self,
        client: Client,
        model: String,
        config: Config,
        messages: Vec<ChatMessage>,
        turn: TurnConfig,
        session_id: Option<String>,
        session_tools: tools::SessionToolState,
        tx: UnboundedSender<EngineEvent>,
    ) {
        let payload = self.build_turn_payload(
            &model,
            &config,
            messages,
            &turn,
            session_id.as_deref(),
            &session_tools,
        );
        self.stream_payload(client, model, config, payload, tx)
            .await;
    }
}

pub fn sanitize_messages_for_turn(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
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
            "local" if m.include_in_context => {
                let body = match m.tool_name {
                    Some(ref label) => {
                        format!(
                            "<tool_output tool=\"{label}\">\n{}\n</tool_output>",
                            m.content
                        )
                    }
                    None => format!(
                        "<shell_command_output>\n{}\n</shell_command_output>",
                        m.content
                    ),
                };
                out.push(ChatMessage::new("user", body));
            }
            "hook" if m.include_in_context => {
                let label = m.tool_name.as_deref().unwrap_or("hook");
                out.push(ChatMessage::new(
                    "user",
                    format!(
                        "<hook_output tool=\"{label}\">\n{}\n</hook_output>",
                        m.content
                    ),
                ));
            }
            "local" | "hook" | "error" => {}
            _ => out.push(m),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_schema_is_flat(tool_name: &str, provider: &str, val: &Value, path: &str) {
        match val {
            Value::Object(obj) => {
                for forbidden in ["$defs", "definitions", "$ref", "oneOf", "anyOf", "const"] {
                    assert!(
                        !obj.contains_key(forbidden),
                        "{provider} schema for '{tool_name}' still contains '{forbidden}' at {path}: {obj:?}"
                    );
                }
                if let Some(ty) = obj.get("type") {
                    assert!(
                        !ty.is_array(),
                        "{provider} schema for '{tool_name}' still has array 'type' at {path}: {ty:?}"
                    );
                }
                for (k, v) in obj {
                    if (k == "properties" || k == "patternProperties") && v.is_object() {
                        if let Some(props) = v.as_object() {
                            for (prop_name, prop_schema) in props {
                                assert_schema_is_flat(
                                    tool_name,
                                    provider,
                                    prop_schema,
                                    &format!("{path}.{k}.{prop_name}"),
                                );
                            }
                        }
                    } else {
                        assert_schema_is_flat(tool_name, provider, v, &format!("{path}.{k}"));
                    }
                }
            }
            Value::Array(arr) => {
                for (idx, item) in arr.iter().enumerate() {
                    assert_schema_is_flat(tool_name, provider, item, &format!("{path}[{idx}]"));
                }
            }
            _ => {}
        }
    }

    #[test]
    fn all_registered_tools_flatten_cleanly_for_ollama_and_gemini() {
        let mut all_names: Vec<String> = tools::ALL_TOOL_NAMES
            .iter()
            .map(|&s| s.to_string())
            .collect();
        all_names.push(tools::LOAD_TOOLS_NAME.to_string());
        let expected_count = all_names.len();
        let raw_schemas = tools::schemas_for_tool_names(&all_names);
        assert_eq!(raw_schemas.len(), expected_count);

        let ollama_payload =
            ollama::build_payload("test-model", &[], "", 4096, None, Some(raw_schemas.clone()));
        let ollama_tools = ollama_payload["tools"].as_array().unwrap();
        assert_eq!(ollama_tools.len(), expected_count);

        for t in ollama_tools {
            let name = t["function"]["name"].as_str().unwrap();
            let params = &t["function"]["parameters"];
            assert_schema_is_flat(name, "ollama", params, "parameters");
        }

        let gemini_payload = gemini::build_payload(&[], "", Some(raw_schemas));
        let gemini_decls = gemini_payload["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap();
        assert_eq!(gemini_decls.len(), expected_count);

        for decl in gemini_decls {
            let name = decl["name"].as_str().unwrap();
            let params = &decl["parameters"];
            assert_schema_is_flat(name, "gemini", params, "parameters");
        }

        // Spot-check key tools for exact flattened shapes.
        let find_ollama = |name: &str| {
            ollama_tools
                .iter()
                .find(|t| t["function"]["name"] == name)
                .map(|t| &t["function"]["parameters"])
                .unwrap()
        };
        let find_gemini = |name: &str| {
            gemini_decls
                .iter()
                .find(|d| d["name"] == name)
                .map(|d| &d["parameters"])
                .unwrap()
        };

        for tool_name in ["task", "todo"] {
            for (provider, params) in [
                ("ollama", find_ollama(tool_name)),
                ("gemini", find_gemini(tool_name)),
            ] {
                let props = &params["properties"];
                assert!(
                    props.get("title").is_some(),
                    "{provider} {tool_name} missing title"
                );
                assert_eq!(
                    props["action"]["enum"],
                    json!(["list", "add", "update", "remove", "clear"]),
                    "{provider} {tool_name} action enum"
                );
                assert_eq!(
                    props["status"]["type"],
                    json!("string"),
                    "{provider} {tool_name} status type"
                );
                assert_eq!(
                    props["status"]["enum"],
                    json!(["pending", "in_progress", "blocked"]),
                    "{provider} {tool_name} status enum"
                );
                if provider == "gemini" {
                    assert_eq!(props["status"]["nullable"], json!(true));
                }
            }
        }

        for (provider, params) in [
            ("ollama", find_ollama("browser")),
            ("gemini", find_gemini("browser")),
        ] {
            assert_eq!(
                params["properties"]["source"]["type"],
                json!("string"),
                "{provider} browser.source type"
            );
            assert_eq!(
                params["properties"]["source"]["enum"],
                json!(["clipboard", "workspace_snapshot", "workspace_snapshot_tree"]),
                "{provider} browser.source enum"
            );
        }

        for (provider, params) in [
            ("ollama", find_ollama("skill")),
            ("gemini", find_gemini("skill")),
        ] {
            let arg_props = &params["properties"]["args"]["items"]["properties"];
            assert!(
                arg_props.get("type").is_some(),
                "{provider} skill.args[].type missing"
            );
            assert!(
                arg_props.get("default").is_some(),
                "{provider} skill.args[].default missing"
            );
        }

        for (provider, params) in [
            ("ollama", find_ollama("edit_file")),
            ("gemini", find_gemini("edit_file")),
        ] {
            let hunk_props = &params["properties"]["edits"]["items"]["properties"];
            assert_eq!(
                hunk_props["search_string"]["type"],
                json!("string"),
                "{provider} edit_file search_string"
            );
            assert_eq!(
                hunk_props["replace_string"]["type"],
                json!("string"),
                "{provider} edit_file replace_string"
            );
        }
    }
}
