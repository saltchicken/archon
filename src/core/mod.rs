pub mod config;
pub mod ollama;
pub mod session;
pub mod tools;
pub mod workspace;

use config::Config;
use session::{ChatMessage, SessionData, SessionManager};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
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
    workspace: ImageWorkspace,
    sessions: SessionManager,
    tools: ToolRegistry,
    session_id: String,
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
        let workspace = ImageWorkspace::new()?;
        let sessions = SessionManager::new(&config.app.history_dir)?;
        let tools = ToolRegistry::new(&config);
        Ok(Self {
            model,
            config,
            workspace,
            sessions,
            tools,
            session_id: SessionManager::new_id(),
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
                match self.workspace.paste_clipboard() {
                    Ok(_) => self.notify("Image pasted from clipboard.", false),
                    Err(e) => self.notify(format!("{e}"), true),
                }
                let _ = self.tx.send(EngineEvent::WorkspaceUpdated(
                    self.workspace.list_attachments(),
                ));
            }
            EngineCommand::ImportFile(path) => {
                match self.workspace.import_file(&path) {
                    Ok(_) => self.notify(format!("Added {}", path.display()), false),
                    Err(e) => self.notify(format!("{e}"), true),
                }
                let _ = self.tx.send(EngineEvent::WorkspaceUpdated(
                    self.workspace.list_attachments(),
                ));
            }
            EngineCommand::ClearImages => {
                self.workspace.clear();
                self.notify("Cleared attached images.", false);
                let _ = self.tx.send(EngineEvent::WorkspaceUpdated(vec![]));
            }
            EngineCommand::SendPrompt {
                prompt,
                system_prompt,
                num_ctx,
                enable_tools,
            } => {
                let mut new_imgs = Vec::new();
                for att in self.workspace.list_attachments() {
                    if !self.sent_images.contains(&att.path) {
                        if let Ok(b64) = ImageWorkspace::encode_image(&att.path) {
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

                let _ = self.sessions.save(
                    &self.session_id,
                    &self.model,
                    &system_prompt,
                    num_ctx,
                    &self.messages,
                );
                self.emit_chat();
                self.spawn_ollama_turn(&system_prompt, num_ctx, enable_tools, internal_tx.clone());
            }
            EngineCommand::CancelStream => {
                if let Some(handle) = self.active_task.take() {
                    handle.abort();
                    if !self.stream_buffer.trim().is_empty() {
                        self.messages.push(ChatMessage {
                            role: "assistant".into(),
                            content: format!("{}\n\n*[Generation cancelled]*", self.stream_buffer),
                            images: None,
                            tool_calls: None,
                        });
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

                let _ = self.sessions.save(
                    &self.session_id,
                    &self.model,
                    &system_prompt,
                    num_ctx,
                    &self.messages,
                );
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
                let _ = self
                    .tx
                    .send(EngineEvent::SessionsListed(self.sessions.list()));
            }
            EngineCommand::LoadSession(data) => {
                if let Some(h) = self.active_task.take() {
                    h.abort();
                }
                self.pending_tools.clear();
                self.pending_batch_total = 0;
                self.session_id = data.id;
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
                let _ = self.sessions.delete(&path);
                let _ = self
                    .tx
                    .send(EngineEvent::SessionsListed(self.sessions.list()));
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
                    let _ = self.sessions.save(
                        &self.session_id,
                        &self.model,
                        "",
                        self.config.app.default_context,
                        &self.messages,
                    );
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

        let url = self.config.ollama.base_url.clone();
        let timeout = self.config.ollama.chat_timeout;

        self.active_task = Some(tokio::spawn(async move {
            ollama::stream_chat(url, timeout, payload, internal_tx).await;
        }));
    }

    fn emit_chat(&self) {
        let _ = self.tx.send(EngineEvent::ChatUpdated {
            messages: self.messages.clone(),
        });
    }

    fn notify(&self, msg: impl Into<String>, is_error: bool) {
        let _ = self.tx.send(EngineEvent::Notification {
            message: msg.into(),
            is_error,
        });
    }
}
