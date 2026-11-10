use crate::core::{
    config::{Config, Preset},
    session::{ChatMessage, SessionData},
    workspace::ImageAttachment,
    EngineCommand, EngineEvent,
};
use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
        MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
        ScrollbarState, Widget, Wrap,
    },
    Frame, Terminal,
};
use std::{io, path::PathBuf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tui_textarea::TextArea;

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
    text_area: TextArea<'static>,

    chat_scroll: u16,
    tool_scroll: u16,
    total_chat_lines: u16,

    msg_scrolls: Vec<usize>,
    msg_unfolded: Vec<bool>,

    chat_area: Rect,
    msg_visual_ranges: Vec<(usize, u16, u16, bool)>,

    sidebar_expanded: bool,

    modal: ActiveModal,
    file_path_input: String,
    sessions: Vec<SessionData>,
    session_idx: usize,
    pending_tool: Option<(String, usize, usize)>,
    tool_feedback: String,
}

fn create_text_area() -> TextArea<'static> {
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
            text_area: create_text_area(),
            chat_scroll: 0,
            tool_scroll: 0,
            total_chat_lines: 0,
            msg_scrolls: Vec::new(),
            msg_unfolded: Vec::new(),
            chat_area: Rect::default(),
            msg_visual_ranges: Vec::new(),
            sidebar_expanded: false,
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
                            let clean_text = text.replace('\r', "");
                            self.text_area.insert_str(clean_text);
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
            EngineEvent::ChatUpdated { messages } => {
                self.messages = messages;
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

    fn find_msg_at(&self, visual_y: usize) -> Option<(usize, bool)> {
        self.msg_visual_ranges
            .iter()
            .find(|(_, start, end, _)| visual_y >= *start as usize && visual_y < *end as usize)
            .map(|&(i, _, _, truncated)| (i, truncated))
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
                if !is_on_scrollbar {
                    if let Some((i, true)) = self.find_msg_at(visual_y) {
                        let cur = self.msg_scrolls.get(i).copied().unwrap_or(0);
                        self.msg_scrolls[i] = if is_up {
                            cur.saturating_sub(3)
                        } else {
                            cur.saturating_add(3)
                        };
                        return;
                    }
                }
                self.chat_scroll = if is_up {
                    self.chat_scroll.saturating_sub(3)
                } else {
                    self.chat_scroll.saturating_add(3)
                };
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
                if let Some((i, _)) = self.find_msg_at(visual_y) {
                    if let Some(unfolded) = self.msg_unfolded.get_mut(i) {
                        *unfolded = !*unfolded;
                    }
                }
            }
            _ => {}
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
                false
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
                false
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
                false
            }
            ActiveModal::None => {
                match key.code {
                    KeyCode::Esc if self.is_streaming => {
                        let _ = self.cmd_tx.send(EngineCommand::CancelStream);
                    }
                    KeyCode::Enter if ctrl => self.submit_prompt(),
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
                    KeyCode::Char('b') if ctrl => self.sidebar_expanded = !self.sidebar_expanded,
                    KeyCode::Char('p') if ctrl && !self.config.presets.is_empty() => {
                        self.preset_idx = (self.preset_idx + 1) % self.config.presets.len();
                        let prompt = &self.config.presets[self.preset_idx].prompt;
                        if !prompt.is_empty() {
                            self.text_area = create_text_area();
                            self.text_area.insert_str(prompt);
                        }
                    }
                    KeyCode::Char('y') if ctrl && !self.config.system_presets.is_empty() => {
                        self.sys_preset_idx = (self.sys_preset_idx + 1) % self.config.system_presets.len();
                    }
                    _ => {
                        self.text_area.input(key);
                    }
                }
                false
            }
        }
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
        let prompt = self.text_area.lines().join("\n").trim().to_string();
        if prompt.is_empty() {
            self.status_bar = ("Prompt cannot be empty.".into(), true);
            return;
        }
        
        self.text_area = create_text_area();
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

        let sidebar_width = if self.sidebar_expanded { 36 } else { 0 };
        let main_panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Min(40)])
            .split(root[0]);

        if self.sidebar_expanded {
            self.draw_sidebar(f, main_panes[0]);
        }
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

        let preset_name = preset_label(&self.config.presets, self.preset_idx);
        let sys_name = preset_label(&self.config.system_presets, self.sys_preset_idx);
        let tools_label = if self.enable_tools { "ON" } else { "OFF" };

        let settings_text = vec![
            Line::from(vec![
                Span::raw("Model:   "),
                Span::styled(&self.model, Style::default().fg(Color::Green)),
            ]),
            Line::from(format!("Context: {}", self.num_ctx)),
            Line::from(format!("Tools:   {tools_label} (^T)")),
            Line::from(format!("Preset:  {preset_name} (^P)")),
            Line::from(format!("System:  {sys_name} (^Y)")),
        ];
        f.render_widget(
            Paragraph::new(settings_text).block(titled_block(" Config ")),
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
            List::new(items).block(titled_block(format!(" Images ({}) ", self.attachments.len()))),
            chunks[1],
        );
    }

    fn render_message_block(
        idx: usize,
        header_label: &str,
        color: Color,
        truncated: bool,
        content_lines: &[String],
        is_unfolded: bool,
        max_folded_lines: usize,
        scroll: &mut usize,
        lines: &mut Vec<Line<'static>>,
        ranges: &mut Vec<(usize, u16, u16, bool)>,
    ) {
        let start_visual = lines.len() as u16;
        let max_lines = if is_unfolded {
            usize::MAX
        } else {
            max_folded_lines
        };
        let toggle_hint = if is_unfolded { " [Click to fold]" } else { "" };

        let mut header_spans = vec![Span::styled(
            format!("━━ {header_label}{toggle_hint} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        if truncated {
            header_spans.push(Span::styled(
                "⚠ TRUNCATED",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(header_spans));

        let total_content = content_lines.len();
        let is_truncated = total_content > max_lines;

        if is_truncated {
            *scroll = (*scroll).min(total_content.saturating_sub(max_lines));
            let start = *scroll;
            let end = (start + max_lines).min(total_content);

            if start > 0 {
                lines.push(Line::from(Span::styled(
                    format!("    ... ({start} lines above) ..."),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            lines.extend(content_lines[start..end].iter().map(|l| Line::from(format!("  {l}"))));
            if end < total_content {
                lines.push(Line::from(Span::styled(
                    format!(
                        "    ... ({} lines below - Click to unfold) ...",
                        total_content - end
                    ),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        } else {
            lines.extend(content_lines.iter().map(|l| Line::from(format!("  {l}"))));
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
        let available_width = chunks[0].width.saturating_sub(4).max(1) as usize;

        self.msg_scrolls.resize(self.messages.len() + 1, 0);
        self.msg_unfolded.resize(self.messages.len() + 1, false);
        self.msg_visual_ranges.clear();

        let mut lines = Vec::new();
        let options = textwrap::Options::new(available_width).break_words(true);

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

            let wrapped_content: Vec<String> = m
                .content
                .lines()
                .flat_map(|line| {
                    if line.is_empty() {
                        vec![String::new()]
                    } else {
                        textwrap::wrap(line, &options)
                            .into_iter()
                            .map(|c| c.into_owned())
                            .collect()
                    }
                })
                .collect();

            Self::render_message_block(
                i,
                &format!("{tag}{img_badge}"),
                color,
                m.truncated,
                &wrapped_content,
                self.msg_unfolded[i],
                self.config.app.max_msg_lines,
                &mut self.msg_scrolls[i],
                &mut lines,
                &mut self.msg_visual_ranges,
            );
        }

        if self.is_streaming {
            let i = self.messages.len();
            let stream_lines: Vec<String> = self
                .streaming_text
                .lines()
                .flat_map(|line| {
                    if line.is_empty() {
                        vec![String::new()]
                    } else {
                        textwrap::wrap(line, &options)
                            .into_iter()
                            .map(|c| c.into_owned())
                            .collect()
                    }
                })
                .collect();
                
            Self::render_message_block(
                i,
                "ARCHON (streaming...)",
                Color::Yellow,
                false,
                &stream_lines,
                self.msg_unfolded[i],
                self.config.app.max_msg_lines,
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
            Paragraph::new(lines)
                .scroll((self.chat_scroll, 0))
                .block(titled_block(" Conversation ")),
            chunks[0],
        );

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll as usize).position(self.chat_scroll as usize);
        f.render_stateful_widget(scrollbar, chunks[0], &mut scrollbar_state);

        f.render_widget(&self.text_area, chunks[1]);
    }

    fn draw_file_modal(&self, f: &mut Frame) {
        let widget = Paragraph::new(format!(
            "Press ⏎ or drag-and-drop image path:\n\n> {}\n\n[⏎] Import    [Esc] Cancel",
            self.file_path_input
        ))
        .block(titled_block(" Add Image File "));
        render_modal(f, 60, 20, widget);
    }

    fn draw_sessions_modal(&self, f: &mut Frame) {
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

        let widget = List::new(items)
            .block(titled_block(" Saved Sessions (⏎=Load, D=Delete, Esc=Close) "));
        render_modal(f, 70, 60, widget);
    }

    fn draw_tool_modal(&self, f: &mut Frame) {
        let (preview, idx, total) = self
            .pending_tool
            .as_ref()
            .map(|(p, i, t)| (p.as_str(), *i, *t))
            .unwrap_or(("", 1, 1));

        let mut lines: Vec<Line> = Vec::new();
        let mut section_color = Color::White;

        for line in preview.lines() {
            if line.starts_with("--- ") && line.ends_with(" ---") {
                let header_color = if line.contains("SEARCH") {
                    section_color = Color::LightRed;
                    Color::Red
                } else if line.contains("REPLACE") || line.contains("CONTENT") {
                    section_color = Color::Cyan;
                    Color::Green
                } else {
                    section_color = Color::White;
                    Color::Cyan
                };
                lines.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(header_color).add_modifier(Modifier::BOLD),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(section_color),
                )));
            }
        }

        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "Optional Feedback / Note:",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("> {}", self.tool_feedback)),
            Line::from(""),
            Line::from(Span::styled(
                "[⏎] Approve & Run      [Esc] Decline      [↑/↓/PgUp/PgDn] Scroll",
                Style::default().fg(Color::Yellow),
            )),
        ]);

        let widget = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.tool_scroll, 0))
            .block(
                titled_block(format!(
                    " ⚠️ Tool Execution Approval Required ({idx}/{total}) "
                ))
                .border_style(Style::default().fg(Color::Yellow)),
            );
        render_modal(f, 80, 75, widget);
    }
}

fn titled_block<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default().borders(Borders::ALL).title(title)
}

fn render_modal<W: Widget>(f: &mut Frame, percent_x: u16, percent_y: u16, widget: W) {
    let area = centered_rect(percent_x, percent_y, f.area());
    f.render_widget(Clear, area);
    f.render_widget(widget, area);
}

fn preset_label(presets: &[Preset], idx: usize) -> &str {
    presets.get(idx).map(|p| p.name.as_str()).unwrap_or("None")
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let split = |dir, pct, rect| {
        let margin = (100 - pct) / 2;
        Layout::default()
            .direction(dir)
            .constraints([
                Constraint::Percentage(margin),
                Constraint::Percentage(pct),
                Constraint::Percentage(margin),
            ])
            .split(rect)[1]
    };
    split(
        Direction::Horizontal,
        percent_x,
        split(Direction::Vertical, percent_y, r),
    )
}
