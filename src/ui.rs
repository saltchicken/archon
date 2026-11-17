mod input;
mod modals;
mod render;

use crate::core::{
    EngineCommand, EngineEvent, TruncatedToolAction, TurnConfig,
    attachments::ImageAttachment,
    config::{
        Config, Memory, MemoryScope, Preset, clean_memory_item, global_memory_path,
        load_scoped_memory_file,
    },
    session::{ChatMessage, SessionSummary},
};
use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, EventStream, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Padding},
};
use ratatui_textarea::{CursorMove, TextArea};
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
        DisableBracketedPaste,
        DisableFocusChange
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
    TruncatedTool,
    ModelSelect,
    SystemPrompt,
    Memory,
    PayloadInspector,
}

pub(super) struct PendingTruncatedUi {
    pub tool_name: String,
    pub summary: String,
    pub preview: String,
    pub raw_bytes: usize,
    pub max_size: usize,
    pub full_cap_bytes: usize,
    pub index: usize,
    pub total: usize,
    pub collapsed: bool,
    pub unfolded: bool,
    pub scroll: usize,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum SlashCommand {
    Exit,
    Fork,
    Inspect,
    Memory,
    Reset,
    Run,
}

impl SlashCommand {
    pub const ALL: &[(&str, Self)] = &[
        ("exit", Self::Exit),
        ("fork", Self::Fork),
        ("inspect", Self::Inspect),
        ("memory", Self::Memory),
        ("reset", Self::Reset),
        ("run", Self::Run),
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(cmd, _)| cmd.eq_ignore_ascii_case(name))
            .map(|(_, variant)| *variant)
    }

    pub fn matching_triggers(prefix: &str) -> Vec<String> {
        let lower = prefix.to_ascii_lowercase();
        Self::ALL
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(&lower))
            .map(|(cmd, _)| format!("/{cmd}"))
            .collect()
    }
}

#[derive(Default, Clone, Copy)]
pub(super) struct MsgUiState {
    pub scroll: usize,
    pub unfolded: bool,
    pub collapsed: bool,
}

pub(super) struct CodeCopyButton {
    pub msg_idx: usize,
    pub content_idx: usize,
    pub visual_y: usize,
    pub x_range: std::ops::Range<usize>,
    pub code: String,
}

pub(super) struct ForkButton {
    pub msg_idx: usize,
    pub visual_y: usize,
    pub x_range: std::ops::Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContextMenuAction {
    CopyToClipboard,
    ForkConversation(usize),
}

impl ContextMenuAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::CopyToClipboard => "Copy to clipboard",
            Self::ForkConversation(_) => "Fork conversation from here",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ContextMenu {
    pub x: u16,
    pub y: u16,
    pub selected: usize,
    pub actions: Vec<ContextMenuAction>,
    pub target_text: String,
}

impl ContextMenu {
    pub fn rect(&self, screen: Rect) -> Rect {
        let max_label = self
            .actions
            .iter()
            .map(|a| a.label().len())
            .max()
            .unwrap_or(17);
        let width = ((max_label + 7) as u16).min(screen.width.max(1));
        let height = (self.actions.len() as u16 + 2).min(screen.height.max(1));
        let max_x = screen.x + screen.width.saturating_sub(width);
        let max_y = screen.y + screen.height.saturating_sub(height);
        let x = self.x.clamp(screen.x, max_x);
        let y = self.y.clamp(screen.y, max_y);
        Rect::new(x, y, width, height)
    }

    pub fn item_at(&self, screen: Rect, col: u16, row: u16) -> Option<usize> {
        let area = self.rect(screen);
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        if !inner.contains(Position::new(col, row)) {
            return None;
        }
        let idx = usize::from(row - inner.y);
        (idx < self.actions.len()).then_some(idx)
    }
}

pub struct TuiApp {
    pub(super) model: String,
    pub(super) config: Config,
    pub(super) cmd_tx: UnboundedSender<EngineCommand>,

    pub(super) attachments: Vec<ImageAttachment>,
    pub(super) messages: Vec<ChatMessage>,
    pub(super) queued_messages: Vec<ChatMessage>,
    pub(super) styled_messages: Vec<Vec<Line<'static>>>,
    pub(super) styled_stream: Vec<Line<'static>>,
    pub(super) styled_stream_thought: Vec<Line<'static>>,
    pub(super) stream_dirty: bool,
    pub(super) stream_thought_dirty: bool,
    pub(super) last_wrap_width: usize,
    pub(super) streaming_text: String,
    pub(super) streaming_thought: String,
    pub(super) is_streaming: bool,
    pub(super) active_tool_status: Option<String>,

    pub(super) model_idx: usize,
    pub(super) sys_preset_idx: usize,
    pub(super) custom_system_prompt: Option<String>,
    pub(super) sys_prompt_area: TextArea<'static>,
    pub(super) memory_scope: MemoryScope,
    pub(super) memory_area: TextArea<'static>,
    pub(super) memory_enabled_draft: bool,
    pub(super) global_memory_draft: Memory,
    pub(super) workspace_memory_draft: Memory,
    pub(super) enable_tools: bool,
    pub(super) num_ctx: u32,
    pub(super) max_tool_output_size: usize,
    pub(super) used_tokens: u64,
    pub(super) text_area: TextArea<'static>,
    pub(super) prompt_history: Vec<String>,
    pub(super) prompt_history_idx: Option<usize>,

    pub(super) chat_scroll: usize,
    pub(super) follow_tail: bool,
    pub(super) tool_scroll: u16,
    pub(super) total_chat_lines: usize,

    pub(super) msg_states: Vec<MsgUiState>,

    pub(super) term_area: Rect,
    pub(super) chat_area: Rect,
    pub(super) prompt_area: Rect,
    pub(super) msg_visual_ranges: Vec<(usize, usize, usize, bool)>,
    pub(super) code_copy_buttons: Vec<CodeCopyButton>,
    pub(super) copied_code_flash: Option<((usize, usize), std::time::Instant)>,
    pub(super) fork_buttons: Vec<ForkButton>,
    pub(super) fork_flash: Option<(usize, std::time::Instant)>,
    pub(super) last_mouse_pos: Option<Position>,
    pub(super) hovered_header_msg: Option<usize>,

    pub(super) sidebar_expanded: bool,
    pub(super) term_focused: bool,
    pub(super) cursor_blink_on: bool,

    pub(super) modal: ActiveModal,
    pub(super) context_menu: Option<ContextMenu>,
    pub(super) file_path_input: String,
    pub(super) sessions: Vec<SessionSummary>,
    pub(super) session_idx: usize,
    pub(super) session_id: String,
    pub(super) session_is_fork: bool,
    pub(super) pending_tool: Option<(String, usize, usize)>,
    pub(super) pending_truncated: Option<PendingTruncatedUi>,
    pub(super) tool_feedback: String,
    pub(super) truncated_guide_prompt: String,
    pub(super) last_payload_preview: String,
    pub(super) payload_scroll: u16,
}

fn create_editor_area(title: &'static str, initial: &str) -> TextArea<'static> {
    let mut ta = if initial.is_empty() {
        TextArea::default()
    } else {
        TextArea::from(initial.lines().map(String::from).collect::<Vec<_>>())
    };
    ta.set_block(titled_block(title).border_style(Style::default().fg(Color::Cyan)));
    ta.set_cursor_line_style(Style::default());
    ta.set_cursor_style(cursor_style(true));
    ta
}

pub(super) fn cursor_style(visible: bool) -> Style {
    if visible {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    }
}

pub(super) fn create_text_area() -> TextArea<'static> {
    create_editor_area(" Prompt ", "")
}

pub(super) fn create_text_area_with(initial: &str) -> TextArea<'static> {
    let mut ta = create_editor_area(" Prompt ", initial);
    if !initial.is_empty() {
        ta.move_cursor(CursorMove::Bottom);
        ta.move_cursor(CursorMove::End);
    }
    ta
}

pub(super) fn create_sys_prompt_area(initial: &str) -> TextArea<'static> {
    create_editor_area(" Edit System Message ", initial)
}

pub(super) fn create_memory_area(scope: MemoryScope, initial: &str) -> TextArea<'static> {
    let title = match scope {
        MemoryScope::Global => " Global Persistent Facts / Environment Context (One per line) ",
        MemoryScope::Workspace => " Workspace Persistent Facts / Project Context (One per line) ",
    };
    create_editor_area(title, initial)
}

pub(super) fn titled_block<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(title)
}

pub(super) fn chat_block<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    titled_block(title).padding(Padding::symmetric(2, 1))
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
        let memory_scope = MemoryScope::Global;
        let memory_initial = config.global_memory.to_editor_text();
        let memory_enabled_draft = config.global_memory.enabled;
        let global_memory_draft = config.global_memory.clone();
        let workspace_memory_draft = config.workspace_memory.clone();

        Self {
            model,
            config,
            cmd_tx,
            attachments: Vec::new(),
            messages: Vec::new(),
            queued_messages: Vec::new(),
            styled_messages: Vec::new(),
            styled_stream: Vec::new(),
            styled_stream_thought: Vec::new(),
            stream_dirty: false,
            stream_thought_dirty: false,
            last_wrap_width: 0,
            streaming_text: String::new(),
            streaming_thought: String::new(),
            is_streaming: false,
            active_tool_status: None,
            model_idx,
            sys_preset_idx: 0,
            custom_system_prompt: None,
            sys_prompt_area: create_sys_prompt_area(""),
            memory_scope,
            memory_area: create_memory_area(memory_scope, &memory_initial),
            memory_enabled_draft,
            global_memory_draft,
            workspace_memory_draft,
            enable_tools: true,
            num_ctx,
            max_tool_output_size,
            used_tokens: 0,
            text_area: create_text_area(),
            prompt_history: Vec::new(),
            prompt_history_idx: None,
            chat_scroll: 0,
            follow_tail: true,
            tool_scroll: 0,
            total_chat_lines: 0,
            msg_states: Vec::new(),
            term_area: Rect::default(),
            chat_area: Rect::default(),
            prompt_area: Rect::default(),
            msg_visual_ranges: Vec::new(),
            code_copy_buttons: Vec::new(),
            copied_code_flash: None,
            fork_buttons: Vec::new(),
            fork_flash: None,
            last_mouse_pos: None,
            hovered_header_msg: None,
            sidebar_expanded: false,
            term_focused: true,
            cursor_blink_on: true,
            modal: ActiveModal::None,
            context_menu: None,
            file_path_input: String::new(),
            sessions: Vec::new(),
            session_idx: 0,
            session_id: String::new(),
            session_is_fork: false,
            pending_tool: None,
            pending_truncated: None,
            tool_feedback: String::new(),
            truncated_guide_prompt: String::new(),
            last_payload_preview: String::new(),
            payload_scroll: 0,
        }
    }

    pub(super) fn chat_area_block<'a>(&self, title: impl Into<Line<'a>>) -> Block<'a> {
        titled_block(title).padding(Padding::new(
            self.config.ui.chat_padding_left,
            self.config.ui.chat_padding_right,
            1,
            1,
        ))
    }

    pub(super) fn chat_inner_area(&self) -> Rect {
        self.chat_area_block("").inner(self.chat_area)
    }

    pub(super) fn chat_view_height(&self) -> usize {
        usize::from(self.chat_inner_area().height)
    }

    pub(super) fn max_chat_scroll(&self) -> usize {
        self.total_chat_lines
            .saturating_sub(self.chat_view_height())
    }

    pub(super) fn scroll_to_bottom(&mut self) {
        self.follow_tail = true;
        self.chat_scroll = usize::MAX;
    }

    pub(super) fn fork_from_message(&mut self, msg_idx: usize) {
        if !self
            .messages
            .get(msg_idx)
            .is_some_and(ChatMessage::is_forkable)
        {
            return;
        }
        self.fork_flash = Some((
            msg_idx,
            std::time::Instant::now() + Duration::from_millis(1500),
        ));
        let _ = self.cmd_tx.send(EngineCommand::ForkSession(msg_idx));
    }

    fn handle_term_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) => {
                self.cursor_blink_on = true;
                return self.handle_key(key);
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => {
                self.cursor_blink_on = true;
                let single_line = text.replace(['\r', '\n'], "");
                match self.modal {
                    ActiveModal::AddFile => self.file_path_input.push_str(&single_line),
                    ActiveModal::ToolApproval => self.tool_feedback.push_str(&single_line),
                    ActiveModal::TruncatedTool => {
                        self.truncated_guide_prompt.push_str(&single_line);
                    }
                    ActiveModal::SystemPrompt => {
                        self.sys_prompt_area.insert_str(text.replace('\r', ""));
                    }
                    ActiveModal::Memory => {
                        self.memory_area.insert_str(text.replace('\r', ""));
                    }
                    ActiveModal::None => {
                        self.prompt_history_idx = None;
                        self.text_area.insert_str(text.replace('\r', ""));
                    }
                    ActiveModal::Sessions
                    | ActiveModal::ModelSelect
                    | ActiveModal::PayloadInspector => {}
                }
            }
            Event::FocusGained => {
                self.term_focused = true;
                self.cursor_blink_on = true;
            }
            Event::FocusLost => {
                self.term_focused = false;
                self.cursor_blink_on = false;
                self.last_mouse_pos = None;
                self.hovered_header_msg = None;
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
            EnableBracketedPaste,
            EnableFocusChange
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

        // 800ms cursor blink ticker so the cursor blinks reliably across all terminal emulators
        let mut blink_tick = tokio::time::interval(Duration::from_millis(800));
        blink_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut dirty = true;

        'main_loop: loop {
            if dirty {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
            }

            tokio::select! {
                _ = render_tick.tick() => {}
                _ = blink_tick.tick() => {
                    if self.term_focused {
                        self.cursor_blink_on = !self.cursor_blink_on;
                        dirty = true;
                    }
                    if self
                        .copied_code_flash
                        .is_some_and(|(_, exp)| std::time::Instant::now() >= exp)
                    {
                        self.copied_code_flash = None;
                        dirty = true;
                    }
                    if self
                        .fork_flash
                        .is_some_and(|(_, exp)| std::time::Instant::now() >= exp)
                    {
                        self.fork_flash = None;
                        dirty = true;
                    }
                }
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
                            if self.cursor_blink_on {
                                blink_tick.reset();
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
            EngineEvent::AttachmentsUpdated(list) => self.attachments = list,
            EngineEvent::ChatUpdated {
                session_id,
                is_fork,
                messages,
                queued_messages,
                num_ctx,
                system_prompt,
            } => {
                if messages.is_empty() {
                    self.used_tokens = 0;
                }
                if self.prompt_history.is_empty() {
                    for m in &messages {
                        let trimmed = m.content.trim();
                        if m.role == "user"
                            && !trimmed.is_empty()
                            && self
                                .prompt_history
                                .last()
                                .is_none_or(|last| last != trimmed)
                        {
                            self.prompt_history.push(trimmed.to_string());
                        }
                    }
                }
                let session_changed = self.session_id != session_id;
                self.session_id = session_id;
                self.session_is_fork = is_fork;

                let is_reset_or_reload = session_changed
                    || messages.len() < self.messages.len()
                    || self
                        .messages
                        .iter()
                        .zip(messages.iter())
                        .any(|(old, new)| old != new);

                let prev_msg_count = if is_reset_or_reload {
                    self.msg_states.clear();
                    self.styled_messages.clear();
                    self.copied_code_flash = None;
                    if self.fork_flash.is_some_and(|(idx, exp)| {
                        idx >= messages.len() || std::time::Instant::now() >= exp
                    }) {
                        self.fork_flash = None;
                    }
                    0
                } else {
                    self.msg_states.truncate(self.messages.len());
                    self.styled_messages.truncate(self.messages.len());
                    self.messages.len()
                };

                self.messages = messages;
                self.queued_messages = queued_messages;
                self.finish_stream();
                self.streaming_text.clear();
                self.streaming_thought.clear();
                self.num_ctx = num_ctx;
                self.sync_system_prompt(&system_prompt);
                let total_slots = self.messages.len() + 1 + self.queued_messages.len();
                self.msg_states.resize(total_slots, MsgUiState::default());

                if is_reset_or_reload {
                    for (i, m) in self.messages.iter().enumerate() {
                        if matches!(m.role.as_str(), "tool" | "hook") {
                            self.msg_states[i].collapsed = true;
                        } else if m.role == "error" {
                            self.msg_states[i].unfolded = true;
                        }
                    }
                    if let Some(last_idx) = self
                        .messages
                        .iter()
                        .rposition(|m| matches!(m.role.as_str(), "assistant" | "local" | "error"))
                    {
                        self.msg_states[last_idx].unfolded = true;
                    }
                    self.scroll_to_bottom();
                } else {
                    for (i, m) in self.messages.iter().enumerate().skip(prev_msg_count) {
                        if matches!(m.role.as_str(), "tool" | "hook") {
                            self.msg_states[i].collapsed = true;
                        } else if matches!(m.role.as_str(), "assistant" | "local" | "error") {
                            self.msg_states[i].unfolded = true;
                        }
                    }
                }
                if self.messages.last().is_some_and(|m| m.role == "error") {
                    self.scroll_to_bottom();
                }
            }
            EngineEvent::QueuedInterjectionsUpdated(queued) => {
                self.queued_messages = queued;
                self.scroll_to_bottom();
            }
            EngineEvent::StreamStarted => {
                self.is_streaming = true;
                self.active_tool_status = None;
                self.streaming_text.clear();
                self.streaming_thought.clear();
                self.styled_stream.clear();
                self.styled_stream_thought.clear();
                self.stream_dirty = true;
                self.stream_thought_dirty = true;
                self.scroll_to_bottom();
            }
            EngineEvent::StreamThoughtChunk(c) => {
                if self.streaming_text.is_empty() {
                    self.streaming_thought.push_str(&c);
                    self.stream_thought_dirty = true;
                }
            }
            EngineEvent::StreamChunk(c) => {
                if self.streaming_text.is_empty() && !c.is_empty() {
                    self.streaming_thought.clear();
                    self.styled_stream_thought.clear();
                    self.stream_thought_dirty = false;
                }
                self.streaming_text.push_str(&c);
                self.stream_dirty = true;
            }
            EngineEvent::StreamDone {
                prompt_tokens,
                eval_tokens,
                ..
            } => {
                self.finish_stream();
                self.used_tokens = prompt_tokens + eval_tokens;
            }
            EngineEvent::StreamError(err) => {
                let partial = std::mem::take(&mut self.streaming_text);
                let partial_thought = std::mem::take(&mut self.streaming_thought);
                self.finish_stream();
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
                            vec![crate::core::session::ThoughtPart {
                                text: Some(partial_thought),
                                thought_signature: None,
                            }]
                        }),
                        ..ChatMessage::new("assistant", final_content)
                    });
                }
                self.push_error_message(err);
            }
            EngineEvent::Error(err) => {
                self.push_error_message(err);
            }
            EngineEvent::ToolStarted {
                summary,
                index,
                total,
            } => {
                let progress = if total > 1 {
                    format!(" ({index}/{total})")
                } else {
                    String::new()
                };
                self.active_tool_status = Some(format!("⚙ {summary}{progress} ..."));
                self.scroll_to_bottom();
            }
            EngineEvent::ToolCompleted { .. }
            | EngineEvent::ShellCommandCompleted { .. }
            | EngineEvent::HookCompleted { .. }
            | EngineEvent::HooksFinished => {
                self.active_tool_status = None;
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
            EngineEvent::TruncatedToolResolutionRequested {
                tool_name,
                summary,
                preview,
                raw_bytes,
                max_size,
                full_cap_bytes,
                index,
                total,
            } => {
                self.active_tool_status = None;
                self.pending_truncated = Some(PendingTruncatedUi {
                    tool_name,
                    summary,
                    preview,
                    raw_bytes,
                    max_size,
                    full_cap_bytes,
                    index,
                    total,
                    collapsed: true,
                    unfolded: false,
                    scroll: 0,
                });
                self.truncated_guide_prompt.clear();
                self.tool_scroll = 0;
                self.modal = ActiveModal::TruncatedTool;
            }
            EngineEvent::ToolCondensed(_) => {}
            EngineEvent::SessionsListed(list) => {
                self.session_idx = list
                    .iter()
                    .position(|s| !self.session_id.is_empty() && s.id == self.session_id)
                    .unwrap_or(0);
                self.sessions = list;
            }
            EngineEvent::SkillsUpdated(skills) => {
                self.config.skills = skills;
            }
            EngineEvent::MemoryUpdated {
                global_memory,
                workspace_memory,
            } => {
                self.config.global_memory = global_memory;
                self.config.workspace_memory = workspace_memory;
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
            EngineEvent::PayloadCaptured(preview) => {
                self.last_payload_preview = preview;
            }
            EngineEvent::PayloadInspectorOpened(preview) => {
                self.last_payload_preview = preview;
                self.payload_scroll = 0;
                self.modal = ActiveModal::PayloadInspector;
            }
        }
    }

    fn finish_stream(&mut self) {
        self.is_streaming = false;
        self.active_tool_status = None;
        self.styled_stream.clear();
        self.styled_stream_thought.clear();
        self.stream_dirty = false;
        self.stream_thought_dirty = false;
    }

    fn push_error_message(&mut self, err: String) {
        self.messages.push(ChatMessage::new("error", err));
        let total_slots = self.messages.len() + 1 + self.queued_messages.len();
        self.msg_states.resize(total_slots, MsgUiState::default());
        if let Some(last_idx) = self.messages.len().checked_sub(1) {
            self.msg_states[last_idx].unfolded = true;
        }
        self.scroll_to_bottom();
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

    pub(super) fn current_system_prompt_text(&self) -> String {
        self.custom_system_prompt.clone().unwrap_or_else(|| {
            self.config
                .system_presets
                .get(self.sys_preset_idx)
                .map(|p| p.prompt.clone())
                .unwrap_or_default()
        })
    }

    pub(super) fn current_turn_config(&self) -> TurnConfig {
        TurnConfig {
            system_prompt: self.current_system_prompt_text(),
            num_ctx: self.num_ctx,
            enable_tools: self.enable_tools,
            active_skill: None,
            skill_args: None,
        }
    }

    pub(super) fn current_system_label(&self) -> &str {
        if self.custom_system_prompt.is_some() {
            "Custom"
        } else {
            preset_label(&self.config.system_presets, self.sys_preset_idx)
        }
    }

    pub(super) fn current_memory_label(&self) -> String {
        let effective = self.config.effective_memory();
        if !effective.enabled {
            let stored = self.config.global_memory.count() + self.config.workspace_memory.count();
            format!("OFF ({stored})")
        } else {
            let count = effective.count();
            if count == 0 {
                "Empty".into()
            } else if count == 1 {
                "1 item (ON)".into()
            } else {
                format!("{count} items (ON)")
            }
        }
    }

    pub(super) fn commit_active_memory_scope_draft(&mut self) {
        let raw_text = self.memory_area.lines().join("\n");
        let mem = Memory::from_editor_text(&raw_text, self.memory_enabled_draft);
        match self.memory_scope {
            MemoryScope::Global => self.global_memory_draft = mem,
            MemoryScope::Workspace => self.workspace_memory_draft = mem,
        }
    }

    pub(super) fn load_active_memory_scope_draft(&mut self) {
        let draft = match self.memory_scope {
            MemoryScope::Global => &self.global_memory_draft,
            MemoryScope::Workspace => &self.workspace_memory_draft,
        };
        self.memory_area = create_memory_area(self.memory_scope, &draft.to_editor_text());
        self.memory_enabled_draft = draft.enabled;
    }

    pub(super) fn toggle_memory_scope(&mut self) {
        self.commit_active_memory_scope_draft();
        self.memory_scope = self.memory_scope.toggle();
        self.load_active_memory_scope_draft();
    }

    pub(super) fn save_memory_modal(&mut self) {
        self.commit_active_memory_scope_draft();
        self.modal = ActiveModal::None;

        let active_scope = self.memory_scope;
        let inactive_scope = active_scope.toggle();

        let inactive_draft = match inactive_scope {
            MemoryScope::Global => self.global_memory_draft.clone(),
            MemoryScope::Workspace => self.workspace_memory_draft.clone(),
        };
        if &inactive_draft != self.config.scoped_memory(inactive_scope) {
            self.config
                .set_scoped_memory(inactive_scope, inactive_draft.clone());
            let _ = self.cmd_tx.send(EngineCommand::SetMemory {
                scope: inactive_scope,
                memory: inactive_draft,
            });
        }

        let active_draft = match active_scope {
            MemoryScope::Global => self.global_memory_draft.clone(),
            MemoryScope::Workspace => self.workspace_memory_draft.clone(),
        };
        self.config
            .set_scoped_memory(active_scope, active_draft.clone());
        let _ = self.cmd_tx.send(EngineCommand::SetMemory {
            scope: active_scope,
            memory: active_draft,
        });
    }

    pub(super) fn open_memory_modal(&mut self) {
        let global_mem = load_scoped_memory_file(&global_memory_path(), true)
            .map(|(_, m)| m)
            .unwrap_or_else(|_| self.config.global_memory.clone());
        let ws_mem = load_scoped_memory_file(&self.config.workspace_memory_path(), false)
            .map(|(_, m)| m)
            .unwrap_or_else(|_| self.config.workspace_memory.clone());

        self.config.global_memory = global_mem.clone();
        self.config.workspace_memory = ws_mem.clone();
        self.global_memory_draft = global_mem;
        self.workspace_memory_draft = ws_mem;
        self.load_active_memory_scope_draft();
        self.modal = ActiveModal::Memory;
    }

    pub(super) fn request_payload_inspector(&mut self) {
        let draft_prompt = self.text_area.lines().join("\n");
        let _ = self.cmd_tx.send(EngineCommand::InspectPayload {
            draft_prompt,
            turn: self.current_turn_config(),
        });
    }

    pub(super) fn is_prompt_empty(&self) -> bool {
        self.text_area.lines().iter().all(String::is_empty)
    }

    fn record_prompt_and_clear(&mut self, raw_prompt: &str) {
        self.prompt_history_idx = None;
        if !raw_prompt.is_empty()
            && self
                .prompt_history
                .last()
                .is_none_or(|last| last != raw_prompt)
        {
            self.prompt_history.push(raw_prompt.to_string());
        }
        self.text_area = create_text_area();
    }

    fn dispatch_shell_command(
        &mut self,
        raw_prompt: &str,
        command: &str,
        include_in_context: bool,
    ) {
        if command.is_empty() {
            return;
        }
        self.record_prompt_and_clear(raw_prompt);
        self.scroll_to_bottom();
        let _ = self.cmd_tx.send(EngineCommand::RunShellCommand {
            command: command.to_string(),
            include_in_context,
        });
    }

    pub(super) fn submit_prompt(&mut self) -> bool {
        let raw_prompt = self.text_area.lines().join("\n").trim().to_string();
        if raw_prompt.is_empty() {
            return false;
        }

        if let Some(rest) = raw_prompt.strip_prefix('!') {
            let (include_in_context, shell_cmd) = match rest.strip_prefix('!') {
                Some(ctx_cmd) => (true, ctx_cmd.trim()),
                None => (false, rest.trim()),
            };
            self.dispatch_shell_command(&raw_prompt, shell_cmd, include_in_context);
            return false;
        }

        if let Some(rest) = raw_prompt.strip_prefix('/') {
            let (cmd_token, args) = match rest.find(char::is_whitespace) {
                Some(idx) => (&rest[..idx], rest[idx..].trim()),
                None => (rest, ""),
            };
            match SlashCommand::from_name(cmd_token) {
                Some(SlashCommand::Run) => {
                    let (include_in_context, cmd_args) = if let Some(rest_args) = args
                        .strip_prefix("-c ")
                        .or_else(|| args.strip_prefix("--context "))
                    {
                        (true, rest_args.trim())
                    } else {
                        (false, args)
                    };
                    self.dispatch_shell_command(&raw_prompt, cmd_args, include_in_context);
                    return false;
                }
                Some(SlashCommand::Memory) => {
                    self.record_prompt_and_clear(&raw_prompt);
                    if args.is_empty() {
                        self.open_memory_modal();
                        return false;
                    }
                    let lower_args = args.to_ascii_lowercase();
                    if matches!(lower_args.as_str(), "global") {
                        self.memory_scope = MemoryScope::Global;
                        self.open_memory_modal();
                        return false;
                    }
                    if matches!(lower_args.as_str(), "workspace" | "ws") {
                        self.memory_scope = MemoryScope::Workspace;
                        self.open_memory_modal();
                        return false;
                    }

                    let (scope, sub_args) = if let Some(rest_args) = args
                        .strip_prefix("workspace ")
                        .or_else(|| args.strip_prefix("ws "))
                    {
                        (MemoryScope::Workspace, rest_args.trim())
                    } else if let Some(rest_args) = args.strip_prefix("global ") {
                        (MemoryScope::Global, rest_args.trim())
                    } else {
                        (MemoryScope::Global, args)
                    };

                    let (mem_path, is_global) = match scope {
                        MemoryScope::Global => (global_memory_path(), true),
                        MemoryScope::Workspace => (self.config.workspace_memory_path(), false),
                    };
                    let mut mem = load_scoped_memory_file(&mem_path, is_global)
                        .map(|(_, m)| m)
                        .unwrap_or_else(|_| self.config.scoped_memory(scope).clone());
                    match sub_args.to_ascii_lowercase().as_str() {
                        "clear" => {
                            mem.items.clear();
                        }
                        "on" | "enable" => {
                            mem.enabled = true;
                        }
                        "off" | "disable" => {
                            mem.enabled = false;
                        }
                        _ => {
                            let fact = clean_memory_item(
                                sub_args.strip_prefix("add ").unwrap_or(sub_args),
                            );
                            if !fact.is_empty() {
                                let mut items = mem.items;
                                items.push(fact.to_string());
                                mem = Memory::new(items, true);
                            }
                        }
                    }
                    self.config.set_scoped_memory(scope, mem.clone());
                    let _ = self
                        .cmd_tx
                        .send(EngineCommand::SetMemory { scope, memory: mem });
                    return false;
                }
                Some(_) if !args.is_empty() => return false,
                Some(SlashCommand::Exit) => return true,
                Some(SlashCommand::Fork) => {
                    self.record_prompt_and_clear(&raw_prompt);
                    if let Some(last_assistant_idx) =
                        self.messages.iter().rposition(ChatMessage::is_forkable)
                    {
                        self.fork_from_message(last_assistant_idx);
                    } else {
                        self.push_error_message(
                            "No assistant response available to fork from.".into(),
                        );
                    }
                    return false;
                }
                Some(SlashCommand::Inspect) => {
                    self.record_prompt_and_clear(&raw_prompt);
                    self.request_payload_inspector();
                    return false;
                }
                Some(SlashCommand::Reset) => {
                    self.record_prompt_and_clear(&raw_prompt);
                    self.scroll_to_bottom();
                    let _ = self.cmd_tx.send(EngineCommand::ResetChat);
                    return false;
                }
                None => return false,
            }
        }

        let mut turn = self.current_turn_config();
        let prompt = if let Some((cmd_token, args_str)) =
            crate::core::skills::parse_at_command(&raw_prompt)
        {
            self.config.skills = crate::core::skills::discover_skills(
                &self.config.app.skills_dir,
                self.config.workspace_skills_dir.as_deref(),
            );
            match crate::core::skills::check_skill_activation(
                cmd_token,
                self.enable_tools,
                &self.config,
            ) {
                Ok(skill_name) => {
                    turn.active_skill = Some(skill_name);
                    turn.skill_args = (!args_str.is_empty()).then(|| args_str.to_string());
                    if args_str.is_empty() {
                        raw_prompt.trim().to_string()
                    } else {
                        args_str.to_string()
                    }
                }
                Err(_) => {
                    turn.active_skill = Some(cmd_token.to_string());
                    let _ = self.cmd_tx.send(EngineCommand::SendPrompt {
                        prompt: raw_prompt,
                        turn,
                    });
                    return false;
                }
            }
        } else {
            raw_prompt.clone()
        };

        self.record_prompt_and_clear(&raw_prompt);
        self.scroll_to_bottom();
        let _ = self.cmd_tx.send(EngineCommand::SendPrompt { prompt, turn });
        false
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

    pub(super) fn resolve_truncated_modal(&mut self, action: TruncatedToolAction) {
        if self.pending_truncated.take().is_some() {
            self.modal = ActiveModal::None;
            self.follow_tail = true;
            let _ = self.cmd_tx.send(EngineCommand::ResolveTruncatedTool {
                action,
                guide_prompt: std::mem::take(&mut self.truncated_guide_prompt),
                turn: self.current_turn_config(),
            });
        }
    }
}
