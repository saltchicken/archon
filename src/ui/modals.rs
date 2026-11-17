use super::{SessionView, TuiApp, chat_block, render::sanitize_line, titled_block};
use crate::core::config::MemoryScope;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
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

fn selection_style(selected: bool, active: bool) -> (&'static str, Style) {
    let prefix = if selected { ">> " } else { "   " };
    let style = if selected {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    (prefix, style)
}

fn format_tool_preview_lines(preview: &str) -> Vec<Line<'static>> {
    let mut section_color = Color::Blue;
    let mut in_diff = false;

    preview
        .lines()
        .map(|raw_line| {
            let line = sanitize_line(raw_line);
            if line.starts_with("--- ") && line.ends_with(" ---") {
                in_diff = line == "--- UNIFIED DIFF ---";
                let header_color = if line.contains("WARNING") || line.contains("SEARCH") {
                    section_color = Color::LightRed;
                    Color::Red
                } else if line.contains("REPLACE")
                    || line.contains("CONTENT")
                    || line.contains("MESSAGE")
                {
                    section_color = Color::Cyan;
                    Color::Green
                } else {
                    section_color = Color::White;
                    Color::Cyan
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
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if line.starts_with("@@") {
                    Style::default().fg(Color::Cyan)
                } else if line.starts_with('+') {
                    Style::default().fg(Color::Green)
                } else if line.starts_with('-') {
                    Style::default().fg(Color::LightRed)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Line::from(Span::styled(line, style))
            } else {
                Line::from(Span::styled(line, Style::default().fg(section_color)))
            }
        })
        .collect()
}

impl TuiApp {
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

        let stored_count = self.sessions.iter().filter(|s| !s.pinned).count();
        let pinned_count = self.sessions.iter().filter(|s| s.pinned).count();

        let (stored_style, pinned_style) = match self.session_view {
            SessionView::AutoStored => (
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                Style::default().fg(Color::DarkGray),
            ),
            SessionView::Pinned => (
                Style::default().fg(Color::DarkGray),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ),
        };

        let pin_action_label = match self.session_view {
            SessionView::AutoStored => "[P] Pin",
            SessionView::Pinned => "[P] Unpin",
        };

        let header = Line::from(vec![
            Span::styled("Conversations ", Style::default().fg(Color::Cyan)),
            Span::raw("│ View: ["),
            Span::styled(format!("Auto-Stored ({stored_count})"), stored_style),
            Span::raw(" / "),
            Span::styled(format!("󰐃 Pinned ({pinned_count})"), pinned_style),
            Span::raw("]"),
        ]);

        let footer_lines = vec![Line::from(Span::styled(
            format!(
                " [Tab/←/→] Switch View   {pin_action_label}   [⏎] Load   [D] Delete   [Esc] Close"
            ),
            Style::default().fg(Color::Yellow),
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
            SessionView::AutoStored => " Auto-Stored Sessions ",
            SessionView::Pinned => " 󰐃 Pinned Sessions ",
        };
        let list_block = chat_block(list_title).border_style(Style::default().fg(Color::Cyan));
        let inner_height = usize::from(list_block.inner(chunks[1]).height).max(1);

        let items: Vec<ListItem> = if filtered.is_empty() {
            let empty_msg = match self.session_view {
                SessionView::AutoStored => "   No auto-stored sessions found.",
                SessionView::Pinned => {
                    "   No pinned sessions yet — press [P] on an auto-stored session or run /pin."
                }
            };
            vec![ListItem::new(Line::from(Span::styled(
                empty_msg,
                Style::default().fg(Color::DarkGray),
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
                            "{prefix}[{}] {pin_badge}{} ({} msgs){active}",
                            s.updated_at,
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
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("> {}", sanitize_line(&self.tool_feedback))),
            Line::from(""),
            Line::from(Span::styled(
                "[⏎] Approve Once",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[^A] Always (Session)",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[Esc] Decline",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[⇧Esc] Cancel All",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[↑/↓/PgUp/PgDn] Scroll",
                Style::default().fg(Color::Yellow),
            )),
        ]);

        let widget = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.tool_scroll, 0))
            .block(
                chat_block(format!(
                    " ⚠ Tool Execution Approval Required ({idx}/{total}) "
                ))
                .border_style(Style::default().fg(Color::Yellow)),
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
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    sanitize_line(&pending.summary),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!(" ({})", sanitize_line(&pending.tool_name)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(Span::styled(
                format!(
                    "Output size (~{raw_kb} KB) exceeded the {max_kb} KB tool limit (context-padded cap: {full_cap_kb} KB).",
                ),
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];

        let mut preview_header = vec![
            Span::styled(
                format!("  ⚙ {} ", sanitize_line(&pending.summary)),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "⚠ TRUNCATED ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ];
        if pending.collapsed {
            preview_header.push(Span::styled(
                format!("+{total_content}"),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
        lines.push(Line::from(preview_header));

        if !pending.collapsed {
            if total_content > max_lines {
                let max_scroll = total_content.saturating_sub(max_lines);
                pending.scroll = pending.scroll.min(max_scroll);
                let start = pending.scroll;
                let end = (start + max_lines).min(total_content);

                if start > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("    ... ({start}) ..."),
                        Style::default().fg(Color::Blue),
                    )));
                } else {
                    lines.push(Line::from(""));
                }
                for l in &preview_lines[start..end] {
                    lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(Color::Gray),
                    )));
                }
                if end < total_content {
                    lines.push(Line::from(Span::styled(
                        format!("    ... ({}) ...", total_content - end),
                        Style::default().fg(Color::Blue),
                    )));
                }
            } else {
                pending.scroll = 0;
                lines.push(Line::from(""));
                for l in &preview_lines {
                    lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(Color::Gray),
                    )));
                }
            }
        }

        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "Optional Condensation Guidance / Note ([^X] Clear):",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("> {}", sanitize_line(&self.truncated_guide_prompt))),
            Line::from(""),
            Line::from(Span::styled(
                format!("[⏎] Proceed Truncated ({max_kb}KB)"),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                format!("[^F] Proceed Full ({captured_full_kb}KB)"),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[^C] Condense Truncated",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[^G] Condense Full",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[Esc/^D] Discard Output",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[⇧Esc] Cancel All",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[Tab/Click] Fold/Unfold",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                "[↑/↓/PgUp/PgDn] Scroll",
                Style::default().fg(Color::Yellow),
            )),
        ]);

        let widget = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            chat_block(format!(
                " ⚠ Tool Output Truncated ({}/{}) ",
                pending.index, pending.total
            ))
            .border_style(Style::default().fg(Color::LightRed)),
        );
        render_modal_at(f, area, widget);
    }

    pub(super) fn draw_system_prompt_modal(&mut self, f: &mut Frame) {
        self.sys_prompt_area.set_cursor_style(super::cursor_style(
            self.term_focused && self.cursor_blink_on,
        ));
        let preset_name = super::preset_label(&self.config.system_presets, self.sys_preset_idx);
        let header = Line::from(vec![
            Span::styled("Base System Message ", Style::default().fg(Color::Cyan)),
            Span::raw("(Invoked @skill context is appended per turn) │ "),
            Span::styled(
                format!("Preset: {preset_name}"),
                Style::default()
                    .fg(Color::Green)
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
            ("ENABLED", Color::Green)
        } else {
            ("DISABLED", Color::Red)
        };
        let (global_style, ws_style) = match self.memory_scope {
            MemoryScope::Global => (
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                Style::default().fg(Color::DarkGray),
            ),
            MemoryScope::Workspace => (
                Style::default().fg(Color::DarkGray),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ),
        };
        let header = Line::from(vec![
            Span::styled(
                "Persistent Memory (MEMORY.md) ",
                Style::default().fg(Color::Cyan),
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
                    .border_style(Style::default().fg(Color::Cyan)),
            );
        f.render_widget(widget, chunks[0]);

        let payload_footer: Vec<Line> = [
            " [↑/↓/PgUp/PgDn/Home/End] Scroll",
            " [C] Copy to Clipboard",
            " [Esc/^G] Close",
        ]
        .iter()
        .map(|s| Line::from(Span::styled(*s, Style::default().fg(Color::Yellow))))
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

        let block = titled_block("").border_style(Style::default().fg(Color::Cyan));
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

    let footer_lines: Vec<Line> = footer_text
        .split('\n')
        .map(|s| {
            Line::from(Span::styled(
                s.to_string(),
                Style::default().fg(Color::Yellow),
            ))
        })
        .collect();

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
