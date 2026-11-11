use super::{preset_label, titled_block, ActiveModal, MsgUiState, TuiApp};
use crate::core::{
    ollama::strip_thinking,
    session::{truncate_str, ChatMessage},
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

fn format_message_display(m: &ChatMessage, show_thinking: bool) -> String {
    let mut text = if show_thinking {
        m.content.clone()
    } else {
        strip_thinking(&m.content).0
    };

    if let Some(calls) = m.tool_calls.as_ref().filter(|c| !c.is_empty()) {
        if !text.trim().is_empty() {
            text.push_str("\n\n");
        }
        let call_lines: Vec<String> = calls
            .iter()
            .map(|c| {
                let args = serde_json::to_string(&c.function.arguments).unwrap_or_default();
                format!(
                    "⚙ [Tool Call: {}({})]",
                    c.function.name,
                    truncate_str(&args, 80)
                )
            })
            .collect();
        text.push_str(&call_lines.join("\n"));
    }
    text
}

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

fn style_inline_markdown(line: &str) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("  ")];
    let trimmed = line.trim_start();

    if trimmed.starts_with("⚙ [Tool Call:") {
        spans.push(Span::styled(
            line.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        ));
        return spans;
    }

    if trimmed.starts_with('#') {
        spans.push(Span::styled(
            line.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        return spans;
    }

    for (idx, segment) in line.split('`').enumerate() {
        if !segment.is_empty() {
            if idx % 2 == 1 {
                spans.push(Span::styled(
                    format!("`{segment}`"),
                    Style::default().fg(Color::Yellow),
                ));
            } else {
                spans.push(Span::raw(segment.to_string()));
            }
        }
    }

    spans
}

fn format_visible_lines(
    content_lines: &[String],
    start: usize,
    end: usize,
) -> Vec<Line<'static>> {
    let mut in_code_block = false;
    let mut out = Vec::with_capacity(end.saturating_sub(start));

    for (idx, line) in content_lines.iter().enumerate().take(end) {
        let is_fence = line.trim_start().starts_with("```");
        if idx >= start {
            let formatted = if is_fence {
                vec![
                    Span::raw("  "),
                    Span::styled(
                        line.clone(),
                        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
                    ),
                ]
            } else if in_code_block {
                vec![
                    Span::styled("  │ ", Style::default().fg(Color::Blue)),
                    Span::styled(line.clone(), Style::default().fg(Color::Cyan)),
                ]
            } else {
                style_inline_markdown(line)
            };
            out.push(Line::from(formatted));
        }
        if is_fence {
            in_code_block = !in_code_block;
        }
    }

    out
}

fn render_message_block(
    idx: usize,
    header_label: &str,
    color: Color,
    truncated: bool,
    content_lines: &[String],
    max_folded_lines: usize,
    state: &mut MsgUiState,
    lines: &mut Vec<Line<'static>>,
    ranges: &mut Vec<(usize, usize, usize, bool)>,
) {
    let start_visual = lines.len();
    let max_lines = if state.unfolded {
        usize::MAX
    } else {
        max_folded_lines
    };
    let toggle_hint = if state.unfolded { " [Click to fold]" } else { "" };

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
        state.scroll = state.scroll.min(total_content.saturating_sub(max_lines));
        let start = state.scroll;
        let end = (start + max_lines).min(total_content);

        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("    ... ({start} lines above) ..."),
                Style::default().fg(Color::Blue),
            )));
        }
        lines.extend(format_visible_lines(content_lines, start, end));
        if end < total_content {
            lines.push(Line::from(Span::styled(
                format!(
                    "    ... ({} lines below - Click to unfold) ...",
                    total_content - end
                ),
                Style::default().fg(Color::Blue),
            )));
        }
    } else {
        lines.extend(format_visible_lines(content_lines, 0, total_content));
    }

    lines.push(Line::from(""));
    ranges.push((idx, start_visual, lines.len(), is_truncated));
}

impl TuiApp {
    pub(super) fn draw(&mut self, f: &mut Frame) {
        let mut root_constraints = vec![Constraint::Min(10), Constraint::Length(1)];
        if self.show_help_bar {
            root_constraints.push(Constraint::Length(1));
        }

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

        let help_label = if self.show_help_bar {
            "[?] Hide Help "
        } else {
            "[?] Show Help "
        };
        f.render_widget(
            Paragraph::new(help_label).style(Style::default().fg(Color::DarkGray)),
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
            ("PgUp/Dn", "Scroll"),
            ("^B", "Sidebar"),
            ("^S", "Sessions"),
            ("^R", "New"),
            ("^V", "Paste Img"),
            ("^F", "Add Img"),
            ("^X", "Clear Imgs"),
            ("^T", "Tools"),
            ("^K", "Think"),
            ("^P", "Preset"),
            ("^Y", "SysPrompt"),
            ("^Q", "Quit"),
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

        let toggle = |on| if on { "ON" } else { "OFF" };
        let (pct, ctx_color) = self.ctx_usage(Color::Cyan);

        let settings_text = vec![
            Line::from(vec![
                Span::raw("Model:   "),
                Span::styled(&self.model, Style::default().fg(Color::Green)),
            ]),
            Line::from(vec![
                Span::raw("Context: "),
                Span::styled(
                    format!("{}/{} ({pct:.0}%)", self.used_tokens, self.num_ctx),
                    Style::default().fg(ctx_color),
                ),
            ]),
            Line::from(format!("Tools:   {} (^T)", toggle(self.enable_tools))),
            Line::from(format!("Think:   {} (^K)", toggle(self.show_thinking))),
            Line::from(format!(
                "Preset:  {} (^P)",
                preset_label(&self.config.presets, self.preset_idx)
            )),
            Line::from(format!("System:  {} (^Y)", self.current_system_label())),
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

        self.msg_states
            .resize(self.messages.len() + 1, MsgUiState::default());
        self.msg_visual_ranges.clear();

        let options = textwrap::Options::new(available_width).break_words(true);

        if self.last_wrap_width != available_width {
            self.wrapped_messages.clear();
            self.last_wrap_width = available_width;
        }

        self.wrapped_messages.truncate(self.messages.len());
        for m in &self.messages[self.wrapped_messages.len()..] {
            let formatted = format_message_display(m, self.show_thinking);
            self.wrapped_messages.push(wrap_text(&formatted, &options));
        }

        let mut lines = Vec::new();
        let max_msg_lines = self.config.app.max_msg_lines;

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
                .map_or_else(String::new, |v| format!(" [+{} img]", v.len()));

            render_message_block(
                i,
                &format!("{tag}{img_badge}"),
                color,
                m.truncated,
                wrapped_content,
                max_msg_lines,
                &mut self.msg_states[i],
                &mut lines,
                &mut self.msg_visual_ranges,
            );
        }

        if self.is_streaming {
            let i = self.messages.len();
            let text_to_wrap = if self.show_thinking {
                self.streaming_text.clone()
            } else {
                let (mut stripped, is_thinking) = strip_thinking(&self.streaming_text);
                if is_thinking {
                    if !stripped.is_empty() && !stripped.ends_with('\n') {
                        stripped.push_str("\n\n");
                    }
                    stripped.push_str("[Thinking...]");
                }
                stripped
            };
            let stream_lines = wrap_text(&text_to_wrap, &options);

            render_message_block(
                i,
                "ARCHON (streaming...)",
                Color::Yellow,
                false,
                &stream_lines,
                max_msg_lines,
                &mut self.msg_states[i],
                &mut lines,
                &mut self.msg_visual_ranges,
            );
        }

        self.total_chat_lines = lines.len();
        let max_scroll = self.max_chat_scroll();
        self.chat_scroll = if self.follow_tail {
            max_scroll
        } else {
            self.chat_scroll.min(max_scroll)
        };

        let view_height = self.chat_view_height();
        let visible_end = (self.chat_scroll + view_height).min(lines.len());
        let visible_lines: Vec<Line<'static>> = if self.chat_scroll < visible_end {
            lines.drain(self.chat_scroll..visible_end).collect()
        } else {
            Vec::new()
        };

        let (pct, ctx_color) = self.ctx_usage(Color::DarkGray);
        let title_line = Line::from(vec![
            Span::raw(" Conversation "),
            Span::styled(
                format!("[Ctx: {} / {} ({pct:.0}%)] ", self.used_tokens, self.num_ctx),
                Style::default().fg(ctx_color).add_modifier(Modifier::BOLD),
            ),
        ]);

        f.render_widget(
            Paragraph::new(visible_lines).block(titled_block(title_line)),
            chunks[0],
        );

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));
        let mut scrollbar_state =
            ScrollbarState::new(max_scroll).position(self.chat_scroll);
        f.render_stateful_widget(scrollbar, chunks[0], &mut scrollbar_state);

        f.render_widget(&self.text_area, chunks[1]);
    }
}
