use super::{
    ActiveModal, SessionView, TuiApp, chat_block, create_memory_area, render::sanitize_line, theme,
    titled_block,
};
use crate::core::{
    EngineCommand, TruncatedToolAction,
    config::{Memory, MemoryScope, global_memory_path, load_scoped_memory_file},
    session::SessionSummary,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, Paragraph, Widget, Wrap},
};

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

fn top_right_rect(
    percent_x: u16,
    percent_y: u16,
    min_width: u16,
    min_height: u16,
    r: Rect,
) -> Rect {
    let margin_x = 3.min(r.width / 10);
    let margin_y = 2.min(r.height / 10);
    let avail_w = r.width.saturating_sub(margin_x * 2);
    let avail_h = r.height.saturating_sub(margin_y * 2);

    let width = (r.width.saturating_mul(percent_x) / 100)
        .max(min_width)
        .min(avail_w.max(1));
    let height = (r.height.saturating_mul(percent_y) / 100)
        .max(min_height)
        .min(avail_h.max(1));

    let x = r.x + r.width.saturating_sub(width.saturating_add(margin_x));
    let y = r.y + margin_y.min(r.height.saturating_sub(height));
    Rect::new(x, y, width, height)
}

fn render_modal_at<W: Widget>(f: &mut Frame, area: Rect, widget: W) {
    f.render_widget(Clear, area);
    f.render_widget(widget, area);
}

fn render_modal<W: Widget>(f: &mut Frame, percent_x: u16, percent_y: u16, widget: W) {
    render_modal_at(f, centered_rect(percent_x, percent_y, f.area()), widget);
}

fn hint_line(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default().fg(theme::MODAL_HINT),
    ))
}

fn tab_styles(
    first_active: bool,
    first_color: ratatui::style::Color,
    second_color: ratatui::style::Color,
) -> (Style, Style) {
    let active = |c| {
        Style::default()
            .fg(c)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    };
    let inactive = Style::default().fg(theme::TAB_INACTIVE);
    if first_active {
        (active(first_color), inactive)
    } else {
        (inactive, active(second_color))
    }
}

fn selection_style(selected: bool, active: bool) -> (&'static str, Style) {
    let prefix = if selected { ">> " } else { "   " };
    let style = if selected {
        Style::default()
            .fg(theme::SELECTION_HIGHLIGHT)
            .add_modifier(Modifier::BOLD)
    } else if active {
        Style::default().fg(theme::SELECTION_ACTIVE)
    } else {
        Style::default()
    };
    (prefix, style)
}

fn format_tool_preview_lines(preview: &str) -> Vec<Line<'static>> {
    let mut section_color = theme::PREVIEW_DEFAULT_SECTION;
    let mut in_diff = false;

    preview
        .lines()
        .map(|raw_line| {
            let line = sanitize_line(raw_line);
            if line.starts_with("--- ") && line.ends_with(" ---") {
                in_diff = line == "--- UNIFIED DIFF ---";
                let header_color = if line.contains("WARNING") || line.contains("SEARCH") {
                    section_color = theme::PREVIEW_SECTION_WARN;
                    theme::PREVIEW_HEADER_WARN
                } else if line.contains("REPLACE")
                    || line.contains("CONTENT")
                    || line.contains("MESSAGE")
                {
                    section_color = theme::PREVIEW_SECTION_CONTENT;
                    theme::PREVIEW_HEADER_CONTENT
                } else {
                    section_color = theme::PREVIEW_SECTION_DEFAULT;
                    theme::PREVIEW_HEADER_DEFAULT
                };
                Line::from(Span::styled(
                    line,
                    Style::default()
                        .fg(header_color)
                        .add_modifier(Modifier::BOLD),
                ))
            } else if in_diff {
                let style = if line.starts_with("+++") || line.starts_with("---") {
                    Style::default()
                        .fg(theme::DIFF_FILE_HEADER)
                        .add_modifier(Modifier::BOLD)
                } else if line.starts_with("@@") {
                    Style::default().fg(theme::DIFF_HUNK_HEADER)
                } else if line.starts_with('+') {
                    Style::default().fg(theme::DIFF_ADD)
                } else if line.starts_with('-') {
                    Style::default().fg(theme::DIFF_DELETE)
                } else {
                    Style::default().fg(theme::DIFF_CONTEXT)
                };
                Line::from(Span::styled(line, style))
            } else {
                Line::from(Span::styled(line, Style::default().fg(section_color)))
            }
        })
        .collect()
}

impl TuiApp {
    pub(super) fn filtered_sessions(&self) -> Vec<&SessionSummary> {
        let want_pinned = self.session_view == SessionView::Pinned;
        self.sessions
            .iter()
            .filter(|s| s.pinned == want_pinned)
            .collect()
    }

    pub(super) fn toggle_session_view(&mut self) {
        self.session_view = self.session_view.toggle();
        let filtered = self.filtered_sessions();
        self.session_idx = filtered
            .iter()
            .position(|s| !self.session_id.is_empty() && s.id == self.session_id)
            .unwrap_or(0);
    }

    fn memory_draft(&self, scope: MemoryScope) -> &Memory {
        match scope {
            MemoryScope::Global => &self.global_memory_draft,
            MemoryScope::Workspace => &self.workspace_memory_draft,
        }
    }

    pub(super) fn commit_active_memory_scope_draft(&mut self) {
        let raw_text = self.memory_area.lines().join("\n");
        let mem = Memory::from_editor_text(&raw_text, self.memory_enabled_draft);
        match self.memory_scope {
            MemoryScope::Global => self.global_memory_draft = mem,
            MemoryScope::Workspace => self.workspace_memory_draft = mem,
        }
    }

    pub(super) fn load_active_memory_scope_draft(&mut self) {
        let draft = self.memory_draft(self.memory_scope);
        let text = draft.to_editor_text();
        self.memory_enabled_draft = draft.enabled;
        self.memory_area = create_memory_area(self.memory_scope, &text);
    }

    pub(super) fn toggle_memory_scope(&mut self) {
        self.commit_active_memory_scope_draft();
        self.memory_scope = self.memory_scope.toggle();
        self.load_active_memory_scope_draft();
    }

    pub(super) fn save_memory_modal(&mut self) {
        self.commit_active_memory_scope_draft();
        self.modal = ActiveModal::None;

        let active_scope = self.memory_scope;
        let inactive_scope = active_scope.toggle();

        for (scope, force_save) in [(inactive_scope, false), (active_scope, true)] {
            let draft = self.memory_draft(scope).clone();
            if force_save || &draft != self.config.scoped_memory(scope) {
                self.config.set_scoped_memory(scope, draft.clone());
                let _ = self.cmd_tx.send(EngineCommand::SetMemory {
                    scope,
                    memory: draft,
                });
            }
        }
    }

    pub(super) fn open_memory_modal(&mut self) {
        let global_mem = load_scoped_memory_file(&global_memory_path(), true)
            .map(|(_, m)| m)
            .unwrap_or_else(|_| self.config.global_memory.clone());
        let ws_mem = load_scoped_memory_file(&self.config.workspace_memory_path(), false)
            .map(|(_, m)| m)
            .unwrap_or_else(|_| self.config.workspace_memory.clone());

        self.config.global_memory = global_mem.clone();
        self.config.workspace_memory = ws_mem.clone();
        self.global_memory_draft = global_mem;
        self.workspace_memory_draft = ws_mem;
        self.load_active_memory_scope_draft();
        self.modal = ActiveModal::Memory;
    }

    pub(super) fn request_payload_inspector(&mut self) {
        let draft_prompt = self.text_area.lines().join("\n");
        let _ = self.cmd_tx.send(EngineCommand::InspectPayload {
            draft_prompt,
            turn: self.current_turn_config(),
        });
    }

    pub(super) fn resolve_modal_tool(&mut self, approved: bool, always_approve: bool) {
        if self.pending_tool.take().is_some() {
            if approved && always_approve {
                self.session_auto_approve_count += 1;
            }
            self.modal = ActiveModal::None;
            self.follow_tail = true;
            let _ = self.cmd_tx.send(EngineCommand::ResolveTool {
                approved,
                always_approve,
                feedback: std::mem::take(&mut self.tool_feedback),
                turn: self.current_turn_config(),
            });
        }
    }

    pub(super) fn resolve_truncated_modal(&mut self, action: TruncatedToolAction) {
        if self.pending_truncated.take().is_some() {
            self.modal = ActiveModal::None;
            self.follow_tail = true;
            let _ = self.cmd_tx.send(EngineCommand::ResolveTruncatedTool {
                action,
                guide_prompt: std::mem::take(&mut self.truncated_guide_prompt),
                turn: self.current_turn_config(),
            });
        }
    }

    pub(super) fn draw_file_modal(&self, f: &mut Frame) {
        let widget = Paragraph::new(format!(
            "Press ⏎ or drag-and-drop image path:\n\n> {}\n\n[⏎] Import\n[Esc] Cancel",
            sanitize_line(&self.file_path_input)
        ))
        .block(chat_block(" Add Image File "));
        render_modal(f, 60, 20, widget);
    }

    pub(super) fn draw_sessions_modal(&self, f: &mut Frame) {
        let area = centered_rect(72, 62, f.area());
        f.render_widget(Clear, area);

        let recent_count = self.sessions.iter().filter(|s| !s.pinned).count();
        let pinned_count = self.sessions.iter().filter(|s| s.pinned).count();

        let (recent_style, pinned_style) = tab_styles(
            self.session_view == SessionView::Recent,
            theme::TAB_RECENT_ACTIVE,
            theme::TAB_PINNED_ACTIVE,
        );

        let pin_action_label = match self.session_view {
            SessionView::Recent => "[P] Pin",
            SessionView::Pinned => "[P] Unpin",
        };

        let header = Line::from(vec![
            Span::styled(
                "Conversations ",
                Style::default().fg(theme::MODAL_TITLE_ACCENT),
            ),
            Span::raw("│ View: ["),
            Span::styled(format!("Recent ({recent_count})"), recent_style),
            Span::raw(" / "),
            Span::styled(format!("󰐃 Pinned ({pinned_count})"), pinned_style),
            Span::raw("]"),
        ]);

        let footer_lines = vec![hint_line(format!(
            " [Tab/←/→] Switch View   {pin_action_label}   [⏎] Load   [D] Delete   [Esc] Close"
        ))];

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Fill(1),
                Constraint::Length(1),
            ])
            .split(area);

        f.render_widget(
            Paragraph::new(header).block(titled_block(" Saved Sessions (^S) ")),
            chunks[0],
        );

        let filtered = self.filtered_sessions();
        let list_title = match self.session_view {
            SessionView::Recent => " Recent Sessions ",
            SessionView::Pinned => " 󰐃 Pinned Sessions ",
        };
        let list_block =
            chat_block(list_title).border_style(Style::default().fg(theme::BORDER_MODAL_ACCENT));
        let inner_height = usize::from(list_block.inner(chunks[1]).height).max(1);

        let items: Vec<ListItem> = if filtered.is_empty() {
            let empty_msg = match self.session_view {
                SessionView::Recent => "   No recent sessions found.",
                SessionView::Pinned => {
                    "   No pinned sessions yet — press [P] on a recent session or run /pin."
                }
            };
            vec![ListItem::new(Line::from(Span::styled(
                empty_msg,
                Style::default().fg(theme::MODAL_MUTED),
            )))]
        } else {
            let scroll_offset = if self.session_idx >= inner_height {
                self.session_idx + 1 - inner_height
            } else {
                0
            };
            filtered
                .iter()
                .enumerate()
                .skip(scroll_offset)
                .take(inner_height)
                .map(|(i, s)| {
                    let is_active = !self.session_id.is_empty() && s.id == self.session_id;
                    let (prefix, style) = selection_style(i == self.session_idx, is_active);
                    let pin_badge = if s.pinned { "󰐃 " } else { "" };
                    let active = if is_active { " [active]" } else { "" };
                    ListItem::new(Line::from(Span::styled(
                        format!(
                            "{prefix}{pin_badge}{} ({} msgs){active}",
                            sanitize_line(&s.title),
                            s.messages
                        ),
                        style,
                    )))
                })
                .collect()
        };

        f.render_widget(List::new(items).block(list_block), chunks[1]);
        f.render_widget(Paragraph::new(footer_lines), chunks[2]);
    }

    pub(super) fn draw_model_modal(&self, f: &mut Frame) {
        let items: Vec<ListItem> = self
            .config
            .models
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let is_active = m.name == self.model;
                let (prefix, style) = selection_style(i == self.model_idx, is_active);
                let active = if is_active { " [active]" } else { "" };
                ListItem::new(Line::from(Span::styled(
                    format!(
                        "{prefix}{} (ctx: {}, tool max: {}KB){active}",
                        m.name,
                        m.context,
                        m.max_tool_output_size / 1024
                    ),
                    style,
                )))
            })
            .collect();

        let widget = List::new(items).block(chat_block(" Select Model (⏎=Switch, Esc=Cancel) "));
        render_modal(f, 65, 45, widget);
    }

    pub(super) fn draw_tool_modal(&self, f: &mut Frame) {
        let (preview, idx, total) = self
            .pending_tool
            .as_ref()
            .map_or(("", 1, 1), |(p, i, t)| (p.as_str(), *i, *t));

        let mut lines = format_tool_preview_lines(preview);
        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "Optional Feedback / Note:",
                Style::default()
                    .fg(theme::MODAL_SECTION_LABEL)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("> {}", sanitize_line(&self.tool_feedback))),
            Line::from(""),
        ]);
        lines.extend(
            [
                "[⏎] Approve Once",
                "[^A] Always (Session)",
                "[Esc] Decline",
                "[⇧Esc] Cancel All",
                "[↑/↓/PgUp/PgDn] Scroll",
            ]
            .into_iter()
            .map(hint_line),
        );

        let widget = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.tool_scroll, 0))
            .block(
                chat_block(format!(
                    " ⚠ Tool Execution Approval Required ({idx}/{total}) "
                ))
                .border_style(Style::default().fg(theme::BORDER_MODAL_WARN)),
            );
        let area = top_right_rect(55, 55, 72, 20, f.area());
        render_modal_at(f, area, widget);
    }

    pub(super) fn draw_truncated_tool_modal(&mut self, f: &mut Frame) {
        let max_msg_lines = self.config.app.max_msg_lines;
        let Some(ref mut pending) = self.pending_truncated else {
            return;
        };

        let (pct_y, min_h) = if pending.collapsed {
            (28, 14)
        } else {
            (66, 26)
        };
        let area = top_right_rect(64, pct_y, 84, min_h, f.area());

        let raw_kb = (pending.raw_bytes / 1024).max(1);
        let max_kb = (pending.max_size / 1024).max(1);
        let full_cap_kb = (pending.full_cap_bytes / 1024).max(1);
        let captured_full_kb = (pending.raw_bytes.min(pending.full_cap_bytes) / 1024).max(1);

        let preview_lines: Vec<String> = pending.preview.lines().map(sanitize_line).collect();
        let total_content = preview_lines.len();
        let inner_h = usize::from(area.height.saturating_sub(14)).max(5);
        let max_lines = if pending.unfolded {
            inner_h
        } else {
            max_msg_lines.min(inner_h)
        };

        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "Tool: ",
                    Style::default()
                        .fg(theme::MODAL_TITLE_ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    sanitize_line(&pending.summary),
                    Style::default().fg(theme::TRUNCATED_SUMMARY),
                ),
                Span::styled(
                    format!(" ({})", sanitize_line(&pending.tool_name)),
                    Style::default().fg(theme::TRUNCATED_TOOL_NAME),
                ),
            ]),
            Line::from(Span::styled(
                format!(
                    "Output size (~{raw_kb} KB) exceeded the {max_kb} KB tool limit (context-padded cap: {full_cap_kb} KB).",
                ),
                Style::default()
                    .fg(theme::TRUNCATED_WARNING)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];

        let mut preview_header = vec![
            Span::styled(
                format!("  ⚙ {} ", sanitize_line(&pending.summary)),
                Style::default()
                    .fg(theme::HEADER_TOOL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "⚠ TRUNCATED ",
                Style::default()
                    .fg(theme::BADGE_TRUNCATED)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if pending.collapsed {
            preview_header.push(Span::styled(
                format!("+{total_content}"),
                Style::default()
                    .fg(theme::BADGE_COLLAPSED)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        lines.push(Line::from(preview_header));

        if !pending.collapsed {
            let (start, end) = if total_content > max_lines {
                pending.scroll = pending.scroll.min(total_content.saturating_sub(max_lines));
                (
                    pending.scroll,
                    (pending.scroll + max_lines).min(total_content),
                )
            } else {
                pending.scroll = 0;
                (0, total_content)
            };

            if start > 0 {
                lines.push(Line::from(Span::styled(
                    format!("    ... ({start}) ..."),
                    Style::default().fg(theme::SCROLL_ELLIPSIS),
                )));
            } else {
                lines.push(Line::from(""));
            }
            for l in &preview_lines[start..end] {
                lines.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(theme::TRUNCATED_PREVIEW_TEXT),
                )));
            }
            if end < total_content {
                lines.push(Line::from(Span::styled(
                    format!("    ... ({}) ...", total_content - end),
                    Style::default().fg(theme::SCROLL_ELLIPSIS),
                )));
            }
        }

        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "Optional Condensation Guidance / Note ([^X] Clear):",
                Style::default()
                    .fg(theme::MODAL_SECTION_LABEL)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("> {}", sanitize_line(&self.truncated_guide_prompt))),
            Line::from(""),
            hint_line(format!("[⏎] Proceed Truncated ({max_kb}KB)")),
            hint_line(format!("[^F] Proceed Full ({captured_full_kb}KB)")),
        ]);
        lines.extend(
            [
                "[^C] Condense Truncated",
                "[^G] Condense Full",
                "[Esc/^D] Discard Output",
                "[⇧Esc] Cancel All",
                "[Tab/Click] Fold/Unfold",
                "[↑/↓/PgUp/PgDn] Scroll",
            ]
            .into_iter()
            .map(hint_line),
        );

        let widget = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            chat_block(format!(
                " ⚠ Tool Output Truncated ({}/{}) ",
                pending.index, pending.total
            ))
            .border_style(Style::default().fg(theme::BORDER_MODAL_DANGER)),
        );
        render_modal_at(f, area, widget);
    }

    pub(super) fn draw_system_prompt_modal(&mut self, f: &mut Frame) {
        self.sys_prompt_area.set_cursor_style(super::cursor_style(
            self.term_focused && self.cursor_blink_on,
        ));
        let preset_name = super::preset_label(&self.config.system_presets, self.sys_preset_idx);
        let header = Line::from(vec![
            Span::styled(
                "Base System Message ",
                Style::default().fg(theme::MODAL_TITLE_ACCENT),
            ),
            Span::raw("(Invoked @skill context is appended per turn) │ "),
            Span::styled(
                format!("Preset: {preset_name}"),
                Style::default()
                    .fg(theme::PRESET_ACTIVE)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        draw_editor_modal(
            f,
            " System Message Control (^Y) ",
            header,
            &self.sys_prompt_area,
            " [^P] Cycle Preset\n [^X] Clear\n [⇧⏎] Newline\n [⏎] Save & Apply\n [Esc] Cancel",
        );
    }

    pub(super) fn draw_memory_modal(&mut self, f: &mut Frame) {
        self.memory_area.set_cursor_style(super::cursor_style(
            self.term_focused && self.cursor_blink_on,
        ));
        let (status_label, status_color) = if self.memory_enabled_draft {
            ("ENABLED", theme::STATUS_ENABLED)
        } else {
            ("DISABLED", theme::STATUS_DISABLED)
        };
        let (global_style, ws_style) = tab_styles(
            self.memory_scope == MemoryScope::Global,
            theme::TAB_GLOBAL_ACTIVE,
            theme::TAB_WORKSPACE_ACTIVE,
        );
        let header = Line::from(vec![
            Span::styled(
                "Persistent Memory (MEMORY.md) ",
                Style::default().fg(theme::MODAL_TITLE_ACCENT),
            ),
            Span::raw("│ Scope: ["),
            Span::styled("Global", global_style),
            Span::raw(" / "),
            Span::styled("Workspace", ws_style),
            Span::raw("] │ "),
            Span::styled(
                format!("Status: {status_label}"),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        draw_editor_modal(
            f,
            " Memory (^P) ",
            header,
            &self.memory_area,
            " [Tab/^S] Toggle Scope (Global/Workspace)\n [^E] Toggle ON/OFF\n [^X] Clear\n [⇧⏎] Newline\n [⏎] Save & Persist\n [Esc] Cancel",
        );
    }

    pub(super) fn draw_payload_inspector_modal(&mut self, f: &mut Frame) {
        let area = centered_rect(85, 80, f.area());
        f.render_widget(Clear, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Fill(1), Constraint::Length(3)])
            .split(area);

        let lines = format_tool_preview_lines(&self.last_payload_preview);
        let inner_h = chunks[0].height.saturating_sub(2).max(1);
        let max_scroll = (lines.len() as u16).saturating_sub(inner_h);
        self.payload_scroll = self.payload_scroll.min(max_scroll);

        let widget = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.payload_scroll, 0))
            .block(
                chat_block(" Outgoing Payload & System Prompt Inspector (^G / /inspect) ")
                    .border_style(Style::default().fg(theme::BORDER_MODAL_ACCENT)),
            );
        f.render_widget(widget, chunks[0]);

        let payload_footer: Vec<Line> = [
            " [↑/↓/PgUp/PgDn/Home/End] Scroll",
            " [C] Copy to Clipboard",
            " [Esc/^G] Close",
        ]
        .into_iter()
        .map(hint_line)
        .collect();
        f.render_widget(Paragraph::new(payload_footer), chunks[1]);
    }

    pub(super) fn draw_context_menu(&self, f: &mut Frame, menu: &super::ContextMenu) {
        let area = menu.rect(f.area());
        let items: Vec<ListItem> = menu
            .actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let (prefix, style) = selection_style(i == menu.selected, false);
                ListItem::new(Line::from(Span::styled(
                    format!("{prefix}{}", action.label()),
                    style,
                )))
            })
            .collect();

        let block = titled_block("").border_style(Style::default().fg(theme::BORDER_MODAL_ACCENT));
        render_modal_at(f, area, List::new(items).block(block));
    }
}

fn draw_editor_modal(
    f: &mut Frame,
    title: &'static str,
    header_line: Line<'_>,
    editor: &ratatui_textarea::TextArea<'static>,
    footer_text: &'static str,
) {
    let area = centered_rect(75, 60, f.area());
    f.render_widget(Clear, area);

    let footer_lines: Vec<Line> = footer_text.split('\n').map(hint_line).collect();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            // `Fill(1)` has the lowest layout priority — the header/footer
            // `Length` rows are satisfied first, then the editor absorbs only
            // the remainder. `Min(6)` instead claims *all* leftover space,
            // which on a short terminal can squeeze the hint bar to zero.
            Constraint::Fill(1),
            Constraint::Length(footer_lines.len() as u16),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(header_line).block(titled_block(title)),
        chunks[0],
    );
    f.render_widget(editor, chunks[1]);
    f.render_widget(Paragraph::new(footer_lines), chunks[2]);
}
