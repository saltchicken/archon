mod input;
mod modals;
mod render;

use crate::core::{
    config::{Config, Preset},
    session::{ChatMessage, SessionSummary},
    workspace::ImageAttachment,
    EngineCommand, EngineEvent,
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
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders},
    Terminal,
};
use ratatui_textarea::TextArea;
use std::io;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

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
}

pub struct TuiApp {
    pub(super) model: String,
    pub(super) config: Config,
    pub(super) cmd_tx: UnboundedSender<EngineCommand>,

    pub(super) attachments: Vec<ImageAttachment>,
    pub(super) messages: Vec<ChatMessage>,
    pub(super) wrapped_messages: Vec<Vec<String>>,
    pub(super) last_wrap_width: usize,
    pub(super) streaming_text: String,
    pub(super) is_streaming: bool,
    pub(super) status_bar: (String, bool),

    pub(super) preset_idx: usize,
    pub(super) sys_preset_idx: usize,
    pub(super) custom_system_prompt: Option<String>,
    pub(super) enable_tools: bool,
    pub(super) num_ctx: u32,
    pub(super) text_area: TextArea<'static>,

    pub(super) chat_scroll: u16,
    pub(super) follow_tail: bool,
    pub(super) tool_scroll: u16,
    pub(super) total_chat_lines: u16,

    pub(super) msg_scrolls: Vec<usize>,
    pub(super) msg_unfolded: Vec<bool>,

    pub(super) chat_area: Rect,
    pub(super) msg_visual_ranges: Vec<(usize, u16, u16, bool)>,

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
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Prompt ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    ta.set_cursor_line_style(Style::default());
    ta
}

pub(super) fn titled_block<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default().borders(Borders::ALL).title(title)
}

pub(super) fn preset_label(presets: &[Preset], idx: usize) -> &str {
    presets.get(idx).map(|p| p.name.as_str()).unwrap_or("None")
}

impl TuiApp {
    pub fn new(model: String, config: Config, cmd_tx: UnboundedSender<EngineCommand>) -> Self {
        let num_ctx = config.app.default_context;
        Self {
            model,
            config,
            cmd_tx,
            attachments: Vec::new(),
            messages: Vec::new(),
            wrapped_messages: Vec::new(),
            last_wrap_width: 0,
            streaming_text: String::new(),
            is_streaming: false,
            status_bar: (
                "Ready. Press Ctrl+⏎ to send prompt, ? to toggle help.".into(),
                false,
            ),
            preset_idx: 0,
            sys_preset_idx: 0,
            custom_system_prompt: None,
            enable_tools: true,
            num_ctx,
            text_area: create_text_area(),
            chat_scroll: 0,
            follow_tail: true,
            tool_scroll: 0,
            total_chat_lines: 0,
            msg_scrolls: Vec::new(),
            msg_unfolded: Vec::new(),
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

        loop {
            terminal.draw(|f| self.draw(f))?;

            tokio::select! {
                Some(Ok(event)) = term_events.next() => {
                    match event {
                        Event::Key(key) if self.handle_key(key) => break,
                        Event::Mouse(mouse) => self.handle_mouse(mouse),
                        Event::Paste(text) => {
                            let single_line = text.replace(['\r', '\n'], "");
                            match self.modal {
                                ActiveModal::AddFile => self.file_path_input.push_str(&single_line),
                                ActiveModal::ToolApproval => self.tool_feedback.push_str(&single_line),
                                ActiveModal::None => {
                                    self.text_area.insert_str(text.replace('\r', ""));
                                }
                                ActiveModal::Sessions => {}
                            }
                        }
                        _ => {}
                    }
                }
                Some(ev) = ev_rx.recv() => self.handle_engine_event(ev),
                else => break,
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
                self.messages = messages;
                self.wrapped_messages.clear();
                self.num_ctx = num_ctx;
                self.sync_system_prompt(&system_prompt);
                self.is_streaming = false;
                self.streaming_text.clear();
                self.msg_scrolls.clear();
                self.msg_unfolded.clear();
            }
            EngineEvent::StreamStarted => {
                self.is_streaming = true;
                self.streaming_text.clear();
                self.status_bar = (
                    "Streaming response from Ollama... (Esc to cancel)".into(),
                    false,
                );
                self.follow_tail = true;
                self.chat_scroll = u16::MAX;
            }
            EngineEvent::StreamChunk(c) => self.streaming_text.push_str(&c),
            EngineEvent::StreamDone { .. } | EngineEvent::StreamError(_) => {
                self.is_streaming = false;
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
        } else if system_prompt.is_empty() {
            if let Some(idx) = self
                .config
                .system_presets
                .iter()
                .position(|p| p.prompt.is_empty())
            {
                self.sys_preset_idx = idx;
            }
            self.custom_system_prompt = None;
        } else {
            self.custom_system_prompt = Some(system_prompt.to_string());
        }
    }

    pub(super) fn current_system_prompt(&self) -> String {
        if let Some(custom) = &self.custom_system_prompt {
            return custom.clone();
        }
        self.config
            .system_presets
            .get(self.sys_preset_idx)
            .map(|p| p.prompt.clone())
            .unwrap_or_default()
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
        self.follow_tail = true;
        self.chat_scroll = u16::MAX;
        let _ = self.cmd_tx.send(EngineCommand::SendPrompt {
            prompt,
            system_prompt: self.current_system_prompt(),
            num_ctx: self.num_ctx,
            enable_tools: self.enable_tools,
        });
    }

    pub(super) fn resolve_modal_tool(&mut self, approved: bool) {
        if self.pending_tool.take().is_some() {
            self.modal = ActiveModal::None;
            self.follow_tail = true;
            let _ = self.cmd_tx.send(EngineCommand::ResolveTool {
                approved,
                feedback: std::mem::take(&mut self.tool_feedback),
                system_prompt: self.current_system_prompt(),
                num_ctx: self.num_ctx,
                enable_tools: self.enable_tools,
            });
        }
    }
}
