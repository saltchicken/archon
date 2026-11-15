use super::{TuiApp, chat_block, render::sanitize_line, titled_block};
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
    let width = (r.width.saturating_mul(percent_x) / 100)
        .max(min_width)
        .min(r.width);
    let height = (r.height.saturating_mul(percent_y) / 100)
        .max(min_height)
        .min(r.height);
    Rect::new(r.x + r.width.saturating_sub(width), r.y, width, height)
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
            "Press ⏎ or drag-and-drop image path:\n\n> {}\n\n[⏎] Import    [Esc] Cancel",
            sanitize_line(&self.file_path_input)
        ))
        .block(chat_block(" Add Image File "));
        render_modal(f, 60, 20, widget);
    }

    pub(super) fn draw_sessions_modal(&self, f: &mut Frame) {
        let items: Vec<ListItem> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let prefix = if i == self.session_idx { ">> " } else { "   " };
                ListItem::new(format!(
                    "{prefix}[{}] {} ({} msgs)",
                    s.updated_at,
                    sanitize_line(&s.title),
                    s.messages
                ))
            })
            .collect();

        let widget = List::new(items).block(chat_block(
            " Saved Sessions (⏎=Load, D=Delete, Esc=Close) ",
        ));
        render_modal(f, 70, 60, widget);
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
                "[⏎] Approve Once    [^A] Always (Session)    [Esc] Decline    [⇧Esc] Cancel All    [↑/↓/PgUp/PgDn] Scroll",
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
        let area = top_right_rect(45, 45, 60, 16, f.area());
        render_modal_at(f, area, widget);
    }

    pub(super) fn draw_system_prompt_modal(&self, f: &mut Frame) {
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
            " [^P] Cycle Preset   [^X] Clear   [⇧⏎] Newline   [⏎] Save & Apply   [Esc] Cancel",
        );
    }

    pub(super) fn draw_personal_intelligence_modal(&self, f: &mut Frame) {
        let (status_label, status_color) = if self.pi_enabled_draft {
            ("ENABLED", Color::Green)
        } else {
            ("DISABLED", Color::Red)
        };
        let header = Line::from(vec![
            Span::styled(
                "Persistent Personal Intelligence ",
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("(Always passed to the model across all sessions) │ "),
            Span::styled(
                format!("Status: {status_label}"),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        draw_editor_modal(
            f,
            " Personal Intelligence (^P) ",
            header,
            &self.pi_area,
            " [^E] Toggle ON/OFF   [^X] Clear   [⇧⏎] Newline   [⏎] Save & Persist   [Esc] Cancel",
        );
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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(header_line).block(titled_block(title)),
        chunks[0],
    );
    f.render_widget(editor, chunks[1]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer_text,
            Style::default().fg(Color::Yellow),
        ))),
        chunks[2],
    );
}
