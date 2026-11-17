pub mod attachments;
pub mod config;
pub mod gemini;
pub mod logger;
pub mod ollama;
pub mod provider;
pub mod session;
pub mod skills;

use crate::tools::{self, ToolCall};
use attachments::{ImageAttachment, ImageStaging};
use config::{Config, Memory, MemoryScope};
use provider::Provider;
use reqwest::Client;
use session::{ChatMessage, SessionManager, SessionSummary, ThoughtPart};
use skills::SkillDefinition;
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub system_prompt: String,
    pub num_ctx: u32,
    pub enable_tools: bool,
    pub active_skill: Option<String>,
    pub skill_args: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedToolAction {
    ProceedTruncated,
    ProceedFull,
    CondenseTruncated,
    CondenseFull,
    Discard,
}

#[derive(Debug)]
pub enum EngineCommand {
    PasteClipboard,
    ImportFile(PathBuf),
    ClearImages,
    SendPrompt {
        prompt: String,
        turn: TurnConfig,
    },
    CancelStream,
    ResolveTool {
        approved: bool,
        always_approve: bool,
        feedback: String,
        turn: TurnConfig,
    },
    ResolveTruncatedTool {
        action: TruncatedToolAction,
        guide_prompt: String,
        turn: TurnConfig,
    },
    LoadSession(String),
    ForkSession(usize),
    RenameSession(String),
    TogglePinSession(Option<String>),
    DeleteSession(String),
    RefreshSessions,
    ResetChat,
    SwitchModel {
        model: String,
        num_ctx: u32,
        max_tool_output_size: usize,
    },
    SetTurnConfig(TurnConfig),
    SetMemory {
        scope: MemoryScope,
        memory: Memory,
    },
    RunShellCommand {
        command: String,
        include_in_context: bool,
    },
    InspectPayload {
        draft_prompt: String,
        turn: TurnConfig,
    },
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    AttachmentsUpdated(Vec<ImageAttachment>),
    ChatUpdated {
        session_id: String,
        is_fork: bool,
        is_pinned: bool,
        messages: Vec<ChatMessage>,
        queued_messages: Vec<ChatMessage>,
        num_ctx: u32,
        system_prompt: String,
    },
    QueuedInterjectionsUpdated(Vec<ChatMessage>),
    StreamStarted,
    StreamThoughtChunk(String),
    StreamChunk(String),
    StreamDone {
        prompt_tokens: u64,
        eval_tokens: u64,
        tool_calls: Vec<ToolCall>,
        thoughts: Vec<ThoughtPart>,
    },
    StreamError(String),
    Error(String),
    ToolStarted {
        summary: String,
        index: usize,
        total: usize,
    },
    ToolCompleted {
        output: String,
        full_output: String,
        raw_bytes: usize,
        truncated: bool,
    },
    ShellCommandCompleted {
        command: String,
        output: String,
        full_output: String,
        raw_bytes: usize,
        truncated: bool,
        include_in_context: bool,
    },
    HookCompleted {
        tool_name: String,
        summary: String,
        output: String,
        truncated: bool,
    },
    HooksFinished,
    ToolApprovalRequested {
        formatted_preview: String,
        index: usize,
        total: usize,
    },
    TruncatedToolResolutionRequested {
        tool_name: String,
        summary: String,
        preview: String,
        raw_bytes: usize,
        max_size: usize,
        full_cap_bytes: usize,
        index: usize,
        total: usize,
    },
    ToolCondensed(Result<String, String>),
    SessionsListed(Vec<SessionSummary>),
    SkillsUpdated(Vec<SkillDefinition>),
    MemoryUpdated {
        global_memory: Memory,
        workspace_memory: Memory,
    },
    ModelConfigUpdated {
        model: String,
        num_ctx: u32,
        max_tool_output_size: usize,
    },
    PayloadCaptured(String),
    PayloadInspectorOpened(String),
}

struct QueuedInterjection {
    msg: ChatMessage,
    new_images: Vec<(String, Vec<u8>)>,
    turn: TurnConfig,
}

enum PendingTruncatedSource {
    ModelTool { tool_name: String },
    ManualShell { command: String },
}

struct PendingTruncatedTool {
    source: PendingTruncatedSource,
    summary: String,
    truncated_output: String,
    full_output: String,
    raw_bytes: usize,
    full_cap_bytes: usize,
    index: usize,
    total: usize,
}

pub struct CoreEngine {
    model: String,
    provider: Provider,
    config: Config,
    client: Client,
    attachments: Arc<ImageStaging>,
    sessions: Arc<SessionManager>,
    session_id: String,
    session_is_fork: bool,
    session_is_pinned: bool,
    session_custom_title: Option<String>,
    turn: TurnConfig,
    active_max_tool_output_size: usize,
    session_auto_approve: HashSet<String>,
    executed_skill_hooks: HashSet<String>,
    messages: Vec<ChatMessage>,
    sent_images: HashSet<PathBuf>,
    stream_buffer: String,
    thought_stream_buffer: String,
    active_task: Option<JoinHandle<()>>,
    internal_rx: Option<UnboundedReceiver<EngineEvent>>,
    executing_tool: bool,
    active_tool_name: Option<String>,
    active_tool_summary: Option<String>,
    pending_truncated: Option<PendingTruncatedTool>,
    queued_interjections: Vec<QueuedInterjection>,
    save_task: Option<JoinHandle<()>>,
    pending_tools: VecDeque<ToolCall>,
    tx: UnboundedSender<EngineEvent>,
}

impl CoreEngine {
    pub fn new(
        model: String,
        config: Config,
        tx: UnboundedSender<EngineEvent>,
    ) -> anyhow::Result<Self> {
        let provider = Provider::from_model(&model);
        let attachments = Arc::new(ImageStaging::new()?);
        let sessions = Arc::new(SessionManager::new(
            &config.app.history_dir,
            config.workspace.as_deref(),
        )?);
        let client = provider.build_client(&config)?;
        let num_ctx = config.context_for_model(&model);
        let active_max_tool_output_size = config.max_tool_output_for_model(&model);

        Ok(Self {
            model,
            provider,
            config,
            client,
            attachments,
            sessions,
            session_id: SessionManager::new_id(),
            session_is_fork: false,
            session_is_pinned: false,
            session_custom_title: None,
            turn: TurnConfig {
                system_prompt: String::new(),
                num_ctx,
                enable_tools: true,
                active_skill: None,
                skill_args: None,
            },
            active_max_tool_output_size,
            session_auto_approve: HashSet::new(),
            executed_skill_hooks: HashSet::new(),
            messages: Vec::new(),
            sent_images: HashSet::new(),
            stream_buffer: String::new(),
            thought_stream_buffer: String::new(),
            active_task: None,
            internal_rx: None,
            executing_tool: false,
            active_tool_name: None,
            active_tool_summary: None,
            pending_truncated: None,
            queued_interjections: Vec::new(),
            save_task: None,
            pending_tools: VecDeque::new(),
            tx,
        })
    }

    fn finish_active_task(&mut self) -> (bool, bool) {
        self.internal_rx = None;
        let was_tool = std::mem::replace(&mut self.executing_tool, false);
        let had_task = self.active_task.take().inspect(|h| h.abort()).is_some();
        (had_task, was_tool)
    }

    fn is_turn_active(&self) -> bool {
        self.active_task.is_some() || self.executing_tool
    }

    fn abort_active_state(&mut self) {
        self.finish_active_task();
        self.active_tool_name = None;
        self.active_tool_summary = None;
        self.pending_truncated = None;
        self.queued_interjections.clear();
        self.pending_tools.clear();
        self.sent_images.clear();
        self.stream_buffer.clear();
        self.thought_stream_buffer.clear();
    }

    fn unresolved_tool_names(&self) -> Vec<String> {
        let mut resolved = 0usize;
        for m in self.messages.iter().rev() {
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

    fn close_unresolved_tool_calls(&mut self, message: &str) -> usize {
        let unresolved_names = self.unresolved_tool_names();
        let count = unresolved_names.len();
        self.pending_tools.clear();
        self.active_tool_name = None;
        self.active_tool_summary = None;
        self.pending_truncated = None;
        for name in unresolved_names {
            self.messages
                .push(ChatMessage::new_tool(name, message, false));
        }
        count
    }

    async fn flush_save_task(&mut self) {
        if let Some(save_handle) = self.save_task.take() {
            let _ = save_handle.await;
        }
    }

    async fn clear_attachments(&self) {
        let staging = Arc::clone(&self.attachments);
        let _ = tokio::task::spawn_blocking(move || staging.clear()).await;
        let _ = self.tx.send(EngineEvent::AttachmentsUpdated(vec![]));
    }

    async fn emit_error(&mut self, err: impl Into<String>) {
        let err = err.into();
        self.messages.push(ChatMessage::new("error", err.clone()));
        self.save_session_bg(Vec::new()).await;
        let _ = self.tx.send(EngineEvent::Error(err));
    }

    fn refresh_skills(&mut self) {
        self.config.skills = skills::discover_skills(
            &self.config.app.skills_dir,
            self.config.workspace_skills_dir.as_deref(),
        );
        let _ = self
            .tx
            .send(EngineEvent::SkillsUpdated(self.config.skills.clone()));
    }

    fn emit_model_config(&self) {
        let _ = self.tx.send(EngineEvent::ModelConfigUpdated {
            model: self.model.clone(),
            num_ctx: self.turn.num_ctx,
            max_tool_output_size: self.active_max_tool_output_size,
        });
    }

    fn update_turn_preserving_skill(&mut self, new_turn: TurnConfig) {
        let active_skill = self.turn.active_skill.take();
        let skill_args = self.turn.skill_args.take();
        self.turn = new_turn;
        if let Some(skill_name) = active_skill {
            if skills::check_skill_activation(&skill_name, self.turn.enable_tools, &self.config)
                .is_ok()
            {
                self.turn.active_skill = Some(skill_name);
                self.turn.skill_args = skill_args;
            } else if (!self.pending_tools.is_empty() || self.is_turn_active())
                && skills::check_skill_activation(&skill_name, true, &self.config).is_ok()
            {
                self.turn.enable_tools = true;
                self.turn.active_skill = Some(skill_name);
                self.turn.skill_args = skill_args;
            }
        }
    }

    fn reset_skill_and_sync_model_limits(&mut self) {
        self.turn.active_skill = None;
        self.turn.skill_args = None;
        self.turn.num_ctx = self.config.context_for_model(&self.model);
        self.active_max_tool_output_size = self.config.max_tool_output_for_model(&self.model);
        self.emit_model_config();
    }

    fn active_turn_skill(&self) -> Option<&str> {
        self.turn.active_skill.as_deref()
    }

    fn is_call_auto_approved(&self, call: &ToolCall) -> bool {
        let all_rules = self
            .config
            .app
            .auto_approve
            .iter()
            .chain(self.session_auto_approve.iter())
            .map(String::as_str);
        tools::is_auto_approved(call, all_rules)
    }

    fn tool_batch_progress(&self) -> (usize, usize) {
        let total = self
            .messages
            .iter()
            .rev()
            .find_map(|m| m.tool_calls.as_ref().map(Vec::len))
            .unwrap_or(self.pending_tools.len())
            .max(self.pending_tools.len());
        let index = total.saturating_sub(self.pending_tools.len()) + 1;
        (index, total)
    }

    fn prompt_next_tool(&mut self) {
        let Some(call) = self.pending_tools.front() else {
            return;
        };
        let (index, total) = self.tool_batch_progress();
        let _ = self.tx.send(EngineEvent::ToolApprovalRequested {
            formatted_preview: tools::format_approval(call),
            index,
            total,
        });
    }

    async fn flush_queued_interjections(&mut self) -> bool {
        if self.queued_interjections.is_empty() {
            return false;
        }
        let skipped = self.close_unresolved_tool_calls(
            "Skipped remaining tool call to process user interjection.",
        );
        if skipped > 0 {
            info!("Skipped {skipped} remaining tool call(s) due to queued user interjection");
        }
        if self
            .queued_interjections
            .iter()
            .any(|i| i.turn.active_skill.is_some())
        {
            self.refresh_skills();
        }
        let mut new_images = Vec::new();
        let mut has_valid_prompt = false;
        for item in self.queued_interjections.drain(..) {
            if let Some(ref skill_name) = item.turn.active_skill
                && let Err(reason) =
                    skills::check_skill_activation(skill_name, item.turn.enable_tools, &self.config)
            {
                warn!("{reason}");
                self.messages.push(ChatMessage::new("error", reason));
                continue;
            }
            self.turn = item.turn;
            self.messages.push(item.msg);
            new_images.extend(item.new_images);
            has_valid_prompt = true;
        }
        self.save_and_emit_chat(new_images).await;
        if has_valid_prompt {
            let turn = self.turn.clone();
            self.start_user_turn(&turn).await;
        }
        true
    }

    fn context_padded_capture_limit(&self) -> usize {
        let ctx_tokens = self.turn.num_ctx as usize;
        // Reserve at least 25% of the context window (minimum 2048 tokens) for prompt overhead and model response.
        let padding_tokens = (ctx_tokens / 4).max(2048);
        let avail_tokens = ctx_tokens.saturating_sub(padding_tokens).max(512);
        // Roughly ~4 bytes per token, capped to fit within the context window budget.
        avail_tokens.saturating_mul(4)
    }

    async fn complete_tool_result(&mut self, tool_name: String, output: String, truncated: bool) {
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
        let raw_bytes = output.len();
        self.messages
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
        self.messages.push(msg);
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
        self.executing_tool = true;
        let tool_name = match &pending.source {
            PendingTruncatedSource::ModelTool { tool_name } => tool_name.clone(),
            PendingTruncatedSource::ManualShell { .. } => "shell_command".to_string(),
        };
        self.active_tool_name = Some(tool_name.clone());
        self.active_tool_summary = Some(pending.summary.clone());
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
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user" && !m.content.trim().is_empty())
            .map(|m| m.content.clone())
            .unwrap_or_default();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.internal_rx = Some(rx);

        self.active_task = Some(tokio::spawn(async move {
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
        self.executing_tool = true;
        self.active_tool_name = Some(call.function.name.clone());
        self.active_tool_summary = Some(sig.clone());
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
        self.internal_rx = Some(rx);

        self.active_task = Some(tokio::spawn(async move {
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

    async fn process_pending_tools(&mut self) {
        if self.flush_queued_interjections().await {
            return;
        }

        let mut modified = false;

        while let Some(call) = self.pending_tools.front() {
            if !tools::is_tool_enabled_for_turn(
                &call.function.name,
                &self.config,
                self.active_turn_skill(),
            ) {
                let call = self.pending_tools.pop_front().unwrap();
                warn!(
                    "Rejecting disabled tool call '{}' without prompting",
                    call.function.name
                );
                let err_msg = format!("Error: Unknown tool: '{}'", call.function.name);
                self.messages
                    .push(ChatMessage::new_tool(call.function.name, err_msg, false));
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
            let call = self.pending_tools.pop_front().unwrap();
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

    async fn update_attachments<F>(&self, op: F)
    where
        F: FnOnce(&ImageStaging) -> anyhow::Result<PathBuf> + Send + 'static,
    {
        let staging = Arc::clone(&self.attachments);
        if let Ok((res, list)) =
            tokio::task::spawn_blocking(move || (op(&staging), staging.list_attachments())).await
        {
            if let Err(e) = res {
                warn!("Attachment operation failed: {e}");
            }
            let _ = self.tx.send(EngineEvent::AttachmentsUpdated(list));
        }
    }

    async fn collect_unsent_images(&mut self) -> (Option<Vec<String>>, Vec<(String, Vec<u8>)>) {
        let staging = Arc::clone(&self.attachments);
        let already_sent = self.sent_images.clone();

        let (names, blobs, newly_sent) = tokio::task::spawn_blocking(move || {
            let mut names = Vec::new();
            let mut blobs = Vec::new();
            let mut sent = Vec::new();

            for att in staging.list_attachments() {
                if already_sent.contains(&att.path) {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&att.path) {
                    names.push(att.name.clone());
                    blobs.push((att.name, bytes));
                    sent.push(att.path);
                }
            }
            (names, blobs, sent)
        })
        .await
        .unwrap_or_default();

        self.sent_images.extend(newly_sent);
        ((!names.is_empty()).then_some(names), blobs)
    }

    async fn emit_sessions(&mut self, delete_first: Option<String>) {
        self.flush_save_task().await;
        let sessions = Arc::clone(&self.sessions);
        if let Ok(list) = tokio::task::spawn_blocking(move || {
            if let Some(id) = delete_first {
                let _ = sessions.delete(&id);
            }
            sessions.list()
        })
        .await
        {
            let _ = self.tx.send(EngineEvent::SessionsListed(list));
        }
    }

    pub async fn run(mut self, mut cmd_rx: UnboundedReceiver<EngineCommand>) {
        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                    Some(cmd) => self.handle_command(cmd).await,
                    None => break,
                },
                maybe_ev = async {
                    match self.internal_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => match maybe_ev {
                    Some(ev) => self.handle_internal_event(ev).await,
                    None => {
                        self.finish_active_task();
                    }
                },
            }
        }

        self.finish_active_task();

        info!("Shutting down CoreEngine, awaiting pending session save...");
        self.flush_save_task().await;
        info!("All background tasks completely flushed.");
    }

    async fn handle_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::PasteClipboard => {
                info!("Pasting image from clipboard");
                self.update_attachments(|s| s.paste_clipboard()).await;
            }
            EngineCommand::ImportFile(path) => {
                info!("Importing file: {}", path.display());
                self.update_attachments(move |s| s.import_file(&path)).await;
            }
            EngineCommand::ClearImages => {
                info!("Clearing attached images");
                self.clear_attachments().await;
            }
            EngineCommand::SendPrompt { prompt, mut turn } => {
                if turn.active_skill.is_some() {
                    self.refresh_skills();
                }
                if let Some(ref skill_name) = turn.active_skill
                    && let Err(reason) =
                        skills::check_skill_activation(skill_name, turn.enable_tools, &self.config)
                {
                    warn!("{reason}");
                    self.emit_error(reason).await;
                    return;
                }
                if let Some(ref skill_name) = turn.active_skill
                    && let Some(skill) = skills::find_skill(&self.config, skill_name)
                {
                    turn.active_skill = Some(skill.name.clone());
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
                    info!(
                        "Queuing user interjection while process is active (executing_tool: {})",
                        self.executing_tool
                    );
                    self.queued_interjections.push(QueuedInterjection {
                        msg: user_msg,
                        new_images: new_image_blobs,
                        turn,
                    });
                    self.emit_queued_interjections();
                    return;
                }

                let skipped = self.close_unresolved_tool_calls(
                    "The user skipped this tool call and sent a new message.",
                );
                if skipped > 0 {
                    warn!("Auto-declined {skipped} unresolved tool call(s) before new prompt");
                }

                self.turn = turn.clone();
                self.messages.push(user_msg);

                self.save_and_emit_chat(new_image_blobs).await;
                self.start_user_turn(&turn).await;
            }
            EngineCommand::CancelStream => {
                let (had_task, was_tool) = self.finish_active_task();
                if !had_task
                    && self.pending_truncated.is_none()
                    && self.pending_tools.is_empty()
                    && self.queued_interjections.is_empty()
                {
                    return;
                }

                info!("User interrupted active turn (was_tool: {was_tool})");
                let text = std::mem::take(&mut self.stream_buffer);
                let thought_text = std::mem::take(&mut self.thought_stream_buffer);
                let had_text = !text.trim().is_empty();
                let had_thought = !thought_text.trim().is_empty();
                if had_text || had_thought {
                    let final_content = if had_text {
                        format!("{text}\n\n*[Generation cancelled]*")
                    } else {
                        String::from("*[Generation cancelled]*")
                    };
                    self.messages.push(ChatMessage {
                        thoughts: had_thought.then(|| {
                            vec![ThoughtPart {
                                text: Some(thought_text),
                                thought_signature: None,
                            }]
                        }),
                        ..ChatMessage::new("assistant", final_content)
                    });
                }

                let closed = self.close_unresolved_tool_calls("Cancelled by user.");

                if self.flush_queued_interjections().await {
                    return;
                }

                if had_text || had_thought || closed > 0 {
                    self.save_session_bg(Vec::new()).await;
                }
                self.emit_chat();
            }
            EngineCommand::ResolveTool {
                approved,
                always_approve,
                feedback,
                turn,
            } => {
                self.update_turn_preserving_skill(turn);
                let (index, total) = self.tool_batch_progress();
                let Some(call) = self.pending_tools.pop_front() else {
                    return;
                };

                let sig = tools::call_signature(&call);
                if approved && always_approve {
                    info!("Adding '{}' to session auto-approve rules", sig);
                    self.session_auto_approve.insert(sig.clone());
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
                        internal_out
                            .push_str(&format!(" User provided this reason/message: {trimmed_fb}"));
                    }
                    self.messages.push(ChatMessage::new_tool(
                        call.function.name,
                        internal_out,
                        false,
                    ));
                    self.save_and_emit_chat(Vec::new()).await;
                    info!("Tool declination recorded to chat history.");
                    self.process_pending_tools().await;
                }
            }
            EngineCommand::ResolveTruncatedTool {
                action,
                guide_prompt,
                turn,
            } => {
                self.update_turn_preserving_skill(turn);
                let Some(pending) = self.pending_truncated.take() else {
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
                        self.finalize_pending_truncated(
                            pending.source,
                            pending.truncated_output,
                            true,
                        )
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
                                pending.summary,
                                pending.raw_bytes,
                                self.active_max_tool_output_size
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
                        self.pending_truncated = Some(pending);
                    }
                    TruncatedToolAction::CondenseFull => {
                        self.spawn_tool_condensation(&pending, true, guide_opt);
                        self.pending_truncated = Some(pending);
                    }
                }
            }
            EngineCommand::RefreshSessions => {
                info!("Refreshing sessions list");
                self.emit_sessions(None).await;
            }
            EngineCommand::LoadSession(session_id) => {
                info!("Loading session from database: {session_id}");
                self.flush_save_task().await;

                let sessions = Arc::clone(&self.sessions);
                let staging = Arc::clone(&self.attachments);
                let res = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                    let mut data = sessions.load(&session_id)?;
                    let mut all_image_names = Vec::new();

                    for msg in &mut data.messages {
                        msg.images = None;
                        if let Some(names) = &msg.image_names {
                            all_image_names.extend(names.iter().cloned());
                        }
                    }

                    let image_blobs = sessions.load_images(&data.id, &all_image_names);
                    let restored_paths = staging.restore_from_bytes(&image_blobs);
                    let attachments = staging.list_attachments();
                    Ok((data, restored_paths, attachments))
                })
                .await;

                match res {
                    Ok(Ok((data, restored_staging_paths, attachments))) => {
                        self.abort_active_state();
                        self.executed_skill_hooks.clear();
                        self.sent_images.extend(restored_staging_paths);
                        let _ = self.tx.send(EngineEvent::AttachmentsUpdated(attachments));

                        let raw_title = data.title.strip_prefix("󰘬 ").unwrap_or(&data.title);
                        let default_title = data
                            .messages
                            .iter()
                            .find(|m| m.role == "user" && !m.content.trim().is_empty())
                            .and_then(|m| m.content.lines().next())
                            .map_or_else(
                                || String::from("Untitled Session"),
                                |line| session::truncate_str(line, 50),
                            );
                        self.session_custom_title = (raw_title != default_title
                            && raw_title != "Untitled Session")
                            .then(|| raw_title.to_string());
                        self.session_id = data.id;
                        self.session_is_fork = data.title.starts_with("󰘬 ");
                        self.session_is_pinned = data.pinned;
                        self.turn.system_prompt = data.system_prompt;

                        let mut model_switch_err = None;
                        if !data.model.is_empty() && data.model != self.model {
                            let loaded_provider = Provider::from_model(&data.model);
                            match loaded_provider.build_client(&self.config) {
                                Ok(new_client) => {
                                    self.model = data.model.clone();
                                    self.provider = loaded_provider;
                                    self.client = new_client;
                                }
                                Err(e) => {
                                    let msg = format!(
                                        "Failed to build HTTP client for session model '{}': {e}",
                                        data.model
                                    );
                                    error!("{msg}");
                                    model_switch_err = Some(msg);
                                }
                            }
                        }

                        self.reset_skill_and_sync_model_limits();
                        let pending_calls = data
                            .messages
                            .last()
                            .and_then(|m| m.tool_calls.clone())
                            .filter(|c| !c.is_empty());
                        self.messages = data.messages;
                        self.emit_chat();
                        if let Some(err) = model_switch_err {
                            self.emit_error(err).await;
                        }

                        if let Some(calls) = pending_calls {
                            self.pending_tools = VecDeque::from(calls);
                            self.process_pending_tools().await;
                        }
                    }
                    Ok(Err(e)) => {
                        let msg = format!("Failed to load session: {e}");
                        error!("{msg}");
                        self.emit_error(msg).await;
                    }
                    Err(e) => {
                        let msg = format!("Session load task panicked: {e}");
                        error!("{msg}");
                        self.emit_error(msg).await;
                    }
                }
            }
            EngineCommand::ForkSession(msg_idx) => {
                let Some(target_msg) = self.messages.get(msg_idx) else {
                    self.emit_error("Cannot fork conversation: message index out of range.")
                        .await;
                    return;
                };
                if !target_msg.is_forkable() {
                    self.emit_error(
                        "Cannot fork conversation: forks can only be created from assistant responses.",
                    )
                    .await;
                    return;
                }

                info!(
                    "Forking session {} from message #{msg_idx}",
                    self.session_id
                );
                self.flush_save_task().await;
                self.abort_active_state();
                self.executed_skill_hooks.clear();

                self.messages.truncate(msg_idx + 1);
                if let Some(last) = self.messages.last_mut() {
                    last.tool_calls = None;
                }

                let old_session_id = self.session_id.clone();
                let mut new_session_id = SessionManager::new_id();
                if new_session_id == old_session_id {
                    new_session_id.push_str("_fork");
                }
                self.session_id = new_session_id;
                self.session_is_fork = true;
                self.session_is_pinned = false;

                let mut all_image_names = Vec::new();
                for msg in &self.messages {
                    if let Some(names) = &msg.image_names {
                        all_image_names.extend(names.iter().cloned());
                    }
                }

                let sessions = Arc::clone(&self.sessions);
                let staging = Arc::clone(&self.attachments);
                let (image_blobs, restored_paths, attachments) =
                    tokio::task::spawn_blocking(move || {
                        let blobs = sessions.load_images(&old_session_id, &all_image_names);
                        let paths = staging.restore_from_bytes(&blobs);
                        let list = staging.list_attachments();
                        (blobs, paths, list)
                    })
                    .await
                    .unwrap_or_default();

                self.sent_images.extend(restored_paths);
                let _ = self.tx.send(EngineEvent::AttachmentsUpdated(attachments));
                self.save_and_emit_chat(image_blobs).await;
            }
            EngineCommand::RenameSession(new_title) => {
                let trimmed = new_title
                    .trim()
                    .strip_prefix("󰘬 ")
                    .map_or_else(|| new_title.trim(), str::trim);
                if trimmed.is_empty() {
                    self.emit_error("Session title cannot be empty.").await;
                    return;
                }
                info!("Renaming session {} to '{trimmed}'", self.session_id);
                self.session_custom_title = Some(trimmed.to_string());
                self.save_session_bg(Vec::new()).await;
                self.emit_sessions(None).await;
            }
            EngineCommand::TogglePinSession(maybe_id) => {
                let target_id = maybe_id.unwrap_or_else(|| self.session_id.clone());
                if target_id == self.session_id {
                    self.session_is_pinned = !self.session_is_pinned;
                    info!(
                        "Toggled pin state for active session {}: {}",
                        self.session_id, self.session_is_pinned
                    );
                    self.save_session_bg(Vec::new()).await;
                    self.emit_chat();
                    self.emit_sessions(None).await;
                } else {
                    self.flush_save_task().await;
                    let sessions = Arc::clone(&self.sessions);
                    let list_res = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                        let new_state = sessions.toggle_pinned(&target_id)?;
                        info!("Toggled pin state for session {target_id}: {new_state}");
                        Ok(sessions.list())
                    })
                    .await;
                    match list_res {
                        Ok(Ok(list)) => {
                            let _ = self.tx.send(EngineEvent::SessionsListed(list));
                        }
                        Ok(Err(e)) => {
                            let msg = format!("Failed to toggle session pin: {e}");
                            error!("{msg}");
                            self.emit_error(msg).await;
                        }
                        Err(e) => {
                            let msg = format!("Session pin task panicked: {e}");
                            error!("{msg}");
                            self.emit_error(msg).await;
                        }
                    }
                }
            }
            EngineCommand::DeleteSession(session_id) => {
                info!("Deleting session: {session_id}");
                if session_id == self.session_id && self.session_is_pinned {
                    self.session_is_pinned = false;
                    self.emit_chat();
                }
                self.emit_sessions(Some(session_id)).await;
            }
            EngineCommand::ResetChat => {
                info!("Resetting chat session");
                self.flush_save_task().await;
                self.abort_active_state();
                self.clear_attachments().await;

                self.session_id = SessionManager::new_id();
                self.session_is_fork = false;
                self.session_is_pinned = false;
                self.session_custom_title = None;
                self.session_auto_approve.clear();
                self.executed_skill_hooks.clear();
                self.reset_skill_and_sync_model_limits();
                self.messages.clear();
                self.emit_chat();
            }
            EngineCommand::SwitchModel {
                model,
                num_ctx,
                max_tool_output_size,
            } => {
                if self.is_turn_active()
                    || self.pending_truncated.is_some()
                    || !self.pending_tools.is_empty()
                    || !self.queued_interjections.is_empty()
                {
                    self.emit_error(
                        "Cannot switch models while a turn or tool approval is active.",
                    )
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
            EngineCommand::SetTurnConfig(new_turn) => {
                self.update_turn_preserving_skill(new_turn);
                self.save_session_bg(Vec::new()).await;
                self.emit_chat();
            }
            EngineCommand::SetMemory { scope, memory } => {
                info!(
                    "Updating {:?} Memory (enabled: {}, items: {})",
                    scope,
                    memory.enabled,
                    memory.items.len()
                );
                let mem_to_save = memory.clone();
                let ws_dir = self.config.workspace_config_dir();
                let save_res = tokio::task::spawn_blocking(move || {
                    config::save_scoped_memory(scope, ws_dir.as_deref(), &mem_to_save)
                })
                .await;

                match save_res {
                    Ok(Ok(_)) => {
                        let (global_memory, workspace_memory) =
                            config::reload_memories(&self.config);
                        self.config.global_memory = global_memory.clone();
                        self.config.workspace_memory = workspace_memory.clone();
                        let _ = self.tx.send(EngineEvent::MemoryUpdated {
                            global_memory,
                            workspace_memory,
                        });
                    }
                    Ok(Err(e)) => {
                        error!("Failed to persist Memory: {e}");
                        self.config.set_scoped_memory(scope, memory);
                    }
                    Err(e) => {
                        error!("Memory save task panicked: {e}");
                        self.config.set_scoped_memory(scope, memory);
                    }
                }
            }
            EngineCommand::RunShellCommand {
                command: raw_cmd,
                include_in_context,
            } => {
                if self.is_turn_active() {
                    return;
                }

                let cmd_str = raw_cmd.trim().to_string();
                let Some(parts) = shlex::split(&cmd_str).filter(|p| !p.is_empty()) else {
                    return;
                };

                let prefix = if include_in_context { "!!" } else { "!" };
                info!("Executing manual shell command ({prefix}): {cmd_str}");
                self.executing_tool = true;
                let _ = self.tx.send(EngineEvent::ToolStarted {
                    summary: format!("{prefix} {cmd_str}"),
                    index: 1,
                    total: 1,
                });

                let cfg = self.config.clone();
                let max_out = self.active_max_tool_output_size;
                let full_cap = self.context_padded_capture_limit();
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                self.internal_rx = Some(rx);

                self.active_task = Some(tokio::spawn(async move {
                    let res =
                        tools::run_manual_command(&parts[0], &parts[1..], &cfg, max_out, full_cap)
                            .await;
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
            EngineCommand::InspectPayload {
                draft_prompt,
                mut turn,
            } => {
                let mut preview_msgs = self
                    .messages
                    .iter()
                    .map(ChatMessage::with_ui_image_placeholders)
                    .collect::<Vec<_>>();

                let trimmed_draft = draft_prompt.trim();
                let effective_draft = if !trimmed_draft.is_empty()
                    && !trimmed_draft.starts_with('/')
                    && !trimmed_draft.starts_with('!')
                {
                    let user_content = if let Some((cmd_token, args_str)) =
                        skills::parse_at_command(trimmed_draft)
                    {
                        self.refresh_skills();
                        if let Ok(skill_name) = skills::check_skill_activation(
                            cmd_token,
                            turn.enable_tools,
                            &self.config,
                        ) {
                            turn.active_skill = Some(skill_name);
                            turn.skill_args = (!args_str.is_empty()).then(|| args_str.to_string());
                        }
                        if args_str.is_empty() {
                            trimmed_draft.to_string()
                        } else {
                            args_str.to_string()
                        }
                    } else {
                        trimmed_draft.to_string()
                    };
                    preview_msgs.push(ChatMessage::new("user", user_content.clone()));
                    for summary in self.preview_due_skill_hooks(&turn) {
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
        }
    }

    async fn handle_internal_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::StreamThoughtChunk(ref chunk) => {
                self.thought_stream_buffer.push_str(chunk);
                let _ = self.tx.send(ev);
            }
            EngineEvent::StreamChunk(ref chunk) => {
                self.stream_buffer.push_str(chunk);
                let _ = self.tx.send(ev);
            }
            EngineEvent::StreamDone {
                prompt_tokens,
                eval_tokens,
                ref tool_calls,
                ref thoughts,
            } => {
                self.finish_active_task();
                let final_text = std::mem::take(&mut self.stream_buffer);
                self.thought_stream_buffer.clear();

                if !final_text.trim().is_empty() || !tool_calls.is_empty() || !thoughts.is_empty() {
                    self.messages.push(ChatMessage {
                        tool_calls: (!tool_calls.is_empty()).then(|| tool_calls.clone()),
                        thoughts: (!thoughts.is_empty()).then(|| thoughts.clone()),
                        ..ChatMessage::new("assistant", final_text)
                    });
                    self.save_and_emit_chat(Vec::new()).await;
                } else {
                    warn!(
                        "Stream finished with empty content and no tool calls (Prompt: {prompt_tokens}, Eval: {eval_tokens})"
                    );
                    self.messages.push(ChatMessage::new(
                        "error",
                        "Model returned an empty response (no content or tool calls).",
                    ));
                    self.save_and_emit_chat(Vec::new()).await;
                }

                let _ = self.tx.send(ev.clone());
                if !tool_calls.is_empty() {
                    self.pending_tools = VecDeque::from(tool_calls.clone());
                    self.process_pending_tools().await;
                } else {
                    self.flush_queued_interjections().await;
                }
            }
            EngineEvent::StreamError(ref err) => {
                self.finish_active_task();
                let partial = std::mem::take(&mut self.stream_buffer);
                let partial_thought = std::mem::take(&mut self.thought_stream_buffer);
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
                error!("API Error: {err}");
                self.messages.push(ChatMessage::new("error", err.clone()));
                self.save_session_bg(Vec::new()).await;
                let _ = self.tx.send(ev);
                self.flush_queued_interjections().await;
            }
            EngineEvent::ToolCompleted {
                output,
                full_output,
                raw_bytes,
                truncated,
            } => {
                self.finish_active_task();
                let tool_name = self
                    .active_tool_name
                    .take()
                    .or_else(|| self.unresolved_tool_names().into_iter().next())
                    .unwrap_or_else(|| "tool_execution".into());
                let summary = self
                    .active_tool_summary
                    .take()
                    .unwrap_or_else(|| tool_name.clone());

                if truncated && self.queued_interjections.is_empty() {
                    let (mut index, total) = self.tool_batch_progress();
                    index = index.saturating_sub(1).max(1);
                    let preview = output.clone();
                    let full_cap_bytes = self.context_padded_capture_limit();
                    self.pending_truncated = Some(PendingTruncatedTool {
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
            EngineEvent::ToolCondensed(res) => {
                self.finish_active_task();
                let Some(pending) = self.pending_truncated.take() else {
                    return;
                };
                self.active_tool_name = None;
                self.active_tool_summary = None;

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
                        self.finalize_pending_truncated(
                            pending.source,
                            pending.truncated_output,
                            true,
                        )
                        .await;
                    }
                }
            }
            EngineEvent::ShellCommandCompleted {
                command,
                output,
                full_output,
                raw_bytes,
                truncated,
                include_in_context,
            } => {
                self.finish_active_task();
                if truncated && include_in_context && self.queued_interjections.is_empty() {
                    let summary = format!("!! {command}");
                    let preview = output.clone();
                    let full_cap_bytes = self.context_padded_capture_limit();
                    self.pending_truncated = Some(PendingTruncatedTool {
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
            EngineEvent::HookCompleted {
                tool_name,
                summary,
                output,
                truncated,
            } => {
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
                info!("Skill hook '{summary}' completed (truncated: {truncated})");
                self.messages
                    .push(ChatMessage::new_hook(summary, output, truncated));
                self.save_and_emit_chat(Vec::new()).await;
            }
            EngineEvent::HooksFinished => {
                self.finish_active_task();
                let _ = self.tx.send(EngineEvent::ToolCompleted {
                    output: String::new(),
                    full_output: String::new(),
                    raw_bytes: 0,
                    truncated: false,
                });
                if self.flush_queued_interjections().await {
                    return;
                }
                let turn = self.turn.clone();
                self.spawn_model_turn(&turn).await;
            }
            other => {
                let _ = self.tx.send(other);
            }
        }
    }

    async fn save_and_emit_chat(&mut self, new_images: Vec<(String, Vec<u8>)>) {
        self.save_session_bg(new_images).await;
        self.emit_chat();
    }

    async fn save_session_bg(&mut self, new_images: Vec<(String, Vec<u8>)>) {
        self.flush_save_task().await;

        let sessions = Arc::clone(&self.sessions);
        let id = self.session_id.clone();
        let is_fork = self.session_is_fork;
        let is_pinned = self.session_is_pinned;
        let custom_title = self.session_custom_title.clone();
        let model = self.model.clone();
        let sys = self.turn.system_prompt.clone();
        let ctx = self.turn.num_ctx;
        let msgs = self.messages.clone();

        self.save_task = Some(tokio::task::spawn_blocking(move || {
            if let Err(e) = sessions.save(
                &id,
                custom_title.as_deref(),
                &model,
                &sys,
                ctx,
                &msgs,
                &new_images,
                is_fork,
                is_pinned,
            ) {
                error!("Failed to save session {id}: {e}");
            }
        }));
    }

    fn hook_execution_key(skill_name: &str, idx: usize, hook: &skills::SkillHook) -> String {
        format!(
            "{}::{idx}::{}::{}",
            skill_name.to_ascii_lowercase(),
            hook.tool,
            hook.args
        )
    }

    fn is_hook_already_in_session(&self, key: &str, summary: &str) -> bool {
        self.executed_skill_hooks.contains(key)
            || self
                .messages
                .iter()
                .any(|m| m.role == "hook" && m.tool_name.as_deref() == Some(summary))
    }

    fn preview_due_skill_hooks(&self, turn: &TurnConfig) -> Vec<String> {
        let Some(skill_name) = turn.active_skill.as_deref() else {
            return Vec::new();
        };
        let Some(skill) = skills::find_skill(&self.config, skill_name) else {
            return Vec::new();
        };
        let mut summaries = Vec::new();
        for (idx, hook) in skill.hooks.iter().enumerate() {
            let summary = skills::format_hook_summary(hook);
            let key = Self::hook_execution_key(&skill.name, idx, hook);
            let due = match hook.trigger {
                skills::HookTrigger::OnActivate => !self.is_hook_already_in_session(&key, &summary),
                skills::HookTrigger::EveryTurn => true,
            };
            if due {
                summaries.push(summary);
            }
        }
        summaries
    }

    fn collect_due_skill_hooks(&mut self, turn: &TurnConfig) -> Vec<(String, ToolCall)> {
        let Some(skill_name) = turn.active_skill.as_deref() else {
            return Vec::new();
        };
        let Some(skill) = skills::find_skill(&self.config, skill_name) else {
            return Vec::new();
        };
        let mut due_hooks = Vec::new();
        for (idx, hook) in skill.hooks.iter().enumerate() {
            let summary = skills::format_hook_summary(hook);
            let key = Self::hook_execution_key(&skill.name, idx, hook);
            let due = match hook.trigger {
                skills::HookTrigger::OnActivate => {
                    let already = self.is_hook_already_in_session(&key, &summary);
                    self.executed_skill_hooks.insert(key);
                    !already
                }
                skills::HookTrigger::EveryTurn => true,
            };
            if due {
                due_hooks.push((
                    summary,
                    ToolCall {
                        function: tools::ToolFunction {
                            name: hook.tool.clone(),
                            arguments: hook.args.clone(),
                        },
                        thought_signature: None,
                    },
                ));
            }
        }
        due_hooks
    }

    async fn start_user_turn(&mut self, turn: &TurnConfig) {
        let hooks = self.collect_due_skill_hooks(turn);
        if hooks.is_empty() {
            self.spawn_model_turn(turn).await;
            return;
        }

        let total = hooks.len();
        info!("Running {total} pre-turn skill hook(s) before model stream");
        self.executing_tool = true;
        let cfg = self.config.clone();
        let active_skill = turn.active_skill.clone();
        let max_out = self.active_max_tool_output_size;
        let full_cap = self.context_padded_capture_limit();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.internal_rx = Some(rx);

        self.active_task = Some(tokio::spawn(async move {
            for (idx, (summary, call)) in hooks.into_iter().enumerate() {
                let _ = tx.send(EngineEvent::ToolStarted {
                    summary: format!("hook: {summary}"),
                    index: idx + 1,
                    total,
                });
                let res =
                    tools::execute_hook(&call, &cfg, active_skill.as_deref(), max_out, full_cap)
                        .await;
                let _ = tx.send(EngineEvent::HookCompleted {
                    tool_name: call.function.name,
                    summary,
                    output: res.truncated_output,
                    truncated: res.truncated,
                });
            }
            let _ = tx.send(EngineEvent::HooksFinished);
        }));
    }

    async fn spawn_model_turn(&mut self, turn: &TurnConfig) {
        self.stream_buffer.clear();
        self.thought_stream_buffer.clear();
        self.flush_save_task().await;

        let sessions = Arc::clone(&self.sessions);
        let session_id = self.session_id.clone();

        // Clone lightweight messages (without Base64 payloads) and hydrate Base64
        // strings directly from the SQLite database into the transient `hydrated_msgs`.
        let mut msgs = self.messages.clone();
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
        .unwrap_or_else(|_| self.messages.clone());

        let _ = self.tx.send(EngineEvent::StreamStarted);
        let provider = self.provider;
        let client = self.client.clone();
        let model = self.model.clone();
        let config = self.config.clone();
        let turn_cfg = turn.clone();
        let (internal_tx, internal_rx) = tokio::sync::mpsc::unbounded_channel();
        self.internal_rx = Some(internal_rx);

        self.active_task = Some(tokio::spawn(async move {
            provider
                .stream_turn(client, model, config, hydrated_msgs, turn_cfg, internal_tx)
                .await;
        }));
    }

    fn queued_ui_messages(&self) -> Vec<ChatMessage> {
        self.queued_interjections
            .iter()
            .map(|q| q.msg.with_ui_image_placeholders())
            .collect()
    }

    fn emit_queued_interjections(&self) {
        let _ = self.tx.send(EngineEvent::QueuedInterjectionsUpdated(
            self.queued_ui_messages(),
        ));
    }

    fn emit_chat(&self) {
        let messages = self
            .messages
            .iter()
            .map(ChatMessage::with_ui_image_placeholders)
            .collect();
        let _ = self.tx.send(EngineEvent::ChatUpdated {
            session_id: self.session_id.clone(),
            is_fork: self.session_is_fork,
            is_pinned: self.session_is_pinned,
            messages,
            queued_messages: self.queued_ui_messages(),
            num_ctx: self.turn.num_ctx,
            system_prompt: self.turn.system_prompt.clone(),
        });
    }
}
