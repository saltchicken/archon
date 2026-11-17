use super::{
    CoreEngine, EngineEvent, TruncatedToolAction, TurnConfig, config, session::ChatMessage,
};
use crate::tools::{self, ToolCall};
use tracing::{error, info, warn};

pub(super) enum PendingTruncatedSource {
    ModelTool { tool_name: String },
    ManualShell { command: String },
}

pub(super) struct PendingTruncatedTool {
    pub(super) source: PendingTruncatedSource,
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
        self.inflight.active_tool_name = None;
        self.inflight.active_tool_summary = None;
        self.inflight.pending_truncated = None;
        for name in unresolved_names {
            self.session
                .messages
                .push(ChatMessage::new_tool(name, message, false));
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
        }
    }

    fn is_call_auto_approved(&self, call: &ToolCall) -> bool {
        let all_rules = self
            .config
            .app
            .auto_approve
            .iter()
            .chain(self.session.auto_approve.iter())
            .map(String::as_str);
        tools::is_auto_approved(call, all_rules)
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
        let _ = self.tx.send(EngineEvent::ToolApprovalRequested {
            formatted_preview: tools::format_approval(call),
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

    async fn complete_tool_result(&mut self, tool_name: String, output: String, truncated: bool) {
        self.sync_tool_side_effects(&tool_name);
        let raw_bytes = output.len();
        self.session
            .messages
            .push(ChatMessage::new_tool(tool_name, output.clone(), truncated));
        self.save_and_emit_chat(Vec::new()).await;
        let _ = self.tx.send(EngineEvent::ToolCompleted {
            output,
            full_output: String::new(),
            raw_bytes,
            truncated,
        });
        info!("Tool execution completed and recorded to chat history.");
        self.process_pending_tools().await;
    }

    async fn complete_shell_result(
        &mut self,
        command: String,
        output: String,
        truncated: bool,
        include_in_context: bool,
    ) {
        let mut msg = ChatMessage::new("local", format!("$ {command}\n{output}"));
        msg.truncated = truncated;
        msg.include_in_context = include_in_context;
        self.session.messages.push(msg);
        self.save_and_emit_chat(Vec::new()).await;
        let _ = self.tx.send(EngineEvent::ToolCompleted {
            output: String::new(),
            full_output: String::new(),
            raw_bytes: 0,
            truncated,
        });
        self.flush_queued_interjections().await;
    }

    async fn finalize_pending_truncated(
        &mut self,
        source: PendingTruncatedSource,
        output: String,
        truncated: bool,
    ) {
        match source {
            PendingTruncatedSource::ModelTool { tool_name } => {
                self.complete_tool_result(tool_name, output, truncated)
                    .await;
            }
            PendingTruncatedSource::ManualShell { command } => {
                self.complete_shell_result(command, output, truncated, true)
                    .await;
            }
        }
    }

    fn spawn_tool_condensation(
        &mut self,
        pending: &PendingTruncatedTool,
        use_full: bool,
        guide_prompt: Option<String>,
    ) {
        self.inflight.executing_tool = true;
        let tool_name = match &pending.source {
            PendingTruncatedSource::ModelTool { tool_name } => tool_name.clone(),
            PendingTruncatedSource::ManualShell { .. } => "shell_command".to_string(),
        };
        self.inflight.active_tool_name = Some(tool_name.clone());
        self.inflight.active_tool_summary = Some(pending.summary.clone());
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

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.inflight.internal_rx = Some(rx);

        self.inflight.active_task = Some(tokio::spawn(async move {
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
        }));
    }

    fn spawn_tool_execution(
        &mut self,
        call: ToolCall,
        index: usize,
        total: usize,
        feedback: Option<String>,
    ) {
        let sig = tools::call_signature(&call);
        self.inflight.executing_tool = true;
        self.inflight.active_tool_name = Some(call.function.name.clone());
        self.inflight.active_tool_summary = Some(sig.clone());
        let _ = self.tx.send(EngineEvent::ToolStarted {
            summary: sig,
            index,
            total,
        });

        let cfg = self.config.clone();
        let active_skill = self.active_turn_skill().map(String::from);
        let max_out = self.active_max_tool_output_size;
        let full_cap = self.context_padded_capture_limit();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.inflight.internal_rx = Some(rx);

        self.inflight.active_task = Some(tokio::spawn(async move {
            let mut res =
                tools::execute(&call, &cfg, active_skill.as_deref(), max_out, full_cap).await;
            if let Some(fb) = feedback.filter(|s| !s.is_empty()) {
                let suffix = format!("\n\n[User attached a message]: {fb}");
                res.truncated_output.push_str(&suffix);
                res.full_output.push_str(&suffix);
            }
            let _ = tx.send(EngineEvent::ToolCompleted {
                output: res.truncated_output,
                full_output: res.full_output,
                raw_bytes: res.raw_bytes,
                truncated: res.truncated,
            });
        }));
    }

    pub(super) async fn process_pending_tools(&mut self) {
        if self.flush_queued_interjections().await {
            return;
        }

        let mut modified = false;

        while let Some(call) = self.inflight.pending_tools.front() {
            if !tools::is_tool_enabled_for_turn(
                &call.function.name,
                &self.config,
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
                ));
                modified = true;
                continue;
            }

            if !self.is_call_auto_approved(call) {
                if modified {
                    self.save_and_emit_chat(Vec::new()).await;
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
            }
            self.spawn_tool_execution(call, index, total, None);
            return;
        }

        if modified {
            self.save_and_emit_chat(Vec::new()).await;
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
            ));
            self.save_and_emit_chat(Vec::new()).await;
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
                self.finalize_pending_truncated(pending.source, pending.truncated_output, true)
                    .await;
            }
            TruncatedToolAction::ProceedFull => {
                let still_truncated = pending.raw_bytes > pending.full_cap_bytes;
                self.finalize_pending_truncated(
                    pending.source,
                    pending.full_output,
                    still_truncated,
                )
                .await;
            }
            TruncatedToolAction::Discard => match pending.source {
                PendingTruncatedSource::ModelTool { tool_name } => {
                    let mut msg = format!(
                        "[DISCARDED: Output from '{}' ({} bytes) exceeded the {} byte limit and was discarded by the user. Try a more targeted query or read smaller ranges.]",
                        pending.summary, pending.raw_bytes, self.active_max_tool_output_size
                    );
                    if let Some(ref note) = guide_opt {
                        msg.push_str(&format!("\n\n[User attached a message]: {note}"));
                    }
                    self.complete_tool_result(tool_name, msg, false).await;
                }
                PendingTruncatedSource::ManualShell { .. } => {
                    info!(
                        "Discarded truncated manual shell command output for '{}'",
                        pending.summary
                    );
                    let _ = self.tx.send(EngineEvent::ToolCompleted {
                        output: String::new(),
                        full_output: String::new(),
                        raw_bytes: 0,
                        truncated: false,
                    });
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
        self.inflight.executing_tool = true;
        let _ = self.tx.send(EngineEvent::ToolStarted {
            summary: format!("{prefix} {cmd_str}"),
            index: 1,
            total: 1,
        });

        let cfg = self.config.clone();
        let max_out = self.active_max_tool_output_size;
        let full_cap = self.context_padded_capture_limit();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.inflight.internal_rx = Some(rx);

        self.inflight.active_task = Some(tokio::spawn(async move {
            let res =
                tools::run_manual_command(&parts[0], &parts[1..], &cfg, max_out, full_cap).await;
            let _ = tx.send(EngineEvent::ShellCommandCompleted {
                command: cmd_str,
                output: res.truncated_output,
                full_output: res.full_output,
                raw_bytes: res.raw_bytes,
                truncated: res.truncated,
                include_in_context,
            });
        }));
    }

    pub(super) async fn on_tool_completed(
        &mut self,
        output: String,
        full_output: String,
        raw_bytes: usize,
        truncated: bool,
    ) {
        self.finish_active_task();
        let tool_name = self
            .inflight
            .active_tool_name
            .take()
            .or_else(|| self.unresolved_tool_names().into_iter().next())
            .unwrap_or_else(|| "tool_execution".into());
        let summary = self
            .inflight
            .active_tool_summary
            .take()
            .unwrap_or_else(|| tool_name.clone());

        if truncated && self.inflight.queued_interjections.is_empty() {
            let (mut index, total) = self.tool_batch_progress();
            index = index.saturating_sub(1).max(1);
            let preview = output.clone();
            let full_cap_bytes = self.context_padded_capture_limit();
            self.inflight.pending_truncated = Some(PendingTruncatedTool {
                source: PendingTruncatedSource::ModelTool {
                    tool_name: tool_name.clone(),
                },
                summary: summary.clone(),
                truncated_output: output,
                full_output,
                raw_bytes,
                full_cap_bytes,
                index,
                total,
            });
            let _ = self.tx.send(EngineEvent::TruncatedToolResolutionRequested {
                tool_name,
                summary,
                preview,
                raw_bytes,
                max_size: self.active_max_tool_output_size,
                full_cap_bytes,
                index,
                total,
            });
            return;
        }

        self.complete_tool_result(tool_name, output, truncated)
            .await;
    }

    pub(super) async fn on_tool_condensed(&mut self, res: Result<String, String>) {
        self.finish_active_task();
        let Some(pending) = self.inflight.pending_truncated.take() else {
            return;
        };
        self.inflight.active_tool_name = None;
        self.inflight.active_tool_summary = None;

        match res {
            Ok(condensed) => {
                let final_out = format!(
                    "[CONDENSED: Original output from '{}' ({} bytes) exceeded the {} byte limit and was condensed by the model.]\n\n{}",
                    pending.summary,
                    pending.raw_bytes,
                    self.active_max_tool_output_size,
                    condensed.trim()
                );
                self.finalize_pending_truncated(pending.source, final_out, false)
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
                self.finalize_pending_truncated(pending.source, pending.truncated_output, true)
                    .await;
            }
        }
    }

    pub(super) async fn on_shell_command_completed(
        &mut self,
        command: String,
        output: String,
        full_output: String,
        raw_bytes: usize,
        truncated: bool,
        include_in_context: bool,
    ) {
        self.finish_active_task();
        if truncated && include_in_context && self.inflight.queued_interjections.is_empty() {
            let summary = format!("!! {command}");
            let preview = output.clone();
            let full_cap_bytes = self.context_padded_capture_limit();
            self.inflight.pending_truncated = Some(PendingTruncatedTool {
                source: PendingTruncatedSource::ManualShell {
                    command: command.clone(),
                },
                summary: summary.clone(),
                truncated_output: output,
                full_output,
                raw_bytes,
                full_cap_bytes,
                index: 1,
                total: 1,
            });
            let _ = self.tx.send(EngineEvent::TruncatedToolResolutionRequested {
                tool_name: "shell".into(),
                summary,
                preview,
                raw_bytes,
                max_size: self.active_max_tool_output_size,
                full_cap_bytes,
                index: 1,
                total: 1,
            });
            return;
        }

        self.complete_shell_result(command, output, truncated, include_in_context)
            .await;
    }
}
