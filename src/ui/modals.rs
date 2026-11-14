use super::{TuiApp, render::sanitize_line, titled_block};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, Paragraph, Widget, Wrap},
};

pub const CTX_PRESETS: &[(u32, &str)] = &[
    (4_096, "4k"),
    (8_192, "8k"),
    (16_384, "16k"),
    (32_768, "32k"),
    (65_536, "64k"),
    (131_072, "128k"),
    (262_144, "256k"),
    (1_048_576, "1M"),
];

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

fn render_modal<W: Widget>(f: &mut Frame, percent_x: u16, percent_y: u16, widget: W) {
    let area = centered_rect(percent_x, percent_y, f.area());
    f.render_widget(Clear, area);
    f.render_widget(widget, area);
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
    let mut section_color = Color::White;
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
        .block(titled_block(" Add Image File "));
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

        let widget = List::new(items).block(titled_block(
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

        let widget = List::new(items).block(titled_block(" Select Model (⏎=Switch, Esc=Cancel) "));
        render_modal(f, 65, 45, widget);
    }

    pub(super) fn draw_context_modal(&self, f: &mut Frame) {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Active Context: ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{} tokens", self.num_ctx),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    "Custom Value:   ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(if self.ctx_input.is_empty() {
                    "(select preset below or type digits)"
                } else {
                    &self.ctx_input
                }),
            ]),
            Line::from(""),
        ];

        for (i, (val, label)) in CTX_PRESETS.iter().enumerate() {
            let selected = i == self.ctx_preset_idx && self.ctx_input.is_empty();
            let (prefix, style) = selection_style(selected, *val == self.num_ctx);
            lines.push(Line::from(Span::styled(
                format!("{prefix}{val:>9} tokens ({label})"),
                style,
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[↑/↓] Select Preset   [0-9] Type Custom   [⏎] Apply   [Esc] Cancel",
            Style::default().fg(Color::Yellow),
        )));

        let widget = Paragraph::new(lines).block(titled_block(" Context Window (^W) "));
        render_modal(f, 55, 50, widget);
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
                titled_block(format!(
                    " ⚠ Tool Execution Approval Required ({idx}/{total}) "
                ))
                .border_style(Style::default().fg(Color::Yellow)),
            );
        render_modal(f, 80, 75, widget);
    }

    pub(super) fn draw_skills_modal(&self, f: &mut Frame) {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Active Skills: ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    if self.active_skills.is_empty() {
                        "None (activate via Space below or `/command` in prompt)".to_string()
                    } else {
                        self.active_skills.join(", ")
                    },
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
        ];

        if self.config.skills.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No skills found in config.toml, ~/.config/archon/skills, or .archon/skills.",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (i, skill) in self.config.skills.iter().enumerate() {
                let is_active = self
                    .active_skills
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(&skill.name));
                let (prefix, style) = selection_style(i == self.skill_idx, is_active);
                let checkbox = if is_active { "[x]" } else { "[ ]" };
                let cmd = if skill.command.is_empty() {
                    String::new()
                } else {
                    format!(" (/{})", skill.command)
                };
                let tools = if skill.tools.is_empty() {
                    String::new()
                } else {
                    format!(" [tools: {}]", skill.tools.join(", "))
                };
                lines.push(Line::from(Span::styled(
                    format!("{prefix}{checkbox} {}{cmd}{tools}", skill.name),
                    style,
                )));
                lines.push(Line::from(Span::styled(
                    format!("       {}", sanitize_line(&skill.description)),
                    Style::default().fg(Color::Gray),
                )));
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[Space] Toggle Skill   [R] Insert /command   [C] Clear All   [⏎/Esc] Apply & Close",
            Style::default().fg(Color::Yellow),
        )));

        let widget = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(titled_block(" Skills Manager (^E) "));
        render_modal(f, 75, 65, widget);
    }

    pub(super) fn draw_system_prompt_modal(&self, f: &mut Frame) {
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

        let preset_name = super::preset_label(&self.config.system_presets, self.sys_preset_idx);
        let header = Paragraph::new(Line::from(vec![
            Span::styled("Base System Message ", Style::default().fg(Color::Cyan)),
            Span::raw("(Active skills are appended automatically) │ "),
            Span::styled(
                format!("Preset: {preset_name}"),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .block(titled_block(" System Message Control (^Y) "));
        f.render_widget(header, chunks[0]);

        f.render_widget(&self.sys_prompt_area, chunks[1]);

        let footer = Paragraph::new(Line::from(Span::styled(
            " [^P] Cycle Preset   [^X] Clear   [⇧⏎] Newline   [⏎] Save & Apply   [Esc] Cancel",
            Style::default().fg(Color::Yellow),
        )));
        f.render_widget(footer, chunks[2]);
    }
}
