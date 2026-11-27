pub mod attachments;
pub mod config;
pub mod db;
pub mod logger;
pub mod processes;
pub mod protocol;
pub mod provider;
pub mod schedule;
pub(crate) mod schema_utils;
pub mod session;
mod session_ops;
pub mod skill_check;
pub mod skills;
mod tool_runner;
mod turns;

pub use protocol::{
    EngineCommand, EngineEvent, SessionToolsAction, ToolExecSource, TruncatedToolAction, TurnConfig,
};

use crate::tools::{SessionToolState, ToolCall};
use attachments::ImageStaging;
use config::Config;
use processes::ProcessRegistry;
use provider::Provider;
use reqwest::Client;
use session::{ChatMessage, SessionManager};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tool_runner::PendingTruncatedTool;
use tracing::info;
use turns::QueuedInterjection;

struct ActiveSessionState {
    id: String,
    is_fork: bool,
    is_pinned: bool,
    custom_title: Option<String>,
    messages: Vec<ChatMessage>,
    sent_images: HashSet<PathBuf>,
    auto_approve: HashSet<String>,
    executed_skill_hooks: HashSet<String>,
    auto_task_turns: usize,
    tools: SessionToolState,
}

impl ActiveSessionState {
    fn new() -> Self {
        Self {
            id: SessionManager::new_id(),
            is_fork: false,
            is_pinned: false,
            custom_title: None,
            messages: Vec::new(),
            sent_images: HashSet::new(),
            auto_approve: HashSet::new(),
            executed_skill_hooks: HashSet::new(),
            auto_task_turns: 0,
            tools: SessionToolState::default(),
        }
    }
}

#[derive(Default)]
struct InFlightState {
    active_task: Option<JoinHandle<()>>,
    internal_rx: Option<UnboundedReceiver<EngineEvent>>,
    pending_truncated: Option<PendingTruncatedTool>,
    pending_tools: VecDeque<ToolCall>,
    pending_stream_done: Option<EngineEvent>,
    queued_interjections: Vec<QueuedInterjection>,
    stream_buffer: String,
    thought_stream_buffer: String,
}

impl InFlightState {
    fn finish_active_task(&mut self) -> bool {
        self.internal_rx = None;
        self.active_task.take().inspect(|h| h.abort()).is_some()
    }

    fn is_turn_active(&self) -> bool {
        self.active_task.is_some()
    }

    fn abort_all(&mut self) {
        self.finish_active_task();
        self.pending_truncated = None;
        self.pending_stream_done = None;
        self.queued_interjections.clear();
        self.pending_tools.clear();
        self.stream_buffer.clear();
        self.thought_stream_buffer.clear();
    }
}

pub struct CoreEngine {
    model: String,
    provider: Provider,
    config: Config,
    client: Client,
    attachments: Arc<ImageStaging>,
    sessions: Arc<SessionManager>,
    session: ActiveSessionState,
    turn: TurnConfig,
    active_max_tool_output_size: usize,
    save_session: bool,
    inflight: InFlightState,
    save_task: Option<JoinHandle<()>>,
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
            session: ActiveSessionState::new(),
            turn: TurnConfig {
                system_prompt: String::new(),
                num_ctx,
                enable_tools: true,
                active_skill: None,
                skill_args: None,
                raw_skill_args: None,
                skill_inputs: BTreeMap::new(),
            },
            active_max_tool_output_size,
            save_session: true,
            inflight: InFlightState::default(),
            save_task: None,
            tx,
        })
    }

    pub fn set_save_session(&mut self, save: bool) {
        self.save_session = save;
    }

    fn finish_active_task(&mut self) -> bool {
        self.inflight.finish_active_task()
    }

    fn is_turn_active(&self) -> bool {
        self.inflight.is_turn_active()
    }

    fn spawn_with_tx<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(UnboundedSender<EngineEvent>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.inflight.internal_rx = Some(rx);
        let progress_tx = tx.clone();
        self.inflight.active_task = Some(tokio::spawn(async move {
            crate::tools::progress::TOOL_PROGRESS_TX
                .scope(progress_tx, f(tx))
                .await;
        }));
    }

    fn abort_active_state(&mut self) {
        self.inflight.abort_all();
        self.session.sent_images.clear();
    }

    async fn emit_error(&mut self, err: impl Into<String>) {
        let err = err.into();
        self.session
            .messages
            .push(ChatMessage::new("error", err.clone()));
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
        let (active_skill, skill_args, raw_skill_args, skill_inputs) = (
            self.turn.active_skill.take(),
            self.turn.skill_args.take(),
            self.turn.raw_skill_args.take(),
            std::mem::take(&mut self.turn.skill_inputs),
        );
        self.turn = new_turn;
        if let Some(name) = active_skill {
            let tools_ok = self.turn.enable_tools
                || (!self.inflight.pending_tools.is_empty() || self.is_turn_active());
            if skills::check_skill_activation(&name, tools_ok, &self.config).is_ok() {
                self.turn.enable_tools = tools_ok;
                self.turn.active_skill = Some(name);
                self.turn.skill_args = skill_args;
                self.turn.raw_skill_args = raw_skill_args;
                self.turn.skill_inputs = skill_inputs;
            }
        }
    }

    fn reset_skill_and_sync_model_limits(&mut self) {
        self.turn.active_skill = None;
        self.turn.skill_args = None;
        self.turn.raw_skill_args = None;
        self.turn.skill_inputs.clear();
        self.turn.num_ctx = self.config.context_for_model(&self.model);
        self.active_max_tool_output_size = self.config.max_tool_output_for_model(&self.model);
        self.emit_model_config();
    }

    fn active_turn_skill(&self) -> Option<&str> {
        self.turn.active_skill.as_deref()
    }

    pub async fn run(mut self, mut cmd_rx: UnboundedReceiver<EngineCommand>) {
        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                    Some(cmd) => self.handle_command(cmd).await,
                    None => break,
                },
                maybe_ev = async {
                    match self.inflight.internal_rx.as_mut() {
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

        info!("Shutting down CoreEngine, terminating background jobs...");
        ProcessRegistry::global().shutdown().await;
        crate::tools::browser::BrowserRegistry::global()
            .shutdown()
            .await;

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
            EngineCommand::SendPrompt { prompt, turn } => {
                self.handle_send_prompt(prompt, turn).await;
            }
            EngineCommand::CancelStream => {
                self.handle_cancel_stream().await;
            }
            EngineCommand::ResolveTool {
                approved,
                always_approve,
                feedback,
                turn,
            } => {
                self.handle_resolve_tool(approved, always_approve, feedback, turn)
                    .await;
            }
            EngineCommand::ResolveTruncatedTool {
                action,
                guide_prompt,
                turn,
            } => {
                self.handle_resolve_truncated_tool(action, guide_prompt, turn)
                    .await;
            }
            EngineCommand::RefreshSessions => {
                info!("Refreshing sessions list");
                self.emit_sessions(None).await;
            }
            EngineCommand::LoadSession(session_id) => {
                self.handle_load_session(session_id).await;
            }
            EngineCommand::ForkSession(msg_idx) => {
                self.handle_fork_session(msg_idx).await;
            }
            EngineCommand::RenameSession(new_title) => {
                self.handle_rename_session(new_title).await;
            }
            EngineCommand::TogglePinSession(maybe_id) => {
                self.handle_toggle_pin_session(maybe_id).await;
            }
            EngineCommand::DeleteSession(session_id) => {
                self.handle_delete_session(session_id).await;
            }
            EngineCommand::ResetChat => {
                self.handle_reset_chat().await;
            }
            EngineCommand::CompactChat {
                skill,
                instructions,
            } => {
                self.handle_compact_chat(skill, instructions).await;
            }
            EngineCommand::SwitchModel {
                model,
                num_ctx,
                max_tool_output_size,
            } => {
                self.handle_switch_model(model, num_ctx, max_tool_output_size)
                    .await;
            }
            EngineCommand::SetTurnConfig(new_turn) => {
                self.handle_set_turn_config(new_turn).await;
            }
            EngineCommand::SetMemory { scope, memory } => {
                self.handle_set_memory(scope, memory).await;
            }
            EngineCommand::RunShellCommand {
                command,
                include_in_context,
            } => {
                self.handle_run_shell_command(command, include_in_context);
            }
            EngineCommand::RunManualTool {
                raw_input,
                include_in_context,
            } => {
                self.handle_run_manual_tool(raw_input, include_in_context)
                    .await;
            }
            EngineCommand::InspectPayload { draft_prompt, turn } => {
                self.handle_inspect_payload(draft_prompt, turn);
            }
            EngineCommand::ManageSessionTools(action) => {
                self.handle_manage_session_tools(action).await;
            }
        }
    }

    async fn handle_internal_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::StreamThoughtChunk(ref chunk) => {
                self.inflight.thought_stream_buffer.push_str(chunk);
                let _ = self.tx.send(ev);
            }
            EngineEvent::StreamChunk(ref chunk) => {
                self.inflight.stream_buffer.push_str(chunk);
                let _ = self.tx.send(ev);
            }
            EngineEvent::StreamDone {
                prompt_tokens,
                eval_tokens,
                ref tool_calls,
                ref thoughts,
            } => {
                let calls = tool_calls.clone();
                let th = thoughts.clone();
                self.on_stream_done(ev, prompt_tokens, eval_tokens, calls, th)
                    .await;
            }
            EngineEvent::StreamError(ref err) => {
                let err_msg = err.clone();
                self.on_stream_error(ev, err_msg).await;
            }
            EngineEvent::ToolExecFinished(source, res) => {
                self.on_tool_exec_finished(source, res).await;
            }
            EngineEvent::ToolCondensed(res) => {
                self.on_tool_condensed(res).await;
            }
            EngineEvent::ChatCompacted {
                res,
                prompt_tokens,
                eval_tokens,
            } => {
                self.on_chat_compacted(res, prompt_tokens, eval_tokens)
                    .await;
            }
            EngineEvent::HookCompleted {
                tool_name,
                summary,
                output,
                truncated,
                after_turn,
            } => {
                self.on_hook_completed(tool_name, summary, output, truncated, after_turn)
                    .await;
            }
            EngineEvent::HooksFinished { after_turn } => {
                self.on_hooks_finished(after_turn).await;
            }
            other => {
                let _ = self.tx.send(other);
            }
        }
    }
}
