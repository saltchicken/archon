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

    fn prompt_next_tool(&mut self) {
        if let Some(call) = self.pending_tools.front() {
            let preview = self.tools.format_approval(call);
            let index = self.pending_batch_total - self.pending_tools.len() + 1;
            let _ = self.tx.send(EngineEvent::ToolApprovalRequested {
                formatted_preview: preview,
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

    pub async fn run(mut self, mut cmd_rx: UnboundedReceiver<EngineCommand>) {
        let (internal_tx, mut internal_rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    self.handle_command(cmd, &internal_tx).await;
                }
                Some(internal_ev) = internal_rx.recv() => {
                    self.handle_internal_event(internal_ev).await;
                }
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
                let ws = Arc::clone(&self.workspace);
                let res = tokio::task::spawn_blocking(move || {
                    let paste_res = ws.paste_clipboard();
                    let list = ws.list_attachments();
                    (paste_res, list)
                })
                .await;

                if let Ok((paste_res, list)) = res {
                    match paste_res {
                        Ok(_) => self.notify("Image pasted from clipboard.", false),
                        Err(e) => self.notify(format!("{e}"), true),
                    }
                    let _ = self.tx.send(EngineEvent::WorkspaceUpdated(list));
                }
            }
            EngineCommand::ImportFile(path) => {
                let ws = Arc::clone(&self.workspace);
                let path_clone = path.clone();
                let res = tokio::task::spawn_blocking(move || {
                    let import_res = ws.import_file(&path_clone);
                    let list = ws.list_attachments();
                    (import_res, list)
                })
                .await;

                if let Ok((import_res, list)) = res {
                    match import_res {
                        Ok(_) => self.notify(format!("Added {}", path.display()), false),
                        Err(e) => self.notify(format!("{e}"), true),
                    }
                    let _ = self.tx.send(EngineEvent::WorkspaceUpdated(list));
                }
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
                let attachments =
                    tokio::task::spawn_blocking(move || ws.list_attachments())
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
                    role: "user".into(),
                    content: prompt,
                    images: if new_imgs.is_empty() {
                        None
                    } else {
                        Some(new_imgs)
                    },
                    tool_calls: None,
                });

                self.save_session_bg();
                self.emit_chat();
                self.spawn_ollama_turn(
                    &system_prompt,
                    num_ctx,
                    enable_tools,
                    internal_tx.clone(),
                );
            }
            EngineCommand::CancelStream => {
                if let Some(handle) = self.active_task.take() {
                    handle.abort();
                    if !self.stream_buffer.trim().is_empty() {
                        self.messages.push(ChatMessage {
                            role: "assistant".into(),
                            content: format!(
                                "{}\n\n*[Generation cancelled]*",
                                self.stream_buffer
                            ),
                            images: None,
                            tool_calls: None,
                        });
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

                if !feedback.trim().is_empty() {
                    if approved {
                        internal_out.push_str(&format!(
                            "\n\n[User attached a message]: {}",
                            feedback.trim()
                        ));
                    } else {
                        internal_out.push_str(&format!(
                            " User provided this reason/message: {}",
                            feedback.trim()
                        ));
                    }
                }

                self.messages.push(ChatMessage {
                    role: "tool".into(),
                    content: internal_out,
                    images: None,
                    tool_calls: None,
                });

                self.save_session_bg();
                self.emit_chat();

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
                if let Some(h) = self.active_task.take() {
                    h.abort();
                }
                self.pending_tools.clear();
                self.pending_batch_total = 0;
                self.session_id = data.id;
                self.active_system_prompt = data.system_prompt;
                self.active_num_ctx = data.num_ctx;
                self.messages = data.messages;
                self.sent_images.clear();
                self.emit_chat();
                self.notify(format!("Loaded session: {}", data.title), false);

                if let Some(last_msg) = self.messages.last() {
                    if let Some(ref calls) = last_msg.tool_calls {
                        if !calls.is_empty() {
                            self.enqueue_tool_calls(calls.clone());
                        }
                    }
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
                if let Some(h) = self.active_task.take() {
                    h.abort();
                }
                self.pending_tools.clear();
                self.pending_batch_total = 0;
                self.session_id = SessionManager::new_id();
                self.messages.clear();
                self.sent_images.clear();
                self.stream_buffer.clear();
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
                        role: "assistant".into(),
                        content: final_text,
                        images: None,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls.clone())
                        },
                    });
                    self.save_session_bg();
                }

                self.emit_chat();
                let _ = self.tx.send(EngineEvent::Notification {
                    message: format!("Tokens — Prompt: {prompt_tokens} | Eval: {eval_tokens}"),
                    is_error: false,
                });

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

    fn save_session_bg(&self) {
        let sessions = Arc::clone(&self.sessions);
        let id = self.session_id.clone();
        let model = self.model.clone();
        let sys = self.active_system_prompt.clone();
        let ctx = self.active_num_ctx;
        let msgs: Vec<ChatMessage> = self
            .messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: m.content.clone(),
                images: None,
                tool_calls: m.tool_calls.clone(),
            })
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
        let tools_schema = if enable_tools {
            Some(self.tools.schemas())
        } else {
            None
        };
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
        // Strip heavy base64 strings when sending messages to the UI
        let ui_messages: Vec<ChatMessage> = self
            .messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: m.content.clone(),
                images: m
                    .images
                    .as_ref()
                    .map(|imgs| vec![String::new(); imgs.len()]),
                tool_calls: m.tool_calls.clone(),
            })
            .collect();

        let _ = self.tx.send(EngineEvent::ChatUpdated {
            messages: ui_messages,
        });
    }

    fn notify(&self, msg: impl Into<String>, is_error: bool) {
        let _ = self.tx.send(EngineEvent::Notification {
            message: msg.into(),
            is_error,
        });
    }
}
