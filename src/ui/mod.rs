mod input;
mod modals;
mod render;

use crate::core::{
    EngineCommand, EngineEvent, TurnConfig,
    config::{Config, Preset},
    session::{ChatMessage, SessionSummary},
    workspace::ImageAttachment,
};
use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use futures_util::{FutureExt, StreamExt};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders},
};
use ratatui_textarea::TextArea;
use std::{io, time::Duration};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    time::MissedTickBehavior,
};

pub fn restore_terminal<W: io::Write>(out: &mut W, keyboard_enhanced: bool) {
    let _ = disable_raw_mode();
    let _ = execute!(
        out,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    if keyboard_enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
}

#[derive(PartialEq, Clone, Copy)]
pub(super) enum ActiveModal {
    None,
    AddFile,
    Sessions,
    ToolApproval,
    ModelSelect,
    ContextWindow,
}

#[derive(Default, Clone, Copy)]
pub(super) struct MsgUiState {
    pub scroll: usize,
    pub unfolded: bool,
}

pub struct TuiApp {
    pub(super) model: String,
    pub(super) config: Config,
    pub(super) cmd_tx: UnboundedSender<EngineCommand>,

    pub(super) attachments: Vec<ImageAttachment>,
    pub(super) messages: Vec<ChatMessage>,
    pub(super) styled_messages: Vec<Vec<Line<'static>>>,
    pub(super) styled_stream: Vec<Line<'static>>,
    pub(super) stream_dirty: bool,
    pub(super) last_wrap_width: usize,
    pub(super) streaming_text: String,
    pub(super) is_streaming: bool,
    pub(super) status_bar: (String, bool),

    pub(super) model_idx: usize,
    pub(super) ctx_preset_idx: usize,
    pub(super) ctx_input: String,
    pub(super) preset_idx: usize,
    pub(super) sys_preset_idx: usize,
    pub(super) custom_system_prompt: Option<String>,
    pub(super) enable_tools: bool,
    pub(super) num_ctx: u32,
    pub(super) max_tool_output_size: usize,
    pub(super) used_tokens: u64,
    pub(super) text_area: TextArea<'static>,

    pub(super) chat_scroll: usize,
    pub(super) follow_tail: bool,
    pub(super) tool_scroll: u16,
    pub(super) total_chat_lines: usize,

    pub(super) msg_states: Vec<MsgUiState>,

    pub(super) chat_area: Rect,
    pub(super) msg_visual_ranges: Vec<(usize, usize, usize, bool)>,

    pub(super) sidebar_expanded: bool,
    pub(super) show_help_bar: bool,

    pub(super) modal: ActiveModal,
    pub(super) file_path_input: String,
    pub(super) sessions: Vec<SessionSummary>,
    pub(super) session_idx: usize,
    pub(super) pending_tool: Option<(String, usize, usize)>,
    pub(super) tool_feedback: String,
}

pub(super) fn create_text_area() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_block(titled_block(" Prompt ").border_style(Style::default().fg(Color::Cyan)));
    ta.set_cursor_line_style(Style::default());
    ta
}

pub(super) fn titled_block<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default().borders(Borders::ALL).title(title)
}

pub(super) fn preset_label(presets: &[Preset], idx: usize) -> &str {
    presets.get(idx).map_or("None", |p| p.name.as_str())
}

impl TuiApp {
    pub fn new(model: String, config: Config, cmd_tx: UnboundedSender<EngineCommand>) -> Self {
        let num_ctx = config.context_for_model(&model);
        let max_tool_output_size = config.max_tool_output_for_model(&model);
        let model_idx = config
            .models
            .iter()
            .position(|m| m.name == model)
            .unwrap_or(0);

        Self {
            model,
            config,
            cmd_tx,
            attachments: Vec::new(),
            messages: Vec::new(),
            styled_messages: Vec::new(),
            styled_stream: Vec::new(),
            stream_dirty: false,
            last_wrap_width: 0,
            streaming_text: String::new(),
            is_streaming: false,
            status_bar: (
                "Ready. Press ⏎ to send prompt (⇧⏎ newline), ? to toggle help.".into(),
                false,
            ),
            model_idx,
            ctx_preset_idx: 0,
            ctx_input: String::new(),
            preset_idx: 0,
            sys_preset_idx: 0,
            custom_system_prompt: None,
            enable_tools: true,
            num_ctx,
            max_tool_output_size,
            used_tokens: 0,
            text_area: create_text_area(),
            chat_scroll: 0,
            follow_tail: true,
            tool_scroll: 0,
            total_chat_lines: 0,
            msg_states: Vec::new(),
            chat_area: Rect::default(),
            msg_visual_ranges: Vec::new(),
            sidebar_expanded: false,
            show_help_bar: false,
            modal: ActiveModal::None,
            file_path_input: String::new(),
            sessions: Vec::new(),
            session_idx: 0,
            pending_tool: None,
            tool_feedback: String::new(),
        }
    }

    pub(super) fn chat_view_height(&self) -> usize {
        usize::from(self.chat_area.height.saturating_sub(2))
    }

    pub(super) fn max_chat_scroll(&self) -> usize {
        self.total_chat_lines
            .saturating_sub(self.chat_view_height())
    }

    pub(super) fn scroll_to_bottom(&mut self) {
        self.follow_tail = true;
        self.chat_scroll = usize::MAX;
    }

    pub(super) fn ctx_usage(&self, default_color: Color) -> (f64, Color) {
        let pct = (self.used_tokens as f64 / f64::from(self.num_ctx.max(1))) * 100.0;
        let color = if pct >= 90.0 {
            Color::Red
        } else if pct >= 75.0 {
            Color::Yellow
        } else {
            default_color
        };
        (pct, color)
    }

    fn handle_term_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) => return self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => {
                let single_line = text.replace(['\r', '\n'], "");
                match self.modal {
                    ActiveModal::AddFile => self.file_path_input.push_str(&single_line),
                    ActiveModal::ToolApproval => self.tool_feedback.push_str(&single_line),
                    ActiveModal::ContextWindow => {
                        for c in single_line.chars().filter(|c| c.is_ascii_digit()) {
                            self.ctx_input.push(c);
                        }
                    }
                    ActiveModal::None => {
                        self.text_area.insert_str(text.replace('\r', ""));
                    }
                    ActiveModal::Sessions | ActiveModal::ModelSelect => {}
                }
            }
            _ => {}
        }
        false
    }

    pub async fn run(mut self, mut ev_rx: UnboundedReceiver<EngineEvent>) -> Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();

        let keyboard_enhanced = supports_keyboard_enhancement().unwrap_or(false);
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        if keyboard_enhanced {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }

        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        let mut term_events = EventStream::new();

        // 16ms (~60 FPS) render ticker to coalesce high-frequency StreamChunk and mouse events
        let mut render_tick = tokio::time::interval(Duration::from_millis(16));
        render_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut dirty = true;

        'main_loop: loop {
            if dirty {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }

            tokio::select! {
                _ = render_tick.tick() => {}
                maybe_term = term_events.next() => {
                    match maybe_term {
                        Some(Ok(event)) => {
                            if self.handle_term_event(event) {
                                break 'main_loop;
                            }
                            dirty = true;
                            while let Some( Some(Ok(next_ev)) ) = term_events.next().now_or_never() {
                                if self.handle_term_event(next_ev) {
                                    break 'main_loop;
                                }
                            }
                        }
                        Some(Err(_)) => {}
                        None => break 'main_loop,
                    }
                }
                maybe_ev = ev_rx.recv() => {
                    match maybe_ev {
                        Some(ev) => {
                            self.handle_engine_event(ev);
                            dirty = true;
                            while let Ok(next_ev) = ev_rx.try_recv() {
                                self.handle_engine_event(next_ev);
                            }
                            // Wait for the next 16ms frame tick before redrawing if streaming
                            if self.is_streaming {
                                render_tick.tick().await;
                            }
                        }
                        None => break 'main_loop,
                    }
                }
            }
        }

        restore_terminal(terminal.backend_mut(), keyboard_enhanced);
        Ok(())
    }

    fn handle_engine_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::Notification { message, is_error } => {
                self.status_bar = (message, is_error);
            }
            EngineEvent::WorkspaceUpdated(list) => self.attachments = list,
            EngineEvent::ChatUpdated {
                messages,
                num_ctx,
                system_prompt,
            } => {
                if messages.is_empty() {
                    self.used_tokens = 0;
                }
                self.messages = messages;
                self.styled_messages.clear();
                self.styled_stream.clear();
                self.stream_dirty = false;
                self.num_ctx = num_ctx;
                self.sync_system_prompt(&system_prompt);
                self.is_streaming = false;
                self.streaming_text.clear();
                self.msg_states.clear();
                self.msg_states
                    .resize(self.messages.len() + 1, MsgUiState::default());

                // Unfold the latest assistant response while keeping separate `tool` blocks folded.
                if let Some(last_assistant_idx) =
                    self.messages.iter().rposition(|m| m.role == "assistant")
                {
                    self.msg_states[last_assistant_idx].unfolded = true;
                }
            }
            EngineEvent::StreamStarted => {
                self.is_streaming = true;
                self.streaming_text.clear();
                self.styled_stream.clear();
                self.stream_dirty = true;
                self.status_bar = (
                    format!("Streaming response from {}... (Esc to cancel)", self.model),
                    false,
                );
                self.scroll_to_bottom();
            }
            EngineEvent::StreamChunk(c) => {
                self.streaming_text.push_str(&c);
                self.stream_dirty = true;
            }
            EngineEvent::StreamDone {
                prompt_tokens,
                eval_tokens,
                ..
            } => {
                self.is_streaming = false;
                self.styled_stream.clear();
                self.stream_dirty = false;
                self.used_tokens = prompt_tokens + eval_tokens;
            }
            EngineEvent::StreamError(_) => {
                self.is_streaming = false;
                self.styled_stream.clear();
                self.stream_dirty = false;
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
            EngineEvent::SessionsListed(list) => {
                self.sessions = list;
                self.session_idx = 0;
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
        }
    }

    fn sync_system_prompt(&mut self, system_prompt: &str) {
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

    fn current_turn_config(&self) -> TurnConfig {
        let system_prompt = self.custom_system_prompt.clone().unwrap_or_else(|| {
            self.config
                .system_presets
                .get(self.sys_preset_idx)
                .map(|p| p.prompt.clone())
                .unwrap_or_default()
        });
        TurnConfig {
            system_prompt,
            num_ctx: self.num_ctx,
            enable_tools: self.enable_tools,
        }
    }

    pub(super) fn current_system_label(&self) -> &str {
        if self.custom_system_prompt.is_some() {
            "Custom (Session)"
        } else {
            preset_label(&self.config.system_presets, self.sys_preset_idx)
        }
    }

    pub(super) fn submit_prompt(&mut self) {
        if self.is_streaming {
            return;
        }
        let prompt = self.text_area.lines().join("\n").trim().to_string();
        if prompt.is_empty() {
            self.status_bar = ("Prompt cannot be empty.".into(), true);
            return;
        }

        self.text_area = create_text_area();
        self.scroll_to_bottom();
        let _ = self.cmd_tx.send(EngineCommand::SendPrompt {
            prompt,
            turn: self.current_turn_config(),
        });
    }

    pub(super) fn resolve_modal_tool(&mut self, approved: bool, always_approve: bool) {
        if self.pending_tool.take().is_some() {
            self.modal = ActiveModal::None;
            self.follow_tail = true;
            let _ = self.cmd_tx.send(EngineCommand::ResolveTool {
                approved,
                always_approve,
                feedback: std::mem::take(&mut self.tool_feedback),
                turn: self.current_turn_config(),
            });
        }
    }
}
