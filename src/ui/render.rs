use super::{
    ActiveModal, CodeCopyButton, ForkButton, MsgUiState, SlashCommand, TuiApp,
    markdown::{
        extract_code_block_from, is_opening_code_fence, render_markdown, render_thought_markdown,
        spans_display_width,
    },
    titled_block,
};
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

/// Horizontal columns occupied by the leading `"  "` body indent before message text.
const MESSAGE_BODY_INDENT_WIDTH: usize = 2;

pub(super) fn sanitize_line(s: &str) -> String {
    s.replace(['\r', '\u{fe0f}'], "").replace('\t', "    ")
}

fn format_param_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => truncate_str(&s.replace('\n', "\\n"), 120),
        serde_json::Value::Array(arr) if arr.iter().all(serde_json::Value::is_object) => {
            format!("[{} items]", arr.len())
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

pub(super) fn format_assistant_display(m: &ChatMessage) -> String {
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

fn is_message_header_line(line: &Line<'_>) -> bool {
    line.spans
        .first()
        .is_some_and(|s| s.content.starts_with("▸ "))
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

    fn message_header(&self, idx: usize) -> (String, Color) {
        let m = &self.messages[idx];
        match m.role.as_str() {
            "user" => (format!("USER{}", image_badge(m)), Color::LightGreen),
            "assistant" => ("ARCHON".to_string(), Color::Green),
            "error" => ("⚠ ERROR".to_string(), Color::LightRed),
            "local" => {
                let ctx_suffix = if m.include_in_context { " [+CTX]" } else { "" };
                if let Some(ref summary) = m.tool_name {
                    (
                        sanitize_line(&format!("TOOL [{summary}]{ctx_suffix}")),
                        Color::Blue,
                    )
                } else {
                    (format!("SHELL{ctx_suffix}"), Color::Blue)
                }
            }
            "tool" => {
                let summary = find_associated_tool_call(&self.messages, idx)
                    .map(tools::call_signature)
                    .or_else(|| m.tool_name.clone())
                    .unwrap_or_else(|| "tool".to_string());
                (sanitize_line(&format!("⚙ {summary}")), Color::Yellow)
            }
            "hook" => {
                let summary = m.tool_name.as_deref().unwrap_or("hook");
                (
                    sanitize_line(&format!("⚡ HOOK [{summary}]")),
                    Color::Yellow,
                )
            }
            _ => ("SYS".to_string(), Color::LightGreen),
        }
    }

    fn rebuild_styled_messages(&mut self, available_width: usize, options: &textwrap::Options<'_>) {
        if self.last_wrap_width != available_width {
            self.styled_messages.clear();
            self.stream_dirty = true;
            self.stream_thought_dirty = true;
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
                    format_assistant_display(m)
                } else if m.role == "tool" {
                    format_tool_display(&self.messages, i, m)
                } else {
                    sanitize_line(&m.content)
                };

                if formatted.trim().is_empty() {
                    self.styled_messages.push(Vec::new());
                } else {
                    self.styled_messages
                        .push(render_markdown(&formatted, options));
                }
            }
        }
    }

    fn render_streaming_or_tool_status(
        &mut self,
        options: &textwrap::Options<'_>,
        lines: &mut Vec<Line<'static>>,
    ) {
        if self.is_streaming {
            let i = self.messages.len();
            self.msg_states[i].unfolded = true;

            if self.stream_thought_dirty {
                let sanitized_thought = sanitize_line(&self.streaming_thought);
                self.styled_stream_thought = if sanitized_thought.trim().is_empty() {
                    Vec::new()
                } else {
                    render_thought_markdown(&sanitized_thought, options)
                };
                self.stream_thought_dirty = false;
            }

            if self.stream_dirty {
                let sanitized_stream = sanitize_line(&self.streaming_text);
                self.styled_stream = if sanitized_stream.trim().is_empty() {
                    Vec::new()
                } else {
                    render_markdown(&sanitized_stream, options)
                };
                self.stream_dirty = false;
            }

            let styled_stream = if self.streaming_text.is_empty() {
                self.styled_stream_thought.clone()
            } else {
                self.styled_stream.clone()
            };
            self.render_message_block(
                i,
                ("ARCHON (...)", Color::Yellow),
                false,
                &styled_stream,
                lines,
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
    }

    fn update_fork_badges(&mut self, lines: &mut [Line<'static>]) {
        self.hovered_header_msg = self.last_mouse_pos.and_then(|pos| self.header_msg_at(pos));
        let fork_flash_idx = self
            .fork_flash
            .filter(|&(_, exp)| std::time::Instant::now() < exp)
            .map(|(target_idx, _)| target_idx);
        for &(idx, start_visual, _, _) in &self.msg_visual_ranges {
            if !self.messages.get(idx).is_some_and(ChatMessage::is_forkable) {
                continue;
            }
            let is_fork_flashed = fork_flash_idx == Some(idx);
            let show_fork_badge = !self.config.ui.fork_icon_hover_only
                || self.hovered_header_msg == Some(idx)
                || is_fork_flashed;
            if show_fork_badge {
                let prefix_w = spans_display_width(&lines[start_visual].spans);
                let (badge_glyph, badge_color) = if is_fork_flashed {
                    ("󰄬", Color::Green)
                } else {
                    ("󰘬", Color::Yellow)
                };
                let badge_w = display_width(badge_glyph).max(2);
                self.fork_buttons.push(ForkButton {
                    msg_idx: idx,
                    visual_y: start_visual,
                    x_range: prefix_w.saturating_sub(1)..(prefix_w + badge_w + 1),
                });
                lines[start_visual].spans.push(Span::styled(
                    badge_glyph,
                    Style::default()
                        .fg(badge_color)
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }
    }

    fn prompt_hint_title(&self) -> Option<String> {
        let first_trimmed = self
            .text_area
            .lines()
            .first()
            .map_or("", |l| l.trim_start());

        if let Some(cmd_prefix) = first_trimmed.strip_prefix('/')
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
                    .map(|s| format!("@{}", s.trigger()))
                    .take(4)
                    .collect();
                (!matches.is_empty()).then(|| format!(" Prompt — Skills: {} ", matches.join(" │ ")))
            }
        } else if !first_trimmed.starts_with("###")
            && let Some((include_ctx, prefix_mark, rest)) = first_trimmed
                .strip_prefix("##")
                .map(|r| (true, "##", r))
                .or_else(|| first_trimmed.strip_prefix('#').map(|r| (false, "#", r)))
            && !rest.starts_with(char::is_whitespace)
        {
            let mode_label = if include_ctx { "+CTX" } else { "local" };
            if let Some(ws_idx) = rest.find(char::is_whitespace) {
                let tool_token = &rest[..ws_idx];
                let base_name = tool_token.split_once(':').map_or(tool_token, |(n, _)| n);
                tools::ALL_TOOL_NAMES
                    .iter()
                    .find(|&&name| name.eq_ignore_ascii_case(base_name))
                    .map(|&canonical| {
                        let hint = tools::tool_param_hint(canonical);
                        let ctx_note = if include_ctx {
                            "adds output to context"
                        } else {
                            "local only; use ## to add to context"
                        };
                        format!(
                            " Prompt — Tool ({mode_label}): {prefix_mark}{tool_token} {hint} ({ctx_note}) "
                        )
                    })
            } else {
                let matches: Vec<String> = tools::matching_tool_triggers(prefix_mark, rest)
                    .into_iter()
                    .take(6)
                    .collect();
                if !matches.is_empty() {
                    let suffix = if !include_ctx && rest.is_empty() {
                        " (or ##<tool> for +CTX)"
                    } else {
                        ""
                    };
                    Some(format!(
                        " Prompt — Tools ({mode_label}): {}{suffix} ",
                        matches.join(" │ ")
                    ))
                } else {
                    None
                }
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
        }
    }

    fn render_message_block(
        &mut self,
        idx: usize,
        (header_label, color): (&str, Color),
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
            if header_label.starts_with("⚙ ") || header_label.starts_with("⚡ HOOK") {
                format!("  {header_label} ")
            } else {
                format!("▸ {header_label} ")
            },
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        if truncated {
            header_spans.push(Span::styled(
                "⚠ TRUNCATED ",
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if state.collapsed {
            header_spans.push(Span::styled(
                format!("+{total_content} "),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        lines.push(Line::from(header_spans));

        let is_truncated = !state.collapsed && total_content > max_lines;

        if !state.collapsed {
            let (slice_start, slice_end) = if is_truncated {
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
                (start, end)
            } else {
                lines.push(Line::from(""));
                (0, total_content)
            };

            let flash_target = self
                .copied_code_flash
                .filter(|&(_, exp)| std::time::Instant::now() < exp)
                .map(|(target, _)| target);

            for (rel_idx, line) in styled_content[slice_start..slice_end].iter().enumerate() {
                let content_idx = slice_start + rel_idx;
                let visual_y = lines.len();
                if is_opening_code_fence(line) {
                    let is_flashed = flash_target == Some((idx, content_idx));
                    let mut rendered_line = line.clone();
                    if is_flashed && let Some(last_span) = rendered_line.spans.last_mut() {
                        *last_span = Span::styled(
                            "󰄬",
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        );
                    }
                    let prefix_w = spans_display_width(
                        &rendered_line.spans[..rendered_line.spans.len().saturating_sub(1)],
                    );
                    let badge_w = rendered_line
                        .spans
                        .last()
                        .map_or(1, |s| display_width(s.content.as_ref()).max(2));
                    self.code_copy_buttons.push(CodeCopyButton {
                        msg_idx: idx,
                        content_idx,
                        visual_y,
                        x_range: prefix_w.saturating_sub(1)..(prefix_w + badge_w + 1),
                        code: extract_code_block_from(styled_content, content_idx),
                    });
                    lines.push(rendered_line);
                } else {
                    lines.push(line.clone());
                }
            }

            if is_truncated && slice_end < total_content {
                lines.push(Line::from(Span::styled(
                    format!("    ... ({}) ...", total_content - slice_end),
                    Style::default().fg(Color::Blue),
                )));
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
            Color::LightRed
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
            vec![ListItem::new("No images attached").style(Style::default().fg(Color::LightGreen))]
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
        let available_width = usize::from(self.chat_inner_area().width)
            .saturating_sub(MESSAGE_BODY_INDENT_WIDTH)
            .max(1);

        let total_slots = self.messages.len() + 1 + self.queued_messages.len();
        self.msg_states.resize(total_slots, MsgUiState::default());
        self.msg_visual_ranges.clear();
        self.code_copy_buttons.clear();
        self.fork_buttons.clear();

        let options = textwrap::Options::new(available_width).break_words(true);
        self.rebuild_styled_messages(available_width, &options);

        let mut lines = Vec::new();
        for i in 0..self.messages.len() {
            if self.messages[i].role == "assistant" && self.styled_messages[i].is_empty() {
                continue;
            }

            let (header_label, color) = self.message_header(i);
            let truncated = self.messages[i].truncated;
            let styled_content = self.styled_messages[i].clone();

            // Collapse the gap after this block when it is a tool call and
            // the next rendered block is also a tool call, so consecutive tool
            // calls stack tightly while the first tool keeps the normal gap
            // after the USER/ARCHON header.
            let compact = matches!(self.messages[i].role.as_str(), "tool" | "hook")
                && self.next_rendered_is_tool(i);

            self.render_message_block(
                i,
                (&header_label, color),
                truncated,
                &styled_content,
                &mut lines,
                compact,
            );
        }

        self.render_streaming_or_tool_status(&options, &mut lines);

        for q_idx in 0..self.queued_messages.len() {
            let block_idx = self.messages.len() + 1 + q_idx;
            self.msg_states[block_idx].unfolded = true;
            let qm = &self.queued_messages[q_idx];
            let header = format!("USER{} (QUEUED — Esc to send now)", image_badge(qm));
            let styled_queued = render_markdown(&sanitize_line(&qm.content), &options);
            self.render_message_block(
                block_idx,
                (&header, Color::LightCyan),
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

        self.update_fork_badges(&mut lines);

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

        let mut badges = String::new();
        if self.session_is_pinned {
            badges.push_str(" 󰐃");
        }
        if self.session_is_fork {
            badges.push_str(" 󰘬");
        }
        let chat_title = format!(" Conversation [{}]{badges} ", self.model);
        f.render_widget(
            Paragraph::new(visible_lines).block(self.chat_area_block(chat_title)),
            chunks[0],
        );

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));
        let mut scrollbar_state = ScrollbarState::new(max_scroll).position(self.chat_scroll);
        f.render_stateful_widget(scrollbar, chunks[0], &mut scrollbar_state);

        if let Some(title) = self.prompt_hint_title() {
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
