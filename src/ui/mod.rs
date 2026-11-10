use crate::core::{
    config::Config,
    session::{ChatMessage, SessionData},
    workspace::ImageAttachment,
    EngineCommand, EngineEvent,
};
use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyModifiers,
        KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame, Terminal,
};
use std::{io, path::PathBuf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const DEFAULT_MAX_MSG_LINES: usize = 15;

pub fn restore_terminal<W: io::Write>(out: &mut W, keyboard_enhanced: bool) {
    let _ = disable_raw_mode();
    let _ = execute!(out, LeaveAlternateScreen, DisableMouseCapture);
    if keyboard_enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
}

#[derive(PartialEq, Clone, Copy)]
enum ActiveModal {
    None,
    AddFile,
    Sessions,
    ToolApproval,
}

pub struct TuiApp {
    model: String,
    config: Config,
    cmd_tx: UnboundedSender<EngineCommand>,

    attachments: Vec<ImageAttachment>,
    messages: Vec<ChatMessage>,
    streaming_text: String,
    is_streaming: bool,
    status_bar: (String, bool),

    preset_idx: usize,
    sys_preset_idx: usize,
    enable_tools: bool,
    num_ctx: u32,
    input_buffer: String,
    input_cursor: usize,

    chat_scroll: u16,
    tool_scroll: u16,
    total_chat_lines: u16,

    msg_scrolls: Vec<usize>,
    msg_unfolded: Vec<bool>,
    cached_wrap_width: usize,
    msg_wrap_cache: Vec<Vec<String>>,

    chat_area: Rect,
    msg_visual_ranges: Vec<(usize, u16, u16, bool)>,

    modal: ActiveModal,
    file_path_input: String,
    sessions: Vec<SessionData>,
    session_idx: usize,
    pending_tool: Option<(String, usize, usize)>,
    tool_feedback: String,
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
            streaming_text: String::new(),
            is_streaming: false,
            status_bar: ("Ready. Press Ctrl+⏎ to send prompt.".into(), false),
            preset_idx: 0,
            sys_preset_idx: 0,
            enable_tools: true,
            num_ctx,
            input_buffer: String::new(),
            input_cursor: 0,
            chat_scroll: 0,
            tool_scroll: 0,
            total_chat_lines: 0,
            msg_scrolls: Vec::new(),
            msg_unfolded: Vec::new(),
            cached_wrap_width: 0,
            msg_wrap_cache: Vec::new(),
            chat_area: Rect::default(),
            msg_visual_ranges: Vec::new(),
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
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        if keyboard_enhanced {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let mut term_events = EventStream::new();

        loop {
            terminal.draw(|f| self.draw(f))?;

            tokio::select! {
                Some(Ok(event)) = term_events.next() => {
                    match event {
                        Event::Key(key) if self.handle_key(key) => break,
                        Event::Mouse(mouse) => self.handle_mouse(mouse),
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
            EngineEvent::ChatUpdated { messages } => {
                self.messages = messages;
                self.is_streaming = false;
                self.streaming_text.clear();
                self.msg_scrolls.clear();
                self.msg_unfolded.clear();
                self.msg_wrap_cache.clear();
            }
            EngineEvent::StreamStarted => {
                self.is_streaming = true;
                self.streaming_text.clear();
                self.status_bar = (
                    "Streaming response from Ollama... (Esc to cancel)".into(),
                    false,
                );
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

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);

        if self.modal == ActiveModal::ToolApproval {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.tool_scroll = self.tool_scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => self.tool_scroll = self.tool_scroll.saturating_add(3),
                _ => {}
            }
            return;
        }

        if !self.chat_area.contains(Position::new(x, y)) {
            return;
        }

        let is_on_scrollbar = x >= self.chat_area.x + self.chat_area.width.saturating_sub(2);
        let inner_y = y.saturating_sub(self.chat_area.y + 1);
        let visual_y = inner_y as usize + self.chat_scroll as usize;

        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let is_up = mouse.kind == MouseEventKind::ScrollUp;
                let step = |val: u16| {
                    if is_up {
                        val.saturating_sub(3)
                    } else {
                        val.saturating_add(3)
                    }
                };

                if !is_on_scrollbar {
                    if let Some((i, _, _, true)) =
                        self.msg_visual_ranges.iter().find(|(_, start, end, _)| {
                            visual_y >= *start as usize && visual_y < *end as usize
                        })
                    {
                        let cur = self.msg_scrolls.get(*i).copied().unwrap_or(0);
                        self.msg_scrolls[*i] = if is_up {
                            cur.saturating_sub(3)
                        } else {
                            cur.saturating_add(3)
                        };
                        return;
                    }
                }
                self.chat_scroll = step(self.chat_scroll);
            }
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
                if is_on_scrollbar =>
            {
                let view_height = self.chat_area.height.saturating_sub(2);
                let max_scroll = self.total_chat_lines.saturating_sub(view_height);
                if max_scroll > 0 && view_height > 1 {
                    let track_height = view_height.saturating_sub(1);
                    let fraction = inner_y.min(track_height) as f32 / track_height as f32;
                    self.chat_scroll = (fraction * max_scroll as f32).round() as u16;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((i, _, _, _)) =
                    self.msg_visual_ranges.iter().find(|(_, start, end, _)| {
                        visual_y >= *start as usize && visual_y < *end as usize
                    })
                {
                    if let Some(unfolded) = self.msg_unfolded.get_mut(*i) {
                        *unfolded = !*unfolded;
                    }
                }
            }
            _ => {}
        }
    }

    fn prev_char_boundary(&self) -> Option<usize> {
        self.input_buffer[..self.input_cursor]
            .char_indices()
            .next_back()
            .map(|(idx, _)| idx)
    }

    fn insert_char(&mut self, c: char) {
        self.input_buffer.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
    }

    fn move_cursor_left(&mut self) {
        if let Some(idx) = self.prev_char_boundary() {
            self.input_cursor = idx;
        }
    }

    fn move_cursor_right(&mut self) {
        if let Some(c) = self.input_buffer[self.input_cursor..].chars().next() {
            self.input_cursor += c.len_utf8();
        }
    }

    fn delete_prev_char(&mut self) {
        if let Some(idx) = self.prev_char_boundary() {
            self.input_buffer.remove(idx);
            self.input_cursor = idx;
        }
    }

    fn delete_next_char(&mut self) {
        if self.input_cursor < self.input_buffer.len() {
            self.input_buffer.remove(self.input_cursor);
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('q') {
            return true;
        }

        match self.modal {
            ActiveModal::AddFile => {
                match key.code {
                    KeyCode::Esc => self.modal = ActiveModal::None,
                    KeyCode::Enter => {
                        let cleaned = self.file_path_input.trim().trim_matches(['\'', '"']);
                        if !cleaned.is_empty() {
                            let _ = self
                                .cmd_tx
                                .send(EngineCommand::ImportFile(PathBuf::from(cleaned)));
                        }
                        self.file_path_input.clear();
                        self.modal = ActiveModal::None;
                    }
                    KeyCode::Backspace => {
                        self.file_path_input.pop();
                    }
                    KeyCode::Char(c) => self.file_path_input.push(c),
                    _ => {}
                }
                return false;
            }
            ActiveModal::Sessions => {
                match key.code {
                    KeyCode::Esc => self.modal = ActiveModal::None,
                    KeyCode::Up => self.session_idx = self.session_idx.saturating_sub(1),
                    KeyCode::Down if self.session_idx + 1 < self.sessions.len() => {
                        self.session_idx += 1;
                    }
                    KeyCode::Enter => {
                        if let Some(s) = self.sessions.get(self.session_idx).cloned() {
                            self.num_ctx = s.num_ctx;
                            let _ = self.cmd_tx.send(EngineCommand::LoadSession(s));
                            self.modal = ActiveModal::None;
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        if let Some(s) = self.sessions.get(self.session_idx) {
                            let _ = self
                                .cmd_tx
                                .send(EngineCommand::DeleteSession(s.file_path.clone()));
                        }
                    }
                    _ => {}
                }
                return false;
            }
            ActiveModal::ToolApproval => {
                match key.code {
                    KeyCode::Enter => self.resolve_modal_tool(true),
                    KeyCode::Esc => self.resolve_modal_tool(false),
                    KeyCode::Up => self.tool_scroll = self.tool_scroll.saturating_sub(1),
                    KeyCode::Down => self.tool_scroll = self.tool_scroll.saturating_add(1),
                    KeyCode::PageUp => self.tool_scroll = self.tool_scroll.saturating_sub(10),
                    KeyCode::PageDown => self.tool_scroll = self.tool_scroll.saturating_add(10),
                    KeyCode::Backspace => {
                        self.tool_feedback.pop();
                    }
                    KeyCode::Char(c) => self.tool_feedback.push(c),
                    _ => {}
                }
                return false;
            }
            ActiveModal::None => {}
        }

        match key.code {
            KeyCode::Esc if self.is_streaming => {
                let _ = self.cmd_tx.send(EngineCommand::CancelStream);
            }
            KeyCode::Enter if ctrl => self.submit_prompt(),
            KeyCode::Enter => self.insert_char('\n'),
            KeyCode::Char('v') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::PasteClipboard);
            }
            KeyCode::Char('f') if ctrl => {
                self.file_path_input.clear();
                self.modal = ActiveModal::AddFile;
            }
            KeyCode::Char('x') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::ClearImages);
            }
            KeyCode::Char('s') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::RefreshSessions);
                self.modal = ActiveModal::Sessions;
            }
            KeyCode::Char('r') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::ResetChat);
            }
            KeyCode::Char('t') if ctrl => self.enable_tools = !self.enable_tools,
            KeyCode::Char('p') if ctrl && !self.config.presets.is_empty() => {
                self.preset_idx = (self.preset_idx + 1) % self.config.presets.len();
                let prompt = &self.config.presets[self.preset_idx].prompt;
                if !prompt.is_empty() {
                    self.input_buffer = prompt.clone();
                    self.input_cursor = self.input_buffer.len();
                }
            }
            KeyCode::Char('y') if ctrl && !self.config.system_presets.is_empty() => {
                self.sys_preset_idx = (self.sys_preset_idx + 1) % self.config.system_presets.len();
            }
            KeyCode::Left => self.move_cursor_left(),
            KeyCode::Right => self.move_cursor_right(),
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::End => self.input_cursor = self.input_buffer.len(),
            KeyCode::Delete => self.delete_next_char(),
            KeyCode::Backspace => self.delete_prev_char(),
            KeyCode::Char(c) if !ctrl => self.insert_char(c),
            _ => {}
        }
        false
    }

    fn current_system_prompt(&self) -> String {
        self.config
            .system_presets
            .get(self.sys_preset_idx)
            .map(|p| p.prompt.clone())
            .unwrap_or_default()
    }

    fn submit_prompt(&mut self) {
        if self.is_streaming {
            return;
        }
        let prompt = self.input_buffer.trim().to_string();
        if prompt.is_empty() {
            self.status_bar = ("Prompt cannot be empty.".into(), true);
            return;
        }
        self.input_buffer.clear();
        self.input_cursor = 0;
        self.chat_scroll = u16::MAX;
        let _ = self.cmd_tx.send(EngineCommand::SendPrompt {
            prompt,
            system_prompt: self.current_system_prompt(),
            num_ctx: self.num_ctx,
            enable_tools: self.enable_tools,
        });
    }

    fn resolve_modal_tool(&mut self, approved: bool) {
        if self.pending_tool.take().is_some() {
            self.modal = ActiveModal::None;
            let _ = self.cmd_tx.send(EngineCommand::ResolveTool {
                approved,
                feedback: std::mem::take(&mut self.tool_feedback),
                system_prompt: self.current_system_prompt(),
                num_ctx: self.num_ctx,
                enable_tools: self.enable_tools,
            });
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(1)])
            .split(f.area());

        let main_panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(36), Constraint::Min(40)])
            .split(root[0]);

        self.draw_sidebar(f, main_panes[0]);
        self.draw_chat_area(f, main_panes[1]);

        let status_style = if self.status_bar.1 {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };
        f.render_widget(
            Paragraph::new(format!(" {}", self.status_bar.0)).style(status_style),
            root[1],
        );

        match self.modal {
            ActiveModal::AddFile => self.draw_file_modal(f),
            ActiveModal::Sessions => self.draw_sessions_modal(f),
            ActiveModal::ToolApproval => self.draw_tool_modal(f),
            ActiveModal::None => {}
        }
    }

    fn draw_sidebar(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(10), Constraint::Min(6)])
            .split(area);

        let preset_name = self
            .config
            .presets
            .get(self.preset_idx)
            .map(|p| p.name.as_str())
            .unwrap_or("None");
        let sys_name = self
            .config
            .system_presets
            .get(self.sys_preset_idx)
            .map(|p| p.name.as_str())
            .unwrap_or("None");
        let settings_text = vec![
            Line::from(vec![
                Span::raw("Model:   "),
                Span::styled(&self.model, Style::default().fg(Color::Green)),
            ]),
            Line::from(format!("Context: {}", self.num_ctx)),
            Line::from(format!(
                "Tools:   {} (^T)",
                if self.enable_tools { "ON" } else { "OFF" }
            )),
            Line::from(format!("Preset:  {preset_name} (^P)")),
            Line::from(format!("System:  {sys_name} (^Y)")),
        ];
        f.render_widget(
            Paragraph::new(settings_text)
                .block(Block::default().borders(Borders::ALL).title(" Config ")),
            chunks[0],
        );

        let items: Vec<ListItem> = if self.attachments.is_empty() {
            vec![ListItem::new("No images attached").style(Style::default().fg(Color::DarkGray))]
        } else {
            self.attachments
                .iter()
                .map(|a| {
                    ListItem::new(format!(
                        "• {} ({}x{}, {}KB)",
                        a.name, a.dimensions.0, a.dimensions.1, a.size_kb
                    ))
                })
                .collect()
        };
        f.render_widget(
            List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Images ({}) ", self.attachments.len())),
            ),
            chunks[1],
        );
    }

    fn render_message_block(
        idx: usize,
        header_label: &str,
        color: Color,
        content_lines: &[String],
        is_unfolded: bool,
        scroll: &mut usize,
        lines: &mut Vec<Line<'static>>,
        ranges: &mut Vec<(usize, u16, u16, bool)>,
    ) {
        let start_visual = lines.len() as u16;
        let max_lines = if is_unfolded {
            usize::MAX
        } else {
            DEFAULT_MAX_MSG_LINES
        };
        let toggle_hint = if is_unfolded { " [Click to fold]" } else { "" };

        lines.push(Line::from(Span::styled(
            format!("━━ {header_label}{toggle_hint} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));

        let total_content = content_lines.len();
        let is_truncated = total_content > max_lines;

        if is_truncated {
            let max_scroll = total_content.saturating_sub(max_lines);
            *scroll = (*scroll).min(max_scroll);
            let start = *scroll;
            let end = (start + max_lines).min(total_content);

            if start > 0 {
                lines.push(Line::from(Span::styled(
                    format!("    ... ({start} lines above) ..."),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            for l in &content_lines[start..end] {
                lines.push(Line::from(format!("  {l}")));
            }
            if end < total_content {
                let remaining = total_content - end;
                lines.push(Line::from(Span::styled(
                    format!("    ... ({remaining} lines below - Click to unfold) ..."),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        } else {
            for l in content_lines {
                lines.push(Line::from(format!("  {l}")));
            }
        }

        lines.push(Line::from(""));
        ranges.push((idx, start_visual, lines.len() as u16, is_truncated));
    }

    fn draw_chat_area(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(8)])
            .split(area);

        self.chat_area = chunks[0];
        let inner_width = chunks[0].width.saturating_sub(2).max(1);
        let available_width = inner_width.saturating_sub(2) as usize;

        if self.cached_wrap_width != available_width {
            self.cached_wrap_width = available_width;
            self.msg_wrap_cache.clear();
        }
        while self.msg_wrap_cache.len() < self.messages.len() {
            let idx = self.msg_wrap_cache.len();
            self.msg_wrap_cache
                .push(reflow_text(&self.messages[idx].content, available_width));
        }

        self.msg_scrolls.resize(self.messages.len() + 1, 0);
        self.msg_unfolded.resize(self.messages.len() + 1, false);
        self.msg_visual_ranges.clear();

        let mut lines = Vec::new();

        for (i, m) in self.messages.iter().enumerate() {
            let (tag, color) = match m.role.as_str() {
                "user" => ("USER", Color::Cyan),
                "assistant" => ("ARCHON", Color::Green),
                "tool" => ("TOOL", Color::Magenta),
                _ => ("SYS", Color::Gray),
            };
            let img_badge = m
                .images
                .as_ref()
                .map(|v| format!(" [+{} img]", v.len()))
                .unwrap_or_default();
            let header = format!("{tag}{img_badge}");

            Self::render_message_block(
                i,
                &header,
                color,
                &self.msg_wrap_cache[i],
                self.msg_unfolded[i],
                &mut self.msg_scrolls[i],
                &mut lines,
                &mut self.msg_visual_ranges,
            );
        }

        if self.is_streaming {
            let i = self.messages.len();
            let stream_lines = reflow_text(&self.streaming_text, available_width);
            Self::render_message_block(
                i,
                "ARCHON (streaming...)",
                Color::Yellow,
                &stream_lines,
                self.msg_unfolded[i],
                &mut self.msg_scrolls[i],
                &mut lines,
                &mut self.msg_visual_ranges,
            );
        }

        self.total_chat_lines = lines.len().min(u16::MAX as usize) as u16;
        let viewable_height = chunks[0].height.saturating_sub(2);
        let max_scroll = self.total_chat_lines.saturating_sub(viewable_height);
        self.chat_scroll = self.chat_scroll.min(max_scroll);

        f.render_widget(
            Paragraph::new(lines).scroll((self.chat_scroll, 0)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Conversation (Scroll near right edge for global chat) "),
            ),
            chunks[0],
        );

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll as usize).position(self.chat_scroll as usize);
        f.render_stateful_widget(scrollbar, chunks[0], &mut scrollbar_state);

        let prompt_inner_width = chunks[1].width.saturating_sub(2).max(1) as usize;
        let prompt_inner_height = chunks[1].height.saturating_sub(2).max(1);
        let (cursor_row, cursor_col) =
            compute_cursor_coords(&self.input_buffer, self.input_cursor, prompt_inner_width);
        let input_scroll = cursor_row.saturating_sub(prompt_inner_height.saturating_sub(1));

        f.render_widget(
            Paragraph::new(self.input_buffer.as_str())
                .wrap(Wrap { trim: false })
                .scroll((input_scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(" Prompt "),
                ),
            chunks[1],
        );

        if self.modal == ActiveModal::None {
            let cx = chunks[1].x + 1 + cursor_col.min(chunks[1].width.saturating_sub(3));
            let cy = chunks[1].y + 1 + (cursor_row - input_scroll);
            f.set_cursor_position(Position::new(cx, cy));
        }
    }

    fn draw_file_modal(&self, f: &mut Frame) {
        let area = centered_rect(60, 20, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(format!(
                "Press ⏎ or drag-and-drop image path:\n\n> {}\n\n[⏎] Import    [Esc] Cancel",
                self.file_path_input
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Add Image File "),
            ),
            area,
        );
    }

    fn draw_sessions_modal(&self, f: &mut Frame) {
        let area = centered_rect(70, 60, f.area());
        f.render_widget(Clear, area);
        let items: Vec<ListItem> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let prefix = if i == self.session_idx { ">> " } else { "   " };
                ListItem::new(format!(
                    "{prefix}[{}] {} ({} msgs)",
                    s.updated_at,
                    s.title,
                    s.messages.len()
                ))
            })
            .collect();

        f.render_widget(
            List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Saved Sessions (⏎=Load, D=Delete, Esc=Close) "),
            ),
            area,
        );
    }

    fn draw_tool_modal(&self, f: &mut Frame) {
        let area = centered_rect(80, 75, f.area());
        f.render_widget(Clear, area);

        let (preview, idx, total) = self
            .pending_tool
            .as_ref()
            .map(|(p, i, t)| (p.as_str(), *i, *t))
            .unwrap_or(("", 1, 1));

        let mut lines: Vec<Line> = Vec::new();
        let mut section_color = Color::White;

        for line in preview.lines() {
            if line == "--- SEARCH ---" {
                section_color = Color::LightRed;
                lines.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )));
            } else if line == "+++ REPLACE +++" || line == "+++ CONTENT +++" {
                section_color = Color::LightGreen;
                lines.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(section_color),
                )));
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Optional Feedback / Note:",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!("> {}", self.tool_feedback)));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[⏎] Approve & Run     [Esc] Decline     [↑/↓/PgUp/PgDn] Scroll",
            Style::default().fg(Color::Yellow),
        )));

        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.tool_scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(format!(
                            " ⚠️ Tool Execution Approval Required ({idx}/{total}) "
                        )),
                ),
            area,
        );
    }
}

fn compute_cursor_coords(text: &str, cursor_byte: usize, width: usize) -> (u16, u16) {
    if width == 0 {
        return (0, 0);
    }
    let slice = &text[..cursor_byte.min(text.len())];
    let mut row: u16 = 0;
    let mut col: usize = 0;

    for c in slice.chars() {
        if c == '\n' {
            row = row.saturating_add(1);
            col = 0;
        } else {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if col + cw > width {
                row = row.saturating_add(1);
                col = cw;
            } else {
                col += cw;
            }
        }
    }
    (row, col as u16)
}

fn flush_word(
    out: &mut Vec<String>,
    current_line: &mut String,
    word_buf: &mut String,
    width: usize,
) {
    if word_buf.is_empty() {
        return;
    }
    let word_w = UnicodeWidthStr::width(word_buf.as_str());
    let cur_w = UnicodeWidthStr::width(current_line.as_str());
    if cur_w + word_w > width && !current_line.is_empty() {
        out.push(std::mem::take(current_line));
    }
    current_line.push_str(word_buf);
    word_buf.clear();
}

fn reflow_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.lines().map(String::from).collect();
    }
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }

        let mut current_line = String::new();
        let mut word_buf = String::new();

        for c in line.chars() {
            word_buf.push(c);
            if c.is_whitespace() {
                flush_word(&mut out, &mut current_line, &mut word_buf, width);
            }
        }
        flush_word(&mut out, &mut current_line, &mut word_buf, width);
        if !current_line.is_empty() {
            out.push(current_line);
        }
    }

    let mut chopped = Vec::new();
    for l in out {
        if l.is_empty() || UnicodeWidthStr::width(l.as_str()) <= width {
            chopped.push(l);
            continue;
        }

        let mut chunk = String::new();
        let mut chunk_w = 0;
        for c in l.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if chunk_w + cw > width && !chunk.is_empty() {
                chopped.push(std::mem::take(&mut chunk));
                chunk_w = 0;
            }
            chunk.push(c);
            chunk_w += cw;
        }
        if !chunk.is_empty() {
            chopped.push(chunk);
        }
    }
    chopped
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}
