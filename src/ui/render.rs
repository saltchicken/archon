use super::{
    ActiveModal, CodeCopyButton, ForkButton, MsgUiState, SlashCommand, TuiApp,
    markdown::{
        extract_code_block_from, is_opening_code_fence, render_markdown, spans_display_width,
    },
    theme, titled_block,
};
use crate::{
    core::{
        config::{ConfigSource, MemoryScope, ToolProvenance},
        session::{ChatMessage, truncate_str},
        skills::{self, SkillDefinition},
    },
    tools::{self, ToolCall},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
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

fn source_style(src: ConfigSource) -> Style {
    match src {
        ConfigSource::Default | ConfigSource::Global => {
            Style::default().fg(theme::TAB_GLOBAL_ACTIVE)
        }
        ConfigSource::Workspace => Style::default().fg(theme::TAB_WORKSPACE_ACTIVE),
        ConfigSource::WorkspaceOverridesGlobal => Style::default()
            .fg(theme::TAB_WORKSPACE_ACTIVE)
            .add_modifier(Modifier::BOLD),
        ConfigSource::Env | ConfigSource::Cli => Style::default()
            .fg(theme::MODAL_HINT)
            .add_modifier(Modifier::BOLD),
    }
}

fn source_badge_span(src: ConfigSource) -> Span<'static> {
    Span::styled(format!(" [{}]", src.short_tag()), source_style(src))
}

fn tool_prov_style(prov: ToolProvenance) -> Style {
    match prov {
        ToolProvenance::DefaultEnabled | ToolProvenance::GlobalEnabled => {
            Style::default().fg(theme::TAB_GLOBAL_ACTIVE)
        }
        ToolProvenance::WorkspaceEnabled
        | ToolProvenance::GlobalAndWorkspaceEnabled
        | ToolProvenance::WorkspaceAdded => Style::default().fg(theme::TAB_WORKSPACE_ACTIVE),
        ToolProvenance::WorkspaceRemoved => Style::default()
            .fg(theme::CTX_USAGE_WARN)
            .add_modifier(Modifier::BOLD),
        ToolProvenance::GlobalDisabled
        | ToolProvenance::GlobalAndWorkspaceDisabled
        | ToolProvenance::CliDisabled => Style::default().fg(theme::STATUS_DISABLED),
    }
}

fn pack_overwritten_lines(keys: &[String], max_width: usize, max_lines: usize) -> Vec<String> {
    let short_keys: Vec<String> = keys
        .iter()
        .map(|k| {
            k.strip_prefix("workspace_snapshot.")
                .map_or_else(|| k.clone(), |rest| format!("snap.{rest}"))
        })
        .collect();

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();

    for (idx, item) in short_keys.iter().enumerate() {
        let is_last = idx + 1 == short_keys.len();
        let piece = if is_last {
            item.clone()
        } else {
            format!("{item},")
        };

        if current.is_empty() {
            current = piece;
        } else if current.len() + 1 + piece.len() <= max_width {
            current.push(' ');
            current.push_str(&piece);
        } else {
            lines.push(std::mem::replace(&mut current, piece));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }

    if lines.len() > max_lines && max_lines > 0 {
        let extra = lines.len() - max_lines + 1;
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            *last = format!("+{extra} more overrides");
        }
    }
    lines
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
            "user" => (format!("USER{}", image_badge(m)), theme::HEADER_USER),
            "assistant" => ("ARCHON".to_string(), theme::HEADER_ASSISTANT),
            "error" => ("⚠ ERROR".to_string(), theme::HEADER_ERROR),
            "local" => {
                let ctx_suffix = if m.include_in_context { " [+CTX]" } else { "" };
                if let Some(ref summary) = m.tool_name {
                    (
                        sanitize_line(&format!("TOOL [{summary}]{ctx_suffix}")),
                        theme::HEADER_LOCAL,
                    )
                } else {
                    (format!("SHELL{ctx_suffix}"), theme::HEADER_LOCAL)
                }
            }
            "tool" => {
                let summary = find_associated_tool_call(&self.messages, idx)
                    .map(tools::call_signature)
                    .or_else(|| m.tool_name.clone())
                    .unwrap_or_else(|| "tool".to_string());
                if m.is_error {
                    (sanitize_line(&format!("⚠ {summary}")), theme::HEADER_ERROR)
                } else {
                    (sanitize_line(&format!("⚙ {summary}")), theme::HEADER_TOOL)
                }
            }
            "hook" => {
                let summary = m.tool_name.as_deref().unwrap_or("hook");
                (
                    sanitize_line(&format!("⚡ HOOK [{summary}]")),
                    theme::HEADER_HOOK,
                )
            }
            _ => ("SYS".to_string(), theme::HEADER_SYSTEM),
        }
    }

    fn rebuild_styled_messages(&mut self, available_width: usize, options: &textwrap::Options<'_>) {
        if self.last_wrap_width != available_width {
            self.styled_messages.clear();
            self.stream_cache.clear();
            self.stream_thought_cache.clear();
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
                    self.styled_messages.push(render_markdown(
                        &formatted,
                        options,
                        self.config.ui.style_code_blocks,
                    ));
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
                self.stream_thought_cache.update(
                    &self.streaming_thought,
                    options,
                    true,
                    self.config.ui.style_code_blocks,
                );
                self.stream_thought_dirty = false;
            }

            if self.stream_dirty {
                self.stream_cache.update(
                    &self.streaming_text,
                    options,
                    false,
                    self.config.ui.style_code_blocks,
                );
                self.stream_dirty = false;
            }

            let use_thought = self.streaming_text.is_empty();
            let styled_stream = if use_thought {
                std::mem::take(&mut self.stream_thought_cache.lines)
            } else {
                std::mem::take(&mut self.stream_cache.lines)
            };
            self.render_message_block(
                i,
                ("ARCHON (...)", theme::HEADER_STREAMING),
                false,
                &styled_stream,
                lines,
                false,
            );
            if use_thought {
                self.stream_thought_cache.lines = styled_stream;
            } else {
                self.stream_cache.lines = styled_stream;
            }
        } else if let Some(ref tool_status) = self.active_tool_status {
            let i = self.messages.len();
            let start_visual = lines.len();
            lines.push(Line::from(vec![Span::styled(
                format!("  {} ", sanitize_line(tool_status)),
                Style::default()
                    .fg(theme::HEADER_TOOL)
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
                    ("󰄬", theme::BADGE_FLASH_SUCCESS)
                } else {
                    ("󰘬", theme::BADGE_FORK)
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
                crate::core::skills::find_skill(&self.config, cmd_token).map(|s| {
                    if s.is_abstract {
                        format!(
                            " Prompt — Skill: {} (abstract base — cannot be invoked directly) ",
                            s.name
                        )
                    } else {
                        let args_hint = s.args_usage_hint();
                        if args_hint.is_empty() {
                            format!(" Prompt — Skill: @{} [+<prompt>] ", s.name)
                        } else {
                            format!(" Prompt — Skill: @{} {args_hint} [+<prompt>] ", s.name)
                        }
                    }
                })
            } else {
                let matches: Vec<String> = self
                    .config
                    .skills
                    .iter()
                    .filter(|s| {
                        !s.is_abstract
                            && ((!s.command.is_empty() && s.command.starts_with(rest))
                                || s.name.starts_with(rest))
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
                    .fg(theme::BADGE_TRUNCATED)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if state.collapsed {
            header_spans.push(Span::styled(
                format!("+{total_content} "),
                Style::default()
                    .fg(theme::BADGE_COLLAPSED)
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
                        Style::default().fg(theme::SCROLL_ELLIPSIS),
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
                                .fg(theme::BADGE_FLASH_SUCCESS)
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
                    Style::default().fg(theme::SCROLL_ELLIPSIS),
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
        let sidebar_width = if self.sidebar_expanded { 38 } else { 0 };
        let main_panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Min(40)])
            .split(self.term_area);

        if self.sidebar_expanded {
            self.sidebar_area = main_panes[0];
            self.draw_sidebar(f, main_panes[0]);
        } else {
            self.sidebar_area = Rect::default();
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

    fn effective_sidebar_skill(&self) -> Option<&SkillDefinition> {
        let first_trimmed = self
            .text_area
            .lines()
            .first()
            .map_or("", |l| l.trim_start());
        if let Some((cmd_token, _)) = skills::parse_at_command(first_trimmed)
            && let Some(skill) = skills::find_skill(&self.config, cmd_token)
        {
            return Some(skill);
        }
        if (self.is_streaming
            || self.active_tool_status.is_some()
            || self.pending_tool.is_some()
            || self.pending_truncated.is_some())
            && let Some(ref name) = self.active_turn_skill
        {
            return skills::find_skill(&self.config, name);
        }
        None
    }

    fn build_sidebar_config_lines(&self) -> (String, Vec<Line<'static>>) {
        let prov = &self.config.provenance;
        let config_title = if !prov.overwritten_keys.is_empty() {
            format!(
                " Config [G → WS: {} overrides] ",
                prov.overwritten_keys.len()
            )
        } else if prov.has_workspace_config {
            " Config [Global + Workspace] ".to_string()
        } else {
            " Config [Global] ".to_string()
        };

        let pct = (self.used_tokens as f64 / f64::from(self.num_ctx.max(1))) * 100.0;
        let ctx_color = if pct >= 90.0 {
            theme::CTX_USAGE_CRIT
        } else if pct >= 75.0 {
            theme::CTX_USAGE_WARN
        } else {
            theme::CTX_USAGE_OK
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

        let model_src_span = if prov.default_model != ConfigSource::Cli
            && self.model != self.config.startup_model()
        {
            Span::styled(" [UI]", Style::default().fg(theme::MODAL_HINT))
        } else {
            source_badge_span(prov.default_model)
        };

        let sys_label = self.current_system_label();
        let sys_enabled = !self.current_system_prompt_text().trim().is_empty();
        let sys_color = if sys_enabled {
            theme::STATUS_ENABLED
        } else {
            theme::STATUS_DISABLED
        };
        let sys_src_span = if self.custom_system_prompt.is_some() {
            Span::styled(" [UI]", Style::default().fg(theme::MODAL_HINT))
        } else {
            source_badge_span(prov.system_presets)
        };

        let mem_badge = |m: &crate::core::config::Memory| {
            let color = if !m.enabled {
                theme::STATUS_DISABLED
            } else if m.count() > 0 {
                theme::STATUS_ENABLED
            } else {
                theme::TAB_GLOBAL_ACTIVE
            };
            let state = if m.enabled { "ON" } else { "OFF" };
            Span::styled(
                format!("{state}({})", m.count()),
                Style::default().fg(color),
            )
        };
        let toggle_badge = |on: bool, on_color: Color, off_color: Color| {
            let (s, c) = if on {
                ("ON", on_color)
            } else {
                ("OFF", off_color)
            };
            Span::styled(s, Style::default().fg(c))
        };

        let rule_count = self.config.app.auto_approve.len();
        let rule_color = if rule_count > 0 {
            theme::STATUS_ENABLED
        } else {
            theme::STATUS_DISABLED
        };
        let mut approve_spans = vec![
            Span::raw("Approve: "),
            Span::styled(
                format!("{rule_count} rules"),
                Style::default().fg(rule_color),
            ),
            source_badge_span(prov.auto_approve),
        ];
        if self.session_auto_approve_count > 0 {
            approve_spans.push(Span::styled(
                format!(" +{} sess", self.session_auto_approve_count),
                Style::default().fg(theme::MODAL_HINT),
            ));
        }

        let snap = &self.config.workspace_snapshot;
        let hidden_neg_count = snap
            .exclude
            .iter()
            .filter_map(|p| p.trim().strip_prefix('!'))
            .map(|p| p.trim().trim_start_matches('/'))
            .filter(|p| p.starts_with('.') || p.contains("/."))
            .count();
        let (hidden_str, hidden_color) = if self.config.app.allow_hidden {
            ("ON (allowed)".to_string(), theme::STATUS_ENABLED)
        } else if hidden_neg_count > 0 {
            (
                format!("OFF (+{hidden_neg_count} allow)"),
                theme::CTX_USAGE_WARN,
            )
        } else {
            ("OFF (blocked)".to_string(), theme::STATUS_DISABLED)
        };

        let df_tool_enabled = self.enable_tools && tools::is_tool_enabled("dotfiles", &self.config);
        let df_dirs = &self.config.dotfiles.allowed_dirs;
        let df_dirs_label = if df_dirs.iter().any(|d| d.trim() == "*") {
            "*".to_string()
        } else {
            format!("{} dirs", df_dirs.len())
        };
        let (df_str, df_color) = if df_tool_enabled {
            (format!("ON ({df_dirs_label})"), theme::STATUS_ENABLED)
        } else {
            (format!("OFF ({df_dirs_label})"), theme::STATUS_DISABLED)
        };

        let sched_badge = |list: &[crate::core::config::ScheduleConfig]| {
            let tot = list.len();
            let on = list.iter().filter(|s| s.enabled).count();
            let color = if tot == 0 {
                theme::TAB_GLOBAL_ACTIVE
            } else if on > 0 {
                theme::STATUS_ENABLED
            } else {
                theme::STATUS_DISABLED
            };
            Span::styled(format!("{on}/{tot}"), Style::default().fg(color))
        };

        let mut lines = vec![
            Line::from(vec![
                Span::raw("Dir:     "),
                Span::styled(
                    truncate_str(&dir, 22),
                    Style::default().fg(theme::SIDEBAR_DIR),
                ),
            ]),
            Line::from(vec![
                Span::raw("Model:   "),
                Span::styled(
                    truncate_str(&self.model, 14),
                    Style::default().fg(theme::SIDEBAR_MODEL),
                ),
                model_src_span,
                Span::raw(" (^M)"),
            ]),
            Line::from(vec![
                Span::raw("Context: "),
                Span::styled(
                    format!("{}/{} ({pct:.0}%)", self.used_tokens, self.num_ctx),
                    Style::default().fg(ctx_color),
                ),
                source_badge_span(prov.models),
            ]),
            Line::from(vec![
                Span::raw("System:  "),
                Span::styled(truncate_str(sys_label, 14), Style::default().fg(sys_color)),
                sys_src_span,
                Span::raw(" (^Y)"),
            ]),
            Line::from(vec![
                Span::raw("Memory:  G:"),
                mem_badge(&self.config.global_memory),
                Span::raw(" WS:"),
                mem_badge(&self.config.workspace_memory),
                Span::raw(" (^P)"),
            ]),
            Line::from(approve_spans),
            Line::from(vec![
                Span::raw("Snap:    git:"),
                toggle_badge(snap.git_root, theme::STATUS_ENABLED, theme::STATUS_DISABLED),
                Span::raw(" tree:"),
                toggle_badge(
                    snap.tree_only,
                    theme::CTX_USAGE_WARN,
                    theme::TAB_GLOBAL_ACTIVE,
                ),
                source_badge_span(prov.workspace_snapshot),
            ]),
            Line::from(vec![
                Span::raw("Hidden:  "),
                Span::styled(hidden_str, Style::default().fg(hidden_color)),
                source_badge_span(prov.allow_hidden),
            ]),
            Line::from(vec![
                Span::raw("Dotfiles:"),
                Span::styled(df_str, Style::default().fg(df_color)),
                source_badge_span(prov.dotfiles),
            ]),
            Line::from(vec![
                Span::raw("Sched:   G:"),
                sched_badge(&self.config.global_schedules),
                Span::raw(" WS:"),
                sched_badge(&self.config.schedules),
            ]),
        ];

        for (i, ov_line) in pack_overwritten_lines(&prov.overwritten_keys, 25, 4)
            .into_iter()
            .enumerate()
        {
            let prefix = if i == 0 { "Overrode:" } else { "         " };
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(ov_line, Style::default().fg(theme::CTX_USAGE_WARN)),
            ]));
        }

        lines.push(Line::from("Inspect: Payload (^G)"));
        (config_title, lines)
    }

    fn build_sidebar_tool_lines(&self) -> (String, Vec<Line<'static>>) {
        let active_skill = self.effective_sidebar_skill();
        let active_skill_name = active_skill.map(|s| s.name.as_str());
        let total_tools = tools::ALL_TOOL_NAMES.len();
        let active_count = if self.enable_tools {
            tools::ALL_TOOL_NAMES
                .iter()
                .filter(|&&name| {
                    tools::is_tool_enabled_for_turn(name, &self.config, active_skill_name)
                })
                .count()
        } else {
            0
        };

        let kb = self.max_tool_output_size / 1024;
        let src_tag = self.config.provenance.enabled_tools.short_tag();
        let title = if !self.enable_tools {
            format!(" Tools: OFF (0/{total_tools}, {kb}KB) [{src_tag}] (^T) ")
        } else if let Some(sk) = active_skill {
            format!(
                " Tools: @{} ({active_count}/{total_tools}, {kb}KB) (^T) ",
                sk.trigger()
            )
        } else {
            format!(" Tools: ON ({active_count}/{total_tools}, {kb}KB) [{src_tag}] (^T) ")
        };

        let lines = tools::ALL_TOOL_NAMES
            .iter()
            .map(|&tool_name| {
                let cfg_enabled = tools::is_tool_enabled(tool_name, &self.config);
                let turn_enabled = self.enable_tools
                    && tools::is_tool_enabled_for_turn(tool_name, &self.config, active_skill_name);
                let prov = self.config.tool_provenance(tool_name);

                let (glyph, name_style, state_str, state_style, tag_str, tag_style) =
                    if !cfg_enabled {
                        (
                            "✗ ",
                            Style::default().fg(theme::STATUS_DISABLED),
                            "OFF",
                            Style::default().fg(theme::STATUS_DISABLED),
                            prov.short_tag(),
                            tool_prov_style(prov),
                        )
                    } else if !self.enable_tools {
                        (
                            "~ ",
                            Style::default().fg(theme::CTX_USAGE_WARN),
                            "^T",
                            Style::default().fg(theme::CTX_USAGE_WARN),
                            prov.short_tag(),
                            tool_prov_style(prov),
                        )
                    } else if !turn_enabled {
                        (
                            "~ ",
                            Style::default().fg(theme::CTX_USAGE_WARN),
                            "@OFF",
                            Style::default().fg(theme::CTX_USAGE_WARN),
                            "[-@sk]",
                            Style::default().fg(theme::CTX_USAGE_WARN),
                        )
                    } else {
                        (
                            "✓ ",
                            Style::default().fg(theme::STATUS_ENABLED),
                            "ON",
                            Style::default()
                                .fg(theme::STATUS_ENABLED)
                                .add_modifier(Modifier::BOLD),
                            prov.short_tag(),
                            tool_prov_style(prov),
                        )
                    };

                Line::from(vec![
                    Span::styled(format!("{glyph}{tool_name:<18} "), name_style),
                    Span::styled(format!("{state_str:<4} "), state_style),
                    Span::styled(format!("{tag_str:>8}"), tag_style),
                ])
            })
            .collect();

        (title, lines)
    }

    fn build_sidebar_skill_lines(&self) -> (String, Vec<Line<'static>>) {
        if self.config.skills.is_empty() {
            return (
                " Skills (0 loaded) ".to_string(),
                vec![Line::from(Span::styled(
                    "No skills loaded",
                    Style::default().fg(theme::SIDEBAR_EMPTY_IMAGES),
                ))],
            );
        }

        let active_skill_name = self.effective_sidebar_skill().map(|s| s.name.as_str());
        let mut ready_count = 0usize;
        let mut invocable_count = 0usize;
        let mut lines = Vec::with_capacity(self.config.skills.len());

        for skill in &self.config.skills {
            if !skill.is_abstract {
                invocable_count += 1;
            }
            let missing = skills::missing_skill_tools(skill, &self.config);
            let needs_tools = !skill.tools.is_empty() || !skill.hooks.is_empty();
            let is_active = active_skill_name.is_some_and(|a| a.eq_ignore_ascii_case(&skill.name));

            let (glyph, name_style, detail, state_style) = if !skill.resolution_errors.is_empty() {
                (
                    "✗ ",
                    Style::default().fg(theme::STATUS_DISABLED),
                    "ERR extends".to_string(),
                    Style::default().fg(theme::STATUS_DISABLED),
                )
            } else if skill.is_abstract {
                let t_cnt = skill.tools.len();
                let h_cnt = skill.hooks.len();
                let d = match (t_cnt, h_cnt) {
                    (0, 0) => "BASE".to_string(),
                    (t, 0) => format!("BASE ({t}t)"),
                    (0, h) => format!("BASE ({h}h)"),
                    (t, h) => format!("BASE ({t}t,{h}h)"),
                };
                (
                    "· ",
                    Style::default().fg(theme::SIDEBAR_EMPTY_IMAGES),
                    d,
                    Style::default().fg(theme::SIDEBAR_EMPTY_IMAGES),
                )
            } else if !missing.is_empty() {
                let d = if missing.len() == 1 && missing[0].len() <= 5 {
                    format!("OFF no:{}", missing[0])
                } else {
                    format!("OFF no:{}t", missing.len())
                };
                (
                    "✗ ",
                    Style::default().fg(theme::STATUS_DISABLED),
                    d,
                    Style::default().fg(theme::STATUS_DISABLED),
                )
            } else if needs_tools && !self.enable_tools {
                (
                    "~ ",
                    Style::default().fg(theme::CTX_USAGE_WARN),
                    "^T  (muted)".to_string(),
                    Style::default().fg(theme::CTX_USAGE_WARN),
                )
            } else {
                ready_count += 1;
                let t_cnt = skill.tools.len();
                let h_cnt = skill.hooks.len();
                let d = match (t_cnt, h_cnt) {
                    (0, 0) => "ON  (prompt)".to_string(),
                    (t, 0) => format!("ON  ({t}t)"),
                    (0, h) => format!("ON  ({h}h)"),
                    (t, h) => format!("ON  ({t}t,{h}h)"),
                };
                let style = if is_active {
                    Style::default()
                        .fg(theme::SELECTION_HIGHLIGHT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::STATUS_ENABLED)
                };
                ("✓ ", style, d, style)
            };

            let (scope_tag, scope_style) = if skill.overrides_global {
                (
                    "[G→WS]",
                    Style::default()
                        .fg(theme::TAB_WORKSPACE_ACTIVE)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                match skill.scope {
                    MemoryScope::Global => ("[G]", Style::default().fg(theme::TAB_GLOBAL_ACTIVE)),
                    MemoryScope::Workspace => {
                        ("[WS]", Style::default().fg(theme::TAB_WORKSPACE_ACTIVE))
                    }
                }
            };

            let display_name = if skill.is_abstract {
                skill.name.clone()
            } else {
                format!("@{}", skill.trigger())
            };
            let trigger = truncate_str(&display_name, 12);
            lines.push(Line::from(vec![
                Span::styled(format!("{glyph}{trigger:<12} "), name_style),
                Span::styled(format!("{detail:<12} "), state_style),
                Span::styled(format!("{scope_tag:>6}"), scope_style),
            ]));
        }

        let title = format!(" Skills ({ready_count}/{invocable_count} ready) ");
        (title, lines)
    }

    fn draw_sidebar(&mut self, f: &mut Frame, area: Rect) {
        let (config_title, settings_text) = self.build_sidebar_config_lines();
        let (tools_title, tool_lines) = self.build_sidebar_tool_lines();
        let (skills_title, skill_lines) = self.build_sidebar_skill_lines();

        let config_h = (settings_text.len() as u16 + 2).min(area.height.saturating_sub(9).max(5));
        let skills_h = (skill_lines.len().clamp(1, 5) as u16) + 2;
        let images_h = (self.attachments.len().clamp(1, 4) as u16) + 2;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(config_h),
                Constraint::Fill(1),
                Constraint::Length(skills_h),
                Constraint::Length(images_h),
            ])
            .split(area);

        f.render_widget(
            Paragraph::new(settings_text).block(titled_block(config_title)),
            chunks[0],
        );

        let tools_inner_h = usize::from(chunks[1].height.saturating_sub(2)).max(1);
        let max_tools_scroll = tool_lines.len().saturating_sub(tools_inner_h);
        self.sidebar_scroll = self.sidebar_scroll.min(max_tools_scroll);
        let visible_tools: Vec<Line<'static>> = tool_lines
            .into_iter()
            .skip(self.sidebar_scroll)
            .take(tools_inner_h)
            .collect();

        f.render_widget(
            Paragraph::new(visible_tools).block(titled_block(tools_title)),
            chunks[1],
        );
        if max_tools_scroll > 0 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"));
            let mut sb_state = ScrollbarState::new(max_tools_scroll).position(self.sidebar_scroll);
            f.render_stateful_widget(scrollbar, chunks[1], &mut sb_state);
        }

        f.render_widget(
            Paragraph::new(skill_lines).block(titled_block(skills_title)),
            chunks[2],
        );

        let items: Vec<ListItem> = if self.attachments.is_empty() {
            vec![
                ListItem::new("No images attached")
                    .style(Style::default().fg(theme::SIDEBAR_EMPTY_IMAGES)),
            ]
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
            List::new(items).block(titled_block(format!(
                " Images ({}) ",
                self.attachments.len()
            ))),
            chunks[3],
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
            let styled_queued = render_markdown(
                &sanitize_line(&qm.content),
                &options,
                self.config.ui.style_code_blocks,
            );
            self.render_message_block(
                block_idx,
                (&header, theme::HEADER_QUEUED),
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
            self.text_area.set_block(
                titled_block(title).border_style(Style::default().fg(theme::BORDER_PROMPT_HINT)),
            );
        } else {
            self.text_area.set_block(
                titled_block(" Prompt ").border_style(Style::default().fg(theme::BORDER_PROMPT)),
            );
        }

        let prompt_cursor_visible =
            self.term_focused && self.modal == ActiveModal::None && self.cursor_blink_on;
        self.text_area
            .set_cursor_style(super::cursor_style(prompt_cursor_visible));
        f.render_widget(&self.text_area, chunks[1]);
    }
}
