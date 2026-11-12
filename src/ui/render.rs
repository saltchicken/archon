use super::{ActiveModal, MsgUiState, TuiApp, preset_label, titled_block};
use crate::{
    core::session::{ChatMessage, truncate_str},
    tools::ToolCall,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use textwrap::core::display_width;

#[derive(Clone, Copy, PartialEq)]
enum ColAlign {
    Left,
    Center,
    Right,
}

pub(super) fn sanitize_line(s: &str) -> String {
    s.replace(['\r', '\u{fe0f}'], "").replace('\t', "    ")
}

fn format_tool_call_summary(c: &ToolCall) -> String {
    let args = serde_json::to_string(&c.function.arguments).unwrap_or_default();
    format!("{}({})", c.function.name, truncate_str(&args, 80))
}

fn format_assistant_display(m: &ChatMessage, resolved_tool_count: usize) -> String {
    let mut text = sanitize_line(&m.content);
    if let Some(calls) = m
        .tool_calls
        .as_ref()
        .filter(|c| resolved_tool_count < c.len())
    {
        if !text.trim().is_empty() {
            text.push_str("\n\n");
        }
        let pending: Vec<String> = calls[resolved_tool_count..]
            .iter()
            .map(|c| format!("⚙ [Pending Tool Approval: {}]", format_tool_call_summary(c)))
            .collect();
        text.push_str(&pending.join("\n"));
    }
    text
}

fn find_associated_tool_call(messages: &[ChatMessage], tool_msg_idx: usize) -> Option<&ToolCall> {
    let mut offset = 0usize;
    for m in messages[..tool_msg_idx].iter().rev() {
        match m.role.as_str() {
            "tool" => offset += 1,
            "assistant" => return m.tool_calls.as_ref()?.get(offset),
            _ => break,
        }
    }
    None
}

fn is_table_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') || (t.ends_with('|') && t.contains('|'))
}

fn parse_table_cells(line: &str) -> Vec<String> {
    let s = line.trim();
    let s = s.strip_prefix('|').unwrap_or(s);
    let s = s.strip_suffix('|').unwrap_or(s);

    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            cur.push(chars.next().unwrap());
        } else if c == '|' {
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

fn parse_separator_row(cells: &[String]) -> Option<Vec<ColAlign>> {
    if cells.is_empty() {
        return None;
    }
    cells
        .iter()
        .map(|cell| {
            let t = cell.trim();
            let core = t.trim_matches(':');
            if core.is_empty() || !core.chars().all(|c| c == '-') {
                return None;
            }
            Some(match (t.starts_with(':'), t.ends_with(':')) {
                (true, true) => ColAlign::Center,
                (false, true) => ColAlign::Right,
                _ => ColAlign::Left,
            })
        })
        .collect()
}

fn pad_cell(text: &str, width: usize, align: ColAlign) -> String {
    let pad = width.saturating_sub(display_width(text));
    match align {
        ColAlign::Left => format!("{text}{}", " ".repeat(pad)),
        ColAlign::Right => format!("{}{text}", " ".repeat(pad)),
        ColAlign::Center => format!("{}{text}{}", " ".repeat(pad / 2), " ".repeat(pad - pad / 2)),
    }
}

fn draw_table_border(col_widths: &[usize], left: char, mid: char, right: char) -> String {
    let segments: Vec<String> = col_widths.iter().map(|&w| "─".repeat(w + 2)).collect();
    format!("{left}{}{right}", segments.join(&mid.to_string()))
}

fn format_markdown_table(raw_lines: &[&str], available_width: usize) -> Option<Vec<String>> {
    if raw_lines.len() < 2 {
        return None;
    }

    let parsed_rows: Vec<Vec<String>> = raw_lines.iter().map(|l| parse_table_cells(l)).collect();
    let mut aligns = parse_separator_row(&parsed_rows[1])?;
    let num_cols = parsed_rows.iter().map(Vec::len).max().filter(|&n| n > 0)?;
    aligns.resize(num_cols, ColAlign::Left);

    let mut col_widths = vec![1usize; num_cols];
    for (_, row) in parsed_rows.iter().enumerate().filter(|&(i, _)| i != 1) {
        for (c_idx, cell) in row.iter().enumerate() {
            col_widths[c_idx] = col_widths[c_idx].max(display_width(cell));
        }
    }

    let overhead = 3 * num_cols + 1;
    if col_widths.iter().sum::<usize>() + overhead > available_width
        && available_width > overhead + num_cols
    {
        let target_sum = available_width - overhead;
        while col_widths.iter().sum::<usize>() > target_sum {
            let Some((max_idx, &max_val)) = col_widths.iter().enumerate().max_by_key(|&(_, &w)| w)
            else {
                break;
            };
            if max_val <= 3 {
                break;
            }
            col_widths[max_idx] -= 1;
        }
    }

    let mut out = vec![draw_table_border(&col_widths, '┌', '┬', '┐')];
    for (r_idx, row) in parsed_rows.iter().enumerate() {
        if r_idx == 1 {
            out.push(draw_table_border(&col_widths, '├', '┼', '┤'));
            continue;
        }

        let wrapped_cells: Vec<Vec<String>> = (0..num_cols)
            .map(|c_idx| {
                let text = row.get(c_idx).map_or("", String::as_str);
                wrap_single_line(
                    text,
                    &textwrap::Options::new(col_widths[c_idx].max(1)).break_words(true),
                )
            })
            .collect();
        let max_sublines = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1);

        for sub in 0..max_sublines {
            let mut line = String::from("│");
            for c_idx in 0..num_cols {
                let part = wrapped_cells[c_idx].get(sub).map_or("", String::as_str);
                line.push_str(&format!(
                    " {} │",
                    pad_cell(part, col_widths[c_idx], aligns[c_idx])
                ));
            }
            out.push(line);
        }
    }

    out.push(draw_table_border(&col_widths, '└', '┴', '┘'));
    Some(out)
}

fn wrap_single_line(line: &str, options: &textwrap::Options<'_>) -> Vec<String> {
    if line.is_empty() {
        vec![String::new()]
    } else {
        textwrap::wrap(line, options)
            .into_iter()
            .map(|c| c.into_owned())
            .collect()
    }
}

fn flush_table_buffer<'a>(
    table_buf: &mut Vec<&'a str>,
    out: &mut Vec<String>,
    options: &textwrap::Options<'_>,
) {
    if table_buf.is_empty() {
        return;
    }
    if let Some(formatted) = format_markdown_table(table_buf, options.width) {
        out.extend(formatted);
    } else {
        for raw in table_buf.iter() {
            out.extend(wrap_single_line(raw, options));
        }
    }
    table_buf.clear();
}

fn wrap_text(content: &str, options: &textwrap::Options<'_>) -> Vec<String> {
    let mut out = Vec::new();
    let mut table_buf: Vec<&str> = Vec::new();
    let mut in_code_block = false;

    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            flush_table_buffer(&mut table_buf, &mut out, options);
            in_code_block = !in_code_block;
            out.extend(wrap_single_line(line, options));
        } else if !in_code_block && is_table_line(line) {
            table_buf.push(line);
        } else {
            flush_table_buffer(&mut table_buf, &mut out, options);
            out.extend(wrap_single_line(line, options));
        }
    }
    flush_table_buffer(&mut table_buf, &mut out, options);
    out
}

fn style_inline_segments(text: &str, base_style: Style) -> Vec<Span<'static>> {
    text.split('`')
        .enumerate()
        .filter(|(_, seg)| !seg.is_empty())
        .map(|(idx, seg)| {
            if idx % 2 == 1 {
                Span::styled(format!("`{seg}`"), base_style.fg(Color::Yellow))
            } else {
                Span::styled(seg.to_string(), base_style)
            }
        })
        .collect()
}

fn style_table_row(line: &str, is_header: bool) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("  ")];
    let border_style = Style::default().fg(Color::DarkGray);
    let cell_style = if is_header {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    for (idx, part) in line.split('│').enumerate() {
        if idx > 0 {
            spans.push(Span::styled("│", border_style));
        }
        spans.extend(style_inline_segments(part, cell_style));
    }
    spans
}

fn style_inline_markdown(line: &str) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("  ")];
    let trimmed = line.trim_start();

    let whole_line_style = if trimmed.starts_with("⚙ [") {
        Some(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        )
    } else if trimmed.starts_with('#') {
        Some(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    } else if trimmed.starts_with(['┌', '├', '└']) {
        Some(Style::default().fg(Color::DarkGray))
    } else {
        None
    };

    if let Some(style) = whole_line_style {
        spans.push(Span::styled(line.to_string(), style));
    } else {
        spans.extend(style_inline_segments(line, Style::default()));
    }
    spans
}

fn style_lines(content_lines: &[String]) -> Vec<Line<'static>> {
    let mut in_code_block = false;
    let mut in_table_header = false;
    let mut out = Vec::with_capacity(content_lines.len());

    for line in content_lines {
        let trimmed = line.trim_start();
        let is_fence = trimmed.starts_with("```");

        if !in_code_block {
            if trimmed.starts_with('┌') {
                in_table_header = true;
            } else if trimmed.starts_with(['├', '└']) {
                in_table_header = false;
            }
        }

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
        } else if trimmed.starts_with('│') && trimmed.ends_with('│') {
            style_table_row(line, in_table_header)
        } else {
            style_inline_markdown(line)
        };
        out.push(Line::from(formatted));

        if is_fence {
            in_code_block = !in_code_block;
            in_table_header = false;
        }
    }
    out
}

fn render_message_block(
    idx: usize,
    header_label: &str,
    color: Color,
    truncated: bool,
    styled_content: &[Line<'static>],
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
    let mut header_spans = vec![Span::styled(
        format!("━━ {header_label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    if truncated {
        header_spans.push(Span::styled(
            "⚠ TRUNCATED",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(header_spans));

    let total_content = styled_content.len();
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
        lines.extend_from_slice(&styled_content[start..end]);
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
        lines.extend_from_slice(styled_content);
    }

    lines.push(Line::from(""));
    ranges.push((idx, start_visual, lines.len(), is_truncated));
}

fn is_message_header_line(line: &Line<'_>) -> bool {
    line.spans
        .first()
        .is_some_and(|s| s.content.starts_with("━━ "))
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
            ActiveModal::ModelSelect => self.draw_model_modal(f),
            ActiveModal::ContextWindow => self.draw_context_modal(f),
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
            Paragraph::new(format!(" {}", sanitize_line(&self.status_bar.0))).style(status_style),
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
            ("⏎", "Send"),
            ("⇧⏎", "Newline"),
            ("^M", "Model"),
            ("^W", "Ctx"),
            ("^B", "Sidebar"),
            ("^S", "Sessions"),
            ("^R", "New"),
            ("^V", "Paste Img"),
            ("^f", "Add Img"),
            ("^X", "Clear Imgs"),
            ("^T", "Tools"),
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
            .constraints([Constraint::Length(9), Constraint::Min(6)])
            .split(area);

        let (pct, ctx_color) = self.ctx_usage(Color::Cyan);
        let settings_text = vec![
            Line::from(vec![
                Span::raw("Model:   "),
                Span::styled(&self.model, Style::default().fg(Color::Green)),
                Span::raw(" (^M)"),
            ]),
            Line::from(vec![
                Span::raw("Context: "),
                Span::styled(
                    format!("{}/{} ({pct:.0}%)", self.used_tokens, self.num_ctx),
                    Style::default().fg(ctx_color),
                ),
                Span::raw(" (^W)"),
            ]),
            Line::from(format!(
                "Tools:   {} ({}KB max) (^T)",
                if self.enable_tools { "ON" } else { "OFF" },
                self.max_tool_output_size / 1024
            )),
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
            List::new(items).block(titled_block(format!(" Images ({}) ", self.attachments.len()))),
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
            self.styled_messages.clear();
            self.stream_dirty = true;
            self.last_wrap_width = available_width;
        }

        if self.styled_messages.len() != self.messages.len() {
            self.styled_messages.clear();
            for (i, m) in self.messages.iter().enumerate() {
                let formatted = if m.role == "assistant" {
                    let resolved_count = self.messages[i + 1..]
                        .iter()
                        .take_while(|n| n.role == "tool")
                        .count();
                    format_assistant_display(m, resolved_count)
                } else {
                    sanitize_line(&m.content)
                };

                if formatted.trim().is_empty() {
                    self.styled_messages.push(Vec::new());
                } else {
                    self.styled_messages
                        .push(style_lines(&wrap_text(&formatted, &options)));
                }
            }
        }

        let mut lines = Vec::new();
        let max_msg_lines = self.config.app.max_msg_lines;

        for (i, (m, styled_content)) in self
            .messages
            .iter()
            .zip(self.styled_messages.iter())
            .enumerate()
        {
            if m.role == "assistant" && styled_content.is_empty() {
                continue;
            }

            let (header_label, color) = match m.role.as_str() {
                "user" => {
                    let badge = m
                        .images
                        .as_ref()
                        .map_or_else(String::new, |v| format!(" [+{} img]", v.len()));
                    (format!("USER{badge}"), Color::Cyan)
                }
                "assistant" => ("ARCHON".to_string(), Color::Green),
                "tool" => {
                    let summary = find_associated_tool_call(&self.messages, i)
                        .map_or_else(|| "tool".to_string(), format_tool_call_summary);
                    (sanitize_line(&format!("⚙ TOOL [{summary}]")), Color::Yellow)
                }
                _ => ("SYS".to_string(), Color::Gray),
            };

            render_message_block(
                i,
                &header_label,
                color,
                m.truncated,
                styled_content,
                max_msg_lines,
                &mut self.msg_states[i],
                &mut lines,
                &mut self.msg_visual_ranges,
            );
        }

        if self.is_streaming {
            let i = self.messages.len();
            self.msg_states[i].unfolded = true;

            if self.stream_dirty {
                let sanitized_stream = sanitize_line(&self.streaming_text);
                self.styled_stream = if sanitized_stream.trim().is_empty() {
                    Vec::new()
                } else {
                    style_lines(&wrap_text(&sanitized_stream, &options))
                };
                self.stream_dirty = false;
            }

            render_message_block(
                i,
                "ARCHON (...)",
                Color::Yellow,
                false,
                &self.styled_stream,
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

        let sticky_header_line = (self.chat_scroll > 0)
            .then(|| {
                lines[..self.chat_scroll]
                    .iter()
                    .rev()
                    .find(|l| is_message_header_line(l))
                    .cloned()
            })
            .flatten();

        let visible_end = (self.chat_scroll + self.chat_view_height()).min(lines.len());
        let mut visible_lines: Vec<Line<'static>> = if self.chat_scroll < visible_end {
            lines.drain(self.chat_scroll..visible_end).collect()
        } else {
            Vec::new()
        };

        if let Some(sticky) = sticky_header_line
            && visible_lines
                .first()
                .is_some_and(|first| !is_message_header_line(first) && !first.spans.is_empty())
        {
            visible_lines[0] = sticky;
        }

        let (pct, ctx_color) = self.ctx_usage(Color::DarkGray);
        let title_line = Line::from(vec![
            Span::raw(format!(" Conversation [{}] ", self.model)),
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
        let mut scrollbar_state = ScrollbarState::new(max_scroll).position(self.chat_scroll);
        f.render_stateful_widget(scrollbar, chunks[0], &mut scrollbar_state);

        f.render_widget(&self.text_area, chunks[1]);
    }
}
