use super::{
    CoreEngine, EngineEvent, TurnConfig,
    provider::{self, Provider},
    session::{ChatMessage, ThoughtPart},
    skills,
};
use crate::tools::{self, ToolCall};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use tracing::{debug, error, info, warn};

pub(super) struct QueuedInterjection {
    pub(super) msg: ChatMessage,
    pub(super) new_images: Vec<(String, Vec<u8>)>,
    pub(super) turn: TurnConfig,
}

impl CoreEngine {
    pub(super) fn queued_ui_messages(&self) -> Vec<ChatMessage> {
        self.inflight
            .queued_interjections
            .iter()
            .map(|q| q.msg.with_ui_image_placeholders())
            .collect()
    }

    pub(super) fn emit_queued_interjections(&self) {
        let _ = self.tx.send(EngineEvent::QueuedInterjectionsUpdated(
            self.queued_ui_messages(),
        ));
    }

    pub(super) fn flush_interrupted_stream_buffers(&mut self, tag: &str) -> bool {
        let text = std::mem::take(&mut self.inflight.stream_buffer);
        let thought_text = std::mem::take(&mut self.inflight.thought_stream_buffer);
        let had_text = !text.trim().is_empty();
        let had_thought = !thought_text.trim().is_empty();
        if had_text || had_thought {
            let final_content = if had_text {
                format!("{text}\n\n*[{tag}]*")
            } else {
                format!("*[{tag}]*")
            };
            self.session.messages.push(ChatMessage {
                thoughts: had_thought.then(|| {
                    vec![ThoughtPart {
                        text: Some(thought_text),
                        thought_signature: None,
                    }]
                }),
                ..ChatMessage::new("assistant", final_content)
            });
        }
        had_text || had_thought
    }

    pub(super) async fn flush_queued_interjections(&mut self) -> bool {
        if self.inflight.queued_interjections.is_empty() {
            return false;
        }
        let skipped = self.close_unresolved_tool_calls(
            "Skipped remaining tool call to process user interjection.",
        );
        if skipped > 0 {
            info!("Skipped {skipped} remaining tool call(s) due to queued user interjection");
        }
        if self
            .inflight
            .queued_interjections
            .iter()
            .any(|i| i.turn.active_skill.is_some())
        {
            self.refresh_skills();
        }
        let mut new_images = Vec::new();
        let mut has_valid_prompt = false;
        for mut item in std::mem::take(&mut self.inflight.queued_interjections) {
            if let Err(reason) = skills::validate_turn_skill(&mut item.turn, &self.config) {
                warn!("{reason}");
                self.session
                    .messages
                    .push(ChatMessage::new("error", reason));
                continue;
            }
            self.turn = item.turn;
            self.session.messages.push(item.msg);
            new_images.extend(item.new_images);
            has_valid_prompt = true;
        }
        self.save_and_emit_chat(new_images).await;
        if has_valid_prompt {
            self.session.auto_task_turns = 0;
            let turn = self.turn.clone();
            self.start_user_turn(&turn).await;
        }
        true
    }

    fn spawn_hooks(
        &mut self,
        hooks: Vec<(String, ToolCall)>,
        active_skill: Option<String>,
        after_turn: bool,
    ) {
        let total = hooks.len();
        let phase = if after_turn { "after-turn" } else { "pre-turn" };
        info!("Running {total} {phase} skill hook(s)");
        let cfg = self.config.clone();
        let session_id = self.session.id.clone();
        let max_out = self.active_max_tool_output_size;
        let full_cap = self.context_padded_capture_limit();

        self.spawn_with_tx(move |tx| async move {
            for (idx, (summary, call)) in hooks.into_iter().enumerate() {
                let _ = tx.send(EngineEvent::ToolStarted {
                    summary: format!("hook: {summary}"),
                    index: idx + 1,
                    total,
                });
                let res = tools::execute_hook(
                    &call,
                    &cfg,
                    active_skill.as_deref(),
                    Some(session_id.as_str()),
                    max_out,
                    full_cap,
                )
                .await;
                let _ = tx.send(EngineEvent::HookCompleted {
                    tool_name: call.function.name,
                    summary,
                    output: res.truncated_output,
                    truncated: res.truncated,
                    after_turn,
                });
            }
            let _ = tx.send(EngineEvent::HooksFinished { after_turn });
        });
    }

    pub(super) async fn start_user_turn(&mut self, turn: &TurnConfig) {
        let hooks = skills::collect_due_hooks(
            &self.config,
            turn.active_skill.as_deref(),
            turn.skill_args.as_deref(),
            turn.raw_skill_args.as_deref(),
            &turn.skill_inputs,
            &mut self.session.executed_skill_hooks,
            &self.session.messages,
        );
        if hooks.is_empty() {
            self.spawn_model_turn(turn).await;
            return;
        }

        self.spawn_hooks(hooks, turn.active_skill.clone(), false);
    }

    pub(super) async fn spawn_model_turn(&mut self, turn: &TurnConfig) {
        self.inflight.stream_buffer.clear();
        self.inflight.thought_stream_buffer.clear();
        self.flush_save_task().await;

        let sessions = Arc::clone(&self.sessions);
        let session_id = self.session.id.clone();

        // Clone lightweight messages (without Base64 payloads) and hydrate Base64
        // strings directly from the SQLite database into the transient `hydrated_msgs`.
        let mut msgs = self.session.messages.clone();
        let hydrated_msgs = tokio::task::spawn_blocking(move || {
            for msg in &mut msgs {
                let Some(names) = &msg.image_names else {
                    continue;
                };
                let b64s = sessions.load_images_base64(&session_id, names);
                if !b64s.is_empty() {
                    msg.images = Some(b64s);
                }
            }
            msgs
        })
        .await
        .unwrap_or_else(|_| self.session.messages.clone());

        let _ = self.tx.send(EngineEvent::StreamStarted);
        let provider = self.provider;
        let client = self.client.clone();
        let model = self.model.clone();
        let config = self.config.clone();
        let turn_cfg = turn.clone();
        let session_id_for_turn = self.session.id.clone();
        let session_tools = self.session.tools.clone();

        self.spawn_with_tx(move |internal_tx| async move {
            provider
                .stream_turn(
                    client,
                    model,
                    config,
                    hydrated_msgs,
                    turn_cfg,
                    Some(session_id_for_turn),
                    session_tools,
                    internal_tx,
                )
                .await;
        });
    }

    pub(super) async fn handle_send_prompt(&mut self, prompt: String, mut turn: TurnConfig) {
        if turn.active_skill.is_some() {
            self.refresh_skills();
        }
        if let Err(reason) = skills::validate_turn_skill(&mut turn, &self.config) {
            warn!("{reason}");
            self.emit_error(reason).await;
            return;
        }

        info!(
            "User submitted prompt. Enable tools: {}, Active skill: {:?}",
            turn.enable_tools, turn.active_skill
        );
        debug!("Prompt content: {}", prompt.replace('\n', " "));

        let (image_names, new_image_blobs) = self.collect_unsent_images().await;
        let user_msg = ChatMessage {
            images: None,
            image_names,
            ..ChatMessage::new("user", prompt)
        };

        // If a model stream or a tool execution is currently in flight, queue the
        // interjection so the active process finishes cleanly before reading it.
        if self.is_turn_active() {
            info!("Queuing user interjection while turn is active");
            self.inflight.queued_interjections.push(QueuedInterjection {
                msg: user_msg,
                new_images: new_image_blobs,
                turn,
            });
            self.emit_queued_interjections();
            return;
        }

        let skipped = self
            .close_unresolved_tool_calls("The user skipped this tool call and sent a new message.");
        if skipped > 0 {
            warn!("Auto-declined {skipped} unresolved tool call(s) before new prompt");
        }

        self.session.auto_task_turns = 0;
        self.turn = turn.clone();
        self.session.messages.push(user_msg);

        self.save_and_emit_chat(new_image_blobs).await;
        self.start_user_turn(&turn).await;
    }

    pub(super) async fn handle_cancel_stream(&mut self) {
        let had_task = self.finish_active_task();
        if !had_task
            && self.inflight.pending_truncated.is_none()
            && self.inflight.pending_tools.is_empty()
            && self.inflight.queued_interjections.is_empty()
        {
            return;
        }

        info!("User interrupted active turn");
        self.session.auto_task_turns = 0;
        self.inflight.pending_stream_done = None;
        let flushed = self.flush_interrupted_stream_buffers("Generation cancelled");
        let closed = self.close_unresolved_tool_calls("Cancelled by user.");

        if self.flush_queued_interjections().await {
            return;
        }

        if flushed || closed > 0 {
            self.save_session_bg(Vec::new()).await;
        }
        self.emit_chat();
    }

    pub(super) async fn handle_switch_model(
        &mut self,
        model: String,
        num_ctx: u32,
        max_tool_output_size: usize,
    ) {
        if self.is_turn_active()
            || self.inflight.pending_truncated.is_some()
            || !self.inflight.pending_tools.is_empty()
            || !self.inflight.queued_interjections.is_empty()
        {
            self.emit_error("Cannot switch models while a turn or tool approval is active.")
                .await;
            return;
        }

        let new_provider = Provider::from_model(&model);
        info!(
            "Hot-swapping to model '{}' ({:?}, ctx: {}, tool_max: {})",
            model, new_provider, num_ctx, max_tool_output_size
        );

        if let Err(e) = new_provider.run_preflight(&model, &self.config).await {
            let msg = format!("Model switch to '{model}' failed preflight: {e}");
            error!("{msg}");
            self.emit_error(msg).await;
            return;
        }

        let new_client = match new_provider.build_client(&self.config) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Failed to build HTTP client for '{model}': {e}");
                error!("{msg}");
                self.emit_error(msg).await;
                return;
            }
        };

        self.model = model;
        self.provider = new_provider;
        self.client = new_client;
        self.turn.num_ctx = num_ctx.max(512);
        self.active_max_tool_output_size = max_tool_output_size.max(1024);

        self.save_session_bg(Vec::new()).await;
        self.emit_model_config();
    }

    pub(super) fn handle_inspect_payload(&mut self, draft_prompt: String, mut turn: TurnConfig) {
        let mut preview_msgs = self
            .session
            .messages
            .iter()
            .map(ChatMessage::with_ui_image_placeholders)
            .collect::<Vec<_>>();

        let trimmed_draft = draft_prompt.trim();
        let effective_draft = if !trimmed_draft.is_empty()
            && !trimmed_draft.starts_with('/')
            && !trimmed_draft.starts_with('!')
            && tools::parse_hash_tool_prefix(trimmed_draft).is_none()
        {
            if trimmed_draft.starts_with('@') {
                self.refresh_skills();
            }
            let user_content =
                skills::apply_at_skill_to_turn(trimmed_draft, &mut turn, &self.config)
                    .unwrap_or_else(|_| {
                        skills::parse_at_command(trimmed_draft)
                            .filter(|(_, args)| !args.is_empty())
                            .map_or_else(|| trimmed_draft.to_string(), |(_, args)| args.to_string())
                    });
            preview_msgs.push(ChatMessage::new("user", user_content.clone()));
            for summary in skills::preview_due_hooks(
                &self.config,
                turn.active_skill.as_deref(),
                turn.skill_args.as_deref(),
                turn.raw_skill_args.as_deref(),
                &turn.skill_inputs,
                &self.session.executed_skill_hooks,
                &self.session.messages,
            ) {
                preview_msgs.push(ChatMessage::new_hook(
                    summary,
                    "[Hook will execute automatically before turn is sent]",
                    false,
                ));
            }
            Some(user_content)
        } else {
            None
        };

        let (effective_sys, sanitized_msgs, payload) = self.provider.build_turn_payload(
            &self.model,
            &self.config,
            preview_msgs,
            &turn,
            Some(self.session.id.as_str()),
            &self.session.tools,
        );
        let formatted = provider::format_payload_inspection(
            self.provider,
            &self.model,
            &turn,
            &effective_sys,
            &sanitized_msgs,
            &payload,
            effective_draft.as_deref(),
        );
        let _ = self.tx.send(EngineEvent::PayloadInspectorOpened(formatted));
    }

    pub(super) async fn on_stream_done(
        &mut self,
        ev: EngineEvent,
        prompt_tokens: u64,
        eval_tokens: u64,
        tool_calls: Vec<ToolCall>,
        thoughts: Vec<ThoughtPart>,
    ) {
        self.finish_active_task();
        let final_text = std::mem::take(&mut self.inflight.stream_buffer);
        self.inflight.thought_stream_buffer.clear();

        let has_response =
            !final_text.trim().is_empty() || !tool_calls.is_empty() || !thoughts.is_empty();
        if has_response {
            self.session.messages.push(ChatMessage {
                tool_calls: (!tool_calls.is_empty()).then(|| tool_calls.clone()),
                thoughts: (!thoughts.is_empty()).then_some(thoughts),
                ..ChatMessage::new("assistant", final_text)
            });
            self.save_and_emit_chat(Vec::new()).await;
        } else {
            warn!(
                "Stream finished with empty content and no tool calls (Prompt: {prompt_tokens}, Eval: {eval_tokens})"
            );
            self.session.messages.push(ChatMessage::new(
                "error",
                "Model returned an empty response (no content or tool calls).",
            ));
            self.save_and_emit_chat(Vec::new()).await;
        }

        if !tool_calls.is_empty() {
            let _ = self.tx.send(ev);
            self.inflight.pending_tools = VecDeque::from(tool_calls);
            self.process_pending_tools().await;
        } else if has_response {
            let max_turns = self.config.app.task_autoloop_max_turns;
            if self.config.app.task_autoloop
                && self.turn.enable_tools
                && tools::is_tool_enabled_for_turn(
                    "task",
                    &self.config,
                    &self.session.tools,
                    self.active_turn_skill(),
                )
                && self.inflight.queued_interjections.is_empty()
                && self.session.auto_task_turns < max_turns
            {
                let cfg = self.config.clone();
                let sid = self.session.id.clone();
                let should_continue =
                    tokio::task::spawn_blocking(move || tools::task::should_autoloop(&cfg, &sid))
                        .await
                        .unwrap_or(false);

                if should_continue {
                    self.session.auto_task_turns += 1;
                    info!(
                        "Active tasks remain; auto-continuing task loop ({}/{max_turns})",
                        self.session.auto_task_turns
                    );
                    let _ = self
                        .tx
                        .send(EngineEvent::TokenUsageUpdated(prompt_tokens + eval_tokens));
                    let prompt = if self.config.app.task_autoloop_ignore_blocked {
                        "Continue working through `<tasks>`. Move on to the next task, mark it `in_progress` while you work on it, and remove it with `task` (`action: \"remove\"`) once finished."
                    } else {
                        "Continue working through `<tasks>`. Move on to the next task, mark it `in_progress` while you work on it, and remove it with `task` (`action: \"remove\"`) once finished. If you need user input before proceeding, mark the task `blocked` (`status: \"blocked\"`) and ask your question."
                    };
                    self.session.messages.push(ChatMessage::new("user", prompt));
                    self.save_and_emit_chat(Vec::new()).await;
                    let turn = self.turn.clone();
                    self.spawn_model_turn(&turn).await;
                    return;
                }
            }
            self.session.auto_task_turns = 0;

            let last_response = self
                .session
                .messages
                .last()
                .filter(|m| m.role == "assistant")
                .map(|m| m.content.as_str())
                .unwrap_or("");
            let after_hooks = skills::collect_after_turn_hooks(
                &self.config,
                self.turn.active_skill.as_deref(),
                self.turn.skill_args.as_deref(),
                self.turn.raw_skill_args.as_deref(),
                &self.turn.skill_inputs,
                last_response,
            );
            if after_hooks.is_empty() {
                let _ = self.tx.send(ev);
                self.flush_queued_interjections().await;
            } else {
                let _ = self
                    .tx
                    .send(EngineEvent::TokenUsageUpdated(prompt_tokens + eval_tokens));
                self.inflight.pending_stream_done = Some(ev);
                self.spawn_hooks(after_hooks, self.turn.active_skill.clone(), true);
            }
        } else {
            let _ = self.tx.send(ev);
            self.flush_queued_interjections().await;
        }
    }

    pub(super) async fn on_stream_error(&mut self, ev: EngineEvent, err: String) {
        self.finish_active_task();
        self.flush_interrupted_stream_buffers("Stream interrupted");
        error!("API Error: {err}");
        self.session.messages.push(ChatMessage::new("error", err));
        self.save_session_bg(Vec::new()).await;
        let _ = self.tx.send(ev);
        self.flush_queued_interjections().await;
    }

    pub(super) async fn on_hook_completed(
        &mut self,
        tool_name: String,
        summary: String,
        output: String,
        truncated: bool,
        after_turn: bool,
    ) {
        self.sync_tool_side_effects(&tool_name);
        info!(
            "Skill hook '{summary}' completed (truncated: {truncated}, after_turn: {after_turn})"
        );
        let mut msg = ChatMessage::new_hook(summary, output, truncated);
        if after_turn {
            msg.include_in_context = false;
        }
        self.session.messages.push(msg);
        self.save_and_emit_chat(Vec::new()).await;
    }

    pub(super) async fn on_hooks_finished(&mut self, after_turn: bool) {
        self.finish_active_task();
        let _ = self.tx.send(EngineEvent::ToolCompleted);
        if after_turn {
            if let Some(done_ev) = self.inflight.pending_stream_done.take() {
                let _ = self.tx.send(done_ev);
            }
            self.flush_queued_interjections().await;
            return;
        }
        if self.flush_queued_interjections().await {
            return;
        }
        let turn = self.turn.clone();
        self.spawn_model_turn(&turn).await;
    }

    pub(super) async fn handle_compact_chat(
        &mut self,
        skill: Option<String>,
        instructions: Option<String>,
    ) {
        if self.is_turn_active()
            || self.inflight.pending_truncated.is_some()
            || !self.inflight.pending_tools.is_empty()
            || !self.inflight.queued_interjections.is_empty()
        {
            self.emit_error("Cannot compact conversation while a turn or tool approval is active.")
                .await;
            return;
        }

        let has_meaningful_msgs = self
            .session
            .messages
            .iter()
            .any(|m| m.role != "error" && !m.content.trim().is_empty());
        if !has_meaningful_msgs {
            self.emit_error("Conversation is empty; nothing to compact.")
                .await;
            return;
        }

        // Resolve the effective compaction-guide skill: an explicit `@name`
        // (from `/compact @name`) wins over the `[app] compact_skill` default.
        let effective_skill: Option<String> = skill
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.config
                    .app
                    .compact_skill
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });

        let instructions = match effective_skill {
            Some(name) => {
                // Re-discover skills from disk so freshly added/edited guide
                // skills are available before we render the instructions.
                self.refresh_skills();
                match self.render_compact_skill(&name, instructions.as_deref()) {
                    Ok(combined) => Some(combined),
                    Err(e) => {
                        self.emit_error(e).await;
                        return;
                    }
                }
            }
            None => instructions,
        };

        info!(
            "Compacting conversation history ({} messages)",
            self.session.messages.len()
        );
        let _ = self.tx.send(EngineEvent::ToolStarted {
            summary: "compacting conversation context".into(),
            index: 1,
            total: 1,
        });

        let provider = self.provider;
        let client = self.client.clone();
        let model = self.model.clone();
        let config = self.config.clone();
        let num_ctx = self.turn.num_ctx;
        let messages = self.session.messages.clone();

        self.spawn_with_tx(move |tx| async move {
            let res = provider
                .compact_conversation(
                    client,
                    model,
                    config,
                    num_ctx,
                    &messages,
                    instructions.as_deref(),
                )
                .await;
            match res {
                Ok((summary, prompt_tokens, eval_tokens)) => {
                    let _ = tx.send(EngineEvent::ChatCompacted {
                        res: Ok(summary),
                        prompt_tokens,
                        eval_tokens,
                    });
                }
                Err(err) => {
                    let _ = tx.send(EngineEvent::ChatCompacted {
                        res: Err(err),
                        prompt_tokens: 0,
                        eval_tokens: 0,
                    });
                }
            }
        });
    }

    /// Renders a compaction-guide skill's prompt body into the compaction
    /// instructions. Only the skill's **prompt body** is used - tools and hooks
    /// are irrelevant to the one-shot, tool-less compaction call. Free text
    /// passed alongside the skill is exposed to the template as `{{args}}` and
    /// also appended verbatim so the user can add a one-off focus.
    fn render_compact_skill(
        &self,
        name: &str,
        free_instructions: Option<&str>,
    ) -> Result<String, String> {
        let clean = name.trim().trim_start_matches('@');
        let skill = skills::find_skill(&self.config, clean)
            .ok_or_else(|| format!("Compaction skill '@{clean}' not found."))?;

        // The guide provides static instructions; the user's one-off focus (if
        // any) is exposed to the template as {{args}} and also appended
        // verbatim below so it is never dropped even when the skill body
        // does not reference it.
        let focus = free_instructions
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let inputs: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let ctx = skills::TemplateContext {
            skill_name: &skill.name,
            args: focus,
            raw_args: focus,
            last_response: None,
            inputs: &inputs,
        };
        let body = skills::render_template(&skill.prompt, &ctx)
            .trim()
            .to_string();
        if body.is_empty() {
            return Err(format!(
                "Compaction skill '@{}' has an empty prompt body.",
                skill.name
            ));
        }

        let mut out = body;
        if !focus.is_empty() {
            out.push_str("\n\nOne-off focus for this compaction: ");
            out.push_str(focus);
        }
        Ok(out)
    }

    pub(super) async fn on_chat_compacted(
        &mut self,
        res: Result<String, String>,
        _prompt_tokens: u64,
        eval_tokens: u64,
    ) {
        self.finish_active_task();
        let _ = self.tx.send(EngineEvent::ToolCompleted);

        match res {
            Ok(summary) => {
                let prev_count = self.session.messages.len();
                let checkpoint_user = ChatMessage::new(
                    "user",
                    format!(
                        "[Conversation History Compacted — Prior {prev_count} messages summarized into checkpoint below]"
                    ),
                );
                let checkpoint_assistant = ChatMessage::new(
                    "assistant",
                    format!("### Compacted Context Checkpoint\n\n{}", summary.trim()),
                );

                self.session.messages = vec![checkpoint_user, checkpoint_assistant];
                let estimated_tokens = if eval_tokens > 0 {
                    eval_tokens + 64
                } else {
                    (summary.len() as u64 / 4).max(1)
                };
                self.save_and_emit_chat(Vec::new()).await;
                let _ = self
                    .tx
                    .send(EngineEvent::TokenUsageUpdated(estimated_tokens));
                info!(
                    "Conversation compacted from {prev_count} messages to 2 checkpoint messages (~{estimated_tokens} tokens)."
                );
            }
            Err(err) => {
                error!("Failed to compact conversation: {err}");
                self.emit_error(format!("Failed to compact conversation: {err}"))
                    .await;
            }
        }
    }
}
