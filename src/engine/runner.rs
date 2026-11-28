use super::{CoreEngine, EngineEvent, ToolExecSource, TruncatedToolAction, TurnConfig};
use crate::config;
use crate::session::ChatMessage;
use crate::tools::{self, ToolCall, ToolExecutionOutput};
use tracing::{error, info, warn};

pub(super) struct PendingTruncatedTool {
    pub(super) source: ToolExecSource,
    pub(super) summary: String,
    pub(super) truncated_output: String,
    pub(super) full_output: String,
    pub(super) raw_bytes: usize,
    pub(super) full_cap_bytes: usize,
    pub(super) index: usize,
    pub(super) total: usize,
}

impl CoreEngine {
    pub(super) fn unresolved_tool_names(&self) -> Vec<String> {
        let mut resolved = 0usize;
        for m in self.session.messages.iter().rev() {
            match m.role.as_str() {
                "tool" => resolved += 1,
                "error" => continue,
                "assistant" => {
                    let Some(calls) = m.tool_calls.as_ref() else {
                        return Vec::new();
                    };
                    if resolved >= calls.len() {
                        return Vec::new();
                    }
                    return calls[resolved..]
                        .iter()
                        .map(|tc| tc.function.name.clone())
                        .collect();
                }
                _ => break,
            }
        }
        Vec::new()
    }

    pub(super) fn close_unresolved_tool_calls(&mut self, message: &str) -> usize {
        let unresolved_names = self.unresolved_tool_names();
        let count = unresolved_names.len();
        self.inflight.pending_tools.clear();
        self.inflight.pending_truncated = None;
        for name in unresolved_names {
            self.session
                .messages
                .push(ChatMessage::new_tool(name, message, false, false));
        }
        count
    }

    pub(super) fn sync_tool_side_effects(&mut self, tool_name: &str) {
        if tool_name == "skill" {
            self.refresh_skills();
        } else if tool_name == "memory" {
            let (global_memory, workspace_memory) = config::reload_memories(&self.config);
            self.config.global_memory = global_memory.clone();
            self.config.workspace_memory = workspace_memory.clone();
            let _ = self.tx.send(EngineEvent::MemoryUpdated {
                global_memory,
                workspace_memory,
            });
        } else if tool_name == "session" {
            if let Ok(data) = self.sessions.load(&self.session.id) {
                self.session.custom_title = data.custom_title();
                self.session.is_pinned = data.pinned;
                self.emit_chat();
            }
            let list = self.sessions.list();
            let _ = self.tx.send(EngineEvent::SessionsListed(list));
        }
    }

    fn is_call_auto_approved(&self, call: &ToolCall) -> bool {
        if call.function.name == tools::LOAD_TOOLS_NAME {
            return true;
        }
        let global_or_session = tools::is_auto_approved(
            call,
            self.config
                .app
                .auto_approve
                .iter()
                .chain(self.session.auto_approve.iter())
                .map(String::as_str),
        );
        if global_or_session {
            return true;
        }
        self.active_turn_skill()
            .and_then(|name| crate::skills::find_skill(&self.config, name))
            .is_some_and(|s| s.auto_approve_allows(call))
    }

    pub(super) fn tool_batch_progress(&self) -> (usize, usize) {
        let pending_len = self.inflight.pending_tools.len();
        let total = self
            .session
            .messages
            .iter()
            .rev()
            .find_map(|m| m.tool_calls.as_ref().map(Vec::len))
            .unwrap_or(pending_len)
            .max(pending_len);
        let index = total.saturating_sub(pending_len) + 1;
        (index, total)
    }

    fn prompt_next_tool(&mut self) {
        let Some(call) = self.inflight.pending_tools.front() else {
            return;
        };
        let (index, total) = self.tool_batch_progress();
        let formatted_preview =
            tools::format_approval(call, &self.config, self.active_turn_skill());
        let _ = self.tx.send(EngineEvent::ToolApprovalRequested {
            formatted_preview,
            index,
            total,
        });
    }

    pub(super) fn context_padded_capture_limit(&self) -> usize {
        let ctx_tokens = self.turn.num_ctx as usize;
        // Reserve at least 25% of the context window (minimum 2048 tokens) for prompt overhead and model response.
        let padding_tokens = (ctx_tokens / 4).max(2048);
        let avail_tokens = ctx_tokens.saturating_sub(padding_tokens).max(512);
        // Roughly ~4 bytes per token, capped to fit within the context window budget.
        avail_tokens.saturating_mul(4)
    }

    fn apply_load_tools_side_effect(&mut self, arguments: &serde_json::Value) {
        let Ok(parsed) =
            serde_json::from_value::<tools::executor::load_tools::LoadToolsArgs>(arguments.clone())
        else {
            return;
        };
        let Ok(res) = tools::executor::load_tools::resolve_load_tools_request(
            &parsed.names,
            &self.config,
            &self.session.tools,
            self.active_turn_skill(),
        ) else {
            return;
        };
        if !res.newly_loaded.is_empty() {
            let core_count = tools::core_regular_tools_for_turn(
                &self.config,
                &self.session.tools,
                self.active_turn_skill(),
            )
            .len();
            let evicted = self.session.tools.apply_loaded(
                &res.newly_loaded,
                core_count,
                self.config.app.max_loaded_tools,
            );
            info!(
                "Activated deferred tool(s): {:?} (evicted: {:?})",
                res.newly_loaded, evicted
            );
            let _ = self
                .tx
                .send(EngineEvent::SessionToolsUpdated(self.session.tools.clone()));
        }
    }

    async fn complete_tool_exec(
        &mut self,
        source: ToolExecSource,
        output: String,
        truncated: bool,
        is_error: bool,
    ) {
        let is_model_tool = matches!(source, ToolExecSource::Model { .. });
        let affects_context = source.include_in_context()
            || matches!(
                &source,
                ToolExecSource::ManualTool { tool_name, .. }
                    if matches!(tool_name.as_str(), "memory" | "plan" | tools::LOAD_TOOLS_NAME)
            );
        match source {
            ToolExecSource::Model {
                tool_name,
                arguments,
                ..
            } => {
                if tool_name == tools::LOAD_TOOLS_NAME && !is_error {
                    self.apply_load_tools_side_effect(&arguments);
                }
                self.sync_tool_side_effects(&tool_name);
                self.session.messages.push(ChatMessage::new_tool(
                    tool_name, output, truncated, is_error,
                ));
                info!("Tool execution completed and recorded to chat history.");
            }
            ToolExecSource::ManualShell {
                command,
                include_in_context,
            } => {
                self.session.messages.push(ChatMessage {
                    truncated,
                    include_in_context,
                    ..ChatMessage::new("local", format!("$ {command}\n{output}"))
                });
            }
            ToolExecSource::ManualTool {
                tool_name,
                summary,
                arguments,
                include_in_context,
            } => {
                if tool_name == tools::LOAD_TOOLS_NAME && !is_error {
                    self.apply_load_tools_side_effect(&arguments);
                }
                self.sync_tool_side_effects(&tool_name);
                self.session.messages.push(ChatMessage {
                    truncated,
                    include_in_context,
                    tool_name: Some(summary),
                    ..ChatMessage::new("local", output)
                });
            }
        }

        self.save_and_emit_chat(Vec::new()).await;
        if affects_context {
            self.emit_estimated_token_usage();
        }
        let _ = self.tx.send(EngineEvent::ToolCompleted);
        if is_model_tool {
            self.process_pending_tools().await;
        } else {
            self.flush_queued_interjections().await;
        }
    }

    fn spawn_tool_condensation(
        &mut self,
        pending: &PendingTruncatedTool,
        use_full: bool,
        guide_prompt: Option<String>,
    ) {
        let tool_name = match &pending.source {
            ToolExecSource::Model { tool_name, .. } => tool_name.clone(),
            ToolExecSource::ManualShell { .. } => "shell_command".to_string(),
            ToolExecSource::ManualTool { summary, .. } => summary.clone(),
        };
        let mode_label = if use_full { "full" } else { "truncated" };
        let _ = self.tx.send(EngineEvent::ToolStarted {
            summary: format!("condensing ({mode_label}) {}", pending.summary),
            index: pending.index,
            total: pending.total,
        });

        let provider = self.provider;
        let client = self.client.clone();
        let model = self.model.clone();
        let config = self.config.clone();
        let num_ctx = self.turn.num_ctx;
        let limit_used = if use_full {
            pending.full_cap_bytes
        } else {
            self.active_max_tool_output_size
        };
        let source_text = if use_full {
            pending.full_output.clone()
        } else {
            pending.truncated_output.clone()
        };
        let summary = pending.summary.clone();

        let last_user_query = self
            .session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user" && !m.content.trim().is_empty())
            .map(|m| m.content.clone())
            .unwrap_or_default();

        self.spawn_with_tx(move |tx| async move {
            let res = provider
                .condense_tool_output(
                    client,
                    model,
                    config,
                    num_ctx,
                    limit_used,
                    &tool_name,
                    &summary,
                    &last_user_query,
                    guide_prompt.as_deref(),
                    &source_text,
                )
                .await;
            let _ = tx.send(EngineEvent::ToolCondensed(res));
        });
    }

    fn spawn_tool_execution(
        &mut self,
        call: ToolCall,
        index: usize,
        total: usize,
        feedback: Option<String>,
    ) {
        let sig = tools::call_signature(&call);
        let _ = self.tx.send(EngineEvent::ToolStarted {
            summary: sig.clone(),
            index,
            total,
        });

        let cfg = self.config.clone();
        let active_skill = self.active_turn_skill().map(String::from);
        let session_id = self.session.id.clone();
        let session_tools = self.session.tools.clone();
        let max_out = self.active_max_tool_output_size;
        let full_cap = self.context_padded_capture_limit();
        let source = ToolExecSource::Model {
            tool_name: call.function.name.clone(),
            summary: sig,
            arguments: call.function.arguments.clone(),
        };

        self.spawn_with_tx(move |tx| async move {
            let mut res = tools::execute(
                &call,
                &cfg,
                active_skill.as_deref(),
                Some(session_id.as_str()),
                &session_tools,
                max_out,
                full_cap,
            )
            .await;
            if let Some(fb) = feedback.filter(|s| !s.is_empty()) {
                let suffix = format!("\n\n[User attached a message]: {fb}");
                res.truncated_output.push_str(&suffix);
                res.full_output.push_str(&suffix);
            }
            let _ = tx.send(EngineEvent::ToolExecFinished(source, res));
        });
    }

    pub(super) async fn process_pending_tools(&mut self) {
        // Ensure any in-flight background session save has completed before
        // spawning the next tool, to avoid concurrent WAL write-lock contention.
        self.flush_save_task().await;

        if self.flush_queued_interjections().await {
            return;
        }

        let mut modified = false;

        while let Some(call) = self.inflight.pending_tools.front() {
            if !tools::is_tool_enabled_for_turn(
                &call.function.name,
                &self.config,
                &self.session.tools,
                self.active_turn_skill(),
            ) {
                let call = self.inflight.pending_tools.pop_front().unwrap();
                warn!(
                    "Rejecting disabled tool call '{}' without prompting",
                    call.function.name
                );
                let err_msg = format!("Error: Unknown tool: '{}'", call.function.name);
                self.session.messages.push(ChatMessage::new_tool(
                    call.function.name,
                    err_msg,
                    false,
                    true,
                ));
                modified = true;
                continue;
            }

            if !self.is_call_auto_approved(call) {
                if modified {
                    self.save_and_emit_chat(Vec::new()).await;
                    self.emit_estimated_token_usage();
                }
                self.prompt_next_tool();
                return;
            }

            let (index, total) = self.tool_batch_progress();
            let call = self.inflight.pending_tools.pop_front().unwrap();
            let sig = tools::call_signature(&call);
            info!(
                "Auto-approving tool call '{}' ({index}/{total}, max_tool_output_size: {})",
                sig, self.active_max_tool_output_size
            );

            if modified {
                self.save_and_emit_chat(Vec::new()).await;
                self.emit_estimated_token_usage();
            }
            self.flush_save_task().await;
            self.spawn_tool_execution(call, index, total, None);
            return;
        }

        if modified {
            self.save_and_emit_chat(Vec::new()).await;
            self.emit_estimated_token_usage();
        }

        let turn = self.turn.clone();
        self.spawn_model_turn(&turn).await;
    }

    pub(super) async fn handle_resolve_tool(
        &mut self,
        approved: bool,
        always_approve: bool,
        feedback: String,
        turn: TurnConfig,
    ) {
        self.update_turn_preserving_skill(turn);
        let (index, total) = self.tool_batch_progress();
        let Some(call) = self.inflight.pending_tools.pop_front() else {
            return;
        };

        let sig = tools::call_signature(&call);
        if approved && always_approve {
            info!("Adding '{}' to session auto-approve rules", sig);
            self.session.auto_approve.insert(sig.clone());
        }

        info!(
            "Resolving tool call: {}. Approved: {}, Always: {} (max_tool_output_size: {})",
            sig, approved, always_approve, self.active_max_tool_output_size
        );

        let trimmed_fb = feedback.trim().to_string();
        if approved {
            // Flush any pending background session save to release the WAL
            // write lock before the next tool opens its own connection.
            self.flush_save_task().await;
            self.spawn_tool_execution(
                call,
                index,
                total,
                (!trimmed_fb.is_empty()).then_some(trimmed_fb),
            );
        } else {
            let mut internal_out = String::from("The user declined to run this command.");
            if !trimmed_fb.is_empty() {
                internal_out.push_str(&format!(" User provided this reason/message: {trimmed_fb}"));
            }
            self.session.messages.push(ChatMessage::new_tool(
                call.function.name,
                internal_out,
                false,
                false,
            ));
            self.save_and_emit_chat(Vec::new()).await;
            self.emit_estimated_token_usage();
            info!("Tool declination recorded to chat history.");
            self.process_pending_tools().await;
        }
    }

    pub(super) async fn handle_resolve_truncated_tool(
        &mut self,
        action: TruncatedToolAction,
        guide_prompt: String,
        turn: TurnConfig,
    ) {
        self.update_turn_preserving_skill(turn);
        let Some(pending) = self.inflight.pending_truncated.take() else {
            return;
        };
        let trimmed_guide = guide_prompt.trim().to_string();
        let guide_opt = (!trimmed_guide.is_empty()).then_some(trimmed_guide);
        info!(
            "Resolving truncated tool output for '{}': {:?}",
            pending.summary, action
        );

        match action {
            TruncatedToolAction::ProceedTruncated => {
                self.complete_tool_exec(pending.source, pending.truncated_output, true, false)
                    .await;
            }
            TruncatedToolAction::ProceedFull => {
                let still_truncated = pending.raw_bytes > pending.full_cap_bytes;
                self.complete_tool_exec(
                    pending.source,
                    pending.full_output,
                    still_truncated,
                    false,
                )
                .await;
            }
            TruncatedToolAction::Discard => match pending.source {
                ToolExecSource::Model { .. } => {
                    let mut msg = format!(
                        "[DISCARDED: Output from '{}' ({} bytes) exceeded the {} byte limit and was discarded by the user. Try a more targeted query or read smaller ranges.]",
                        pending.summary, pending.raw_bytes, self.active_max_tool_output_size
                    );
                    if let Some(ref note) = guide_opt {
                        msg.push_str(&format!("\n\n[User attached a message]: {note}"));
                    }
                    self.complete_tool_exec(pending.source, msg, false, false)
                        .await;
                }
                ToolExecSource::ManualShell { .. } | ToolExecSource::ManualTool { .. } => {
                    info!(
                        "Discarded truncated manual command/tool output for '{}'",
                        pending.summary
                    );
                    let _ = self.tx.send(EngineEvent::ToolCompleted);
                    self.emit_chat();
                    self.flush_queued_interjections().await;
                }
            },
            TruncatedToolAction::CondenseTruncated => {
                self.spawn_tool_condensation(&pending, false, guide_opt);
                self.inflight.pending_truncated = Some(pending);
            }
            TruncatedToolAction::CondenseFull => {
                self.spawn_tool_condensation(&pending, true, guide_opt);
                self.inflight.pending_truncated = Some(pending);
            }
        }
    }

    pub(super) fn handle_run_shell_command(&mut self, raw_cmd: String, include_in_context: bool) {
        if self.is_turn_active() {
            return;
        }

        let cmd_str = raw_cmd.trim().to_string();
        let Some(parts) = shlex::split(&cmd_str).filter(|p| !p.is_empty()) else {
            return;
        };

        let prefix = if include_in_context { "!!" } else { "!" };
        info!("Executing manual shell command ({prefix}): {cmd_str}");
        let _ = self.tx.send(EngineEvent::ToolStarted {
            summary: format!("{prefix} {cmd_str}"),
            index: 1,
            total: 1,
        });

        let cfg = self.config.clone();
        let max_out = self.active_max_tool_output_size;
        let full_cap = self.context_padded_capture_limit();
        let source = ToolExecSource::ManualShell {
            command: cmd_str,
            include_in_context,
        };

        self.spawn_with_tx(move |tx| async move {
            let res =
                tools::run_manual_command(&parts[0], &parts[1..], &cfg, max_out, full_cap).await;
            let _ = tx.send(EngineEvent::ToolExecFinished(source, res));
        });
    }

    pub(super) async fn handle_run_manual_tool(
        &mut self,
        raw_input: String,
        include_in_context: bool,
    ) {
        if self.is_turn_active() {
            return;
        }

        let (call, summary) = match tools::parse_manual_tool_call(&raw_input) {
            Ok(res) => res,
            Err(e) => {
                self.emit_error(e.to_string()).await;
                return;
            }
        };

        let prefix = if include_in_context { "##" } else { "#" };
        info!("Executing manual tool call ({prefix}): {summary}");
        let _ = self.tx.send(EngineEvent::ToolStarted {
            summary: format!("{prefix}{summary}"),
            index: 1,
            total: 1,
        });

        let cfg = self.config.clone();
        let active_skill = self.active_turn_skill().map(String::from);
        let session_id = self.session.id.clone();
        let session_tools = self.session.tools.clone();
        let max_out = self.active_max_tool_output_size;
        let full_cap = self.context_padded_capture_limit();
        let source = ToolExecSource::ManualTool {
            tool_name: call.function.name.clone(),
            summary,
            arguments: call.function.arguments.clone(),
            include_in_context,
        };

        self.spawn_with_tx(move |tx| async move {
            let res = tools::execute_manual(
                &call,
                &cfg,
                active_skill.as_deref(),
                Some(session_id.as_str()),
                &session_tools,
                max_out,
                full_cap,
            )
            .await;
            let _ = tx.send(EngineEvent::ToolExecFinished(source, res));
        });
    }

    pub(super) async fn on_tool_exec_finished(
        &mut self,
        source: ToolExecSource,
        res: ToolExecutionOutput,
    ) {
        self.finish_active_task();
        if let ToolExecSource::ManualTool { ref tool_name, .. } = source {
            self.sync_tool_side_effects(tool_name);
        }

        if res.truncated
            && source.include_in_context()
            && self.inflight.queued_interjections.is_empty()
        {
            let (tool_name, modal_summary, index, total) = match &source {
                ToolExecSource::Model {
                    tool_name, summary, ..
                } => {
                    let (idx, tot) = self.tool_batch_progress();
                    (
                        tool_name.clone(),
                        summary.clone(),
                        idx.saturating_sub(1).max(1),
                        tot,
                    )
                }
                ToolExecSource::ManualShell { command, .. } => {
                    ("shell".into(), format!("!! {command}"), 1, 1)
                }
                ToolExecSource::ManualTool {
                    tool_name, summary, ..
                } => (tool_name.clone(), format!("##{summary}"), 1, 1),
            };

            let preview = res.truncated_output.clone();
            let full_cap_bytes = self.context_padded_capture_limit();
            self.inflight.pending_truncated = Some(PendingTruncatedTool {
                source,
                summary: modal_summary.clone(),
                truncated_output: res.truncated_output,
                full_output: res.full_output,
                raw_bytes: res.raw_bytes,
                full_cap_bytes,
                index,
                total,
            });
            let _ = self.tx.send(EngineEvent::TruncatedToolResolutionRequested {
                tool_name,
                summary: modal_summary,
                preview,
                raw_bytes: res.raw_bytes,
                max_size: self.active_max_tool_output_size,
                full_cap_bytes,
                index,
                total,
            });
            return;
        }

        self.complete_tool_exec(source, res.truncated_output, res.truncated, res.is_error)
            .await;
    }

    pub(super) async fn on_tool_condensed(&mut self, res: Result<String, String>) {
        self.finish_active_task();
        let Some(pending) = self.inflight.pending_truncated.take() else {
            return;
        };

        match res {
            Ok(condensed) => {
                let final_out = format!(
                    "[CONDENSED: Original output from '{}' ({} bytes) exceeded the {} byte limit and was condensed by the model.]\n\n{}",
                    pending.summary,
                    pending.raw_bytes,
                    self.active_max_tool_output_size,
                    condensed.trim()
                );
                self.complete_tool_exec(pending.source, final_out, false, false)
                    .await;
            }
            Err(err) => {
                error!(
                    "Failed to condense tool output for '{}': {err}",
                    pending.summary
                );
                self.emit_error(format!(
                    "Failed to condense truncated tool output: {err}. Proceeding with truncated output."
                ))
                .await;
                self.complete_tool_exec(pending.source, pending.truncated_output, true, false)
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::parse_skill_markdown;
    use crate::tools::ToolFunction;
    use serde_json::json;
    use std::path::Path;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn skill_auto_approve_applies_only_when_skill_is_active() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config::Config::default();
        cfg.app.history_dir = tmp.path().to_string_lossy().into_owned();
        cfg.app.auto_approve = vec!["read_file".to_string()];

        let coder = parse_skill_markdown(
            r#"+++
name = "coder"
command = "coder"
tools = ["edit_file", "read_file", "cargo"]
auto_approve = ["edit_file", "cargo:check"]
+++
Coder."#,
            Path::new("/tmp/coder"),
        )
        .unwrap();
        cfg.skills = crate::skills::resolve_skills(vec![coder]);

        let (tx, _rx) = unbounded_channel();
        let mut engine = CoreEngine::new("test-model".to_string(), cfg, tx).unwrap();

        let edit_call = ToolCall {
            function: ToolFunction {
                name: "edit_file".to_string(),
                arguments: json!({}),
            },
            thought_signature: None,
        };
        let read_call = ToolCall {
            function: ToolFunction {
                name: "read_file".to_string(),
                arguments: json!({}),
            },
            thought_signature: None,
        };
        let cargo_check = ToolCall {
            function: ToolFunction {
                name: "cargo".to_string(),
                arguments: json!({ "action": "check" }),
            },
            thought_signature: None,
        };
        let cargo_fmt = ToolCall {
            function: ToolFunction {
                name: "cargo".to_string(),
                arguments: json!({ "action": "fmt" }),
            },
            thought_signature: None,
        };

        // Without active skill, only global `auto_approve` (`read_file`) applies.
        assert!(engine.is_call_auto_approved(&read_call));
        assert!(!engine.is_call_auto_approved(&edit_call));
        assert!(!engine.is_call_auto_approved(&cargo_check));

        // With `coder` active, skill-level `auto_approve` applies alongside global rules.
        engine.turn.active_skill = Some("coder".to_string());
        assert!(engine.is_call_auto_approved(&read_call));
        assert!(engine.is_call_auto_approved(&edit_call));
        assert!(engine.is_call_auto_approved(&cargo_check));
        assert!(!engine.is_call_auto_approved(&cargo_fmt));
    }

    #[tokio::test]
    async fn core_engine_load_tools_slash_tools_fork_and_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config::Config::default();
        cfg.app.history_dir = tmp.path().to_string_lossy().into_owned();
        cfg.app.enabled_tools = vec!["read_file".into(), "git".into(), "cargo".into()];
        cfg.app.core_tools = vec!["read_file".into()];

        let (tx, mut rx) = unbounded_channel();
        let mut engine = CoreEngine::new("test-model".to_string(), cfg, tx).unwrap();
        engine.set_save_session(false);

        // `load_tools` is auto-approved even when `config.app.auto_approve` is empty
        // (e.g. upgraded user configs that predate `load_tools`).
        engine.config.app.auto_approve.clear();
        let load_call = ToolCall {
            function: ToolFunction {
                name: tools::LOAD_TOOLS_NAME.to_string(),
                arguments: json!({ "names": ["git"] }),
            },
            thought_signature: None,
        };
        assert!(engine.is_call_auto_approved(&load_call));

        // Applying `load_tools` side effect adds `git` to `session.tools.loaded` and emits event.
        engine.apply_load_tools_side_effect(&load_call.function.arguments);
        assert_eq!(engine.session.tools.loaded, vec!["git"]);
        let mut saw_updated = false;
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::SessionToolsUpdated(st) = ev
                && st.loaded == vec!["git"]
            {
                saw_updated = true;
            }
        }
        assert!(saw_updated);

        // `/tools list` stays local-only (`include_in_context == false`).
        engine
            .handle_manage_session_tools(crate::engine::SessionToolsAction::List)
            .await;
        assert!(!engine.session.messages.last().unwrap().include_in_context);

        // `/tools +browser -read_file` force-enables disabled `browser` and disables core `read_file`.
        engine
            .handle_manage_session_tools(crate::engine::SessionToolsAction::Update {
                add: vec!["browser".into()],
                remove: vec!["read_file".into()],
            })
            .await;
        assert!(engine.session.messages.last().unwrap().include_in_context);
        assert!(engine.session.tools.is_force_enabled("browser"));
        assert!(engine.session.tools.is_loaded("browser"));
        assert!(engine.session.tools.is_force_disabled("read_file"));
        assert!(!tools::is_tool_enabled_for_turn(
            "read_file",
            &engine.config,
            &engine.session.tools,
            None,
        ));
        assert!(tools::is_tool_enabled_for_turn(
            "browser",
            &engine.config,
            &engine.session.tools,
            None,
        ));

        // Fork preserves `session.tools`.
        engine
            .session
            .messages
            .push(ChatMessage::new("assistant", "Reply"));
        let fork_idx = engine.session.messages.len() - 1;
        engine.handle_fork_session(fork_idx).await;
        assert_eq!(engine.session.tools.loaded, vec!["git", "browser"]);
        assert!(engine.session.tools.is_force_disabled("read_file"));

        // `/tools reset` clears all overrides back to `core_tools` and informs the model.
        engine
            .handle_manage_session_tools(crate::engine::SessionToolsAction::Reset)
            .await;
        assert!(engine.session.tools.loaded.is_empty());
        assert!(engine.session.tools.force_enabled.is_empty());
        assert!(engine.session.tools.force_disabled.is_empty());
        let reset_msg = engine.session.messages.last().unwrap();
        assert!(reset_msg.include_in_context);
        assert!(reset_msg.content.contains("git, browser"));

        // Reload `git`, then `/reset` clears `session.tools`.
        engine.apply_load_tools_side_effect(&load_call.function.arguments);
        assert_eq!(engine.session.tools.loaded, vec!["git"]);
        engine.handle_reset_chat().await;
        assert!(engine.session.tools.loaded.is_empty());
    }
}
