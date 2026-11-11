use super::{preset_label, titled_block, ActiveModal, TuiApp};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

fn wrap_text(content: &str, options: &textwrap::Options<'_>) -> Vec<String> {
    content
        .lines()
        .flat_map(|line| {
            if line.is_empty() {
                vec![String::new()]
            } else {
                textwrap::wrap(line, options)
                    .into_iter()
                    .map(|c| c.into_owned())
                    .collect()
            }
        })
        .collect()
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

impl TuiApp {
    pub(super) fn draw(&mut self, f: &mut Frame) {
        let root_constraints = if self.show_help_bar {
            vec![
                Constraint::Min(10),
                Constraint::Length(1),
                Constraint::Length(1),
            ]
        } else {
            vec![Constraint::Min(10), Constraint::Length(1)]
        };

        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints(root_constraints)
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
        self.draw_status_bar(f, root[1]);

        if self.show_help_bar {
            self.draw_help_bar(f, root[2]);
        }

        match self.modal {
            ActiveModal::AddFile => self.draw_file_modal(f),
            ActiveModal::Sessions => self.draw_sessions_modal(f),
            ActiveModal::ToolApproval => self.draw_tool_modal(f),
            ActiveModal::None => {}
        }
    }

    fn draw_status_bar(&self, f: &mut Frame, area: Rect) {
        let status_style = if self.status_bar.1 {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };

        let status_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(10), Constraint::Length(16)])
            .split(area);

        f.render_widget(
            Paragraph::new(format!(" {}", self.status_bar.0)).style(status_style),
            status_chunks[0],
        );

        let help_toggle_label = if self.show_help_bar {
            "[?] Hide Help "
        } else {
            "[?] Show Help "
        };
        f.render_widget(
            Paragraph::new(help_toggle_label).style(Style::default().fg(Color::DarkGray)),
            status_chunks[1],
        );
    }

    fn draw_help_bar(&self, f: &mut Frame, area: Rect) {
        let key_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let desc_style = Style::default().fg(Color::Gray);
        let sep_style = Style::default().fg(Color::DarkGray);

        let bindings = [
            ("^⏎", "Send"),
            ("^B", "Sidebar"),
            ("^S", "Sessions"),
            ("^R", "New"),
            ("^V", "Paste Img"),
            ("^F", "Add Img"),
            ("^X", "Clear Imgs"),
            ("^T", "Tools"),
            ("^P", "Preset"),
            ("^Y", "SysPrompt"),
            ("^Q", "Quit"),
            ("?", "Toggle Bar"),
        ];

        let mut spans = vec![Span::raw(" ")];
        for (idx, (key, desc)) in bindings.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::styled(" │ ", sep_style));
            }
            spans.push(Span::styled(*key, key_style));
            spans.push(Span::styled(format!(" {desc}"), desc_style));
        }

        f.render_widget(Paragraph::new(Line::from(spans)), area);
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
            List::new(items)
                .block(titled_block(format!(" Images ({}) ", self.attachments.len()))),
            chunks[1],
        );
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

        let options = textwrap::Options::new(available_width).break_words(true);

        if self.last_wrap_width != available_width {
            self.wrapped_messages.clear();
            self.last_wrap_width = available_width;
        }

        if self.wrapped_messages.len() > self.messages.len() {
            self.wrapped_messages.truncate(self.messages.len());
        }

        for m in &self.messages[self.wrapped_messages.len()..] {
            self.wrapped_messages.push(wrap_text(&m.content, &options));
        }

        let mut lines = Vec::new();

        for (i, (m, wrapped_content)) in self
            .messages
            .iter()
            .zip(self.wrapped_messages.iter())
            .enumerate()
        {
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

            render_message_block(
                i,
                &format!("{tag}{img_badge}"),
                color,
                m.truncated,
                wrapped_content,
                self.msg_unfolded[i],
                self.config.app.max_msg_lines,
                &mut self.msg_scrolls[i],
                &mut lines,
                &mut self.msg_visual_ranges,
            );
        }

        if self.is_streaming {
            let i = self.messages.len();
            let stream_lines = wrap_text(&self.streaming_text, &options);

            render_message_block(
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
}
