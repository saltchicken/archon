mod commands;
mod events;
mod input;
mod markdown;
mod modals;
mod render;
pub mod theme;

use commands::SlashCommand;

use crate::{
    attachments::ImageAttachment,
    config::{Config, Memory, MemoryScope, Preset},
    engine::{EngineCommand, EngineEvent, TurnConfig},
    session::{ChatMessage, SessionSummary},
    tools::SessionToolState,
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
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Padding},
};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use std::{collections::BTreeMap, io, time::Duration};
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SessionView {
    #[default]
    Recent,
    Pinned,
}

impl SessionView {
    pub fn toggle(self) -> Self {
        match self {
            Self::Recent => Self::Pinned,
            Self::Pinned => Self::Recent,
        }
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
    pub(super) stream_cache: markdown::MarkdownStreamCache,
    pub(super) stream_thought_cache: markdown::MarkdownStreamCache,
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
    pub(super) used_tokens_estimated: bool,
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
    pub(super) sidebar_area: Rect,
    pub(super) sidebar_scroll: usize,
    pub(super) active_turn_skill: Option<String>,
    pub(super) session_tools: SessionToolState,
    pub(super) session_auto_approve_count: usize,
    pub(super) term_focused: bool,
    pub(super) cursor_blink_on: bool,

    pub(super) modal: ActiveModal,
    pub(super) context_menu: Option<ContextMenu>,
    pub(super) file_path_input: String,
    pub(super) sessions: Vec<SessionSummary>,
    pub(super) session_view: SessionView,
    pub(super) session_idx: usize,
    pub(super) sessions_modal_just_opened: bool,
    pub(super) session_id: String,
    pub(super) session_is_fork: bool,
    pub(super) session_is_pinned: bool,
    pub(super) pending_tool: Option<(String, usize, usize)>,
    pub(super) pending_truncated: Option<PendingTruncatedUi>,
    pub(super) tool_feedback: String,
    pub(super) truncated_guide_prompt: String,
}

fn create_editor_area(title: &'static str, initial: &str) -> TextArea<'static> {
    let mut ta = if initial.is_empty() {
        TextArea::default()
    } else {
        TextArea::from(initial.lines().map(String::from).collect::<Vec<_>>())
    };
    ta.set_block(titled_block(title).border_style(Style::default().fg(theme::BORDER_PROMPT)));
    ta.set_cursor_line_style(Style::default());
    ta.set_cursor_style(cursor_style(true));
    ta.set_wrap_mode(WrapMode::WordOrGlyph);
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
            stream_cache: markdown::MarkdownStreamCache::default(),
            stream_thought_cache: markdown::MarkdownStreamCache::default(),
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
            used_tokens_estimated: true,
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
            sidebar_area: Rect::default(),
            sidebar_scroll: 0,
            active_turn_skill: None,
            session_tools: SessionToolState::default(),
            session_auto_approve_count: 0,
            term_focused: true,
            cursor_blink_on: true,
            modal: ActiveModal::None,
            context_menu: None,
            file_path_input: String::new(),
            sessions: Vec::new(),
            session_view: SessionView::default(),
            session_idx: 0,
            sessions_modal_just_opened: false,
            session_id: String::new(),
            session_is_fork: false,
            session_is_pinned: false,
            pending_tool: None,
            pending_truncated: None,
            tool_feedback: String::new(),
            truncated_guide_prompt: String::new(),
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
                    ActiveModal::Sessions | ActiveModal::ModelSelect => {}
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
            raw_skill_args: None,
            skill_inputs: BTreeMap::new(),
        }
    }

    pub(super) fn current_system_label(&self) -> &str {
        if self.custom_system_prompt.is_some() {
            "Custom"
        } else {
            preset_label(&self.config.system_presets, self.sys_preset_idx)
        }
    }
}
