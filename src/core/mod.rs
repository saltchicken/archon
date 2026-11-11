pub mod config;
pub mod logger;
pub mod ollama;
pub mod session;
pub mod tools;
pub mod workspace;

use config::Config;
use reqwest::Client;
use session::{ChatMessage, SessionManager, SessionSummary};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tools::ToolCall;
use tracing::{debug, error, info, warn};
use workspace::{ImageAttachment, ImageWorkspace};

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
        feedback: String,
        turn: TurnConfig,
    },
    LoadSession(PathBuf),
    DeleteSession(PathBuf),
    RefreshSessions,
    ResetChat,
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Notification {
        message: String,
        is_error: bool,
    },
    WorkspaceUpdated(Vec<ImageAttachment>),
    ChatUpdated {
        messages: Vec<ChatMessage>,
        num_ctx: u32,
        system_prompt: String,
    },
    StreamStarted,
    StreamChunk(String),
    StreamDone {
        prompt_tokens: u64,
        eval_tokens: u64,
        done_reason: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    StreamError(String),
    ToolApprovalRequested {
        formatted_preview: String,
        index: usize,
        total: usize,
    },
    SessionsListed(Vec<SessionSummary>),
}

pub struct CoreEngine {
    model: String,
    config: Config,
    client: Client,
    workspace: Arc<ImageWorkspace>,
    sessions: Arc<SessionManager>,
    session_id: String,
    active_system_prompt: String,
    active_num_ctx: u32,
    messages: Vec<ChatMessage>,
    sent_images: HashSet<PathBuf>,
    stream_buffer: String,
    active_task: Option<JoinHandle<()>>,
    save_task: Option<JoinHandle<()>>,
    pending_tools: VecDeque<ToolCall>,
    tx: UnboundedSender<EngineEvent>,
}

fn resolve_session_image_path(p: &Path, asset_dir: &Path) -> PathBuf {
    let candidates = [
        Some(p.to_path_buf()),
        Some(config::expand_tilde(&p.to_string_lossy())),
        p.file_name().map(|f| asset_dir.join(f)),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|c| c.exists())
        .unwrap_or_else(|| p.to_path_buf())
}

impl CoreEngine {
    pub fn new(
        model: String,
        config: Config,
        tx: UnboundedSender<EngineEvent>,
    ) -> anyhow::Result<Self> {
        let workspace = Arc::new(ImageWorkspace::new()?);
        let sessions = Arc::new(SessionManager::new(&config.app.history_dir)?);
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(config.ollama.status_timeout.max(5)))
            .read_timeout(Duration::from_secs(config.ollama.chat_timeout))
            .build()?;
        let active_num_ctx = config.app.default_context;

        Ok(Self {
            model,
            config,
            client,
            workspace,
            sessions,
            session_id: SessionManager::new_id(),
            active_system_prompt: String::new(),
            active_num_ctx,
            messages: Vec::new(),
            sent_images: HashSet::new(),
            stream_buffer: String::new(),
            active_task: None,
            save_task: None,
            pending_tools: VecDeque::new(),
            tx,
        })
    }

    fn abort_active_state(&mut self) {
        if let Some(h) = self.active_task.take() {
            h.abort();
        }
        self.pending_tools.clear();
        self.sent_images.clear();
        self.stream_buffer.clear();
    }

    async fn flush_save_task(&mut self) {
        if let Some(save_handle) = self.save_task.take() {
            let _ = save_handle.await;
        }
    }

    async fn clear_workspace(&self) {
        let ws = Arc::clone(&self.workspace);
        let _ = tokio::task::spawn_blocking(move || ws.clear()).await;
        let _ = self.tx.send(EngineEvent::WorkspaceUpdated(vec![]));
    }

    fn apply_turn_config(&mut self, turn: &TurnConfig) {
        self.active_system_prompt = turn.system_prompt.clone();
        self.active_num_ctx = turn.num_ctx;
    }

    fn prompt_next_tool(&mut self) {
        let Some(call) = self.pending_tools.front() else {
            return;
        };
        let total = self
            .messages
            .iter()
            .rev()
            .find_map(|m| m.tool_calls.as_ref().map(Vec::len))
            .unwrap_or(self.pending_tools.len())
            .max(self.pending_tools.len());
        let index = total - self.pending_tools.len() + 1;
        let _ = self.tx.send(EngineEvent::ToolApprovalRequested {
            formatted_preview: tools::format_approval(call),
            index,
            total,
        });
    }

    fn enqueue_tool_calls(&mut self, calls: Vec<ToolCall>) {
        self.pending_tools = VecDeque::from(calls);
        self.prompt_next_tool();
    }

    async fn update_workspace<F>(&self, op: F, success_msg: impl Into<String>)
    where
        F: FnOnce(&ImageWorkspace) -> anyhow::Result<PathBuf> + Send + 'static,
    {
        let ws = Arc::clone(&self.workspace);
        if let Ok((res, list)) =
            tokio::task::spawn_blocking(move || (op(&ws), ws.list_attachments())).await
        {
            match res {
                Ok(_) => self.notify(success_msg, false),
                Err(e) => self.notify(e.to_string(), true),
            }
            let _ = self.tx.send(EngineEvent::WorkspaceUpdated(list));
        }
    }

    async fn collect_unsent_images(&mut self) -> (Option<Vec<String>>, Option<Vec<PathBuf>>) {
        let asset_dir = self.sessions.asset_dir(&self.session_id);
        let ws = Arc::clone(&self.workspace);
        let already_sent = self.sent_images.clone();

        let (imgs, paths, newly_sent) = tokio::task::spawn_blocking(move || {
            let _ = std::fs::create_dir_all(&asset_dir);
            let mut imgs = Vec::new();
            let mut paths = Vec::new();
            let mut sent = Vec::new();

            for att in ws.list_attachments() {
                if already_sent.contains(&att.path) {
                    continue;
                }
                let dest = asset_dir.join(att.path.file_name().unwrap_or_default());
                let _ = std::fs::copy(&att.path, &dest);
                if let Ok(b64) = ImageWorkspace::encode_image(&att.path) {
                    imgs.push(b64);
                    paths.push(dest);
                    sent.push(att.path);
                }
            }
            (imgs, paths, sent)
        })
        .await
        .unwrap_or_default();

        self.sent_images.extend(newly_sent);
        (
            (!imgs.is_empty()).then_some(imgs),
            (!paths.is_empty()).then_some(paths),
        )
    }

    async fn emit_sessions(&self, delete_first: Option<PathBuf>) {
        let sessions = Arc::clone(&self.sessions);
        if let Ok(list) = tokio::task::spawn_blocking(move || {
            if let Some(path) = delete_first {
                let _ = sessions.delete(&path);
            }
            sessions.list()
        })
        .await
        {
            let _ = self.tx.send(EngineEvent::SessionsListed(list));
        }
    }

    pub async fn run(mut self, mut cmd_rx: UnboundedReceiver<EngineCommand>) {
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                    Some(cmd) => self.handle_command(cmd, &internal_tx).await,
                    None => break,
                },
                Some(ev) = internal_rx.recv() => self.handle_internal_event(ev).await,
            }
        }

        if let Some(h) = self.active_task.take() {
            h.abort();
        }

        info!("Shutting down CoreEngine, awaiting pending session save...");
        self.flush_save_task().await;
        info!("All background tasks completely flushed.");
    }

    async fn handle_command(
        &mut self,
        cmd: EngineCommand,
        internal_tx: &UnboundedSender<EngineEvent>,
    ) {
        match cmd {
            EngineCommand::PasteClipboard => {
                info!("Pasting image from clipboard");
                self.update_workspace(|ws| ws.paste_clipboard(), "Image pasted from clipboard.")
                    .await;
            }
            EngineCommand::ImportFile(path) => {
                info!("Importing file: {}", path.display());
                let msg = format!("Added {}", path.display());
                self.update_workspace(move |ws| ws.import_file(&path), msg)
                    .await;
            }
            EngineCommand::ClearImages => {
                info!("Clearing attached images");
                self.clear_workspace().await;
                self.notify("Cleared attached images.", false);
            }
            EngineCommand::SendPrompt { prompt, turn } => {
                info!("User submitted prompt. Enable tools: {}", turn.enable_tools);
                debug!("Prompt content: {}", prompt.replace('\n', " "));

                if let Some(h) = self.active_task.take() {
                    h.abort();
                }

                for call in self.pending_tools.drain(..) {
                    warn!(
                        "Auto-declining unresolved tool call '{}' before new prompt",
                        call.function.name
                    );
                    self.messages.push(ChatMessage::new(
                        "tool",
                        "The user skipped this tool call and sent a new message.",
                    ));
                }

                self.apply_turn_config(&turn);
                let (images, image_paths) = self.collect_unsent_images().await;

                self.messages.push(ChatMessage {
                    images,
                    image_paths,
                    ..ChatMessage::new("user", prompt)
                });

                self.save_and_emit_chat().await;
                self.spawn_ollama_turn(&turn, internal_tx.clone()).await;
            }
            EngineCommand::CancelStream => {
                info!("User cancelled stream");
                let Some(handle) = self.active_task.take() else {
                    return;
                };
                handle.abort();
                let text = std::mem::take(&mut self.stream_buffer);
                if !text.trim().is_empty() {
                    self.messages.push(ChatMessage::new(
                        "assistant",
                        format!("{text}\n\n*[Generation cancelled]*"),
                    ));
                    self.save_session_bg().await;
                }
                self.emit_chat();
                self.notify("Stream cancelled.", false);
            }
            EngineCommand::ResolveTool {
                approved,
                feedback,
                turn,
            } => {
                self.apply_turn_config(&turn);
                let Some(call) = self.pending_tools.pop_front() else {
                    return;
                };

                info!(
                    "Resolving tool call: {}. Approved: {}",
                    call.function.name, approved
                );

                let (mut internal_out, truncated) = if approved {
                    tools::execute(
                        &call,
                        &self.config.app.allowed_commands,
                        self.config.app.max_tool_output_size,
                    )
                    .await
                } else {
                    ("The user declined to run this command.".into(), false)
                };

                let trimmed_fb = feedback.trim();
                if !trimmed_fb.is_empty() {
                    let prefix = if approved {
                        "\n\n[User attached a message]:"
                    } else {
                        " User provided this reason/message:"
                    };
                    internal_out.push_str(&format!("{prefix} {trimmed_fb}"));
                }

                self.messages.push(ChatMessage {
                    truncated,
                    ..ChatMessage::new("tool", internal_out)
                });
                self.save_and_emit_chat().await;
                info!("Tool execution resolved and recorded to chat history.");

                if self.pending_tools.is_empty() {
                    self.spawn_ollama_turn(&turn, internal_tx.clone()).await;
                } else {
                    self.prompt_next_tool();
                }
            }
            EngineCommand::RefreshSessions => {
                info!("Refreshing sessions list");
                self.emit_sessions(None).await;
            }
            EngineCommand::LoadSession(path) => {
                info!("Loading session from path: {}", path.display());
                self.flush_save_task().await;

                let sessions = Arc::clone(&self.sessions);
                let res = tokio::task::spawn_blocking(move || sessions.load(&path)).await;

                match res {
                    Ok(Ok(mut data)) => {
                        self.abort_active_state();
                        let asset_dir = self.sessions.asset_dir(&data.id);

                        let mut session_img_paths = Vec::new();
                        for p in data
                            .messages
                            .iter_mut()
                            .filter_map(|m| m.image_paths.as_mut())
                            .flatten()
                        {
                            *p = resolve_session_image_path(p, &asset_dir);
                            session_img_paths.push(p.clone());
                        }

                        let ws = Arc::clone(&self.workspace);
                        let (restored_ws_paths, attachments) =
                            tokio::task::spawn_blocking(move || {
                                (ws.restore_from_session(&session_img_paths), ws.list_attachments())
                            })
                            .await
                            .unwrap_or_default();

                        self.sent_images.extend(restored_ws_paths);
                        let _ = self.tx.send(EngineEvent::WorkspaceUpdated(attachments));

                        self.session_id = data.id;
                        self.active_system_prompt = data.system_prompt;
                        self.active_num_ctx = data.num_ctx.max(self.config.app.default_context);
                        self.messages = data.messages;
                        self.emit_chat();
                        self.notify(format!("Loaded session: {}", data.title), false);

                        if let Some(calls) = self
                            .messages
                            .last()
                            .and_then(|m| m.tool_calls.clone())
                            .filter(|c| !c.is_empty())
                        {
                            self.enqueue_tool_calls(calls);
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
            EngineCommand::DeleteSession(path) => {
                info!("Deleting session at path: {}", path.display());
                self.emit_sessions(Some(path)).await;
            }
            EngineCommand::ResetChat => {
                info!("Resetting chat session");
                self.flush_save_task().await;
                self.abort_active_state();
                self.clear_workspace().await;

                self.session_id = SessionManager::new_id();
                self.active_num_ctx = self.config.app.default_context;
                self.messages.clear();
                self.emit_chat();
                self.notify("Started new chat session.", false);
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
            } => {
                self.active_task = None;
                let final_text = std::mem::take(&mut self.stream_buffer);
                let total_used = prompt_tokens + eval_tokens;
                let ctx_limit = self.active_num_ctx as u64;
                let usage_pct = (total_used as f64 / ctx_limit.max(1) as f64) * 100.0;
                let hit_length = done_reason.as_deref() == Some("length");
                let is_near_limit = hit_length
                    || usage_pct >= 90.0
                    || prompt_tokens >= ctx_limit.saturating_sub(256);

                if !final_text.trim().is_empty() || !tool_calls.is_empty() {
                    self.messages.push(ChatMessage {
                        tool_calls: (!tool_calls.is_empty()).then(|| tool_calls.clone()),
                        ..ChatMessage::new("assistant", final_text)
                    });
                    self.save_and_emit_chat().await;

                    let prefix = match (hit_length, is_near_limit) {
                        (true, _) => "⚠️ CTX MAXED OUT (Truncated) — ",
                        (false, true) => "⚠️ CTX LIMIT NEAR — ",
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
                    self.enqueue_tool_calls(tool_calls.clone());
                }
            }
            EngineEvent::StreamError(ref err) => {
                self.active_task = None;
                self.stream_buffer.clear();
                self.notify(format!("API Error: {err}"), true);
                let _ = self.tx.send(ev);
            }
            other => {
                let _ = self.tx.send(other);
            }
        }
    }

    async fn save_and_emit_chat(&mut self) {
        self.save_session_bg().await;
        self.emit_chat();
    }

    async fn save_session_bg(&mut self) {
        self.flush_save_task().await;

        let sessions = Arc::clone(&self.sessions);
        let id = self.session_id.clone();
        let model = self.model.clone();
        let sys = self.active_system_prompt.clone();
        let ctx = self.active_num_ctx;
        let msgs: Vec<ChatMessage> = self
            .messages
            .iter()
            .map(ChatMessage::without_images)
            .collect();

        self.save_task = Some(tokio::task::spawn_blocking(move || {
            if let Err(e) = sessions.save(&id, &model, &sys, ctx, &msgs) {
                error!("Failed to save session {id}: {e}");
            }
        }));
    }

    async fn spawn_ollama_turn(
        &mut self,
        turn: &TurnConfig,
        internal_tx: UnboundedSender<EngineEvent>,
    ) {
        self.stream_buffer.clear();
        let tools_schema = turn.enable_tools.then(crate::tools::schemas);
        let asset_dir = self.sessions.asset_dir(&self.session_id);

        let mut msgs = self.messages.clone();
        if let Ok(updated_msgs) = tokio::task::spawn_blocking(move || {
            for msg in &mut msgs {
                if msg.images.is_some() {
                    continue;
                }
                let Some(paths) = &msg.image_paths else {
                    continue;
                };
                let b64s: Vec<String> = paths
                    .iter()
                    .filter_map(|p| {
                        let resolved = resolve_session_image_path(p, &asset_dir);
                        ImageWorkspace::encode_image(&resolved)
                            .inspect_err(|e| {
                                warn!("Failed to encode session image {}: {e}", resolved.display())
                            })
                            .ok()
                    })
                    .collect();
                if !b64s.is_empty() {
                    msg.images = Some(b64s);
                }
            }
            msgs
        })
        .await
        {
            self.messages = updated_msgs;
        }

        let payload = ollama::build_payload(
            &self.model,
            &self.messages,
            &turn.system_prompt,
            turn.num_ctx,
            tools_schema,
        );

        let _ = self.tx.send(EngineEvent::StreamStarted);
        let client = self.client.clone();
        let url = self.config.ollama.base_url.clone();

        self.active_task = Some(tokio::spawn(async move {
            ollama::stream_chat(client, url, payload, internal_tx).await;
        }));
    }

    fn emit_chat(&self) {
        let messages = self
            .messages
            .iter()
            .map(ChatMessage::with_ui_image_placeholders)
            .collect();
        let _ = self.tx.send(EngineEvent::ChatUpdated {
            messages,
            num_ctx: self.active_num_ctx,
            system_prompt: self.active_system_prompt.clone(),
        });
    }

    fn notify(&self, msg: impl Into<String>, is_error: bool) {
        let _ = self.tx.send(EngineEvent::Notification {
            message: msg.into(),
            is_error,
        });
    }
}
