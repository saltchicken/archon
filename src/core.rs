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
    path::{Path, PathBuf},
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
    LoadSession(PathBuf),
    DeleteSession(PathBuf),
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
        thoughts: Vec<ThoughtPart>,
    },
    StreamError(String),
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

    async fn process_pending_tools(&mut self, internal_tx: &UnboundedSender<EngineEvent>) {
        let mut auto_executed = false;

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
                auto_executed = true;
                continue;
            }

            if !self.is_call_auto_approved(call) {
                if auto_executed {
                    self.save_and_emit_chat().await;
                }
                self.prompt_next_tool();
                return;
            }

            let call = self.pending_tools.pop_front().unwrap();
            let sig = tools::call_signature(&call);
            info!(
                "Auto-approving tool call '{}' (max_tool_output_size: {})",
                sig, self.active_max_tool_output_size
            );

            let (internal_out, truncated) =
                tools::execute(&call, &self.config, self.active_max_tool_output_size).await;

            self.messages.push(ChatMessage {
                truncated,
                ..ChatMessage::new("tool", internal_out)
            });
            auto_executed = true;
        }

        if auto_executed {
            self.save_and_emit_chat().await;
        }

        let turn = self.turn.clone();
        self.spawn_model_turn(&turn, internal_tx.clone()).await;
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

    async fn collect_unsent_images(&mut self) -> Option<Vec<PathBuf>> {
        let asset_dir = self.sessions.asset_dir(&self.session_id);
        let staging = Arc::clone(&self.attachments);
        let already_sent = self.sent_images.clone();

        let (paths, newly_sent) = tokio::task::spawn_blocking(move || {
            let _ = std::fs::create_dir_all(&asset_dir);
            let mut paths = Vec::new();
            let mut sent = Vec::new();

            for att in staging.list_attachments() {
                if already_sent.contains(&att.path) {
                    continue;
                }
                let dest = asset_dir.join(att.path.file_name().unwrap_or_default());
                if std::fs::copy(&att.path, &dest).is_ok() {
                    paths.push(dest);
                    sent.push(att.path);
                }
            }
            (paths, sent)
        })
        .await
        .unwrap_or_default();

        self.sent_images.extend(newly_sent);
        (!paths.is_empty()).then_some(paths)
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
                Some(ev) = internal_rx.recv() => self.handle_internal_event(ev, &internal_tx).await,
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

                self.turn = turn.clone();
                let image_paths = self.collect_unsent_images().await;

                self.messages.push(ChatMessage {
                    images: None,
                    image_paths,
                    ..ChatMessage::new("user", prompt)
                });

                self.save_and_emit_chat().await;
                self.spawn_model_turn(&turn, internal_tx.clone()).await;
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
                always_approve,
                feedback,
                turn,
            } => {
                self.turn = turn;
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

                let (mut internal_out, truncated) = if approved {
                    tools::execute(&call, &self.config, self.active_max_tool_output_size).await
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

                self.process_pending_tools(internal_tx).await;
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
                        for msg in &mut data.messages {
                            // Ensure no base64 image payloads are retained in memory from disk
                            msg.images = None;
                            if let Some(paths) = msg.image_paths.as_mut() {
                                for p in paths {
                                    *p = resolve_session_image_path(p, &asset_dir);
                                    session_img_paths.push(p.clone());
                                }
                            }
                        }

                        let staging = Arc::clone(&self.attachments);
                        let (restored_staging_paths, attachments) =
                            tokio::task::spawn_blocking(move || {
                                (
                                    staging.restore_from_session(&session_img_paths),
                                    staging.list_attachments(),
                                )
                            })
                            .await
                            .unwrap_or_default();

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
                            self.process_pending_tools(internal_tx).await;
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
                if self.active_task.is_some() {
                    self.notify(
                        "Cannot switch model while streaming. Cancel stream first.",
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

                self.save_session_bg().await;
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
                self.save_session_bg().await;
                self.emit_model_config();
                self.notify(format!("Context window set to {} tokens.", clamped), false);
            }
        }
    }

    async fn handle_internal_event(
        &mut self,
        ev: EngineEvent,
        internal_tx: &UnboundedSender<EngineEvent>,
    ) {
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
                    self.save_and_emit_chat().await;

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
                    self.process_pending_tools(internal_tx).await;
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
        let sys = self.turn.system_prompt.clone();
        let ctx = self.turn.num_ctx;
        let msgs = self.messages.clone();

        self.save_task = Some(tokio::task::spawn_blocking(move || {
            if let Err(e) = sessions.save(&id, &model, &sys, ctx, &msgs) {
                error!("Failed to save session {id}: {e}");
            }
        }));
    }

    async fn spawn_model_turn(
        &mut self,
        turn: &TurnConfig,
        internal_tx: UnboundedSender<EngineEvent>,
    ) {
        self.stream_buffer.clear();
        let asset_dir = self.sessions.asset_dir(&self.session_id);

        // Clone lightweight messages (without Base64 payloads) and hydrate Base64
        // strings only into the transient `hydrated_msgs` sent to the provider.
        let mut msgs = self.messages.clone();
        let hydrated_msgs = tokio::task::spawn_blocking(move || {
            for msg in &mut msgs {
                let Some(paths) = &msg.image_paths else {
                    continue;
                };
                let b64s: Vec<String> = paths
                    .iter()
                    .filter_map(|p| {
                        let resolved = resolve_session_image_path(p, &asset_dir);
                        ImageStaging::encode_image(&resolved)
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
        .unwrap_or_else(|_| self.messages.clone());

        let _ = self.tx.send(EngineEvent::StreamStarted);
        let provider = self.provider;
        let client = self.client.clone();
        let model = self.model.clone();
        let config = self.config.clone();
        let turn_cfg = turn.clone();

        self.active_task = Some(tokio::spawn(async move {
            provider
                .stream_turn(client, model, config, hydrated_msgs, turn_cfg, internal_tx)
                .await;
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
