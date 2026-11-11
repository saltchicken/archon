use super::{titled_block, TuiApp};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, Paragraph, Widget, Wrap},
    Frame,
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

fn render_modal<W: Widget>(f: &mut Frame, percent_x: u16, percent_y: u16, widget: W) {
    let area = centered_rect(percent_x, percent_y, f.area());
    f.render_widget(Clear, area);
    f.render_widget(widget, area);
}

fn format_tool_preview_lines(preview: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut section_color = Color::White;
    let mut in_diff = false;

    for line in preview.lines() {
        if line.starts_with("--- ") && line.ends_with(" ---") {
            in_diff = line == "--- UNIFIED DIFF ---";
            let header_color = if line.contains("WARNING") || line.contains("SEARCH") {
                section_color = Color::LightRed;
                Color::Red
            } else if line.contains("REPLACE") || line.contains("CONTENT") {
                section_color = Color::Cyan;
                Color::Green
            } else {
                section_color = Color::White;
                Color::Cyan
            };
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(header_color).add_modifier(Modifier::BOLD),
            )));
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
            lines.push(Line::from(Span::styled(line, style)));
        } else {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(section_color),
            )));
        }
    }

    lines
}

impl TuiApp {
    pub(super) fn draw_file_modal(&self, f: &mut Frame) {
        let widget = Paragraph::new(format!(
            "Press ⏎ or drag-and-drop image path:\n\n> {}\n\n[⏎] Import    [Esc] Cancel",
            self.file_path_input
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
                    s.updated_at, s.title, s.messages
                ))
            })
            .collect();

        let widget = List::new(items)
            .block(titled_block(" Saved Sessions (⏎=Load, D=Delete, Esc=Close) "));
        render_modal(f, 70, 60, widget);
    }

    pub(super) fn draw_tool_modal(&self, f: &mut Frame) {
        let (preview, idx, total) = self
            .pending_tool
            .as_ref()
            .map(|(p, i, t)| (p.as_str(), *i, *t))
            .unwrap_or(("", 1, 1));

        let mut lines = format_tool_preview_lines(preview);

        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "Optional Feedback / Note:",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("> {}", self.tool_feedback)),
            Line::from(""),
            Line::from(Span::styled(
                "[⏎] Approve & Run      [Esc] Decline      [↑/↓/PgUp/PgDn] Scroll",
                Style::default().fg(Color::Yellow),
            )),
        ]);

        let widget = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.tool_scroll, 0))
            .block(
                titled_block(format!(
                    " ⚠️ Tool Execution Approval Required ({idx}/{total}) "
                ))
                .border_style(Style::default().fg(Color::Yellow)),
            );
        render_modal(f, 80, 75, widget);
    }
}
