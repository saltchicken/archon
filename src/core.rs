pub mod attachments;
pub mod config;
pub mod gemini;
pub mod logger;
pub mod ollama;
pub mod provider;
pub mod session;

use crate::tools::{self, ToolCall};
use attachments::{ImageAttachment, ImageStaging};
use config::Config;
use provider::Provider;
use reqwest::Client;
use session::{ChatMessage, SessionManager, SessionSummary, ThoughtPart};
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
    LoadSession(String),
    DeleteSession(String),
    RefreshSessions,
    ResetChat,
    SwitchModel {
        model: String,
        num_ctx: u32,
        max_tool_output_size: usize,
    },
    SetContextWindow(u32),
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Notification {
        message: String,
        is_error: bool,
    },
    AttachmentsUpdated(Vec<ImageAttachment>),
    ChatUpdated {
        messages: Vec<ChatMessage>,
        queued_messages: Vec<ChatMessage>,
        num_ctx: u32,
        system_prompt: String,
    },
    QueuedInterjectionsUpdated(Vec<ChatMessage>),
    StreamStarted,
    StreamChunk(String),
    StreamDone {
        prompt_tokens: u64,
        eval_tokens: u64,
        done_reason: Option<String>,
        tool_calls: Vec<ToolCall>,
        thoughts: Vec<ThoughtPart>,
    },
    StreamError(String),
    ToolStarted {
        summary: String,
        index: usize,
        total: usize,
    },
    ToolCompleted {
        output: String,
        truncated: bool,
    },
    ToolApprovalRequested {
        formatted_preview: String,
        index: usize,
        total: usize,
    },
    SessionsListed(Vec<SessionSummary>),
    ModelConfigUpdated {
        model: String,
        num_ctx: u32,
        max_tool_output_size: usize,
    },
}

struct QueuedInterjection {
    msg: ChatMessage,
    new_images: Vec<(String, Vec<u8>)>,
    turn: TurnConfig,
}

pub struct CoreEngine {
    model: String,
    provider: Provider,
    config: Config,
    client: Client,
    attachments: Arc<ImageStaging>,
    sessions: Arc<SessionManager>,
    session_id: String,
    turn: TurnConfig,
    active_max_tool_output_size: usize,
    session_auto_approve: HashSet<String>,
    messages: Vec<ChatMessage>,
    sent_images: HashSet<PathBuf>,
    stream_buffer: String,
    active_task: Option<JoinHandle<()>>,
    internal_rx: Option<UnboundedReceiver<EngineEvent>>,
    executing_tool: bool,
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
        let sessions = Arc::new(SessionManager::new(&config.app.history_dir)?);
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
            turn: TurnConfig {
                system_prompt: String::new(),
                num_ctx,
                enable_tools: true,
            },
            active_max_tool_output_size,
            session_auto_approve: HashSet::new(),
            messages: Vec::new(),
            sent_images: HashSet::new(),
            stream_buffer: String::new(),
            active_task: None,
            internal_rx: None,
            executing_tool: false,
            queued_interjections: Vec::new(),
            save_task: None,
            pending_tools: VecDeque::new(),
            tx,
        })
    }

    fn abort_active_task(&mut self) -> bool {
        self.internal_rx = None;
        if let Some(h) = self.active_task.take() {
            h.abort();
            true
        } else {
            false
        }
    }

    fn abort_active_state(&mut self) {
        self.abort_active_task();
        self.executing_tool = false;
        self.queued_interjections.clear();
        self.pending_tools.clear();
        self.sent_images.clear();
        self.stream_buffer.clear();
    }

    fn unresolved_tool_call_count(&self) -> usize {
        let mut resolved = 0usize;
        for m in self.messages.iter().rev() {
            match m.role.as_str() {
                "tool" => resolved += 1,
                "assistant" => {
                    let total = m.tool_calls.as_ref().map_or(0, Vec::len);
                    return total.saturating_sub(resolved);
                }
                _ => break,
            }
        }
        0
    }

    fn close_unresolved_tool_calls(&mut self, message: &str) -> usize {
        let unresolved = self.unresolved_tool_call_count();
        self.pending_tools.clear();
        for _ in 0..unresolved {
            self.messages.push(ChatMessage::new("tool", message));
        }
        unresolved
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

    fn emit_model_config(&self) {
        let _ = self.tx.send(EngineEvent::ModelConfigUpdated {
            model: self.model.clone(),
            num_ctx: self.turn.num_ctx,
            max_tool_output_size: self.active_max_tool_output_size,
        });
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
        let mut new_images = Vec::new();
        for item in self.queued_interjections.drain(..) {
            self.turn = item.turn;
            self.messages.push(item.msg);
            new_images.extend(item.new_images);
        }
        self.save_and_emit_chat(new_images).await;
        let turn = self.turn.clone();
        self.spawn_model_turn(&turn).await;
        true
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
        let _ = self.tx.send(EngineEvent::ToolStarted {
            summary: sig,
            index,
            total,
        });

        let cfg = self.config.clone();
        let max_out = self.active_max_tool_output_size;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.internal_rx = Some(rx);

        self.active_task = Some(tokio::spawn(async move {
            let (mut output, truncated) = tools::execute(&call, &cfg, max_out).await;
            if let Some(fb) = feedback.filter(|s| !s.is_empty()) {
                output.push_str(&format!("\n\n[User attached a message]: {fb}"));
            }
            let _ = tx.send(EngineEvent::ToolCompleted { output, truncated });
        }));
    }

    async fn process_pending_tools(&mut self) {
        if self.flush_queued_interjections().await {
            return;
        }

        let mut modified = false;

        while let Some(call) = self.pending_tools.front() {
            if !tools::is_tool_enabled(&call.function.name, &self.config) {
                let call = self.pending_tools.pop_front().unwrap();
                warn!(
                    "Rejecting disabled tool call '{}' without prompting",
                    call.function.name
                );
                self.messages.push(ChatMessage::new(
                    "tool",
                    format!("Error: Unknown tool: '{}'", call.function.name),
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

    async fn update_attachments<F>(&self, op: F, success_msg: impl Into<String>)
    where
        F: FnOnce(&ImageStaging) -> anyhow::Result<PathBuf> + Send + 'static,
    {
        let staging = Arc::clone(&self.attachments);
        if let Ok((res, list)) =
            tokio::task::spawn_blocking(move || (op(&staging), staging.list_attachments())).await
        {
            match res {
                Ok(_) => self.notify(success_msg, false),
                Err(e) => self.notify(e.to_string(), true),
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

    async fn emit_sessions(&self, delete_first: Option<String>) {
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
                        self.active_task = None;
                        self.internal_rx = None;
                        self.executing_tool = false;
                    }
                },
            }
        }

        self.abort_active_task();

        info!("Shutting down CoreEngine, awaiting pending session save...");
        self.flush_save_task().await;
        info!("All background tasks completely flushed.");
    }

    async fn handle_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::PasteClipboard => {
                info!("Pasting image from clipboard");
                self.update_attachments(|s| s.paste_clipboard(), "Image pasted from clipboard.")
                    .await;
            }
            EngineCommand::ImportFile(path) => {
                info!("Importing file: {}", path.display());
                let msg = format!("Added {}", path.display());
                self.update_attachments(move |s| s.import_file(&path), msg)
                    .await;
            }
            EngineCommand::ClearImages => {
                info!("Clearing attached images");
                self.clear_attachments().await;
                self.notify("Cleared attached images.", false);
            }
            EngineCommand::SendPrompt { prompt, turn } => {
                info!("User submitted prompt. Enable tools: {}", turn.enable_tools);
                debug!("Prompt content: {}", prompt.replace('\n', " "));

                let (image_names, new_image_blobs) = self.collect_unsent_images().await;
                let user_msg = ChatMessage {
                    images: None,
                    image_names,
                    ..ChatMessage::new("user", prompt)
                };

                // If a model stream or a tool execution is currently in flight, queue the
                // interjection so the active process finishes cleanly before reading it.
                if self.active_task.is_some() || self.executing_tool {
                    info!(
                        "Queuing user interjection while process is active (executing_tool: {})",
                        self.executing_tool
                    );
                    self.queued_interjections.push(QueuedInterjection {
                        msg: user_msg,
                        new_images: new_image_blobs,
                        turn,
                    });
                    let queued_total = self.queued_interjections.len();
                    self.emit_queued_interjections();
                    self.notify(
                        format!(
                            "Interjection queued ({queued_total}). Will send as soon as current step finishes (Esc to send now)."
                        ),
                        false,
                    );
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
                self.spawn_model_turn(&turn).await;
            }
            EngineCommand::CancelStream => {
                let was_tool = self.executing_tool;
                let had_task = self.abort_active_task();
                self.executing_tool = false;

                if !had_task
                    && self.pending_tools.is_empty()
                    && self.queued_interjections.is_empty()
                {
                    return;
                }

                info!("User interrupted active turn (was_tool: {was_tool})");
                let mut changed = false;

                let text = std::mem::take(&mut self.stream_buffer);
                if !text.trim().is_empty() {
                    self.messages.push(ChatMessage::new(
                        "assistant",
                        format!("{text}\n\n*[Generation cancelled]*"),
                    ));
                    changed = true;
                }

                let closed = self.close_unresolved_tool_calls("Cancelled by user.");
                if closed > 0 {
                    changed = true;
                }

                let had_interjections = !self.queued_interjections.is_empty();
                let mut extra_images = Vec::new();
                for item in self.queued_interjections.drain(..) {
                    self.turn = item.turn;
                    self.messages.push(item.msg);
                    extra_images.extend(item.new_images);
                    changed = true;
                }

                if had_interjections {
                    self.save_and_emit_chat(extra_images).await;
                    let turn = self.turn.clone();
                    self.spawn_model_turn(&turn).await;
                    self.notify(
                        "Interrupted active step — sending queued interjection...",
                        false,
                    );
                    return;
                }

                if changed {
                    self.save_session_bg(extra_images).await;
                }
                self.emit_chat();
                self.notify(
                    if was_tool || closed > 0 {
                        "Tool execution cancelled."
                    } else {
                        "Stream cancelled."
                    },
                    false,
                );
            }
            EngineCommand::ResolveTool {
                approved,
                always_approve,
                feedback,
                turn,
            } => {
                self.turn = turn;
                let (index, total) = self.tool_batch_progress();
                let Some(call) = self.pending_tools.pop_front() else {
                    return;
                };

                let sig = tools::call_signature(&call);
                if approved && always_approve {
                    info!("Adding '{}' to session auto-approve rules", sig);
                    self.session_auto_approve.insert(sig.clone());
                    self.notify(
                        format!("Auto-approving '{sig}' for the rest of this session."),
                        false,
                    );
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
                    self.messages.push(ChatMessage {
                        truncated: false,
                        ..ChatMessage::new("tool", internal_out)
                    });
                    self.save_and_emit_chat(Vec::new()).await;
                    info!("Tool declination recorded to chat history.");
                    self.process_pending_tools().await;
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
                        self.sent_images.extend(restored_staging_paths);
                        let _ = self.tx.send(EngineEvent::AttachmentsUpdated(attachments));

                        self.session_id = data.id;
                        self.turn.system_prompt = data.system_prompt;

                        if !data.model.is_empty() && data.model != self.model {
                            let loaded_provider = Provider::from_model(&data.model);
                            if let Ok(new_client) = loaded_provider.build_client(&self.config) {
                                self.model = data.model.clone();
                                self.provider = loaded_provider;
                                self.client = new_client;
                            }
                        }

                        self.turn.num_ctx = if data.num_ctx > 0 {
                            data.num_ctx
                        } else {
                            self.config.context_for_model(&self.model)
                        };
                        self.active_max_tool_output_size =
                            self.config.max_tool_output_for_model(&self.model);

                        self.emit_model_config();
                        self.messages = data.messages;
                        self.emit_chat();
                        self.notify(format!("Loaded session: {}", data.title), false);

                        if let Some(calls) = self
                            .messages
                            .last()
                            .and_then(|m| m.tool_calls.clone())
                            .filter(|c| !c.is_empty())
                        {
                            self.pending_tools = VecDeque::from(calls);
                            self.process_pending_tools().await;
                        }
                    }
                    Ok(Err(e)) => {
                        error!("Failed to load session: {e}");
                        self.notify(format!("Failed to load session: {e}"), true);
                    }
                    Err(e) => {
                        error!("Session load task panicked: {e}");
                        self.notify(format!("Session load failed: {e}"), true);
                    }
                }
            }
            EngineCommand::DeleteSession(session_id) => {
                info!("Deleting session: {session_id}");
                self.emit_sessions(Some(session_id)).await;
            }
            EngineCommand::ResetChat => {
                info!("Resetting chat session");
                self.flush_save_task().await;
                self.abort_active_state();
                self.clear_attachments().await;

                self.session_id = SessionManager::new_id();
                self.session_auto_approve.clear();
                self.turn.num_ctx = self.config.context_for_model(&self.model);
                self.active_max_tool_output_size =
                    self.config.max_tool_output_for_model(&self.model);
                self.emit_model_config();
                self.messages.clear();
                self.emit_chat();
                self.notify("Started new chat session.", false);
            }
            EngineCommand::SwitchModel {
                model,
                num_ctx,
                max_tool_output_size,
            } => {
                if self.active_task.is_some()
                    || self.executing_tool
                    || !self.pending_tools.is_empty()
                    || !self.queued_interjections.is_empty()
                {
                    self.notify(
                        "Cannot switch model while a turn or tool approval is active. Cancel first.",
                        true,
                    );
                    return;
                }

                let new_provider = Provider::from_model(&model);
                info!(
                    "Hot-swapping to model '{}' ({:?}, ctx: {}, tool_max: {})",
                    model, new_provider, num_ctx, max_tool_output_size
                );

                if let Err(e) = new_provider.run_preflight(&model, &self.config).await {
                    error!("Hot-swap preflight failed for '{}': {}", model, e);
                    self.notify(format!("Model switch failed: {e}"), true);
                    return;
                }

                let Ok(new_client) = new_provider.build_client(&self.config) else {
                    self.notify("Failed to build HTTP client for new model.", true);
                    return;
                };

                self.model = model;
                self.provider = new_provider;
                self.client = new_client;
                self.turn.num_ctx = num_ctx.max(512);
                self.active_max_tool_output_size = max_tool_output_size.max(1024);

                self.save_session_bg(Vec::new()).await;
                self.emit_model_config();
                self.notify(
                    format!(
                        "Switched to {} (Ctx: {}, Tool Max: {}KB)",
                        self.model,
                        self.turn.num_ctx,
                        self.active_max_tool_output_size / 1024
                    ),
                    false,
                );
            }
            EngineCommand::SetContextWindow(new_ctx) => {
                let clamped = new_ctx.max(512);
                info!("Updating context window to {}", clamped);
                self.turn.num_ctx = clamped;
                self.save_session_bg(Vec::new()).await;
                self.emit_model_config();
                self.notify(format!("Context window set to {} tokens.", clamped), false);
            }
        }
    }

    async fn handle_internal_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::StreamChunk(ref chunk) => {
                self.stream_buffer.push_str(chunk);
                let _ = self.tx.send(ev);
            }
            EngineEvent::StreamDone {
                prompt_tokens,
                eval_tokens,
                ref done_reason,
                ref tool_calls,
                ref thoughts,
            } => {
                self.active_task = None;
                self.internal_rx = None;
                let final_text = std::mem::take(&mut self.stream_buffer);
                let total_used = prompt_tokens + eval_tokens;
                let ctx_limit = self.turn.num_ctx as u64;
                let usage_pct = (total_used as f64 / ctx_limit.max(1) as f64) * 100.0;
                let hit_length = done_reason.as_deref() == Some("length");
                let is_near_limit = hit_length
                    || usage_pct >= 90.0
                    || prompt_tokens >= ctx_limit.saturating_sub(256);

                if !final_text.trim().is_empty() || !tool_calls.is_empty() || !thoughts.is_empty() {
                    self.messages.push(ChatMessage {
                        tool_calls: (!tool_calls.is_empty()).then(|| tool_calls.clone()),
                        thoughts: (!thoughts.is_empty()).then(|| thoughts.clone()),
                        ..ChatMessage::new("assistant", final_text)
                    });
                    self.save_and_emit_chat(Vec::new()).await;

                    let prefix = match (hit_length, is_near_limit) {
                        (true, _) => "⚠ CTX MAXED OUT (Truncated) — ",
                        (false, true) => "⚠ CTX LIMIT NEAR — ",
                        _ => "",
                    };

                    self.notify(
                        format!(
                            "{prefix}Context: {total_used}/{ctx_limit} ({usage_pct:.1}%) [Prompt: {prompt_tokens} | Eval: {eval_tokens}]"
                        ),
                        is_near_limit,
                    );
                } else {
                    warn!(
                        "Stream finished with empty content and no tool calls (Prompt: {prompt_tokens}, Eval: {eval_tokens})"
                    );
                    self.emit_chat();
                    self.notify(
                        format!(
                            "Model returned an empty response (Context: {total_used}/{ctx_limit} | Prompt: {prompt_tokens} | Eval: {eval_tokens})"
                        ),
                        true,
                    );
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
                self.active_task = None;
                self.internal_rx = None;
                self.executing_tool = false;
                self.stream_buffer.clear();
                self.notify(format!("API Error: {err}"), true);
                let _ = self.tx.send(ev);
                self.flush_queued_interjections().await;
            }
            EngineEvent::ToolCompleted { output, truncated } => {
                self.active_task = None;
                self.internal_rx = None;
                self.executing_tool = false;
                self.messages.push(ChatMessage {
                    truncated,
                    ..ChatMessage::new("tool", output)
                });
                self.save_and_emit_chat(Vec::new()).await;
                info!("Tool execution completed and recorded to chat history.");
                self.process_pending_tools().await;
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
        let model = self.model.clone();
        let sys = self.turn.system_prompt.clone();
        let ctx = self.turn.num_ctx;
        let msgs = self.messages.clone();

        self.save_task = Some(tokio::task::spawn_blocking(move || {
            if let Err(e) = sessions.save(&id, &model, &sys, ctx, &msgs, &new_images) {
                error!("Failed to save session {id}: {e}");
            }
        }));
    }

    async fn spawn_model_turn(&mut self, turn: &TurnConfig) {
        self.stream_buffer.clear();
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
            messages,
            queued_messages: self.queued_ui_messages(),
            num_ctx: self.turn.num_ctx,
            system_prompt: self.turn.system_prompt.clone(),
        });
    }

    fn notify(&self, msg: impl Into<String>, is_error: bool) {
        let _ = self.tx.send(EngineEvent::Notification {
            message: msg.into(),
            is_error,
        });
    }
}
