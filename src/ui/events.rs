use super::{ActiveModal, MsgUiState, PendingTruncatedUi, SessionView, TuiApp};
use crate::core::{
    EngineEvent,
    session::{ChatMessage, SessionSummary, ThoughtPart},
};

impl TuiApp {
    pub(super) fn handle_engine_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::AttachmentsUpdated(list) => self.attachments = list,
            EngineEvent::ChatUpdated {
                session_id,
                is_fork,
                is_pinned,
                messages,
                queued_messages,
                num_ctx,
                system_prompt,
            } => {
                self.on_chat_updated(
                    session_id,
                    is_fork,
                    is_pinned,
                    messages,
                    queued_messages,
                    num_ctx,
                    &system_prompt,
                );
            }
            EngineEvent::QueuedInterjectionsUpdated(queued) => {
                self.queued_messages = queued;
                self.scroll_to_bottom();
            }
            EngineEvent::StreamStarted => {
                self.on_stream_started();
            }
            EngineEvent::StreamThoughtChunk(c) => {
                if self.streaming_text.is_empty() {
                    self.streaming_thought.push_str(&c);
                    self.stream_thought_dirty = true;
                }
            }
            EngineEvent::StreamChunk(c) => {
                if self.streaming_text.is_empty() && !c.is_empty() {
                    self.streaming_thought.clear();
                    self.stream_thought_cache.clear();
                    self.stream_thought_dirty = false;
                }
                self.streaming_text.push_str(&c);
                self.stream_dirty = true;
            }
            EngineEvent::StreamDone {
                prompt_tokens,
                eval_tokens,
                tool_calls,
                ..
            } => {
                if tool_calls.is_empty() {
                    self.active_turn_skill = None;
                }
                self.finish_stream();
                self.used_tokens = prompt_tokens + eval_tokens;
            }
            EngineEvent::StreamError(err) => {
                self.active_turn_skill = None;
                self.on_stream_error(err);
            }
            EngineEvent::Error(err) => {
                self.active_turn_skill = None;
                self.push_error_message(err);
            }
            EngineEvent::ToolStarted {
                summary,
                index,
                total,
            } => {
                let progress = if total > 1 {
                    format!(" ({index}/{total})")
                } else {
                    String::new()
                };
                self.active_tool_status = Some(format!("⚙ {summary}{progress} ..."));
                self.scroll_to_bottom();
            }
            EngineEvent::ToolCompleted
            | EngineEvent::ToolExecFinished(..)
            | EngineEvent::HookCompleted { .. }
            | EngineEvent::HooksFinished => {
                self.active_tool_status = None;
            }
            EngineEvent::ToolApprovalRequested {
                formatted_preview,
                index,
                total,
            } => {
                self.pending_tool = Some((formatted_preview, index, total));
                self.tool_feedback.clear();
                self.tool_scroll = 0;
                self.modal = ActiveModal::ToolApproval;
            }
            EngineEvent::TruncatedToolResolutionRequested {
                tool_name,
                summary,
                preview,
                raw_bytes,
                max_size,
                full_cap_bytes,
                index,
                total,
            } => {
                self.active_tool_status = None;
                self.pending_truncated = Some(PendingTruncatedUi {
                    tool_name,
                    summary,
                    preview,
                    raw_bytes,
                    max_size,
                    full_cap_bytes,
                    index,
                    total,
                    collapsed: true,
                    unfolded: false,
                    scroll: 0,
                });
                self.truncated_guide_prompt.clear();
                self.tool_scroll = 0;
                self.modal = ActiveModal::TruncatedTool;
            }
            EngineEvent::ToolCondensed(_) | EngineEvent::ChatCompacted { .. } => {}
            EngineEvent::TokenUsageUpdated(tokens) => {
                self.used_tokens = tokens;
            }
            EngineEvent::SessionsListed(list) => {
                self.on_sessions_listed(list);
            }
            EngineEvent::SkillsUpdated(skills) => {
                self.config.skills = skills;
            }
            EngineEvent::MemoryUpdated {
                global_memory,
                workspace_memory,
            } => {
                self.config.global_memory = global_memory;
                self.config.workspace_memory = workspace_memory;
            }
            EngineEvent::ModelConfigUpdated {
                model,
                num_ctx,
                max_tool_output_size,
            } => {
                self.model = model;
                self.num_ctx = num_ctx;
                self.max_tool_output_size = max_tool_output_size;
                if let Some(idx) = self.config.models.iter().position(|m| m.name == self.model) {
                    self.model_idx = idx;
                }
            }
            EngineEvent::PayloadCaptured(preview) => {
                self.last_payload_preview = preview;
            }
            EngineEvent::PayloadInspectorOpened(preview) => {
                self.last_payload_preview = preview;
                self.payload_scroll = 0;
                self.modal = ActiveModal::PayloadInspector;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_chat_updated(
        &mut self,
        session_id: String,
        is_fork: bool,
        is_pinned: bool,
        messages: Vec<ChatMessage>,
        queued_messages: Vec<ChatMessage>,
        num_ctx: u32,
        system_prompt: &str,
    ) {
        if messages.is_empty() {
            self.used_tokens = 0;
        } else if self.session_id != session_id || messages.len() < self.messages.len() {
            let approx_chars: usize = messages.iter().map(|m| m.content.len()).sum();
            self.used_tokens = (approx_chars as u64 / 4).max(1);
        }
        if self.prompt_history.is_empty() {
            for m in &messages {
                let trimmed = m.content.trim();
                if m.role == "user"
                    && !trimmed.is_empty()
                    && self
                        .prompt_history
                        .last()
                        .is_none_or(|last| last != trimmed)
                {
                    self.prompt_history.push(trimmed.to_string());
                }
            }
        }
        let session_changed = self.session_id != session_id;
        if session_changed {
            self.session_auto_approve_count = 0;
        }
        self.session_id = session_id;
        self.session_is_fork = is_fork;
        self.session_is_pinned = is_pinned;

        let is_reset_or_reload = session_changed
            || messages.len() < self.messages.len()
            || self
                .messages
                .iter()
                .zip(messages.iter())
                .any(|(old, new)| old != new);

        let prev_msg_count = if is_reset_or_reload {
            self.active_turn_skill = None;
            self.msg_states.clear();
            self.styled_messages.clear();
            self.copied_code_flash = None;
            if self
                .fork_flash
                .is_some_and(|(idx, exp)| idx >= messages.len() || std::time::Instant::now() >= exp)
            {
                self.fork_flash = None;
            }
            0
        } else {
            self.msg_states.truncate(self.messages.len());
            self.styled_messages.truncate(self.messages.len());
            self.messages.len()
        };

        self.messages = messages;
        self.queued_messages = queued_messages;
        self.finish_stream();
        self.streaming_text.clear();
        self.streaming_thought.clear();
        self.num_ctx = num_ctx;
        self.sync_system_prompt(system_prompt);
        let total_slots = self.messages.len() + 1 + self.queued_messages.len();
        self.msg_states.resize(total_slots, MsgUiState::default());

        for (i, m) in self.messages.iter().enumerate().skip(prev_msg_count) {
            if matches!(m.role.as_str(), "tool" | "hook") {
                self.msg_states[i].collapsed = true;
            } else if m.role == "error"
                || (!is_reset_or_reload && matches!(m.role.as_str(), "assistant" | "local"))
            {
                self.msg_states[i].unfolded = true;
            }
        }
        if is_reset_or_reload {
            if let Some(last_idx) = self
                .messages
                .iter()
                .rposition(|m| matches!(m.role.as_str(), "assistant" | "local" | "error"))
            {
                self.msg_states[last_idx].unfolded = true;
            }
            self.scroll_to_bottom();
        } else if self.messages.last().is_some_and(|m| m.role == "error") {
            self.scroll_to_bottom();
        }
    }

    fn on_stream_started(&mut self) {
        self.is_streaming = true;
        self.active_tool_status = None;
        self.streaming_text.clear();
        self.streaming_thought.clear();
        self.stream_cache.clear();
        self.stream_thought_cache.clear();
        self.stream_dirty = true;
        self.stream_thought_dirty = true;
        self.scroll_to_bottom();
    }

    fn on_stream_error(&mut self, err: String) {
        let partial = std::mem::take(&mut self.streaming_text);
        let partial_thought = std::mem::take(&mut self.streaming_thought);
        self.finish_stream();
        let had_text = !partial.trim().is_empty();
        let had_thought = !partial_thought.trim().is_empty();
        if had_text || had_thought {
            let final_content = if had_text {
                format!("{partial}\n\n*[Stream interrupted]*")
            } else {
                String::from("*[Stream interrupted]*")
            };
            self.messages.push(ChatMessage {
                thoughts: had_thought.then(|| {
                    vec![ThoughtPart {
                        text: Some(partial_thought),
                        thought_signature: None,
                    }]
                }),
                ..ChatMessage::new("assistant", final_content)
            });
        }
        self.push_error_message(err);
    }

    fn on_sessions_listed(&mut self, list: Vec<SessionSummary>) {
        self.sessions = list;
        if self.sessions_modal_just_opened {
            self.sessions_modal_just_opened = false;
            if let Some(active) = self
                .sessions
                .iter()
                .find(|s| !self.session_id.is_empty() && s.id == self.session_id)
            {
                self.session_view = if active.pinned {
                    SessionView::Pinned
                } else {
                    SessionView::Recent
                };
            }
            let filtered = self.filtered_sessions();
            self.session_idx = filtered
                .iter()
                .position(|s| !self.session_id.is_empty() && s.id == self.session_id)
                .unwrap_or(0);
        } else {
            let filtered_len = self.filtered_sessions().len();
            self.session_idx = self.session_idx.min(filtered_len.saturating_sub(1));
        }
    }

    pub(super) fn finish_stream(&mut self) {
        self.is_streaming = false;
        self.active_tool_status = None;
        self.stream_cache.clear();
        self.stream_thought_cache.clear();
        self.stream_dirty = false;
        self.stream_thought_dirty = false;
    }

    pub(super) fn push_error_message(&mut self, err: String) {
        self.messages.push(ChatMessage::new("error", err));
        let total_slots = self.messages.len() + 1 + self.queued_messages.len();
        self.msg_states.resize(total_slots, MsgUiState::default());
        if let Some(last_idx) = self.messages.len().checked_sub(1) {
            self.msg_states[last_idx].unfolded = true;
        }
        self.scroll_to_bottom();
    }

    pub(super) fn sync_system_prompt(&mut self, system_prompt: &str) {
        if let Some(idx) = self
            .config
            .system_presets
            .iter()
            .position(|p| p.prompt == system_prompt)
        {
            self.sys_preset_idx = idx;
            self.custom_system_prompt = None;
        } else {
            self.custom_system_prompt = (!system_prompt.is_empty()).then(|| system_prompt.into());
        }
    }
}
