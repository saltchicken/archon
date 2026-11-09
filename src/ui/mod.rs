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
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
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

    // Global scrolling
    chat_scroll: u16,
    tool_scroll: u16,
    total_chat_lines: u16,

    // Per-message internal scrolling state
    msg_scrolls: Vec<usize>,
    msg_unfolded: Vec<bool>,

    // Mouse tracking UI math
    chat_area: Rect,
    msg_visual_ranges: Vec<(usize, u16, u16, bool)>, // (msg_idx, start_y, end_y, is_truncated)

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
            status_bar: (
                "Ready. Press Ctrl+S to send prompt.".into(),
                false,
            ),
            preset_idx: 0,
            sys_preset_idx: 0,
            enable_tools: true,
            num_ctx,
            input_buffer: String::new(),
            chat_scroll: 0,
            tool_scroll: 0,
            total_chat_lines: 0,
            msg_scrolls: Vec::new(),
            msg_unfolded: Vec::new(),
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
        // Enable mouse capture during initialization
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let mut term_events = EventStream::new();

        loop {
            terminal.draw(|f| self.draw(f))?;

            tokio::select! {
                Some(event_res) = term_events.next() => {
                    match event_res {
                        Ok(Event::Key(key)) => {
                            if self.handle_key(key) {
                                break;
                            }
                        }
                        Ok(Event::Mouse(mouse)) => {
                            self.handle_mouse(mouse);
                        }
                        Ok(Event::Resize(_, _)) => {
                            // Automatically loop around and redraw without crashing
                        }
                        _ => {}
                    }
                }
                Some(ev) = ev_rx.recv() => {
                    self.handle_engine_event(ev);
                }
                else => break,
            }
        }

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
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
                self.chat_scroll = u16::MAX; // Auto-scroll down on new streams
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
        let x = mouse.column;
        let y = mouse.row;

        // Active modal mouse scrolling
        if matches!(self.modal, ActiveModal::ToolApproval) {
            if mouse.kind == MouseEventKind::ScrollUp {
                self.tool_scroll = self.tool_scroll.saturating_sub(3);
            } else if mouse.kind == MouseEventKind::ScrollDown {
                self.tool_scroll = self.tool_scroll.saturating_add(3);
            }
            return;
        }

        // Check if mouse is hovering over the main chat box
        if x >= self.chat_area.x
            && x < self.chat_area.x + self.chat_area.width
            && y >= self.chat_area.y
            && y < self.chat_area.y + self.chat_area.height
        {
            // Designate the rightmost 2 columns as the dedicated "Global Scrollbar Area"
            let is_on_scrollbar = x >= self.chat_area.x + self.chat_area.width.saturating_sub(2);

            if mouse.kind == MouseEventKind::ScrollUp || mouse.kind == MouseEventKind::ScrollDown {
                let is_up = mouse.kind == MouseEventKind::ScrollUp;

                if is_on_scrollbar {
                    // Force global scroll when on the right edge
                    if is_up {
                        self.chat_scroll = self.chat_scroll.saturating_sub(3);
                    } else {
                        self.chat_scroll = self.chat_scroll.saturating_add(3);
                    }
                } else {
                    let inner_y = y.saturating_sub(self.chat_area.y + 1);
                    let visual_y = inner_y as usize + self.chat_scroll as usize;

                    let mut scrolled_internal = false;
                    for (i, start, end, is_truncated) in &self.msg_visual_ranges {
                        // Check which specific message we are hovering over
                        if visual_y >= *start as usize && visual_y < *end as usize {
                            if *is_truncated {
                                let current = *self.msg_scrolls.get(*i).unwrap_or(&0);
                                if is_up {
                                    self.msg_scrolls[*i] = current.saturating_sub(3);
                                } else {
                                    self.msg_scrolls[*i] = current.saturating_add(3);
                                }
                                scrolled_internal = true;
                            }
                            break;
                        }
                    }

                    // Fall back to main chat scroll if not hovering over a truncated message
                    if !scrolled_internal {
                        if is_up {
                            self.chat_scroll = self.chat_scroll.saturating_sub(3);
                        } else {
                            self.chat_scroll = self.chat_scroll.saturating_add(3);
                        }
                    }
                }
            } else if matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
            ) {
                if is_on_scrollbar {
                    // Handle clicking and dragging the scrollbar physically
                    let view_height = self.chat_area.height.saturating_sub(2);
                    let inner_y = y.saturating_sub(self.chat_area.y + 1);
                    let max_scroll = self.total_chat_lines.saturating_sub(view_height);

                    if max_scroll > 0 && view_height > 1 {
                        let track_height = view_height.saturating_sub(1);
                        let clamped_y = inner_y.min(track_height);
                        let fraction = clamped_y as f32 / track_height as f32;
                        self.chat_scroll = (fraction * max_scroll as f32).round() as u16;
                    }
                } else if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                    // Handle clicking anywhere on the message to toggle fold/unfold
                    let inner_y = y.saturating_sub(self.chat_area.y + 1);
                    let visual_y = inner_y as usize + self.chat_scroll as usize;

                    for (i, start, end, _) in &self.msg_visual_ranges {
                        if visual_y >= *start as usize && visual_y < *end as usize {
                            if *i < self.msg_unfolded.len() {
                                self.msg_unfolded[*i] = !self.msg_unfolded[*i];
                            }
                            break;
                        }
                    }
                }
            }
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
                        let cleaned = self
                            .file_path_input
                            .trim()
                            .trim_matches('\'')
                            .trim_matches('"');
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
                    KeyCode::Up if self.session_idx > 0 => self.session_idx -= 1,
                    KeyCode::Down if self.session_idx + 1 < self.sessions.len() => {
                        self.session_idx += 1
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
            KeyCode::Char('s') if ctrl => self.submit_prompt(),
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
            KeyCode::Char('l') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::RefreshSessions);
                self.modal = ActiveModal::Sessions;
            }
            KeyCode::Char('r') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::ResetChat);
            }
            KeyCode::Char('t') if ctrl => self.enable_tools = !self.enable_tools,
            KeyCode::Char('p') if ctrl => {
                self.preset_idx = (self.preset_idx + 1) % self.config.presets.len();
                let p = &self.config.presets[self.preset_idx];
                if !p.prompt.is_empty() {
                    self.input_buffer = p.prompt.clone();
                }
            }
            KeyCode::Char('y') if ctrl => {
                self.sys_preset_idx = (self.sys_preset_idx + 1) % self.config.system_presets.len();
            }

            KeyCode::Enter => self.input_buffer.push('\n'),
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) if !ctrl => self.input_buffer.push(c),
            _ => {}
        }
        false
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
        self.chat_scroll = u16::MAX; // Jump to the bottom of the chat when sending a new prompt
        let sys = self.config.system_presets[self.sys_preset_idx]
            .prompt
            .clone();
        let _ = self.cmd_tx.send(EngineCommand::SendPrompt {
            prompt,
            system_prompt: sys,
            num_ctx: self.num_ctx,
            enable_tools: self.enable_tools,
        });
    }

    fn resolve_modal_tool(&mut self, approved: bool) {
        if self.pending_tool.take().is_some() {
            self.modal = ActiveModal::None;
            let sys = self.config.system_presets[self.sys_preset_idx]
                .prompt
                .clone();
            let _ = self.cmd_tx.send(EngineCommand::ResolveTool {
                approved,
                feedback: std::mem::take(&mut self.tool_feedback),
                system_prompt: sys,
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
            .constraints([
                Constraint::Length(10),
                Constraint::Min(6),
                Constraint::Length(8),
            ])
            .split(area);

        let preset_name = &self.config.presets[self.preset_idx].name;
        let sys_name = &self.config.system_presets[self.sys_preset_idx].name;
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
            Line::from(format!("Preset:  {} (^P)", preset_name)),
            Line::from(format!("System:  {} (^Y)", sys_name)),
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

        let keys = vec![
            Line::from("^V Paste    ^F Add File"),
            Line::from("^X Clear    ^L Sessions"),
            Line::from("^⏎ Send     ^L Sessions"),
            Line::from("^R Reset"),
            Line::from("^Q Quit"),
        ];
        f.render_widget(
            Paragraph::new(keys)
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::ALL).title(" Shortcuts ")),
            chunks[2],
        );
    }

    fn draw_chat_area(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(6)])
            .split(area);

        // Store chat rect for mouse hit-testing later
        self.chat_area = chunks[0];
        let inner_width = chunks[0].width.saturating_sub(2).max(1);
        let available_width = inner_width.saturating_sub(2) as usize; // for "  " visual padding

        self.msg_scrolls.resize(self.messages.len() + 1, 0);
        self.msg_unfolded.resize(self.messages.len() + 1, false);
        self.msg_visual_ranges.clear();

        let default_max_lines = 15;
        let mut lines = Vec::new();
        let mut total_visual_lines: u16 = 0;

        // Since we explicitly format the word-wrapping beforehand, each Line takes exactly 1 visual height.
        let add_line = |lines_vec: &mut Vec<Line<'static>>, total: &mut u16, l: Line<'static>| {
            *total = total.saturating_add(1);
            lines_vec.push(l);
        };

        for (i, m) in self.messages.iter().enumerate() {
            let start_visual = total_visual_lines;

            let is_unfolded = self.msg_unfolded[i];
            let max_lines = if is_unfolded {
                usize::MAX
            } else {
                default_max_lines
            };

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
            let toggle_hint = if is_unfolded { " [Click to fold]" } else { "" };

            add_line(
                &mut lines,
                &mut total_visual_lines,
                Line::from(Span::styled(
                    format!("━━ {tag}{img_badge}{toggle_hint} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )),
            );

            let msg_content_lines = reflow_text(&m.content, available_width);
            let total_content = msg_content_lines.len();
            let is_truncated = total_content > max_lines;

            if is_truncated {
                let scroll = self.msg_scrolls[i];
                let max_scroll = total_content.saturating_sub(max_lines);
                let scroll = scroll.min(max_scroll);
                self.msg_scrolls[i] = scroll;

                let start = scroll;
                let end = (start + max_lines).min(total_content);

                if start > 0 {
                    add_line(
                        &mut lines,
                        &mut total_visual_lines,
                        Line::from(Span::styled(
                            format!("    ... ({} lines above) ...", start),
                            Style::default().fg(Color::DarkGray),
                        )),
                    );
                }

                for l in &msg_content_lines[start..end] {
                    add_line(
                        &mut lines,
                        &mut total_visual_lines,
                        Line::from(format!("  {l}")),
                    );
                }

                if end < total_content {
                    let remaining = total_content - end;
                    add_line(
                        &mut lines,
                        &mut total_visual_lines,
                        Line::from(Span::styled(
                            format!("    ... ({} lines below - Click to unfold) ...", remaining),
                            Style::default().fg(Color::DarkGray),
                        )),
                    );
                }
            } else {
                for l in msg_content_lines {
                    add_line(
                        &mut lines,
                        &mut total_visual_lines,
                        Line::from(format!("  {l}")),
                    );
                }
            }
            add_line(&mut lines, &mut total_visual_lines, Line::from(""));
            let end_visual = total_visual_lines;
            self.msg_visual_ranges
                .push((i, start_visual, end_visual, is_truncated));
        }

        if self.is_streaming {
            let i = self.messages.len();
            let start_visual = total_visual_lines;

            let is_unfolded = self.msg_unfolded[i];
            let max_lines = if is_unfolded {
                usize::MAX
            } else {
                default_max_lines
            };
            let toggle_hint = if is_unfolded { " [Click to fold]" } else { "" };

            add_line(
                &mut lines,
                &mut total_visual_lines,
                Line::from(Span::styled(
                    format!("━━ ARCHON (streaming...){toggle_hint} "),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
            );

            let msg_content_lines = reflow_text(&self.streaming_text, available_width);
            let total_content = msg_content_lines.len();
            let is_truncated = total_content > max_lines;

            if is_truncated {
                let scroll = self.msg_scrolls[i];
                let max_scroll = total_content.saturating_sub(max_lines);
                let scroll = scroll.min(max_scroll);
                self.msg_scrolls[i] = scroll;

                let start = scroll;
                let end = (start + max_lines).min(total_content);

                if start > 0 {
                    add_line(
                        &mut lines,
                        &mut total_visual_lines,
                        Line::from(Span::styled(
                            format!("    ... ({} lines above) ...", start),
                            Style::default().fg(Color::DarkGray),
                        )),
                    );
                }
                for l in &msg_content_lines[start..end] {
                    add_line(
                        &mut lines,
                        &mut total_visual_lines,
                        Line::from(format!("  {l}")),
                    );
                }
                if end < total_content {
                    let remaining = total_content - end;
                    add_line(
                        &mut lines,
                        &mut total_visual_lines,
                        Line::from(Span::styled(
                            format!("    ... ({} lines below - Click to unfold) ...", remaining),
                            Style::default().fg(Color::DarkGray),
                        )),
                    );
                }
            } else {
                for l in msg_content_lines {
                    add_line(
                        &mut lines,
                        &mut total_visual_lines,
                        Line::from(format!("  {l}")),
                    );
                }
            }
            add_line(&mut lines, &mut total_visual_lines, Line::from(""));
            let end_visual = total_visual_lines;
            self.msg_visual_ranges
                .push((i, start_visual, end_visual, is_truncated));
        }

        // Clamp global scrolling based on total height
        self.total_chat_lines = total_visual_lines;
        let viewable_height = chunks[0].height.saturating_sub(2); // Subtract borders
        let max_scroll = self.total_chat_lines.saturating_sub(viewable_height);
        self.chat_scroll = self.chat_scroll.min(max_scroll);

        // Render main chat paragraph (Wrap removed! We manage it fully in reflow_text)
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

        f.render_widget(
            Paragraph::new(self.input_buffer.as_str())
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan))
                        .title(" Prompt (Ctrl+S to Send, Enter for newline) "),
                ),
            chunks[1],
        );
    }

    fn draw_file_modal(&self, f: &mut Frame) {
        let area = centered_rect(60, 20, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(format!(
                "Enter or drag-and-drop image path:\n\n> {}\n\n[Enter] Import    [Esc] Cancel",
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
                    .title(" Saved Sessions (Enter=Load, D=Delete, Esc=Close) "),
            ),
            area,
        );
    }

    fn draw_tool_modal(&self, f: &mut Frame) {
        let area = centered_rect(75, 60, f.area());
        f.render_widget(Clear, area);

        let (preview, idx, total) = self
            .pending_tool
            .as_ref()
            .map(|(p, i, t)| (p.as_str(), *i, *t))
            .unwrap_or(("", 1, 1));

        let text = format!(
            "{preview}\n\nOptional Feedback / Note:\n> {}\n\n[Enter] Approve & Run    [Esc] Decline",
            self.tool_feedback
        );

        f.render_widget(
            Paragraph::new(text)
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

// Custom manual text wrap to predict accurate heights while preserving indentation
fn reflow_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.lines().map(|s| s.to_string()).collect();
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
                let word_len = word_buf.chars().count();
                if current_line.chars().count() + word_len <= width {
                    current_line.push_str(&word_buf);
                    word_buf.clear();
                } else {
                    if !current_line.is_empty() {
                        out.push(current_line);
                        current_line = String::new();
                    }
                    current_line.push_str(&word_buf);
                    word_buf.clear();
                }
            }
        }
        if !word_buf.is_empty() {
            let word_len = word_buf.chars().count();
            if current_line.chars().count() + word_len <= width {
                current_line.push_str(&word_buf);
            } else {
                if !current_line.is_empty() {
                    out.push(current_line);
                    current_line = String::new();
                }
                current_line.push_str(&word_buf);
            }
        }
        if !current_line.is_empty() {
            out.push(current_line);
        }
    }

    // Aggressive line-chopping pass to catch strings/words exceeding bounds (avoids sideways cutoffs)
    let mut chopped = Vec::new();
    for l in out {
        if l.is_empty() {
            chopped.push(String::new());
            continue;
        }
        let mut rem = l.as_str();
        while rem.chars().count() > width {
            let split_byte = rem
                .char_indices()
                .nth(width)
                .map(|(i, _)| i)
                .unwrap_or(rem.len());
            let (chunk, rest) = rem.split_at(split_byte);
            chopped.push(chunk.to_string());
            rem = rest;
        }
        if !rem.is_empty() {
            chopped.push(rem.to_string());
        }
    }
    chopped
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
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
        .split(popup_layout[1])[1]
}
