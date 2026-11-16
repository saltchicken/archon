use super::{ActiveModal, MsgUiState, SlashCommand, TuiApp, titled_block};
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

#[derive(Clone, Copy, PartialEq)]
enum ColAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone)]
enum ListMarker {
    Bullet {
        depth: usize,
    },
    Ordered {
        number: String,
        delim: char,
        depth: usize,
    },
    Task {
        checked: bool,
        depth: usize,
        ordered: Option<(String, char)>,
    },
}

enum QuoteContent {
    Plain(Vec<Span<'static>>),
    Header {
        level: usize,
        spans: Vec<Span<'static>>,
    },
    ListItem {
        indent: usize,
        marker: ListMarker,
        spans: Vec<Span<'static>>,
    },
    ListContinuation {
        indent: usize,
        spans: Vec<Span<'static>>,
    },
}

enum WrappedLine {
    CodeFence(String),
    CodeLine(String),
    TableBorder(String),
    TableRow(Vec<Span<'static>>),
    Header {
        level: usize,
        spans: Vec<Span<'static>>,
    },
    ListItem {
        indent: usize,
        marker: ListMarker,
        spans: Vec<Span<'static>>,
    },
    ListContinuation {
        indent: usize,
        spans: Vec<Span<'static>>,
    },
    Blockquote {
        indent: usize,
        depth: usize,
        content: QuoteContent,
    },
    Plain(Vec<Span<'static>>),
}

enum ParsedListKind {
    Bullet,
    Ordered {
        number: String,
        delim: char,
    },
    Task {
        checked: bool,
        ordered: Option<(String, char)>,
    },
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

#[derive(Clone, Debug)]
enum InlineTokenKind {
    Text(String),
    Code(String),
    Delim {
        ch: char,
        count: usize,
        orig_count: usize,
        can_open: bool,
        can_close: bool,
    },
}

#[derive(Clone, Debug)]
struct InlineToken {
    kind: InlineTokenKind,
    modifiers: Modifier,
}

fn find_closing_backticks(s: &str, tick_count: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run_start = i;
            while i < bytes.len() && bytes[i] == b'`' {
                i += 1;
            }
            if i - run_start == tick_count {
                return Some(run_start);
            }
        } else {
            i += 1;
        }
    }
    None
}

fn is_markdown_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace())
}

fn tokenize_inline_markdown(text: &str) -> Vec<InlineToken> {
    let bytes = text.as_bytes();
    let mut pos = 0usize;
    let mut text_start = 0usize;
    let mut tokens = Vec::new();

    let flush_text = |tokens: &mut Vec<InlineToken>, start: usize, end: usize| {
        if start < end {
            tokens.push(InlineToken {
                kind: InlineTokenKind::Text(text[start..end].to_string()),
                modifiers: Modifier::empty(),
            });
        }
    };

    while pos < bytes.len() {
        let b = bytes[pos];
        if b == b'\\'
            && pos + 1 < bytes.len()
            && matches!(bytes[pos + 1], b'*' | b'_' | b'~' | b'`' | b'\\')
        {
            flush_text(&mut tokens, text_start, pos);
            tokens.push(InlineToken {
                kind: InlineTokenKind::Text(text[pos + 1..pos + 2].to_string()),
                modifiers: Modifier::empty(),
            });
            pos += 2;
            text_start = pos;
            continue;
        }

        if b == b'`' {
            let mut tick_count = 0usize;
            while pos + tick_count < bytes.len() && bytes[pos + tick_count] == b'`' {
                tick_count += 1;
            }
            let after_open = pos + tick_count;
            if let Some(rel_close) = find_closing_backticks(&text[after_open..], tick_count) {
                flush_text(&mut tokens, text_start, pos);
                let end_pos = after_open + rel_close + tick_count;
                tokens.push(InlineToken {
                    kind: InlineTokenKind::Code(text[pos..end_pos].to_string()),
                    modifiers: Modifier::empty(),
                });
                pos = end_pos;
                text_start = pos;
            } else {
                pos = after_open;
            }
            continue;
        }

        if matches!(b, b'*' | b'_' | b'~') {
            flush_text(&mut tokens, text_start, pos);
            let ch = b as char;
            let mut count = 0usize;
            while pos + count < bytes.len() && bytes[pos + count] == b {
                count += 1;
            }
            let end_pos = pos + count;
            let prev_char = text[..pos].chars().next_back();
            let next_char = text[end_pos..].chars().next();

            let prev_ws = prev_char.is_none_or(char::is_whitespace);
            let next_ws = next_char.is_none_or(char::is_whitespace);
            let prev_punct = prev_char.is_some_and(is_markdown_punct);
            let next_punct = next_char.is_some_and(is_markdown_punct);

            let left_flanking = !next_ws && (!next_punct || prev_ws || prev_punct);
            let right_flanking = !prev_ws && (!prev_punct || next_ws || next_punct);

            let is_glob_slash =
                ch == '*' && prev_char == Some('/') && matches!(next_char, Some('/' | '.'));

            let (can_open, can_close) = if is_glob_slash {
                (false, false)
            } else {
                match ch {
                    '*' => (left_flanking, right_flanking),
                    '_' => (
                        left_flanking && (!right_flanking || prev_punct),
                        right_flanking && (!left_flanking || next_punct),
                    ),
                    '~' => (left_flanking && count == 2, right_flanking && count == 2),
                    _ => (false, false),
                }
            };

            tokens.push(InlineToken {
                kind: InlineTokenKind::Delim {
                    ch,
                    count,
                    orig_count: count,
                    can_open,
                    can_close,
                },
                modifiers: Modifier::empty(),
            });
            pos = end_pos;
            text_start = pos;
            continue;
        }

        pos += text[pos..].chars().next().map_or(1, char::len_utf8);
    }

    flush_text(&mut tokens, text_start, text.len());
    tokens
}

fn push_merged_span(target: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = target.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
    } else {
        target.push(Span::styled(text.to_string(), style));
    }
}

fn parse_inline_spans(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut tokens = tokenize_inline_markdown(text);

    let mut closer_idx = 0usize;
    while closer_idx < tokens.len() {
        let (ch, closer_count, closer_orig, closer_can_open, closer_can_close) =
            match tokens[closer_idx].kind {
                InlineTokenKind::Delim {
                    ch,
                    count,
                    orig_count,
                    can_open,
                    can_close,
                } if can_close && count > 0 => (ch, count, orig_count, can_open, can_close),
                _ => {
                    closer_idx += 1;
                    continue;
                }
            };

        let mut found_opener = None;
        for opener_idx in (0..closer_idx).rev() {
            if let InlineTokenKind::Delim {
                ch: o_ch,
                count: o_count,
                orig_count: o_orig,
                can_open: o_can_open,
                can_close: o_can_close,
            } = tokens[opener_idx].kind
            {
                if o_ch != ch || !o_can_open || o_count == 0 {
                    continue;
                }
                if ch == '~' {
                    if o_count >= 2 && closer_count >= 2 {
                        found_opener = Some(opener_idx);
                        break;
                    }
                    continue;
                }
                if ((o_can_open && o_can_close) || (closer_can_open && closer_can_close))
                    && (o_orig + closer_orig) % 3 == 0
                    && !(o_orig % 3 == 0 && closer_orig % 3 == 0)
                {
                    continue;
                }
                found_opener = Some(opener_idx);
                break;
            }
        }

        let Some(opener_idx) = found_opener else {
            closer_idx += 1;
            continue;
        };

        let o_count = match tokens[opener_idx].kind {
            InlineTokenKind::Delim { count, .. } => count,
            _ => unreachable!(),
        };

        let (use_delims, modifier) = if ch == '~' {
            (2, Modifier::CROSSED_OUT)
        } else if o_count >= 2 && closer_count >= 2 {
            (2, Modifier::BOLD)
        } else {
            (1, Modifier::ITALIC)
        };

        if let InlineTokenKind::Delim { count, .. } = &mut tokens[opener_idx].kind {
            *count -= use_delims;
        }
        if let InlineTokenKind::Delim { count, .. } = &mut tokens[closer_idx].kind {
            *count -= use_delims;
        }

        for token in &mut tokens[(opener_idx + 1)..closer_idx] {
            token.modifiers.insert(modifier);
            if let InlineTokenKind::Delim { can_open, .. } = &mut token.kind {
                *can_open = false;
            }
        }
    }

    let mut spans = Vec::new();
    for token in tokens {
        let style = base_style.add_modifier(token.modifiers);
        match token.kind {
            InlineTokenKind::Text(s) => push_merged_span(&mut spans, &s, style),
            InlineTokenKind::Code(s) => push_merged_span(&mut spans, &s, style.fg(Color::Yellow)),
            InlineTokenKind::Delim { ch, count, .. } if count > 0 => {
                push_merged_span(&mut spans, &ch.to_string().repeat(count), style);
            }
            InlineTokenKind::Delim { .. } => {}
        }
    }
    spans
}

fn spans_display_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum()
}

fn wrap_styled_spans(
    spans: Vec<Span<'static>>,
    width: usize,
    break_words: bool,
) -> Vec<Vec<Span<'static>>> {
    if spans.is_empty() || spans.iter().all(|s| s.content.is_empty()) {
        return vec![Vec::new()];
    }

    let mut pieces: Vec<(bool, Vec<Span<'static>>)> = Vec::new();
    for span in spans {
        let s = span.content.as_ref();
        let style = span.style;
        let mut start = 0usize;
        let mut cur_is_space: Option<bool> = None;

        for (idx, ch) in s.char_indices() {
            let is_space = ch == ' ';
            if let Some(prev_space) = cur_is_space {
                if prev_space != is_space {
                    let seg = &s[start..idx];
                    if let Some((last_space, last_vec)) = pieces.last_mut()
                        && *last_space == prev_space
                    {
                        push_merged_span(last_vec, seg, style);
                    } else {
                        let mut v = Vec::new();
                        push_merged_span(&mut v, seg, style);
                        pieces.push((prev_space, v));
                    }
                    start = idx;
                    cur_is_space = Some(is_space);
                }
            } else {
                cur_is_space = Some(is_space);
            }
        }

        if let Some(prev_space) = cur_is_space {
            let seg = &s[start..];
            if let Some((last_space, last_vec)) = pieces.last_mut()
                && *last_space == prev_space
            {
                push_merged_span(last_vec, seg, style);
            } else {
                let mut v = Vec::new();
                push_merged_span(&mut v, seg, style);
                pieces.push((prev_space, v));
            }
        }
    }

    let max_width = width.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur_line: Vec<Span<'static>> = Vec::new();
    let mut cur_width = 0usize;
    let mut pending_space: Vec<Span<'static>> = Vec::new();

    for (is_space, piece_spans) in pieces {
        if is_space {
            if lines.is_empty() && cur_line.is_empty() {
                for sp in piece_spans {
                    let mut taken = String::new();
                    for ch in sp.content.chars() {
                        let mut buf = [0u8; 4];
                        let cw = display_width(ch.encode_utf8(&mut buf));
                        if cur_width + cw > max_width {
                            break;
                        }
                        taken.push(ch);
                        cur_width += cw;
                    }
                    push_merged_span(&mut cur_line, &taken, sp.style);
                }
            } else if !cur_line.is_empty() {
                for sp in piece_spans {
                    push_merged_span(&mut pending_space, &sp.content, sp.style);
                }
            }
            continue;
        }

        let word_width = spans_display_width(&piece_spans);
        let space_width = spans_display_width(&pending_space);

        if cur_width > 0 && cur_width + space_width + word_width <= max_width {
            for sp in pending_space.drain(..) {
                push_merged_span(&mut cur_line, &sp.content, sp.style);
            }
            for sp in piece_spans {
                push_merged_span(&mut cur_line, &sp.content, sp.style);
            }
            cur_width += space_width + word_width;
            continue;
        }

        pending_space.clear();
        if cur_width > 0 {
            lines.push(std::mem::take(&mut cur_line));
            cur_width = 0;
        }

        if word_width <= max_width || !break_words {
            for sp in piece_spans {
                push_merged_span(&mut cur_line, &sp.content, sp.style);
            }
            cur_width = word_width;
        } else {
            for sp in piece_spans {
                let mut chunk = String::new();
                for ch in sp.content.chars() {
                    let mut buf = [0u8; 4];
                    let cw = display_width(ch.encode_utf8(&mut buf));
                    if cur_width + cw > max_width && cur_width > 0 {
                        push_merged_span(&mut cur_line, &chunk, sp.style);
                        chunk.clear();
                        lines.push(std::mem::take(&mut cur_line));
                        cur_width = 0;
                    }
                    chunk.push(ch);
                    cur_width += cw;
                }
                push_merged_span(&mut cur_line, &chunk, sp.style);
            }
        }
    }

    if !cur_line.is_empty() || lines.is_empty() {
        lines.push(cur_line);
    }
    lines
}

fn wrap_markdown_line(
    line: &str,
    base_style: Style,
    options: &textwrap::Options<'_>,
) -> Vec<Vec<Span<'static>>> {
    if line.is_empty() {
        vec![Vec::new()]
    } else {
        wrap_styled_spans(
            parse_inline_spans(line, base_style),
            options.width,
            options.break_words,
        )
    }
}

fn wrap_raw_line(
    line: &str,
    style: Style,
    options: &textwrap::Options<'_>,
) -> Vec<Vec<Span<'static>>> {
    if line.is_empty() {
        vec![Vec::new()]
    } else {
        wrap_styled_spans(
            vec![Span::styled(line.to_string(), style)],
            options.width,
            options.break_words,
        )
    }
}

fn draw_table_border(col_widths: &[usize], left: char, mid: char, right: char) -> String {
    let segments: Vec<String> = col_widths.iter().map(|&w| "─".repeat(w + 2)).collect();
    format!("{left}{}{right}", segments.join(&mid.to_string()))
}

fn format_markdown_table(raw_lines: &[&str], available_width: usize) -> Option<Vec<WrappedLine>> {
    if raw_lines.len() < 2 {
        return None;
    }

    let parsed_rows: Vec<Vec<String>> = raw_lines.iter().map(|l| parse_table_cells(l)).collect();
    let mut aligns = parse_separator_row(&parsed_rows[1])?;
    let num_cols = parsed_rows.iter().map(Vec::len).max().filter(|&n| n > 0)?;
    aligns.resize(num_cols, ColAlign::Left);

    let styled_rows: Vec<Vec<Vec<Span<'static>>>> = parsed_rows
        .iter()
        .enumerate()
        .map(|(r_idx, row)| {
            if r_idx == 1 {
                return Vec::new();
            }
            let cell_style = if r_idx == 0 {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            (0..num_cols)
                .map(|c_idx| {
                    let text = row.get(c_idx).map_or("", String::as_str);
                    parse_inline_spans(text, cell_style)
                })
                .collect()
        })
        .collect();

    let mut col_widths = vec![1usize; num_cols];
    for (r_idx, row) in styled_rows.iter().enumerate().filter(|&(i, _)| i != 1) {
        for (c_idx, cell_spans) in row.iter().enumerate() {
            let _ = r_idx;
            col_widths[c_idx] = col_widths[c_idx].max(spans_display_width(cell_spans));
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

    let border_style = Style::default().fg(Color::DarkGray);
    let mut out = vec![WrappedLine::TableBorder(draw_table_border(
        &col_widths,
        '┌',
        '┬',
        '┐',
    ))];
    for (r_idx, row) in styled_rows.into_iter().enumerate() {
        if r_idx == 1 {
            out.push(WrappedLine::TableBorder(draw_table_border(
                &col_widths,
                '├',
                '┼',
                '┤',
            )));
            continue;
        }

        let cell_style = if r_idx == 0 {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let wrapped_cells: Vec<Vec<Vec<Span<'static>>>> = row
            .into_iter()
            .enumerate()
            .map(|(c_idx, cell_spans)| {
                wrap_styled_spans(cell_spans, col_widths[c_idx].max(1), true)
            })
            .collect();
        let max_sublines = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1);

        for sub in 0..max_sublines {
            let mut spans = vec![Span::raw("  ")];
            for c_idx in 0..num_cols {
                spans.push(Span::styled("│", border_style));
                let part = wrapped_cells[c_idx].get(sub);
                let part_width = part.map_or(0, |s| spans_display_width(s));
                let pad = col_widths[c_idx].saturating_sub(part_width);
                let (left_pad, right_pad) = match aligns[c_idx] {
                    ColAlign::Left => (1, 1 + pad),
                    ColAlign::Right => (1 + pad, 1),
                    ColAlign::Center => (1 + pad / 2, 1 + pad - pad / 2),
                };
                spans.push(Span::styled(" ".repeat(left_pad), cell_style));
                if let Some(part_spans) = part {
                    spans.extend(part_spans.iter().cloned());
                }
                spans.push(Span::styled(" ".repeat(right_pad), cell_style));
            }
            spans.push(Span::styled("│", border_style));
            out.push(WrappedLine::TableRow(spans));
        }
    }

    out.push(WrappedLine::TableBorder(draw_table_border(
        &col_widths,
        '└',
        '┴',
        '┘',
    )));
    Some(out)
}

fn flush_table_buffer(
    table_buf: &mut Vec<&str>,
    out: &mut Vec<WrappedLine>,
    options: &textwrap::Options<'_>,
    list_levels: &mut Vec<(usize, usize)>,
) {
    if table_buf.is_empty() {
        return;
    }
    if let Some(formatted) = format_markdown_table(table_buf, options.width) {
        list_levels.clear();
        out.extend(formatted);
    } else {
        list_levels.clear();
        for raw in table_buf.iter() {
            for part in wrap_markdown_line(raw, Style::default(), options) {
                out.push(WrappedLine::Plain(part));
            }
        }
    }
    table_buf.clear();
}

fn is_thematic_break(trimmed: &str) -> bool {
    let mut non_ws = trimmed.chars().filter(|c| !c.is_whitespace());
    let Some(first @ ('-' | '*' | '_')) = non_ws.next() else {
        return false;
    };
    let count = 1 + non_ws.filter(|&c| c == first).count();
    count >= 3 && trimmed.chars().all(|c| c == first || c.is_whitespace())
}

fn parse_task_prefix(content: &str) -> Option<(bool, &str)> {
    let bytes = content.as_bytes();
    if bytes.len() < 3 || bytes[0] != b'[' || bytes[2] != b']' {
        return None;
    }
    let checked = match bytes[1] {
        b' ' => false,
        b'x' | b'X' => true,
        _ => return None,
    };
    let rest = &content[3..];
    if rest.is_empty() {
        Some((checked, ""))
    } else if rest.starts_with(' ') {
        Some((checked, rest.trim_start_matches(' ')))
    } else {
        None
    }
}

fn parse_list_item(line: &str) -> Option<(usize, ParsedListKind, &str)> {
    let raw_indent = line.bytes().take_while(|&b| b == b' ').count();
    let rest = &line[raw_indent..];
    if rest.is_empty() || is_thematic_break(rest) {
        return None;
    }

    let bytes = rest.as_bytes();
    if matches!(bytes[0], b'-' | b'*' | b'+') && bytes.get(1) == Some(&b' ') {
        let content = rest[1..].trim_start_matches(' ');
        if let Some((checked, task_text)) = parse_task_prefix(content) {
            return Some((
                raw_indent,
                ParsedListKind::Task {
                    checked,
                    ordered: None,
                },
                task_text,
            ));
        }
        return Some((raw_indent, ParsedListKind::Bullet, content));
    }

    let digit_count = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if (1..=9).contains(&digit_count)
        && let Some(&delim @ (b'.' | b')')) = bytes.get(digit_count)
        && bytes.get(digit_count + 1) == Some(&b' ')
    {
        let number = rest[..digit_count].to_string();
        let delim = delim as char;
        let content = rest[digit_count + 1..].trim_start_matches(' ');
        if let Some((checked, task_text)) = parse_task_prefix(content) {
            return Some((
                raw_indent,
                ParsedListKind::Task {
                    checked,
                    ordered: Some((number, delim)),
                },
                task_text,
            ));
        }
        return Some((
            raw_indent,
            ParsedListKind::Ordered { number, delim },
            content,
        ));
    }

    None
}

fn list_marker_width(marker: &ListMarker) -> usize {
    match marker {
        ListMarker::Bullet { .. } => 2,
        ListMarker::Ordered { number, .. } => number.len() + 2,
        ListMarker::Task { ordered, .. } => ordered.as_ref().map_or(0, |(n, _)| n.len() + 2) + 2,
    }
}

fn is_diff_hunk_header(line: &str) -> bool {
    line.starts_with("@@ ") && line[3..].contains(" @@")
}

fn is_diff_body_line(line: &str) -> bool {
    !line.is_empty()
        && line.starts_with([' ', '+', '-', '\\'])
        && !line.starts_with("--- ")
        && !line.starts_with("+++ ")
}

fn parse_blockquote(line: &str) -> Option<(usize, usize, &str)> {
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    let mut rest = &line[leading_spaces..];
    let mut depth = 0usize;

    while let Some(after_gt) = rest.strip_prefix('>') {
        depth += 1;
        let one_space = after_gt.strip_prefix(' ').unwrap_or(after_gt);
        let trimmed = after_gt.trim_start_matches(' ');
        rest = if trimmed.starts_with('>') {
            trimmed
        } else {
            one_space
        };
    }

    (depth > 0).then_some((leading_spaces, depth, rest))
}

fn wrap_list_item_lines(
    raw_indent: usize,
    kind: ParsedListKind,
    item_text: &str,
    base_style: Style,
    list_levels: &mut Vec<(usize, usize)>,
    avail_width: usize,
    break_words: bool,
) -> Vec<(usize, Option<ListMarker>, Vec<Span<'static>>)> {
    while list_levels
        .last()
        .is_some_and(|&(top_indent, _)| top_indent > raw_indent)
    {
        list_levels.pop();
    }
    let depth = if let Some(&(top_indent, _)) = list_levels.last() {
        if raw_indent > top_indent {
            list_levels.len()
        } else {
            list_levels.len().saturating_sub(1)
        }
    } else {
        0
    };

    let marker = match kind {
        ParsedListKind::Bullet => ListMarker::Bullet { depth },
        ParsedListKind::Ordered { number, delim } => ListMarker::Ordered {
            number,
            delim,
            depth,
        },
        ParsedListKind::Task { checked, ordered } => ListMarker::Task {
            checked,
            depth,
            ordered,
        },
    };

    let max_indent = avail_width.saturating_sub(8);
    let visual_indent = (depth * 2).min(max_indent);
    let marker_width = list_marker_width(&marker);
    let hanging_indent = (visual_indent + marker_width).min(avail_width.saturating_sub(1));

    if list_levels
        .last()
        .is_none_or(|&(top_indent, _)| raw_indent > top_indent)
    {
        list_levels.push((raw_indent, hanging_indent));
    } else if let Some(top) = list_levels.last_mut() {
        top.1 = hanging_indent;
    }

    let text_width = avail_width.saturating_sub(hanging_indent).max(1);
    let item_opts = textwrap::Options::new(text_width).break_words(break_words);
    let mut wrapped = wrap_markdown_line(item_text, base_style, &item_opts).into_iter();
    let mut result = Vec::new();
    if let Some(first) = wrapped.next() {
        result.push((visual_indent, Some(marker), first));
    }
    for cont in wrapped {
        result.push((hanging_indent, None, cont));
    }
    result
}

fn header_text_style(level: usize) -> Style {
    match level {
        1 => Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        2 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        3 => Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD),
        _ => Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC),
    }
}

fn wrap_text(content: &str, options: &textwrap::Options<'_>) -> Vec<WrappedLine> {
    let mut out = Vec::new();
    let mut table_buf: Vec<&str> = Vec::new();
    let mut list_levels: Vec<(usize, usize)> = Vec::new();
    let mut quote_list_levels: Vec<(usize, usize)> = Vec::new();
    let mut last_quote_depth: Option<usize> = None;
    let mut in_code_block = false;
    let mut in_diff_hunk = false;
    let mut in_params_block = false;
    let quote_style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC);

    for line in content.lines() {
        if line.trim_start().starts_with("```") {
            flush_table_buffer(&mut table_buf, &mut out, options, &mut list_levels);
            quote_list_levels.clear();
            last_quote_depth = None;
            in_diff_hunk = false;
            in_params_block = false;
            in_code_block = !in_code_block;
            out.push(WrappedLine::CodeFence(line.trim().to_string()));
            continue;
        }

        if in_code_block {
            out.push(WrappedLine::CodeLine(line.to_string()));
            continue;
        }

        if is_diff_hunk_header(line) {
            flush_table_buffer(&mut table_buf, &mut out, options, &mut list_levels);
            list_levels.clear();
            quote_list_levels.clear();
            last_quote_depth = None;
            in_params_block = false;
            in_diff_hunk = true;
            for part in wrap_raw_line(line, Style::default(), options) {
                out.push(WrappedLine::Plain(part));
            }
            continue;
        }

        if in_diff_hunk {
            if is_diff_body_line(line) {
                for part in wrap_raw_line(line, Style::default(), options) {
                    out.push(WrappedLine::Plain(part));
                }
                continue;
            }
            in_diff_hunk = false;
        }

        if !line.trim_start().starts_with('>') && is_table_line(line) {
            quote_list_levels.clear();
            last_quote_depth = None;
            in_params_block = false;
            table_buf.push(line);
            continue;
        }

        flush_table_buffer(&mut table_buf, &mut out, options, &mut list_levels);

        if let Some((leading_spaces, depth, quote_text)) = parse_blockquote(line) {
            in_params_block = false;
            while list_levels
                .last()
                .is_some_and(|&(raw_indent, _)| raw_indent >= leading_spaces)
            {
                list_levels.pop();
            }
            let base_indent = list_levels
                .last()
                .map_or(0, |&(_, hanging_indent)| hanging_indent);
            if last_quote_depth != Some(depth) {
                quote_list_levels.clear();
                last_quote_depth = Some(depth);
            }

            let inner_width = options.width.saturating_sub(base_indent + depth * 2).max(1);
            if let Some((level, hdr_content)) = parse_markdown_header(quote_text) {
                quote_list_levels.clear();
                let hdr_opts = textwrap::Options::new(inner_width.saturating_sub(6).max(1))
                    .break_words(options.break_words);
                for spans in wrap_markdown_line(hdr_content, header_text_style(level), &hdr_opts) {
                    out.push(WrappedLine::Blockquote {
                        indent: base_indent,
                        depth,
                        content: QuoteContent::Header { level, spans },
                    });
                }
            } else if let Some((q_raw_indent, kind, item_text)) = parse_list_item(quote_text) {
                for (item_indent, maybe_marker, spans) in wrap_list_item_lines(
                    q_raw_indent,
                    kind,
                    item_text,
                    quote_style,
                    &mut quote_list_levels,
                    inner_width,
                    options.break_words,
                ) {
                    let content = match maybe_marker {
                        Some(marker) => QuoteContent::ListItem {
                            indent: item_indent,
                            marker,
                            spans,
                        },
                        None => QuoteContent::ListContinuation {
                            indent: item_indent,
                            spans,
                        },
                    };
                    out.push(WrappedLine::Blockquote {
                        indent: base_indent,
                        depth,
                        content,
                    });
                }
            } else if quote_text.trim().is_empty() {
                out.push(WrappedLine::Blockquote {
                    indent: base_indent,
                    depth,
                    content: QuoteContent::Plain(Vec::new()),
                });
            } else {
                let q_leading = quote_text.bytes().take_while(|&b| b == b' ').count();
                while quote_list_levels
                    .last()
                    .is_some_and(|&(raw_indent, _)| raw_indent >= q_leading)
                {
                    quote_list_levels.pop();
                }
                if let Some(&(_, hanging_indent)) = quote_list_levels.last() {
                    let text_width = inner_width.saturating_sub(hanging_indent).max(1);
                    let cont_opts =
                        textwrap::Options::new(text_width).break_words(options.break_words);
                    for spans in
                        wrap_markdown_line(quote_text.trim_start(), quote_style, &cont_opts)
                    {
                        out.push(WrappedLine::Blockquote {
                            indent: base_indent,
                            depth,
                            content: QuoteContent::ListContinuation {
                                indent: hanging_indent,
                                spans,
                            },
                        });
                    }
                } else {
                    let q_opts =
                        textwrap::Options::new(inner_width).break_words(options.break_words);
                    for spans in wrap_markdown_line(quote_text, quote_style, &q_opts) {
                        out.push(WrappedLine::Blockquote {
                            indent: base_indent,
                            depth,
                            content: QuoteContent::Plain(spans),
                        });
                    }
                }
            }
            continue;
        }

        quote_list_levels.clear();
        last_quote_depth = None;

        if let Some((level, hdr_content)) = parse_markdown_header(line) {
            list_levels.clear();
            in_params_block = false;
            let hdr_opts = textwrap::Options::new(options.width.saturating_sub(6).max(1))
                .break_words(options.break_words);
            for spans in wrap_markdown_line(hdr_content, header_text_style(level), &hdr_opts) {
                out.push(WrappedLine::Header { level, spans });
            }
        } else if let Some((raw_indent, kind, item_text)) = parse_list_item(line) {
            in_params_block = false;
            for (indent, maybe_marker, spans) in wrap_list_item_lines(
                raw_indent,
                kind,
                item_text,
                Style::default(),
                &mut list_levels,
                options.width,
                options.break_words,
            ) {
                match maybe_marker {
                    Some(marker) => out.push(WrappedLine::ListItem {
                        indent,
                        marker,
                        spans,
                    }),
                    None => out.push(WrappedLine::ListContinuation { indent, spans }),
                }
            }
        } else if line.trim().is_empty() {
            in_params_block = false;
            out.push(WrappedLine::Plain(Vec::new()));
        } else {
            let trimmed = line.trim_start();
            if trimmed == "Parameters:" {
                in_params_block = true;
            }
            if in_params_block {
                list_levels.clear();
                let style = if trimmed == "Parameters:" {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                for spans in wrap_raw_line(line, style, options) {
                    out.push(WrappedLine::Plain(spans));
                }
                continue;
            }

            let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
            while list_levels
                .last()
                .is_some_and(|&(raw_indent, _)| raw_indent >= leading_spaces)
            {
                list_levels.pop();
            }
            if let Some(&(_, hanging_indent)) = list_levels.last() {
                let text_width = options.width.saturating_sub(hanging_indent).max(1);
                let cont_opts = textwrap::Options::new(text_width).break_words(options.break_words);
                for spans in wrap_markdown_line(line.trim_start(), Style::default(), &cont_opts) {
                    out.push(WrappedLine::ListContinuation {
                        indent: hanging_indent,
                        spans,
                    });
                }
            } else if trimmed.starts_with("⚙ [") {
                let style = Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::ITALIC);
                for spans in wrap_raw_line(line, style, options) {
                    out.push(WrappedLine::Plain(spans));
                }
            } else {
                for spans in wrap_markdown_line(line, Style::default(), options) {
                    out.push(WrappedLine::Plain(spans));
                }
            }
        }
    }
    flush_table_buffer(&mut table_buf, &mut out, options, &mut list_levels);
    out
}

fn parse_markdown_header(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let trimmed_rest = rest.trim();
    let stripped = trimmed_rest.trim_end_matches('#');
    let content = if stripped.is_empty() || stripped.ends_with(char::is_whitespace) {
        stripped.trim_end()
    } else {
        trimmed_rest
    };
    Some((level, content))
}

fn style_markdown_header_inner(
    level: usize,
    content_spans: &[Span<'static>],
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    match level {
        1 => {
            let accent = Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD);
            spans.push(Span::styled("▌ ", accent));
            spans.extend(content_spans.iter().cloned());
        }
        2 => {
            let accent = Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD);
            spans.push(Span::styled("▎ ", accent));
            spans.extend(content_spans.iter().cloned());
            spans.push(Span::styled(" ───", Style::default().fg(Color::Blue)));
        }
        3 => {
            let accent = Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD);
            spans.push(Span::styled("▸ ", accent));
            spans.extend(content_spans.iter().cloned());
        }
        _ => {
            let accent = Style::default().fg(Color::DarkGray);
            spans.push(Span::styled("• ", accent));
            spans.extend(content_spans.iter().cloned());
        }
    }
    spans
}

fn style_markdown_header(level: usize, content_spans: &[Span<'static>]) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(style_markdown_header_inner(level, content_spans));
    spans
}

fn style_blockquote_bars(depth: usize) -> Vec<Span<'static>> {
    (0..depth)
        .map(|level| {
            let color = match level {
                0 => Color::Blue,
                1 => Color::Cyan,
                2 => Color::LightBlue,
                _ => Color::DarkGray,
            };
            Span::styled(
                "▎ ",
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

fn ordered_marker_style(depth: usize) -> Style {
    match depth {
        0 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        1 => Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD),
        _ => Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    }
}

fn style_list_marker(marker: &ListMarker) -> Vec<Span<'static>> {
    match marker {
        ListMarker::Bullet { depth } => {
            let (glyph, style) = match depth {
                0 => (
                    "• ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                1 => ("◦ ", Style::default().fg(Color::LightBlue)),
                2 => (
                    "▪ ",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                _ => ("▹ ", Style::default().fg(Color::DarkGray)),
            };
            vec![Span::styled(glyph, style)]
        }
        ListMarker::Ordered {
            number,
            delim,
            depth,
        } => {
            vec![Span::styled(
                format!("{number}{delim} "),
                ordered_marker_style(*depth),
            )]
        }
        ListMarker::Task {
            checked,
            depth,
            ordered,
        } => {
            let mut spans = Vec::new();
            if let Some((number, delim)) = ordered {
                spans.push(Span::styled(
                    format!("{number}{delim} "),
                    ordered_marker_style(*depth),
                ));
            }
            let (box_glyph, box_style) = if *checked {
                (
                    "☑ ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("☐ ", Style::default().fg(Color::Gray))
            };
            spans.push(Span::styled(box_glyph, box_style));
            spans
        }
    }
}

fn style_lines(content_lines: &[WrappedLine]) -> Vec<Line<'static>> {
    let mut in_code_block = false;
    let mut code_fence_width = 7usize;
    let mut out = Vec::with_capacity(content_lines.len());
    let border_style = Style::default().fg(Color::Blue);

    for item in content_lines {
        let formatted = match item {
            WrappedLine::CodeFence(trimmed) => {
                let spans = if !in_code_block {
                    let lang = trimmed.trim_start_matches('`').trim();
                    if lang.is_empty() {
                        code_fence_width = 7;
                        vec![
                            Span::raw("  "),
                            Span::styled(
                                format!("╭{}", "─".repeat(code_fence_width)),
                                border_style,
                            ),
                        ]
                    } else {
                        code_fence_width = display_width(lang) + 6;
                        vec![
                            Span::raw("  "),
                            Span::styled("╭─ ", border_style),
                            Span::styled(
                                lang.to_string(),
                                border_style.add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(" ───", border_style),
                        ]
                    }
                } else {
                    vec![
                        Span::raw("  "),
                        Span::styled(format!("╰{}", "─".repeat(code_fence_width)), border_style),
                    ]
                };
                in_code_block = !in_code_block;
                spans
            }
            WrappedLine::CodeLine(line) => {
                vec![
                    Span::styled("  │ ", border_style),
                    Span::styled(line.clone(), Style::default().fg(Color::Cyan)),
                ]
            }
            WrappedLine::TableBorder(line) => {
                vec![
                    Span::raw("  "),
                    Span::styled(line.clone(), Style::default().fg(Color::DarkGray)),
                ]
            }
            WrappedLine::TableRow(spans) => spans.clone(),
            WrappedLine::Header { level, spans } => style_markdown_header(*level, spans),
            WrappedLine::ListItem {
                indent,
                marker,
                spans: item_spans,
            } => {
                let mut spans = vec![Span::raw("  ")];
                if *indent > 0 {
                    spans.push(Span::raw(" ".repeat(*indent)));
                }
                spans.extend(style_list_marker(marker));
                spans.extend(item_spans.iter().cloned());
                spans
            }
            WrappedLine::ListContinuation {
                indent,
                spans: cont_spans,
            } => {
                let mut spans = vec![Span::raw("  ")];
                if *indent > 0 {
                    spans.push(Span::raw(" ".repeat(*indent)));
                }
                spans.extend(cont_spans.iter().cloned());
                spans
            }
            WrappedLine::Blockquote {
                indent,
                depth,
                content,
            } => {
                let mut spans = vec![Span::raw("  ")];
                if *indent > 0 {
                    spans.push(Span::raw(" ".repeat(*indent)));
                }
                spans.extend(style_blockquote_bars(*depth));
                match content {
                    QuoteContent::Plain(q_spans) => {
                        spans.extend(q_spans.iter().cloned());
                    }
                    QuoteContent::Header {
                        level,
                        spans: h_spans,
                    } => {
                        spans.extend(style_markdown_header_inner(*level, h_spans));
                    }
                    QuoteContent::ListItem {
                        indent: item_indent,
                        marker,
                        spans: item_spans,
                    } => {
                        if *item_indent > 0 {
                            spans.push(Span::raw(" ".repeat(*item_indent)));
                        }
                        spans.extend(style_list_marker(marker));
                        spans.extend(item_spans.iter().cloned());
                    }
                    QuoteContent::ListContinuation {
                        indent: cont_indent,
                        spans: cont_spans,
                    } => {
                        if *cont_indent > 0 {
                            spans.push(Span::raw(" ".repeat(*cont_indent)));
                        }
                        spans.extend(cont_spans.iter().cloned());
                    }
                }
                spans
            }
            WrappedLine::Plain(line_spans) => {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(line_spans.iter().cloned());
                spans
            }
        };
        out.push(Line::from(formatted));
    }
    out
}

fn style_thought_lines(wrapped: &[WrappedLine]) -> Vec<Line<'static>> {
    let thought_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);
    style_lines(wrapped)
        .into_iter()
        .map(|line| {
            Line::from(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content, span.style.patch(thought_style)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
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
        let available_width = usize::from(self.chat_inner_area().width)
            .saturating_sub(MESSAGE_BODY_INDENT_WIDTH)
            .max(1);

        let total_slots = self.messages.len() + 1 + self.queued_messages.len();
        self.msg_states.resize(total_slots, MsgUiState::default());
        self.msg_visual_ranges.clear();

        let options = textwrap::Options::new(available_width).break_words(true);

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
                    (
                        sanitize_line(&format!("⚡ HOOK [{summary}]")),
                        Color::Yellow,
                    )
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

            if self.stream_thought_dirty {
                let sanitized_thought = sanitize_line(&self.streaming_thought);
                self.styled_stream_thought = if sanitized_thought.trim().is_empty() {
                    Vec::new()
                } else {
                    style_thought_lines(&wrap_text(&sanitized_thought, &options))
                };
                self.stream_thought_dirty = false;
            }

            if self.stream_dirty {
                let sanitized_stream = sanitize_line(&self.streaming_text);
                self.styled_stream = if sanitized_stream.trim().is_empty() {
                    Vec::new()
                } else {
                    style_lines(&wrap_text(&sanitized_stream, &options))
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
                .block(self.chat_area_block(format!(" Conversation [{}] ", self.model))),
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
