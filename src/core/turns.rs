use super::{
    CoreEngine, EngineEvent, TurnConfig,
    config::Config,
    provider::Provider,
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

pub(super) struct PendingAutoCompact {
    pub(super) turn: TurnConfig,
    pub(super) preserved_trailing: Vec<ChatMessage>,
    pub(super) pre_tokens: u64,
    pub(super) compacted_count: usize,
}

/// Splits `messages` into `(history_to_compact, preserved_trailing)` for
/// pre-flight auto-compaction. If the conversation ends with an unanswered user
/// turn (a `user` message plus any pre-turn `hook`/`local` context messages
/// after the last `assistant` or `tool` message), those trailing messages are
/// preserved verbatim so the resumed model turn still directly answers them.
pub(super) fn split_messages_for_auto_compact(
    messages: &[ChatMessage],
) -> (&[ChatMessage], Vec<ChatMessage>) {
    let last_settled_idx = messages
        .iter()
        .rposition(|m| matches!(m.role.as_str(), "assistant" | "tool"));
    if let Some(idx) = last_settled_idx
        && idx + 1 < messages.len()
    {
        let tail = &messages[idx + 1..];
        let has_trailing_user = tail.iter().any(|m| {
            m.role == "user"
                && (!m.content.trim().is_empty()
                    || m.image_names.as_ref().is_some_and(|i| !i.is_empty())
                    || m.images.as_ref().is_some_and(|i| !i.is_empty()))
        });
        if has_trailing_user {
            return (&messages[..=idx], tail.to_vec());
        }
    }
    (messages, Vec::new())
}

/// Returns `true` when pre-flight auto-compaction should run before sending
/// the next model turn.
pub(super) fn should_auto_compact(
    config: &Config,
    estimated_tokens: u64,
    num_ctx: u32,
    history_to_compact: &[ChatMessage],
    turns_since_auto_compact: u32,
    has_queued_interjections: bool,
    allow_auto_compact: bool,
) -> bool {
    if !allow_auto_compact || !config.app.auto_compact || has_queued_interjections {
        return false;
    }
    let threshold_tokens =
        (f64::from(num_ctx.max(1)) * config.app.effective_auto_compact_threshold()).ceil() as u64;
    if estimated_tokens < threshold_tokens {
        return false;
    }
    if turns_since_auto_compact < config.app.auto_compact_cooldown_turns {
        return false;
    }
    let meaningful_msgs = history_to_compact
        .iter()
        .filter(|m| match m.role.as_str() {
            "error" => false,
            "local" | "hook" => m.include_in_context && !m.content.trim().is_empty(),
            "assistant" => {
                !m.content.trim().is_empty()
                    || m.tool_calls.as_ref().is_some_and(|t| !t.is_empty())
                    || m.thoughts.as_ref().is_some_and(|t| !t.is_empty())
            }
            _ => {
                !m.content.trim().is_empty()
                    || m.image_names.as_ref().is_some_and(|i| !i.is_empty())
                    || m.images.as_ref().is_some_and(|i| !i.is_empty())
            }
        })
        .count();
    meaningful_msgs >= 2
}

/// Builds the replacement message list after a successful compaction.
pub(super) fn build_compacted_messages(
    summary: &str,
    prev_count: usize,
    auto_compact: Option<(u64, Vec<ChatMessage>)>,
) -> Vec<ChatMessage> {
    let (header, trailing) = match auto_compact {
        Some((pre_tokens, preserved)) => (
            format!(
                "[Context Auto-Compacted — Prior {prev_count} messages (~{pre_tokens} tokens) summarized into checkpoint below]"
            ),
            Some(preserved),
        ),
        None => (
            format!(
                "[Conversation History Compacted — Prior {prev_count} messages summarized into checkpoint below]"
            ),
            None,
        ),
    };

    let mut out = vec![
        ChatMessage::new("user", header),
        ChatMessage::new(
            "assistant",
            format!("### Compacted Context Checkpoint\n\n{}", summary.trim()),
        ),
    ];

    if let Some(preserved) = trailing {
        if !preserved.is_empty() {
            out.extend(preserved);
        } else {
            out.push(ChatMessage::new(
                "user",
                "Context was automatically compacted. Continue answering the user's request based on the checkpoint summary above.",
            ));
        }
    }

    out
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
        self.emit_estimated_token_usage();
        if has_valid_prompt {
            self.session.auto_plan_turns = 0;
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
        self.spawn_model_turn_inner(turn, true).await;
    }

    async fn spawn_model_turn_inner(&mut self, turn: &TurnConfig, allow_auto_compact: bool) {
        self.inflight.stream_buffer.clear();
        self.inflight.thought_stream_buffer.clear();
        self.flush_save_task().await;

        let (raw_tokens, calibrated_tokens) = self.emit_estimated_token_usage_for(turn);
        debug!(
            "Pre-flight context estimate: ~{calibrated_tokens} tokens (raw: {raw_tokens}, scale: {:.2}, num_ctx: {})",
            self.token_calibration.scale(),
            turn.num_ctx
        );

        let (history_to_compact, preserved_trailing) =
            split_messages_for_auto_compact(&self.session.messages);
        if should_auto_compact(
            &self.config,
            calibrated_tokens,
            turn.num_ctx,
            history_to_compact,
            self.session.turns_since_auto_compact,
            !self.inflight.queued_interjections.is_empty(),
            allow_auto_compact,
        ) {
            let pct = (calibrated_tokens as f64 / f64::from(turn.num_ctx.max(1))) * 100.0;
            let history_vec = history_to_compact.to_vec();
            let compacted_count = history_vec.len();
            info!(
                "Pre-flight context usage (~{calibrated_tokens}/{} tokens, {pct:.0}%) reached auto-compact threshold ({:.0}%); auto-compacting {compacted_count} message(s) (preserving {} trailing message(s))",
                turn.num_ctx,
                self.config.app.effective_auto_compact_threshold() * 100.0,
                preserved_trailing.len()
            );

            let instructions = self
                .resolve_compact_instructions(None, None, true)
                .unwrap_or(None);
            self.inflight.pending_auto_compact = Some(PendingAutoCompact {
                turn: turn.clone(),
                preserved_trailing,
                pre_tokens: calibrated_tokens,
                compacted_count,
            });
            self.spawn_compaction_task(
                history_vec,
                instructions,
                format!("auto-compacting context (~{pct:.0}%)"),
            );
            return;
        }

        self.inflight.last_preflight_raw_tokens = Some(raw_tokens);

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

        self.session.auto_plan_turns = 0;
        self.turn = turn.clone();
        self.session.messages.push(user_msg);

        self.save_and_emit_chat(new_image_blobs).await;
        self.emit_estimated_token_usage();
        self.start_user_turn(&turn).await;
    }

    pub(super) async fn handle_cancel_stream(&mut self) {
        let had_task = self.finish_active_task();
        let had_pending_auto_compact = self.inflight.pending_auto_compact.take().is_some();
        if !had_task
            && !had_pending_auto_compact
            && self.inflight.pending_truncated.is_none()
            && self.inflight.pending_tools.is_empty()
            && self.inflight.queued_interjections.is_empty()
        {
            return;
        }

        info!("User interrupted active turn");
        self.session.auto_plan_turns = 0;
        self.inflight.pending_stream_done = None;
        self.inflight.last_preflight_raw_tokens = None;
        if had_task || had_pending_auto_compact {
            let _ = self.tx.send(EngineEvent::ToolCompleted);
        }
        let flushed = self.flush_interrupted_stream_buffers("Generation cancelled");
        let closed = self.close_unresolved_tool_calls("Cancelled by user.");

        if self.flush_queued_interjections().await {
            return;
        }

        if flushed || closed > 0 {
            self.save_session_bg(Vec::new()).await;
        }
        self.emit_chat();
        if flushed || closed > 0 {
            self.emit_estimated_token_usage();
        }
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
        self.token_calibration.reset();

        self.save_session_bg(Vec::new()).await;
        self.emit_model_config();
        self.emit_estimated_token_usage();
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
        self.session.turns_since_auto_compact =
            self.session.turns_since_auto_compact.saturating_add(1);
        if let Some(raw_est) = self.inflight.last_preflight_raw_tokens.take()
            && prompt_tokens > 0
        {
            self.token_calibration.observe(raw_est, prompt_tokens);
        }
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
            let max_turns = self.config.app.plan_autoloop_max_turns;
            if self.config.app.plan_autoloop
                && self.turn.enable_tools
                && tools::is_tool_enabled_for_turn(
                    "plan",
                    &self.config,
                    &self.session.tools,
                    self.active_turn_skill(),
                )
                && self.inflight.queued_interjections.is_empty()
                && self.session.auto_plan_turns < max_turns
            {
                let cfg = self.config.clone();
                let sid = self.session.id.clone();
                let should_continue =
                    tokio::task::spawn_blocking(move || tools::plan::should_autoloop(&cfg, &sid))
                        .await
                        .unwrap_or(false);

                if should_continue {
                    self.session.auto_plan_turns += 1;
                    info!(
                        "Active plan tasks remain; auto-continuing plan loop ({}/{max_turns})",
                        self.session.auto_plan_turns
                    );
                    let reported_tokens = prompt_tokens.saturating_add(eval_tokens);
                    if reported_tokens > 0 {
                        self.emit_exact_token_usage(reported_tokens);
                    }
                    let prompt = if self.config.app.plan_autoloop_ignore_blocked {
                        "Continue working through `<plan>`. Move on to the next task, mark it `in_progress` while you work on it, and remove it with `plan` (`action: \"remove\"`) once finished."
                    } else {
                        "Continue working through `<plan>`. Move on to the next task, mark it `in_progress` while you work on it, and remove it with `plan` (`action: \"remove\"`) once finished. If you need user input before proceeding, mark the task `blocked` (`status: \"blocked\"`) and ask your question."
                    };
                    self.session.messages.push(ChatMessage::new("user", prompt));
                    self.save_and_emit_chat(Vec::new()).await;
                    let turn = self.turn.clone();
                    self.spawn_model_turn(&turn).await;
                    return;
                }
            }
            self.session.auto_plan_turns = 0;

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
                let reported_tokens = prompt_tokens.saturating_add(eval_tokens);
                if reported_tokens > 0 {
                    self.emit_exact_token_usage(reported_tokens);
                }
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
        self.inflight.last_preflight_raw_tokens = None;
        let flushed = self.flush_interrupted_stream_buffers("Stream interrupted");
        error!("API Error: {err}");
        self.session.messages.push(ChatMessage::new("error", err));
        self.save_session_bg(Vec::new()).await;
        let _ = self.tx.send(ev);
        if flushed {
            self.emit_estimated_token_usage();
        }
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
        if !after_turn {
            self.emit_estimated_token_usage();
        }
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

    fn resolve_compact_instructions(
        &mut self,
        skill: Option<&str>,
        instructions: Option<String>,
        fallback_on_skill_err: bool,
    ) -> Result<Option<String>, String> {
        // Resolve the effective compaction-guide skill: an explicit `@name`
        // (from `/compact @name`) wins over the `[app] compact_skill` default.
        let effective_skill: Option<String> = skill
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

        match effective_skill {
            Some(name) => {
                // Re-discover skills from disk so freshly added/edited guide
                // skills are available before we render the instructions.
                self.refresh_skills();
                match self.render_compact_skill(&name, instructions.as_deref()) {
                    Ok(combined) => Ok(Some(combined)),
                    Err(e) if fallback_on_skill_err => {
                        warn!(
                            "Auto-compact skill '@{name}' unavailable ({e}); falling back to default compaction prompt."
                        );
                        Ok(instructions)
                    }
                    Err(e) => Err(e),
                }
            }
            None => Ok(instructions),
        }
    }

    fn spawn_compaction_task(
        &mut self,
        messages: Vec<ChatMessage>,
        instructions: Option<String>,
        status_summary: String,
    ) {
        let _ = self.tx.send(EngineEvent::ToolStarted {
            summary: status_summary,
            index: 1,
            total: 1,
        });

        let provider = self.provider;
        let client = self.client.clone();
        let model = self.model.clone();
        let config = self.config.clone();
        let num_ctx = self.turn.num_ctx;

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

        let instructions =
            match self.resolve_compact_instructions(skill.as_deref(), instructions, false) {
                Ok(inst) => inst,
                Err(e) => {
                    self.emit_error(e).await;
                    return;
                }
            };

        info!(
            "Compacting conversation history ({} messages)",
            self.session.messages.len()
        );
        let messages = self.session.messages.clone();
        self.spawn_compaction_task(
            messages,
            instructions,
            "compacting conversation context".into(),
        );
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
        let pending_auto = self.inflight.pending_auto_compact.take();

        match res {
            Ok(summary) => {
                self.session.turns_since_auto_compact = 0;
                match pending_auto {
                    Some(pending) => {
                        let prev_count = pending.compacted_count;
                        let pre_tokens = pending.pre_tokens;
                        self.session.messages = build_compacted_messages(
                            &summary,
                            prev_count,
                            Some((pre_tokens, pending.preserved_trailing)),
                        );
                        self.save_and_emit_chat(Vec::new()).await;
                        let (_, estimated_tokens) =
                            self.emit_estimated_token_usage_for(&pending.turn);
                        info!(
                            "Auto-compacted conversation ({prev_count} messages, ~{pre_tokens} -> ~{estimated_tokens} tokens, summary eval: {eval_tokens}); resuming model turn."
                        );
                        self.spawn_model_turn_inner(&pending.turn, false).await;
                    }
                    None => {
                        let prev_count = self.session.messages.len();
                        self.session.messages =
                            build_compacted_messages(&summary, prev_count, None);
                        self.save_and_emit_chat(Vec::new()).await;
                        let estimated_tokens = self.emit_estimated_token_usage();
                        info!(
                            "Conversation compacted from {prev_count} messages to 2 checkpoint messages (~{estimated_tokens} tokens, summary eval: {eval_tokens})."
                        );
                    }
                }
            }
            Err(err) => {
                if let Some(pending) = pending_auto {
                    self.session.turns_since_auto_compact = 0;
                    warn!(
                        "Auto-compaction failed ({err}); continuing model turn without compacting."
                    );
                    self.session.messages.push(ChatMessage::new(
                        "error",
                        format!("Auto-compaction failed ({err}); continuing without compacting."),
                    ));
                    self.save_and_emit_chat(Vec::new()).await;
                    self.spawn_model_turn_inner(&pending.turn, false).await;
                } else {
                    error!("Failed to compact conversation: {err}");
                    self.emit_error(format!("Failed to compact conversation: {err}"))
                        .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolFunction;
    use serde_json::json;

    fn test_config(auto_compact: bool, threshold: f64, cooldown: u32) -> Config {
        let mut cfg = Config::default();
        cfg.app.auto_compact = auto_compact;
        cfg.app.auto_compact_threshold = threshold;
        cfg.app.auto_compact_cooldown_turns = cooldown;
        cfg
    }

    fn test_turn(num_ctx: u32) -> TurnConfig {
        TurnConfig {
            system_prompt: String::new(),
            num_ctx,
            enable_tools: true,
            active_skill: None,
            skill_args: None,
            raw_skill_args: None,
            skill_inputs: BTreeMap::new(),
        }
    }

    #[test]
    fn split_messages_preserves_trailing_user_prompt_and_pre_turn_hooks() {
        let msgs = vec![
            ChatMessage::new("user", "first question"),
            ChatMessage::new("assistant", "first answer"),
            ChatMessage::new("user", "second question"),
            ChatMessage::new_hook("git status", "clean", false),
        ];

        let (history, preserved) = split_messages_for_auto_compact(&msgs);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "first question");
        assert_eq!(history[1].content, "first answer");
        assert_eq!(preserved.len(), 2);
        assert_eq!(preserved[0].content, "second question");
        assert_eq!(preserved[1].role, "hook");
    }

    #[test]
    fn split_messages_preserves_image_only_trailing_user_prompt() {
        let msgs = vec![
            ChatMessage::new("user", "hello"),
            ChatMessage::new("assistant", "hi"),
            ChatMessage {
                image_names: Some(vec!["screen.png".into()]),
                ..ChatMessage::new("user", "")
            },
        ];

        let (history, preserved) = split_messages_for_auto_compact(&msgs);
        assert_eq!(history.len(), 2);
        assert_eq!(preserved.len(), 1);
        assert_eq!(
            preserved[0].image_names.as_deref(),
            Some(&["screen.png".to_string()][..])
        );
    }

    #[test]
    fn split_messages_keeps_all_messages_mid_tool_loop_or_first_turn() {
        let mid_tool = vec![
            ChatMessage::new("user", "read file"),
            ChatMessage {
                tool_calls: Some(vec![ToolCall {
                    function: ToolFunction {
                        name: "read_file".into(),
                        arguments: json!({ "file_path": "a.rs" }),
                    },
                    thought_signature: None,
                }]),
                ..ChatMessage::new("assistant", "")
            },
            ChatMessage::new_tool("read_file", "huge file contents", false, false),
        ];
        let (history, preserved) = split_messages_for_auto_compact(&mid_tool);
        assert_eq!(history.len(), 3);
        assert!(preserved.is_empty());

        let first_turn = vec![ChatMessage::new("user", "giant first prompt")];
        let (history, preserved) = split_messages_for_auto_compact(&first_turn);
        assert_eq!(history.len(), 1);
        assert!(preserved.is_empty());
    }

    #[test]
    fn should_auto_compact_enforces_flag_threshold_cooldown_queue_and_min_history() {
        let history = vec![
            ChatMessage::new("user", "question"),
            ChatMessage::new("assistant", "answer"),
        ];

        // Disabled by default.
        let disabled_cfg = test_config(false, 0.75, 5);
        assert!(!should_auto_compact(
            &disabled_cfg,
            900,
            1000,
            &history,
            u32::MAX,
            false,
            true
        ));

        let cfg = test_config(true, 0.75, 5);
        // Below threshold (749 < 750).
        assert!(!should_auto_compact(
            &cfg,
            749,
            1000,
            &history,
            u32::MAX,
            false,
            true
        ));
        // At threshold (750 >= 750) with initial cooldown satisfied.
        assert!(should_auto_compact(
            &cfg,
            750,
            1000,
            &history,
            u32::MAX,
            false,
            true
        ));
        // Blocked while cooldown hasn't elapsed (4 < 5).
        assert!(!should_auto_compact(
            &cfg, 900, 1000, &history, 4, false, true
        ));
        // Allowed once cooldown reaches 5.
        assert!(should_auto_compact(
            &cfg, 900, 1000, &history, 5, false, true
        ));
        // Blocked when queued interjections exist.
        assert!(!should_auto_compact(
            &cfg,
            900,
            1000,
            &history,
            u32::MAX,
            true,
            true
        ));
        // Blocked on resumed turn (`allow_auto_compact = false`) even when cooldown == 0.
        let zero_cooldown = test_config(true, 0.75, 0);
        assert!(!should_auto_compact(
            &zero_cooldown,
            950,
            1000,
            &history,
            0,
            false,
            false
        ));

        // Single-turn prompt with excluded local/hook/error messages is not compacted.
        let single = vec![
            ChatMessage::new("error", "old error"),
            ChatMessage {
                include_in_context: false,
                ..ChatMessage::new("local", "ignored shell output")
            },
            ChatMessage {
                include_in_context: false,
                ..ChatMessage::new_hook("after_hook", "ignored hook output", false)
            },
            ChatMessage::new("user", "only user msg"),
        ];
        assert!(!should_auto_compact(
            &cfg,
            950,
            1000,
            &single,
            u32::MAX,
            false,
            true
        ));

        // Image-only user message + thought-only assistant message count as meaningful.
        let non_text = vec![
            ChatMessage {
                image_names: Some(vec!["shot.png".into()]),
                ..ChatMessage::new("user", "")
            },
            ChatMessage {
                thoughts: Some(vec![ThoughtPart {
                    text: Some("thinking".into()),
                    thought_signature: None,
                }]),
                ..ChatMessage::new("assistant", "")
            },
        ];
        assert!(should_auto_compact(
            &cfg,
            950,
            1000,
            &non_text,
            u32::MAX,
            false,
            true
        ));

        // Tool-call-only assistant message counts as meaningful alongside user + tool result.
        let tool_history = vec![
            ChatMessage::new("user", "read huge file"),
            ChatMessage {
                tool_calls: Some(vec![ToolCall {
                    function: ToolFunction {
                        name: "read_file".into(),
                        arguments: json!({ "file_path": "big.rs" }),
                    },
                    thought_signature: None,
                }]),
                ..ChatMessage::new("assistant", "")
            },
            ChatMessage::new_tool("read_file", "big output", false, false),
        ];
        assert!(should_auto_compact(
            &cfg,
            900,
            1000,
            &tool_history,
            u32::MAX,
            false,
            true
        ));
    }

    #[test]
    fn build_compacted_messages_handles_manual_trailing_prompt_and_tool_loop_continuation() {
        let manual = build_compacted_messages("Summary body", 12, None);
        assert_eq!(manual.len(), 2);
        assert_eq!(manual[0].role, "user");
        assert!(manual[0].content.contains("Prior 12 messages"));
        assert_eq!(manual[1].role, "assistant");
        assert!(manual[1].content.contains("Summary body"));

        let trailing = vec![ChatMessage::new("user", "Please refactor foo.rs")];
        let with_trailing = build_compacted_messages("Summary body", 8, Some((25_000, trailing)));
        assert_eq!(with_trailing.len(), 3);
        assert!(with_trailing[0].content.contains("Auto-Compacted"));
        assert!(with_trailing[0].content.contains("~25000 tokens"));
        assert_eq!(with_trailing[1].role, "assistant");
        assert_eq!(with_trailing[2].role, "user");
        assert_eq!(with_trailing[2].content, "Please refactor foo.rs");

        // Mid-tool-loop auto-compaction appends a continuation user cue.
        let mid_tool = build_compacted_messages("Tool findings", 5, Some((28_000, Vec::new())));
        assert_eq!(mid_tool.len(), 3);
        assert_eq!(mid_tool[2].role, "user");
        assert!(mid_tool[2].content.contains("Continue answering"));
    }

    #[tokio::test]
    async fn cancel_clears_pending_auto_compact_and_failure_falls_back_to_model_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(true, 0.75, 5);
        cfg.app.history_dir = tmp.path().to_string_lossy().into_owned();
        cfg.ollama.base_url = "http://127.0.0.1:1".into();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut engine = CoreEngine::new("qwen3.8:27b".into(), cfg, tx).unwrap();
        engine.set_save_session(false);

        // 1. Cancelling during auto-compaction clears pending state and emits ToolCompleted.
        engine.inflight.pending_auto_compact = Some(PendingAutoCompact {
            turn: test_turn(32_768),
            preserved_trailing: vec![ChatMessage::new("user", "keep me")],
            pre_tokens: 26_000,
            compacted_count: 4,
        });
        engine.handle_cancel_stream().await;
        assert!(engine.inflight.pending_auto_compact.is_none());
        let mut saw_tool_completed = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, EngineEvent::ToolCompleted) {
                saw_tool_completed = true;
            }
        }
        assert!(
            saw_tool_completed,
            "expected ToolCompleted on cancel during auto-compact"
        );

        // 2. Auto-compact failure logs a warning message, resets cooldown, and
        //    proceeds to start the model turn instead of blocking.
        engine.session.messages = vec![
            ChatMessage::new("user", "old q"),
            ChatMessage::new("assistant", "old a"),
            ChatMessage::new("user", "new q"),
        ];
        engine.session.turns_since_auto_compact = 10;
        engine.inflight.pending_auto_compact = Some(PendingAutoCompact {
            turn: test_turn(32_768),
            preserved_trailing: vec![ChatMessage::new("user", "new q")],
            pre_tokens: 26_000,
            compacted_count: 2,
        });

        engine
            .on_chat_compacted(Err("simulated failure".into()), 0, 0)
            .await;

        assert!(engine.inflight.pending_auto_compact.is_none());
        assert_eq!(engine.session.turns_since_auto_compact, 0);
        assert!(
            engine
                .session
                .messages
                .iter()
                .any(|m| { m.role == "error" && m.content.contains("Auto-compaction failed") }),
            "expected warning message in chat history"
        );
        let mut saw_stream_started = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, EngineEvent::StreamStarted) {
                saw_stream_started = true;
            }
        }
        assert!(
            saw_stream_started,
            "expected resumed model turn to emit StreamStarted after compaction failure"
        );

        // Cleanly abort the spawned background stream task.
        engine.finish_active_task();
    }

    #[tokio::test]
    async fn auto_compact_success_replaces_history_preserves_tail_and_resumes_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = test_config(true, 0.75, 5);
        cfg.app.history_dir = tmp.path().to_string_lossy().into_owned();
        cfg.ollama.base_url = "http://127.0.0.1:1".into();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut engine = CoreEngine::new("qwen3.8:27b".into(), cfg, tx).unwrap();
        engine.set_save_session(false);

        engine.session.messages = vec![
            ChatMessage::new("user", "old q"),
            ChatMessage::new("assistant", "old a"),
            ChatMessage::new("user", "current q"),
        ];
        engine.session.turns_since_auto_compact = 10;
        engine.inflight.pending_auto_compact = Some(PendingAutoCompact {
            turn: test_turn(32_768),
            preserved_trailing: vec![ChatMessage::new("user", "current q")],
            pre_tokens: 26_000,
            compacted_count: 2,
        });

        engine
            .on_chat_compacted(Ok("Checkpoint summary".into()), 500, 120)
            .await;

        assert!(engine.inflight.pending_auto_compact.is_none());
        assert_eq!(engine.session.turns_since_auto_compact, 0);
        assert_eq!(engine.session.messages.len(), 3);
        assert!(
            engine.session.messages[0]
                .content
                .contains("Prior 2 messages")
        );
        assert!(
            engine.session.messages[1]
                .content
                .contains("Checkpoint summary")
        );
        assert_eq!(engine.session.messages[2].content, "current q");

        let mut saw_stream_started = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, EngineEvent::StreamStarted) {
                saw_stream_started = true;
            }
        }
        assert!(
            saw_stream_started,
            "expected resumed model turn to emit StreamStarted after compaction success"
        );

        engine.finish_active_task();
    }
}
