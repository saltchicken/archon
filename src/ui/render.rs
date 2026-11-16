use super::{ActiveModal, MsgUiState, SlashCommand, TuiApp, chat_block, titled_block};
use crate::{
    core::session::{ChatMessage, truncate_str},
    tools::{self, ToolCall},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        List, ListItem, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    },
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

fn format_param_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => truncate_str(&s.replace('\n', "\\n"), 120),
        serde_json::Value::Array(arr) => {
            if arr.iter().all(serde_json::Value::is_object) {
                format!("[{} items]", arr.len())
            } else {
                let s = serde_json::to_string(v).unwrap_or_default();
                truncate_str(&s.replace('\n', "\\n"), 120)
            }
        }
        _ => {
            let s = serde_json::to_string(v).unwrap_or_default();
            truncate_str(&s.replace('\n', "\\n"), 120)
        }
    }
}

fn format_tool_display(messages: &[ChatMessage], tool_msg_idx: usize, m: &ChatMessage) -> String {
    let body = sanitize_line(&m.content);
    let Some(call) = find_associated_tool_call(messages, tool_msg_idx) else {
        return body;
    };
    let Some(obj) = call
        .function
        .arguments
        .as_object()
        .filter(|o| !o.is_empty())
    else {
        return body;
    };

    let mut param_lines = vec![String::from("Parameters:")];
    for (k, v) in obj {
        let val_str = sanitize_line(&format_param_value(v));
        param_lines.push(format!("  {k}: {val_str}"));
    }
    let params_block = param_lines.join("\n");

    if body.trim().is_empty() {
        params_block
    } else {
        format!("{params_block}\n\n{body}")
    }
}

pub(super) fn format_assistant_display(m: &ChatMessage, _resolved_tool_count: usize) -> String {
    let mut text = sanitize_line(&m.content);
    if text.trim().is_empty()
        && m.tool_calls.as_ref().is_none_or(Vec::is_empty)
        && m.thoughts.as_ref().is_some_and(|t| !t.is_empty())
    {
        text = String::from("*[Model produced internal thoughts without a final response]*");
    }
    text
}

fn find_associated_tool_call(messages: &[ChatMessage], tool_msg_idx: usize) -> Option<&ToolCall> {
    let mut offset = 0usize;
    for m in messages[..tool_msg_idx].iter().rev() {
        match m.role.as_str() {
            "tool" => offset += 1,
            "error" => continue,
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

fn flush_table_buffer(
    table_buf: &mut Vec<&str>,
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
            out.push(line.trim().to_string());
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
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
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
        Some(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
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
    let mut code_fence_width = 7usize;
    let mut in_table_header = false;
    let mut in_params_block = false;
    let mut out = Vec::with_capacity(content_lines.len());
    let border_style = Style::default().fg(Color::Blue);

    for line in content_lines {
        let trimmed = line.trim_start();
        let is_fence = trimmed.starts_with("```");

        if !in_code_block {
            if trimmed == "Parameters:" {
                in_params_block = true;
            } else if trimmed.is_empty() {
                in_params_block = false;
            }

            if trimmed.starts_with('┌') {
                in_table_header = true;
            } else if trimmed.starts_with(['├', '└']) {
                in_table_header = false;
            }
        }

        let formatted = if is_fence {
            in_params_block = false;
            if !in_code_block {
                let lang = trimmed.trim_start_matches('`').trim();
                if lang.is_empty() {
                    code_fence_width = 7;
                    vec![
                        Span::raw("  "),
                        Span::styled(format!("╭{}", "─".repeat(code_fence_width)), border_style),
                    ]
                } else {
                    code_fence_width = display_width(lang) + 6;
                    vec![
                        Span::raw("  "),
                        Span::styled("╭─ ", border_style),
                        Span::styled(lang.to_string(), border_style.add_modifier(Modifier::BOLD)),
                        Span::styled(" ───", border_style),
                    ]
                }
            } else {
                vec![
                    Span::raw("  "),
                    Span::styled(format!("╰{}", "─".repeat(code_fence_width)), border_style),
                ]
            }
        } else if in_code_block {
            vec![
                Span::styled("  │ ", border_style),
                Span::styled(line.clone(), Style::default().fg(Color::Cyan)),
            ]
        } else if in_params_block {
            let style = if trimmed == "Parameters:" {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Yellow)
            };
            vec![Span::raw("  "), Span::styled(line.clone(), style)]
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

fn is_message_header_line(line: &Line<'_>) -> bool {
    line.spans
        .first()
        .is_some_and(|s| s.content.starts_with("━━ "))
}

fn image_badge(m: &ChatMessage) -> String {
    m.images
        .as_ref()
        .map_or_else(String::new, |v| format!(" [+{} img]", v.len()))
}

impl TuiApp {
    /// Whether the next *rendered* block after message `i` is a tool call.
    ///
    /// The render loop skips assistant messages that have no content, so we
    /// walk past those to find the block that will actually be drawn.
    fn next_rendered_is_tool(&self, i: usize) -> bool {
        for j in (i + 1)..self.messages.len() {
            let m = &self.messages[j];
            if m.role == "assistant" && self.styled_messages.get(j).is_some_and(Vec::is_empty) {
                continue;
            }
            return matches!(m.role.as_str(), "tool" | "hook");
        }
        false
    }

    fn render_message_block(
        &mut self,
        idx: usize,
        header_label: &str,
        color: Color,
        truncated: bool,
        styled_content: &[Line<'static>],
        lines: &mut Vec<Line<'static>>,
        compact: bool,
    ) {
        let start_visual = lines.len();
        let state = &mut self.msg_states[idx];
        let total_content = styled_content.len();
        let max_lines = if state.unfolded {
            usize::MAX
        } else {
            self.config.app.max_msg_lines
        };

        let mut header_spans = vec![Span::styled(
            if header_label.starts_with("⚙ TOOL") || header_label.starts_with("⚡ HOOK") {
                format!("  {header_label} ")
            } else {
                format!("━━ {header_label} ")
            },
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        if truncated {
            header_spans.push(Span::styled(
                "⚠ TRUNCATED ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        if state.collapsed {
            header_spans.push(Span::styled(
                format!("[+{total_content}]"),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        lines.push(Line::from(header_spans));

        let is_truncated = !state.collapsed && total_content > max_lines;

        if !state.collapsed {
            if is_truncated {
                state.scroll = state.scroll.min(total_content.saturating_sub(max_lines));
                let start = state.scroll;
                let end = (start + max_lines).min(total_content);

                if start > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("    ... ({start}) ..."),
                        Style::default().fg(Color::Blue),
                    )));
                } else {
                    lines.push(Line::from(""));
                }
                lines.extend_from_slice(&styled_content[start..end]);
                if end < total_content {
                    lines.push(Line::from(Span::styled(
                        format!("    ... ({}) ...", total_content - end),
                        Style::default().fg(Color::Blue),
                    )));
                }
            } else {
                lines.push(Line::from(""));
                lines.extend_from_slice(styled_content);
            }
        } else {
            lines.push(Line::from(""));
        }

        // Blank line(s) between the end of this message and the next header.
        // Add/remove blank lines here to control the gap.
        // Consecutive tool calls are grouped tightly (no blank line) so they
        // don't carry the same visual weight as the main message headers.
        if !compact {
            lines.push(Line::from(""));
            lines.push(Line::from(""));
        }
        self.msg_visual_ranges
            .push((idx, start_visual, lines.len(), is_truncated));
    }

    pub(super) fn draw(&mut self, f: &mut Frame) {
        self.term_area = f.area();
        let sidebar_width = if self.sidebar_expanded { 36 } else { 0 };
        let main_panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Min(40)])
            .split(self.term_area);

        if self.sidebar_expanded {
            self.draw_sidebar(f, main_panes[0]);
        }
        self.draw_chat_area(f, main_panes[1]);

        match self.modal {
            ActiveModal::AddFile => self.draw_file_modal(f),
            ActiveModal::Sessions => self.draw_sessions_modal(f),
            ActiveModal::ToolApproval => self.draw_tool_modal(f),
            ActiveModal::TruncatedTool => self.draw_truncated_tool_modal(f),
            ActiveModal::ModelSelect => self.draw_model_modal(f),
            ActiveModal::SystemPrompt => self.draw_system_prompt_modal(f),
            ActiveModal::Memory => self.draw_memory_modal(f),
            ActiveModal::PayloadInspector => self.draw_payload_inspector_modal(f),
            ActiveModal::None => {}
        }

        if let Some(ref menu) = self.context_menu {
            self.draw_context_menu(f, menu);
        }
    }

    fn draw_sidebar(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(11), Constraint::Min(6)])
            .split(area);

        let pct = (self.used_tokens as f64 / f64::from(self.num_ctx.max(1))) * 100.0;
        let ctx_color = if pct >= 90.0 {
            Color::Red
        } else if pct >= 75.0 {
            Color::Yellow
        } else {
            Color::Cyan
        };
        let dir = self
            .config
            .workspace
            .as_deref()
            .map(|ws| crate::core::config::format_tilde_path(std::path::Path::new(ws)))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| crate::core::config::format_tilde_path(&p))
                    .unwrap_or_else(|_| "unknown".into())
            });
        let settings_text = vec![
            Line::from(vec![
                Span::raw("Dir:     "),
                Span::styled(dir, Style::default().fg(Color::Cyan)),
            ]),
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
            ]),
            Line::from(format!(
                "Tools:   {} ({}KB max) (^T)",
                if self.enable_tools { "ON" } else { "OFF" },
                self.max_tool_output_size / 1024
            )),
            Line::from(format!("Skills:  {} loaded", self.config.skills.len())),
            Line::from(format!("System:  {} (^Y)", self.current_system_label())),
            Line::from(format!("Memory:  {} (^P)", self.current_memory_label())),
            Line::from("Inspect: Payload (^G)"),
        ];
        f.render_widget(
            Paragraph::new(settings_text)
                .block(titled_block(" Config ").padding(Padding::uniform(1))),
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
                titled_block(format!(" Images ({}) ", self.attachments.len()))
                    .padding(Padding::uniform(1)),
            ),
            chunks[1],
        );
    }

    fn draw_chat_area(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(8)])
            .split(area);

        self.chat_area = chunks[0];
        self.prompt_area = chunks[1];
        // Reserve an extra column on the right so wrapped text doesn't butt up
        // against the vertical scrollbar rendered on the chat area's right edge.
        let available_width = self.chat_inner_area().width.saturating_sub(6).max(1) as usize;

        self.msg_states.resize(
            self.messages.len() + 1 + self.queued_messages.len(),
            MsgUiState::default(),
        );
        self.msg_visual_ranges.clear();

        let options = textwrap::Options::new(available_width).break_words(true);

        if self.last_wrap_width != available_width {
            self.styled_messages.clear();
            self.stream_dirty = true;
            self.last_wrap_width = available_width;
        }

        if self.styled_messages.len() > self.messages.len() {
            self.styled_messages.truncate(self.messages.len());
        }

        if self.styled_messages.len() < self.messages.len() {
            let start_idx = self.styled_messages.len();
            self.styled_messages
                .reserve(self.messages.len() - start_idx);
            for (i, m) in self.messages.iter().enumerate().skip(start_idx) {
                let formatted = if m.role == "assistant" {
                    let resolved_count = self.messages[i + 1..]
                        .iter()
                        .filter(|n| n.role != "error")
                        .take_while(|n| n.role == "tool")
                        .count();
                    format_assistant_display(m, resolved_count)
                } else if m.role == "tool" {
                    format_tool_display(&self.messages, i, m)
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
        for i in 0..self.messages.len() {
            if self.messages[i].role == "assistant" && self.styled_messages[i].is_empty() {
                continue;
            }

            let m = &self.messages[i];
            let (header_label, color) = match m.role.as_str() {
                "user" => (format!("USER{}", image_badge(m)), Color::LightGreen),
                "assistant" => ("ARCHON".to_string(), Color::Green),
                "error" => ("⚠ ERROR".to_string(), Color::LightRed),
                "local" => {
                    let label = if m.include_in_context {
                        "SHELL [+CTX]"
                    } else {
                        "SHELL"
                    };
                    (label.to_string(), Color::Blue)
                }
                "tool" => {
                    let summary = find_associated_tool_call(&self.messages, i)
                        .map(tools::call_signature)
                        .or_else(|| m.tool_name.clone())
                        .unwrap_or_else(|| "tool".to_string());
                    (sanitize_line(&format!("⚙ TOOL [{summary}]")), Color::Yellow)
                }
                "hook" => {
                    let summary = m.tool_name.as_deref().unwrap_or("hook");
                    (sanitize_line(&format!("⚡ HOOK [{summary}]")), Color::Cyan)
                }
                _ => ("SYS".to_string(), Color::Gray),
            };
            let truncated = m.truncated;
            let styled_content = self.styled_messages[i].clone();

            // Collapse the gap after this block when it is a tool call and
            // the next rendered block is also a tool call, so consecutive tool
            // calls stack tightly while the first tool keeps the normal gap
            // after the USER/ARCHON header.
            let compact =
                matches!(m.role.as_str(), "tool" | "hook") && self.next_rendered_is_tool(i);

            self.render_message_block(
                i,
                &header_label,
                color,
                truncated,
                &styled_content,
                &mut lines,
                compact,
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

            let styled_stream = self.styled_stream.clone();
            self.render_message_block(
                i,
                "ARCHON (...)",
                Color::Yellow,
                false,
                &styled_stream,
                &mut lines,
                false,
            );
        } else if let Some(ref tool_status) = self.active_tool_status {
            let i = self.messages.len();
            let start_visual = lines.len();
            lines.push(Line::from(vec![Span::styled(
                format!("  {} ", sanitize_line(tool_status)),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]));
            lines.push(Line::from(""));
            lines.push(Line::from(""));
            self.msg_visual_ranges
                .push((i, start_visual, lines.len(), false));
        }

        for q_idx in 0..self.queued_messages.len() {
            let block_idx = self.messages.len() + 1 + q_idx;
            self.msg_states[block_idx].unfolded = true;
            let qm = &self.queued_messages[q_idx];
            let header = format!("USER{} (QUEUED — Esc to send now)", image_badge(qm));
            let styled_queued = style_lines(&wrap_text(&sanitize_line(&qm.content), &options));
            self.render_message_block(
                block_idx,
                &header,
                Color::LightCyan,
                false,
                &styled_queued,
                &mut lines,
                false,
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

        f.render_widget(
            Paragraph::new(visible_lines)
                .block(chat_block(format!(" Conversation [{}] ", self.model))),
            chunks[0],
        );

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));
        let mut scrollbar_state = ScrollbarState::new(max_scroll).position(self.chat_scroll);
        f.render_stateful_widget(scrollbar, chunks[0], &mut scrollbar_state);

        let first_trimmed = self
            .text_area
            .lines()
            .first()
            .map_or("", |l| l.trim_start());

        let prompt_hint = if let Some(cmd_prefix) = first_trimmed.strip_prefix('/')
            && !cmd_prefix.contains(char::is_whitespace)
        {
            let matches = SlashCommand::matching_triggers(cmd_prefix);
            (!matches.is_empty()).then(|| format!(" Prompt — Commands: {} ", matches.join(" │ ")))
        } else if let Some(rest) = first_trimmed.strip_prefix('@') {
            if let Some(ws_idx) = rest.find(char::is_whitespace) {
                let cmd_token = &rest[..ws_idx];
                crate::core::skills::find_skill(&self.config, cmd_token)
                    .map(|s| format!(" Prompt — Skill: @{} [+<prompt>] ", s.name))
            } else {
                let matches: Vec<String> = self
                    .config
                    .skills
                    .iter()
                    .filter(|s| {
                        (!s.command.is_empty() && s.command.starts_with(rest))
                            || s.name.starts_with(rest)
                    })
                    .map(|s| {
                        let trigger = if s.command.is_empty() {
                            &s.name
                        } else {
                            &s.command
                        };
                        format!("@{trigger}")
                    })
                    .take(4)
                    .collect();
                (!matches.is_empty()).then(|| format!(" Prompt — Skills: {} ", matches.join(" │ ")))
            }
        } else if first_trimmed.starts_with("!!") {
            Some(
                " Prompt — Shell in Context: !!<command> (runs in workspace root & adds output to context) "
                    .into(),
            )
        } else if first_trimmed.starts_with('!') {
            Some(
                " Prompt — Shell Escape: !<command> (local only; use !!<command> to add to context) "
                    .into(),
            )
        } else {
            None
        };

        if let Some(title) = prompt_hint {
            self.text_area
                .set_block(titled_block(title).border_style(Style::default().fg(Color::Yellow)));
        } else {
            self.text_area
                .set_block(titled_block(" Prompt ").border_style(Style::default().fg(Color::Cyan)));
        }

        let prompt_cursor_visible =
            self.term_focused && self.modal == ActiveModal::None && self.cursor_blink_on;
        self.text_area
            .set_cursor_style(super::cursor_style(prompt_cursor_visible));
        f.render_widget(&self.text_area, chunks[1]);
    }
}
