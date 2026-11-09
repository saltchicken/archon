use crate::core::{
    config::Config,
    session::{ChatMessage, SessionData},
    workspace::ImageAttachment,
    EngineCommand, EngineEvent,
};
use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{io, path::PathBuf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(PartialEq, Clone, Copy)]
enum ActiveModal {
    None,
    AddFile,
    Sessions,
    Debug,
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
    debug_json: String,

    preset_idx: usize,
    sys_preset_idx: usize,
    enable_tools: bool,
    num_ctx: u32,
    input_buffer: String,
    
    // Global scrolling
    chat_scroll: u16,
    tool_scroll: u16,

    // Per-message internal scrolling
    selected_msg_idx: Option<usize>,
    msg_scroll: usize,

    modal: ActiveModal,
    file_path_input: String,
    sessions: Vec<SessionData>,
    session_idx: usize,
    pending_tool: Option<(String, usize, usize)>,
    tool_feedback: String,

    needs_capture: bool,
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
            status_bar: ("Ready. Press Ctrl+S to send prompt, Ctrl+C to Capture.".into(), false),
            debug_json: "No API requests sent yet.".into(),
            preset_idx: 0,
            sys_preset_idx: 0,
            enable_tools: true,
            num_ctx,
            input_buffer: String::new(),
            chat_scroll: 0,
            tool_scroll: 0,
            selected_msg_idx: None,
            msg_scroll: 0,
            modal: ActiveModal::None,
            file_path_input: String::new(),
            sessions: Vec::new(),
            session_idx: 0,
            pending_tool: None,
            tool_feedback: String::new(),
            needs_capture: false,
        }
    }

    pub async fn run(mut self, mut ev_rx: UnboundedReceiver<EngineEvent>) -> Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        let mut term_events = EventStream::new();

        loop {
            if self.needs_capture {
                self.needs_capture = false;
                disable_raw_mode()?;
                execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                let _ = self.cmd_tx.send(EngineCommand::CaptureScreen);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                enable_raw_mode()?;
                execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                terminal.clear()?;
            }

            terminal.draw(|f| self.draw(f))?;

            tokio::select! {
                Some(event_res) = term_events.next() => {
                    // Gracefully ignore Resize and other non-key events so the UI loop just redraws
                    if let Ok(Event::Key(key)) = event_res {
                        if self.handle_key(key) {
                            break;
                        }
                    }
                }
                Some(ev) = ev_rx.recv() => {
                    self.handle_engine_event(ev);
                }
                else => break,
            }
        }

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        Ok(())
    }

    fn handle_engine_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::Notification { message, is_error } => {
                self.status_bar = (message, is_error);
            }
            EngineEvent::WorkspaceUpdated(list) => self.attachments = list,
            EngineEvent::ChatUpdated { messages, .. } => {
                self.messages = messages;
                self.is_streaming = false;
                self.streaming_text.clear();
                
                let count = self.messages.len();
                if count > 0 {
                    self.selected_msg_idx = Some(count - 1);
                } else {
                    self.selected_msg_idx = None;
                }
                self.msg_scroll = 0;
            }
            EngineEvent::StreamStarted => {
                self.is_streaming = true;
                self.streaming_text.clear();
                self.status_bar = ("Streaming response from Ollama... (Esc to cancel)".into(), false);
                
                let count = self.messages.len() + 1;
                self.selected_msg_idx = Some(count - 1);
                self.msg_scroll = 0;
            }
            EngineEvent::StreamChunk(c) => self.streaming_text.push_str(&c),
            EngineEvent::StreamDone { .. } | EngineEvent::StreamError(_) => {
                self.is_streaming = false;
            }
            EngineEvent::ToolApprovalRequested { formatted_preview, index, total } => {
                self.pending_tool = Some((formatted_preview, index, total));
                self.tool_feedback.clear();
                self.tool_scroll = 0;
                self.modal = ActiveModal::ToolApproval;
            }
            EngineEvent::SessionsListed(list) => {
                self.sessions = list;
                self.session_idx = 0;
            }
            EngineEvent::DebugPayloadUpdated(json) => self.debug_json = json,
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
                        let cleaned = self.file_path_input.trim().trim_matches('\'').trim_matches('"');
                        if !cleaned.is_empty() {
                            let _ = self.cmd_tx.send(EngineCommand::ImportFile(PathBuf::from(cleaned)));
                        }
                        self.file_path_input.clear();
                        self.modal = ActiveModal::None;
                    }
                    KeyCode::Backspace => { self.file_path_input.pop(); }
                    KeyCode::Char(c) => self.file_path_input.push(c),
                    _ => {}
                }
                return false;
            }
            ActiveModal::Sessions => {
                match key.code {
                    KeyCode::Esc => self.modal = ActiveModal::None,
                    KeyCode::Up if self.session_idx > 0 => self.session_idx -= 1,
                    KeyCode::Down if self.session_idx + 1 < self.sessions.len() => self.session_idx += 1,
                    KeyCode::Enter => {
                        if let Some(s) = self.sessions.get(self.session_idx).cloned() {
                            self.num_ctx = s.num_ctx;
                            let _ = self.cmd_tx.send(EngineCommand::LoadSession(s));
                            self.modal = ActiveModal::None;
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        if let Some(s) = self.sessions.get(self.session_idx) {
                            let _ = self.cmd_tx.send(EngineCommand::DeleteSession(s.file_path.clone()));
                        }
                    }
                    _ => {}
                }
                return false;
            }
            ActiveModal::Debug => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    self.modal = ActiveModal::None;
                }
                return false;
            }
            ActiveModal::ToolApproval => {
                match key.code {
                    KeyCode::Enter => self.resolve_modal_tool(true),
                    KeyCode::Esc => self.resolve_modal_tool(false),
                    KeyCode::Backspace => { self.tool_feedback.pop(); }
                    KeyCode::Up => self.tool_scroll = self.tool_scroll.saturating_sub(1),
                    KeyCode::Down => self.tool_scroll = self.tool_scroll.saturating_add(1),
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
            KeyCode::Char('g') if ctrl => self.needs_capture = true,
            KeyCode::Char('v') if ctrl => { let _ = self.cmd_tx.send(EngineCommand::PasteClipboard); }
            KeyCode::Char('f') if ctrl => {
                self.file_path_input.clear();
                self.modal = ActiveModal::AddFile;
            }
            KeyCode::Char('x') if ctrl => { let _ = self.cmd_tx.send(EngineCommand::ClearImages); }
            KeyCode::Char('l') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::RefreshSessions);
                self.modal = ActiveModal::Sessions;
            }
            KeyCode::Char('r') if ctrl => { let _ = self.cmd_tx.send(EngineCommand::ResetChat); }
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
            KeyCode::F(1) => self.modal = ActiveModal::Debug,
            
            // Global chat scrolling
            KeyCode::Up if !ctrl => self.chat_scroll = self.chat_scroll.saturating_sub(1),
            KeyCode::Down if !ctrl => self.chat_scroll = self.chat_scroll.saturating_add(1),
            
            // Internal scrolling of the selected contained message
            KeyCode::Up if ctrl => self.msg_scroll = self.msg_scroll.saturating_sub(1),
            KeyCode::Down if ctrl => self.msg_scroll = self.msg_scroll.saturating_add(1),
            
            // Navigate selected message
            KeyCode::Tab => {
                let count = self.messages.len() + if self.is_streaming { 1 } else { 0 };
                if count > 0 {
                    let current = self.selected_msg_idx.unwrap_or(count.saturating_sub(1));
                    self.selected_msg_idx = Some((current + 1) % count);
                    self.msg_scroll = 0;
                }
            }
            KeyCode::BackTab => {
                let count = self.messages.len() + if self.is_streaming { 1 } else { 0 };
                if count > 0 {
                    let current = self.selected_msg_idx.unwrap_or(0);
                    self.selected_msg_idx = Some(if current == 0 { count.saturating_sub(1) } else { current - 1 });
                    self.msg_scroll = 0;
                }
            }

            KeyCode::Enter => self.input_buffer.push('\n'),
            KeyCode::Backspace => { self.input_buffer.pop(); }
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
        self.chat_scroll = 0;
        self.selected_msg_idx = None;
        self.msg_scroll = 0;
        let sys = self.config.system_presets[self.sys_preset_idx].prompt.clone();
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
            let sys = self.config.system_presets[self.sys_preset_idx].prompt.clone();
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
        f.render_widget(Paragraph::new(format!(" {}", self.status_bar.0)).style(status_style), root[1]);

        match self.modal {
            ActiveModal::AddFile => self.draw_file_modal(f),
            ActiveModal::Sessions => self.draw_sessions_modal(f),
            ActiveModal::Debug => self.draw_debug_modal(f),
            ActiveModal::ToolApproval => self.draw_tool_modal(f),
            ActiveModal::None => {}
        }
    }

    fn draw_sidebar(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(10), Constraint::Min(6), Constraint::Length(10)])
            .split(area);

        let preset_name = &self.config.presets[self.preset_idx].name;
        let sys_name = &self.config.system_presets[self.sys_preset_idx].name;
        let settings_text = vec![
            Line::from(vec![Span::raw("Model:   "), Span::styled(&self.model, Style::default().fg(Color::Green))]),
            Line::from(format!("Context: {}", self.num_ctx)),
            Line::from(format!("Tools:   {} (^T)", if self.enable_tools { "ON" } else { "OFF" })),
            Line::from(format!("Preset:  {} (^P)", preset_name)),
            Line::from(format!("System:  {} (^Y)", sys_name)),
        ];
        f.render_widget(
            Paragraph::new(settings_text).block(Block::default().borders(Borders::ALL).title(" Config ")),
            chunks[0],
        );

        let items: Vec<ListItem> = if self.attachments.is_empty() {
            vec![ListItem::new("No images attached").style(Style::default().fg(Color::DarkGray))]
        } else {
            self.attachments
                .iter()
                .map(|a| ListItem::new(format!("• {} ({}x{}, {}KB)", a.name, a.dimensions.0, a.dimensions.1, a.size_kb)))
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
            Line::from("^G Capture  ^V Paste"),
            Line::from("^F Add File ^X Clear Imgs"),
            Line::from("^S Send     ^L Sessions"),
            Line::from("^R Reset    F1 Debug API"),
            Line::from("Up/Dn Scroll Chat"),
            Line::from("^Up/^Dn Scroll Msg"),
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

        let mut lines = Vec::new();
        let max_lines = 15; // Max height to view at once for a single message

        for (i, m) in self.messages.iter().enumerate() {
            let is_selected = Some(i) == self.selected_msg_idx;
            
            let (tag, color) = match m.role.as_str() {
                "user" => ("USER", Color::Cyan),
                "assistant" => ("ARCHON", Color::Green),
                "tool" => ("TOOL", Color::Magenta),
                _ => ("SYS", Color::Gray),
            };
            let img_badge = m.images.as_ref().map(|v| format!(" [+{} img]", v.len())).unwrap_or_default();
            
            let prefix = if is_selected { "▶ " } else { "  " };
            lines.push(Line::from(Span::styled(
                format!("{prefix}━━ {tag}{img_badge} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));

            let msg_lines: Vec<&str> = m.content.lines().collect();

            if msg_lines.len() > max_lines {
                let scroll = if is_selected { self.msg_scroll } else { 0 };
                let max_scroll = msg_lines.len().saturating_sub(max_lines);
                
                let scroll = scroll.min(max_scroll);
                if is_selected {
                    self.msg_scroll = scroll;
                }

                let start = scroll;
                let end = (start + max_lines).min(msg_lines.len());

                if start > 0 {
                    lines.push(Line::from(Span::styled(format!("    ... ({} lines above, Ctrl+Up to scroll) ...", start), Style::default().fg(Color::DarkGray))));
                }
                
                for l in &msg_lines[start..end] {
                    lines.push(Line::from(format!("  {l}")));
                }
                
                if end < msg_lines.len() {
                    let remaining = msg_lines.len() - end;
                    lines.push(Line::from(Span::styled(format!("    ... ({} lines below, Ctrl+Down to scroll) ...", remaining), Style::default().fg(Color::DarkGray))));
                }
            } else {
                for l in msg_lines {
                    lines.push(Line::from(format!("  {l}")));
                }
            }
            lines.push(Line::from(""));
        }

        if self.is_streaming {
            let i = self.messages.len();
            let is_selected = Some(i) == self.selected_msg_idx;
            let prefix = if is_selected { "▶ " } else { "  " };
            
            lines.push(Line::from(Span::styled(
                format!("{prefix}━━ ARCHON (streaming...) "),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));
            
            let msg_lines: Vec<&str> = self.streaming_text.lines().collect();

            if msg_lines.len() > max_lines {
                let scroll = if is_selected { self.msg_scroll } else { 0 };
                let max_scroll = msg_lines.len().saturating_sub(max_lines);
                let scroll = scroll.min(max_scroll);
                if is_selected {
                    self.msg_scroll = scroll;
                }

                let start = scroll;
                let end = (start + max_lines).min(msg_lines.len());

                if start > 0 {
                    lines.push(Line::from(Span::styled(format!("    ... ({} lines above, Ctrl+Up to scroll) ...", start), Style::default().fg(Color::DarkGray))));
                }
                for l in &msg_lines[start..end] {
                    lines.push(Line::from(format!("  {l}")));
                }
                if end < msg_lines.len() {
                    let remaining = msg_lines.len() - end;
                    lines.push(Line::from(Span::styled(format!("    ... ({} lines below, Ctrl+Down to scroll) ...", remaining), Style::default().fg(Color::DarkGray))));
                }
            } else {
                for l in msg_lines {
                    lines.push(Line::from(format!("  {l}")));
                }
            }
            lines.push(Line::from(""));
        }

        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.chat_scroll, 0))
                .block(Block::default().borders(Borders::ALL).title(" Conversation (Tab: Select Msg, Ctrl+Up/Dn: Scroll Msg) ")),
            chunks[0],
        );

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
            .block(Block::default().borders(Borders::ALL).title(" Add Image File ")),
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
                ListItem::new(format!("{prefix}[{}] {} ({} msgs)", s.updated_at, s.title, s.messages.len()))
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

    fn draw_debug_modal(&self, f: &mut Frame) {
        let area = centered_rect(80, 80, f.area());
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(self.debug_json.as_str())
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" Last API Request Payload (Esc to Close) ")),
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
                        .title(format!(" ⚠️ Tool Execution Approval Required ({idx}/{total}) ")),
                ),
            area,
        );
    }
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
