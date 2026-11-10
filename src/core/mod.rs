pub mod config;
pub mod ollama;
pub mod session;
pub mod tools;
pub mod workspace;

use config::Config;
use reqwest::Client;
use session::{ChatMessage, SessionData, SessionManager};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tools::{ToolCall, ToolRegistry};
use workspace::{ImageAttachment, ImageWorkspace};

#[derive(Debug)]
pub enum EngineCommand {
    PasteClipboard,
    ImportFile(PathBuf),
    ClearImages,
    SendPrompt {
        prompt: String,
        system_prompt: String,
        num_ctx: u32,
        enable_tools: bool,
    },
    CancelStream,
    ResolveTool {
        approved: bool,
        feedback: String,
        system_prompt: String,
        num_ctx: u32,
        enable_tools: bool,
    },
    LoadSession(SessionData),
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
    },
    StreamStarted,
    StreamChunk(String),
    StreamDone {
        prompt_tokens: u64,
        eval_tokens: u64,
        tool_calls: Vec<ToolCall>,
    },
    StreamError(String),
    ToolApprovalRequested {
        formatted_preview: String,
        index: usize,
        total: usize,
    },
    SessionsListed(Vec<SessionData>),
}

pub struct CoreEngine {
    model: String,
    config: Config,
    client: Client,
    workspace: Arc<ImageWorkspace>,
    sessions: Arc<SessionManager>,
    tools: ToolRegistry,
    session_id: String,
    active_system_prompt: String,
    active_num_ctx: u32,
    messages: Vec<ChatMessage>,
    sent_images: HashSet<PathBuf>,
    stream_buffer: String,
    active_task: Option<JoinHandle<()>>,
    pending_tools: VecDeque<ToolCall>,
    pending_batch_total: usize,
    tx: UnboundedSender<EngineEvent>,
}

impl CoreEngine {
    pub fn new(
        model: String,
        config: Config,
        tx: UnboundedSender<EngineEvent>,
    ) -> anyhow::Result<Self> {
        let workspace = Arc::new(ImageWorkspace::new()?);
        let sessions = Arc::new(SessionManager::new(&config.app.history_dir)?);
        let tools = ToolRegistry::new(&config);
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
            tools,
            session_id: SessionManager::new_id(),
            active_system_prompt: String::new(),
            active_num_ctx,
            messages: Vec::new(),
            sent_images: HashSet::new(),
            stream_buffer: String::new(),
            active_task: None,
            pending_tools: VecDeque::new(),
            pending_batch_total: 0,
            tx,
        })
    }

    fn abort_active_state(&mut self) {
        if let Some(h) = self.active_task.take() {
            h.abort();
        }
        self.pending_tools.clear();
        self.pending_batch_total = 0;
        self.sent_images.clear();
        self.stream_buffer.clear();
    }

    fn prompt_next_tool(&mut self) {
        if let Some(call) = self.pending_tools.front() {
            let formatted_preview = self.tools.format_approval(call);
            let index = self.pending_batch_total - self.pending_tools.len() + 1;
            let _ = self.tx.send(EngineEvent::ToolApprovalRequested {
                formatted_preview,
                index,
                total: self.pending_batch_total,
            });
        } else {
            self.pending_batch_total = 0;
        }
    }

    fn enqueue_tool_calls(&mut self, calls: Vec<ToolCall>) {
        self.pending_tools = VecDeque::from(calls);
        self.pending_batch_total = self.pending_tools.len();
        self.prompt_next_tool();
    }

    async fn update_workspace<F>(&self, op: F, success_msg: String)
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

    pub async fn run(mut self, mut cmd_rx: UnboundedReceiver<EngineCommand>) {
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => self.handle_command(cmd, &internal_tx).await,
                Some(ev) = internal_rx.recv() => self.handle_internal_event(ev).await,
                else => break,
            }
        }
    }

    async fn handle_command(
        &mut self,
        cmd: EngineCommand,
        internal_tx: &UnboundedSender<EngineEvent>,
    ) {
        match cmd {
            EngineCommand::PasteClipboard => {
                self.update_workspace(
                    |ws| ws.paste_clipboard(),
                    "Image pasted from clipboard.".into(),
                )
                .await;
            }
            EngineCommand::ImportFile(path) => {
                let msg = format!("Added {}", path.display());
                self.update_workspace(move |ws| ws.import_file(&path), msg)
                    .await;
            }
            EngineCommand::ClearImages => {
                let ws = Arc::clone(&self.workspace);
                let _ = tokio::task::spawn_blocking(move || ws.clear()).await;
                self.notify("Cleared attached images.", false);
                let _ = self.tx.send(EngineEvent::WorkspaceUpdated(vec![]));
            }
            EngineCommand::SendPrompt {
                prompt,
                system_prompt,
                num_ctx,
                enable_tools,
            } => {
                self.active_system_prompt = system_prompt.clone();
                self.active_num_ctx = num_ctx;

                let ws = Arc::clone(&self.workspace);
                let attachments = tokio::task::spawn_blocking(move || ws.list_attachments())
                    .await
                    .unwrap_or_default();

                let mut new_imgs = Vec::new();
                for att in attachments {
                    if !self.sent_images.contains(&att.path) {
                        let p = att.path.clone();
                        if let Ok(Ok(b64)) =
                            tokio::task::spawn_blocking(move || ImageWorkspace::encode_image(&p))
                                .await
                        {
                            new_imgs.push(b64);
                            self.sent_images.insert(att.path);
                        }
                    }
                }

                self.messages.push(ChatMessage {
                    images: (!new_imgs.is_empty()).then_some(new_imgs),
                    ..ChatMessage::new("user", prompt)
                });

                self.save_and_emit_chat();
                self.spawn_ollama_turn(&system_prompt, num_ctx, enable_tools, internal_tx.clone());
            }
            EngineCommand::CancelStream => {
                if let Some(handle) = self.active_task.take() {
                    handle.abort();
                    if !self.stream_buffer.trim().is_empty() {
                        self.messages.push(ChatMessage::new(
                            "assistant",
                            format!("{}\n\n*[Generation cancelled]*", self.stream_buffer),
                        ));
                        self.save_session_bg();
                    }
                    self.stream_buffer.clear();
                    self.emit_chat();
                    self.notify("Stream cancelled.", false);
                }
            }
            EngineCommand::ResolveTool {
                approved,
                feedback,
                system_prompt,
                num_ctx,
                enable_tools,
            } => {
                self.active_system_prompt = system_prompt.clone();
                self.active_num_ctx = num_ctx;

                let Some(call) = self.pending_tools.pop_front() else {
                    return;
                };

                let (mut internal_out, _) = if approved {
                    self.tools.execute(&call).await
                } else {
                    (
                        "The user declined to run this command.".into(),
                        "Declined.".into(),
                    )
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

                self.messages.push(ChatMessage::new("tool", internal_out));
                self.save_and_emit_chat();

                if self.pending_tools.is_empty() {
                    self.pending_batch_total = 0;
                    self.spawn_ollama_turn(
                        &system_prompt,
                        num_ctx,
                        enable_tools,
                        internal_tx.clone(),
                    );
                } else {
                    self.prompt_next_tool();
                }
            }
            EngineCommand::RefreshSessions => {
                let sessions = Arc::clone(&self.sessions);
                if let Ok(list) = tokio::task::spawn_blocking(move || sessions.list()).await {
                    let _ = self.tx.send(EngineEvent::SessionsListed(list));
                }
            }
            EngineCommand::LoadSession(data) => {
                self.abort_active_state();
                self.session_id = data.id;
                self.active_system_prompt = data.system_prompt;
                self.active_num_ctx = data.num_ctx;
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
            EngineCommand::DeleteSession(path) => {
                let sessions = Arc::clone(&self.sessions);
                if let Ok(list) = tokio::task::spawn_blocking(move || {
                    let _ = sessions.delete(&path);
                    sessions.list()
                })
                .await
                {
                    let _ = self.tx.send(EngineEvent::SessionsListed(list));
                }
            }
            EngineCommand::ResetChat => {
                self.abort_active_state();
                self.session_id = SessionManager::new_id();
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
                ref tool_calls,
            } => {
                self.active_task = None;
                let final_text = std::mem::take(&mut self.stream_buffer);

                if !final_text.trim().is_empty() || !tool_calls.is_empty() {
                    self.messages.push(ChatMessage {
                        tool_calls: (!tool_calls.is_empty()).then(|| tool_calls.clone()),
                        ..ChatMessage::new("assistant", final_text)
                    });
                    self.save_session_bg();
                }

                self.emit_chat();
                self.notify(
                    format!("Tokens — Prompt: {prompt_tokens} | Eval: {eval_tokens}"),
                    false,
                );

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

    fn save_and_emit_chat(&self) {
        self.save_session_bg();
        self.emit_chat();
    }

    fn save_session_bg(&self) {
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

        tokio::task::spawn_blocking(move || {
            let _ = sessions.save(&id, &model, &sys, ctx, &msgs);
        });
    }

    fn spawn_ollama_turn(
        &mut self,
        system_prompt: &str,
        num_ctx: u32,
        enable_tools: bool,
        internal_tx: UnboundedSender<EngineEvent>,
    ) {
        self.stream_buffer.clear();
        let tools_schema = enable_tools.then(|| self.tools.schemas());
        let payload = ollama::build_payload(
            &self.model,
            &self.messages,
            system_prompt,
            num_ctx,
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
        let _ = self.tx.send(EngineEvent::ChatUpdated { messages });
    }

    fn notify(&self, msg: impl Into<String>, is_error: bool) {
        let _ = self.tx.send(EngineEvent::Notification {
            message: msg.into(),
            is_error,
        });
    }
}
