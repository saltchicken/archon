use super::theme;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use textwrap::core::display_width;

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
        content: Box<WrappedLine>,
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
                let raw_inner = &text[after_open..after_open + rel_close];
                let inner = if raw_inner.len() >= 2
                    && raw_inner.starts_with(' ')
                    && raw_inner.ends_with(' ')
                    && raw_inner.bytes().any(|b| b != b' ')
                {
                    &raw_inner[1..raw_inner.len() - 1]
                } else {
                    raw_inner
                };
                let end_pos = after_open + rel_close + tick_count;
                tokens.push(InlineToken {
                    kind: InlineTokenKind::Code(inner.to_string()),
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
            InlineTokenKind::Code(s) => {
                push_merged_span(&mut spans, &s, style.fg(theme::MD_INLINE_CODE))
            }
            InlineTokenKind::Delim { ch, count, .. } if count > 0 => {
                push_merged_span(&mut spans, &ch.to_string().repeat(count), style);
            }
            InlineTokenKind::Delim { .. } => {}
        }
    }
    spans
}

pub(super) fn spans_display_width(spans: &[Span<'_>]) -> usize {
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
    let push_piece =
        |pieces: &mut Vec<(bool, Vec<Span<'static>>)>, is_space: bool, seg: &str, style: Style| {
            if let Some((last_space, last_vec)) = pieces.last_mut()
                && *last_space == is_space
            {
                push_merged_span(last_vec, seg, style);
            } else {
                let mut v = Vec::new();
                push_merged_span(&mut v, seg, style);
                pieces.push((is_space, v));
            }
        };

    for span in spans {
        let s = span.content.as_ref();
        let style = span.style;
        let mut start = 0usize;
        let mut cur_is_space: Option<bool> = None;

        for (idx, ch) in s.char_indices() {
            let is_space = ch == ' ';
            if let Some(prev_space) = cur_is_space {
                if prev_space != is_space {
                    push_piece(&mut pieces, prev_space, &s[start..idx], style);
                    start = idx;
                    cur_is_space = Some(is_space);
                }
            } else {
                cur_is_space = Some(is_space);
            }
        }

        if let Some(prev_space) = cur_is_space {
            push_piece(&mut pieces, prev_space, &s[start..], style);
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

fn format_markdown_table(raw_lines: &[String], available_width: usize) -> Option<Vec<WrappedLine>> {
    if raw_lines.len() < 2 {
        return None;
    }

    let parsed_rows: Vec<Vec<String>> = raw_lines.iter().map(|l| parse_table_cells(l)).collect();
    let mut aligns = parse_separator_row(&parsed_rows[1])?;
    let num_cols = parsed_rows.iter().map(Vec::len).max().filter(|&n| n > 0)?;
    aligns.resize(num_cols, ColAlign::Left);

    let row_style = |r_idx: usize| {
        if r_idx == 0 {
            Style::default()
                .fg(theme::MD_TABLE_HEADER)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    };

    let styled_rows: Vec<Vec<Vec<Span<'static>>>> = parsed_rows
        .iter()
        .enumerate()
        .map(|(r_idx, row)| {
            if r_idx == 1 {
                return Vec::new();
            }
            let cell_style = row_style(r_idx);
            (0..num_cols)
                .map(|c_idx| {
                    let text = row.get(c_idx).map_or("", String::as_str);
                    parse_inline_spans(text, cell_style)
                })
                .collect()
        })
        .collect();

    let mut col_widths = vec![1usize; num_cols];
    for row in &styled_rows {
        for (c_idx, cell_spans) in row.iter().enumerate() {
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

    let border_style = Style::default().fg(theme::MD_TABLE_BORDER);
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

        let cell_style = row_style(r_idx);

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
    table_buf: &mut Vec<String>,
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
            .fg(theme::MD_H1)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        2 => Style::default()
            .fg(theme::MD_H2)
            .add_modifier(Modifier::BOLD),
        3 => Style::default()
            .fg(theme::MD_H3)
            .add_modifier(Modifier::BOLD),
        _ => Style::default()
            .fg(theme::MD_H4)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC),
    }
}

fn parse_opening_fence(line: &str) -> Option<(char, usize, &str)> {
    let trimmed = line.trim_start();
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed.bytes().take_while(|&b| b == first as u8).count();
    if count < 3 {
        return None;
    }
    let rest = &trimmed[count..];
    if first == '`' && rest.contains('`') {
        return None;
    }
    Some((first, count, rest.trim()))
}

fn is_closing_fence(line: &str, fence_char: char, min_len: usize) -> bool {
    let trimmed = line.trim_start();
    let count = trimmed
        .bytes()
        .take_while(|&b| b == fence_char as u8)
        .count();
    count >= min_len && trimmed[count..].trim().is_empty()
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum CodeLang {
    Rust,
    Toml,
    Json,
    Yaml,
    Shell,
    Python,
    TypeScript,
    Go,
    Lua,
    Sql,
    Diff,
    #[default]
    Plain,
}

impl CodeLang {
    fn from_fence(tag: &str) -> Self {
        let token = tag
            .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_ascii_lowercase();
        match token.as_str() {
            "rs" | "rust" => Self::Rust,
            "toml" | "ini" | "cfg" | "conf" => Self::Toml,
            "json" | "jsonc" | "jsonl" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "sh" | "bash" | "zsh" | "fish" | "shell" | "console" => Self::Shell,
            "py" | "python" | "python3" => Self::Python,
            "ts" | "tsx" | "js" | "jsx" | "typescript" | "javascript" | "c" | "cpp" | "c++"
            | "h" | "hpp" | "java" | "csharp" | "cs" | "kotlin" | "kt" | "swift" | "zig"
            | "css" | "scss" => Self::TypeScript,
            "go" | "golang" => Self::Go,
            "lua" => Self::Lua,
            "sql" | "sqlite" | "postgres" | "postgresql" | "mysql" => Self::Sql,
            "diff" | "patch" => Self::Diff,
            _ => Self::Plain,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum CodeHighlightState {
    #[default]
    Normal,
    BlockComment(usize),
    MultiLineString {
        quote: char,
        count: usize,
        raw_hashes: Option<usize>,
    },
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_code_keyword(lang: CodeLang, word: &str) -> bool {
    match lang {
        CodeLang::Rust => matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
                | "yield"
        ),
        CodeLang::Python => matches!(
            word,
            "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "match"
                | "case"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
                | "self"
        ),
        CodeLang::TypeScript => matches!(
            word,
            "async"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "debugger"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "export"
                | "extends"
                | "finally"
                | "for"
                | "from"
                | "function"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "instanceof"
                | "interface"
                | "let"
                | "namespace"
                | "new"
                | "of"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "readonly"
                | "return"
                | "static"
                | "super"
                | "switch"
                | "this"
                | "throw"
                | "try"
                | "type"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "with"
                | "yield"
                | "struct"
                | "fn"
                | "pub"
                | "comptime"
                | "defer"
                | "errdefer"
        ),
        CodeLang::Go => matches!(
            word,
            "break"
                | "case"
                | "chan"
                | "const"
                | "continue"
                | "default"
                | "defer"
                | "else"
                | "fallthrough"
                | "for"
                | "func"
                | "go"
                | "goto"
                | "if"
                | "import"
                | "interface"
                | "map"
                | "package"
                | "range"
                | "return"
                | "select"
                | "struct"
                | "switch"
                | "type"
                | "var"
        ),
        CodeLang::Lua => matches!(
            word,
            "and"
                | "break"
                | "do"
                | "else"
                | "elseif"
                | "end"
                | "for"
                | "function"
                | "goto"
                | "if"
                | "in"
                | "local"
                | "not"
                | "or"
                | "repeat"
                | "return"
                | "then"
                | "until"
                | "while"
                | "self"
        ),
        CodeLang::Shell => matches!(
            word,
            "if" | "then"
                | "else"
                | "elif"
                | "fi"
                | "case"
                | "esac"
                | "for"
                | "select"
                | "while"
                | "until"
                | "do"
                | "done"
                | "in"
                | "function"
                | "time"
                | "export"
                | "local"
                | "readonly"
                | "return"
                | "exit"
                | "set"
                | "unset"
                | "end"
                | "switch"
                | "break"
                | "continue"
                | "source"
        ),
        CodeLang::Sql => {
            let upper = word.to_ascii_uppercase();
            matches!(
                upper.as_str(),
                "SELECT"
                    | "FROM"
                    | "WHERE"
                    | "INSERT"
                    | "INTO"
                    | "VALUES"
                    | "UPDATE"
                    | "SET"
                    | "DELETE"
                    | "CREATE"
                    | "DROP"
                    | "ALTER"
                    | "TABLE"
                    | "INDEX"
                    | "VIEW"
                    | "JOIN"
                    | "INNER"
                    | "LEFT"
                    | "RIGHT"
                    | "FULL"
                    | "OUTER"
                    | "ON"
                    | "AS"
                    | "AND"
                    | "OR"
                    | "NOT"
                    | "IN"
                    | "IS"
                    | "BY"
                    | "GROUP"
                    | "ORDER"
                    | "HAVING"
                    | "LIMIT"
                    | "OFFSET"
                    | "UNION"
                    | "ALL"
                    | "DISTINCT"
                    | "PRIMARY"
                    | "KEY"
                    | "FOREIGN"
                    | "REFERENCES"
                    | "DEFAULT"
                    | "CASCADE"
                    | "BEGIN"
                    | "COMMIT"
                    | "ROLLBACK"
                    | "RETURNING"
                    | "WITH"
                    | "CASE"
                    | "WHEN"
                    | "THEN"
                    | "ELSE"
                    | "END"
                    | "ASC"
                    | "DESC"
                    | "EXISTS"
            )
        }
        _ => false,
    }
}

fn is_code_literal_or_builtin_type(lang: CodeLang, word: &str) -> Option<Style> {
    let lit_style = Style::default()
        .fg(theme::MD_CODE_NUMBER)
        .add_modifier(Modifier::BOLD);
    let type_style = Style::default().fg(theme::MD_CODE_TYPE);

    match lang {
        CodeLang::Rust => {
            if matches!(word, "true" | "false") {
                return Some(lit_style);
            }
            if matches!(
                word,
                "bool"
                    | "char"
                    | "str"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "u128"
                    | "usize"
                    | "i8"
                    | "i16"
                    | "i32"
                    | "i64"
                    | "i128"
                    | "isize"
                    | "f32"
                    | "f64"
                    | "String"
                    | "Vec"
                    | "Option"
                    | "Result"
                    | "Box"
                    | "Arc"
                    | "Rc"
                    | "Some"
                    | "None"
                    | "Ok"
                    | "Err"
            ) {
                return Some(type_style);
            }
        }
        CodeLang::Toml | CodeLang::Json | CodeLang::Yaml => {
            if matches!(
                word,
                "true" | "false" | "null" | "nil" | "True" | "False" | "None" | "yes" | "no"
            ) {
                return Some(lit_style);
            }
        }
        CodeLang::Python => {
            if matches!(word, "True" | "False" | "None") {
                return Some(lit_style);
            }
            if matches!(
                word,
                "int"
                    | "str"
                    | "float"
                    | "bool"
                    | "list"
                    | "dict"
                    | "set"
                    | "tuple"
                    | "bytes"
                    | "object"
                    | "Any"
                    | "Optional"
                    | "Union"
                    | "List"
                    | "Dict"
                    | "Set"
                    | "Tuple"
            ) {
                return Some(type_style);
            }
        }
        CodeLang::TypeScript => {
            if matches!(
                word,
                "true" | "false" | "null" | "undefined" | "NaN" | "Infinity"
            ) {
                return Some(lit_style);
            }
            if matches!(
                word,
                "string"
                    | "number"
                    | "boolean"
                    | "any"
                    | "unknown"
                    | "never"
                    | "symbol"
                    | "bigint"
                    | "object"
                    | "Promise"
                    | "Record"
                    | "Partial"
                    | "Array"
                    | "Map"
                    | "Set"
            ) {
                return Some(type_style);
            }
        }
        CodeLang::Go => {
            if matches!(word, "true" | "false" | "nil" | "iota") {
                return Some(lit_style);
            }
            if matches!(
                word,
                "bool"
                    | "string"
                    | "int"
                    | "int8"
                    | "int16"
                    | "int32"
                    | "int64"
                    | "uint"
                    | "uint8"
                    | "uint16"
                    | "uint32"
                    | "uint64"
                    | "uintptr"
                    | "byte"
                    | "rune"
                    | "float32"
                    | "float64"
                    | "complex64"
                    | "complex128"
                    | "error"
                    | "any"
            ) {
                return Some(type_style);
            }
        }
        CodeLang::Lua => {
            if matches!(word, "true" | "false" | "nil") {
                return Some(lit_style);
            }
        }
        CodeLang::Shell => {
            if matches!(word, "true" | "false") {
                return Some(lit_style);
            }
        }
        CodeLang::Sql => {
            let upper = word.to_ascii_uppercase();
            if matches!(upper.as_str(), "NULL" | "TRUE" | "FALSE") {
                return Some(lit_style);
            }
            if matches!(
                upper.as_str(),
                "INT"
                    | "INTEGER"
                    | "BIGINT"
                    | "SMALLINT"
                    | "TEXT"
                    | "VARCHAR"
                    | "CHAR"
                    | "BOOLEAN"
                    | "BOOL"
                    | "REAL"
                    | "FLOAT"
                    | "DOUBLE"
                    | "NUMERIC"
                    | "DECIMAL"
                    | "DATE"
                    | "TIMESTAMP"
                    | "TIMESTAMPTZ"
                    | "UUID"
                    | "JSON"
                    | "JSONB"
                    | "BLOB"
                    | "SERIAL"
            ) {
                return Some(type_style);
            }
        }
        CodeLang::Diff | CodeLang::Plain => {}
    }

    if matches!(
        lang,
        CodeLang::Rust | CodeLang::Python | CodeLang::TypeScript | CodeLang::Go
    ) && word.chars().next().is_some_and(|c| c.is_ascii_uppercase())
    {
        return Some(type_style);
    }

    None
}

fn highlight_code_line(
    line: &str,
    lang: CodeLang,
    state: &mut CodeHighlightState,
) -> Vec<Span<'static>> {
    let text_style = Style::default().fg(theme::MD_CODE_TEXT);
    let kw_style = Style::default()
        .fg(theme::MD_CODE_KEYWORD)
        .add_modifier(Modifier::BOLD);
    let fn_style = Style::default().fg(theme::MD_CODE_FUNCTION);
    let str_style = Style::default().fg(theme::MD_CODE_STRING);
    let num_style = Style::default().fg(theme::MD_CODE_NUMBER);
    let comment_style = Style::default()
        .fg(theme::MD_CODE_COMMENT)
        .add_modifier(Modifier::ITALIC);
    let key_style = Style::default()
        .fg(theme::MD_CODE_KEY)
        .add_modifier(Modifier::BOLD);
    let macro_style = Style::default()
        .fg(theme::MD_CODE_MACRO)
        .add_modifier(Modifier::ITALIC);
    let punct_style = Style::default().fg(theme::MD_CODE_PUNCT);
    let type_style = Style::default().fg(theme::MD_CODE_TYPE);

    if line.is_empty() {
        return Vec::new();
    }

    if lang == CodeLang::Plain {
        return vec![Span::styled(line.to_string(), text_style)];
    }

    if lang == CodeLang::Diff {
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
            text_style
        };
        return vec![Span::styled(line.to_string(), style)];
    }

    let mut spans = Vec::new();
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    // Handle TOML / INI section headers and keys on normal lines
    if lang == CodeLang::Toml && *state == CodeHighlightState::Normal {
        let leading_ws = line.bytes().take_while(|b| b.is_ascii_whitespace()).count();
        let trimmed = &line[leading_ws..];
        if trimmed.starts_with('[') {
            if leading_ws > 0 {
                push_merged_span(&mut spans, &line[..leading_ws], text_style);
            }
            if let Some(comment_pos) = trimmed.find('#') {
                push_merged_span(
                    &mut spans,
                    &trimmed[..comment_pos],
                    type_style.add_modifier(Modifier::BOLD),
                );
                push_merged_span(&mut spans, &trimmed[comment_pos..], comment_style);
            } else {
                push_merged_span(&mut spans, trimmed, type_style.add_modifier(Modifier::BOLD));
            }
            return spans;
        }
        // Check for `key =` at line start (not inside array/inline table)
        if !trimmed.starts_with('#')
            && let Some(eq_rel) = trimmed.find('=')
        {
            let before_eq = &trimmed[..eq_rel];
            let before_trimmed = before_eq.trim_end();
            if !before_trimmed.is_empty()
                && before_trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '"' | '\''))
            {
                if leading_ws > 0 {
                    push_merged_span(&mut spans, &line[..leading_ws], text_style);
                }
                let key_len = before_trimmed.len();
                push_merged_span(&mut spans, before_trimmed, key_style);
                if eq_rel > key_len {
                    push_merged_span(&mut spans, &trimmed[key_len..eq_rel], text_style);
                }
                push_merged_span(&mut spans, "=", punct_style);
                i = leading_ws + eq_rel + 1;
            }
        }
    }

    // Handle YAML keys at start of line
    if lang == CodeLang::Yaml && *state == CodeHighlightState::Normal {
        let leading_ws = line.bytes().take_while(|b| b.is_ascii_whitespace()).count();
        let mut offset = leading_ws;
        if line[offset..].starts_with("- ") {
            if leading_ws > 0 {
                push_merged_span(&mut spans, &line[..leading_ws], text_style);
            }
            push_merged_span(&mut spans, "- ", punct_style);
            offset += 2;
        }
        let rest = &line[offset..];
        if !rest.starts_with('#')
            && let Some(colon_rel) = rest.find(':')
        {
            let after_colon = &rest[colon_rel + 1..];
            let before_colon = &rest[..colon_rel];
            if (after_colon.is_empty()
                || after_colon.starts_with(' ')
                || after_colon.starts_with('\t'))
                && !before_colon.is_empty()
                && before_colon.chars().all(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '"' | '\'')
                })
            {
                if offset == leading_ws && leading_ws > 0 {
                    push_merged_span(&mut spans, &line[..leading_ws], text_style);
                }
                push_merged_span(&mut spans, before_colon, key_style);
                push_merged_span(&mut spans, ":", punct_style);
                i = offset + colon_rel + 1;
            }
        }
    }

    while i < len {
        // 1. Continuation of multi-line state
        match *state {
            CodeHighlightState::BlockComment(mut depth) => {
                let start = i;
                while i < len {
                    if lang == CodeLang::Rust && line[i..].starts_with("/*") {
                        depth += 1;
                        i += 2;
                    } else if line[i..].starts_with("*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            *state = CodeHighlightState::Normal;
                            break;
                        }
                    } else {
                        i += line[i..].chars().next().map_or(1, char::len_utf8);
                    }
                }
                if depth > 0 {
                    *state = CodeHighlightState::BlockComment(depth);
                }
                push_merged_span(&mut spans, &line[start..i], comment_style);
                continue;
            }
            CodeHighlightState::MultiLineString {
                quote,
                count,
                raw_hashes,
            } => {
                let start = i;
                if let Some(hashes) = raw_hashes {
                    while i < len {
                        if bytes[i] == quote as u8 {
                            let h_count = line[i + 1..].bytes().take_while(|&b| b == b'#').count();
                            if h_count >= hashes {
                                i += 1 + hashes;
                                *state = CodeHighlightState::Normal;
                                break;
                            }
                        }
                        i += line[i..].chars().next().map_or(1, char::len_utf8);
                    }
                } else if count == 3 {
                    let delim = if quote == '"' { "\"\"\"" } else { "'''" };
                    while i < len {
                        if bytes[i] == b'\\' && i + 1 < len {
                            i += 1 + line[i + 1..].chars().next().map_or(1, char::len_utf8);
                            continue;
                        }
                        if line[i..].starts_with(delim) {
                            i += 3;
                            *state = CodeHighlightState::Normal;
                            break;
                        }
                        i += line[i..].chars().next().map_or(1, char::len_utf8);
                    }
                } else {
                    while i < len {
                        if bytes[i] == b'\\' && i + 1 < len {
                            i += 1 + line[i + 1..].chars().next().map_or(1, char::len_utf8);
                            continue;
                        }
                        if bytes[i] == quote as u8 {
                            i += 1;
                            *state = CodeHighlightState::Normal;
                            break;
                        }
                        i += line[i..].chars().next().map_or(1, char::len_utf8);
                    }
                }
                push_merged_span(&mut spans, &line[start..i], str_style);
                continue;
            }
            CodeHighlightState::Normal => {}
        }

        let rest = &line[i..];
        let b = bytes[i];

        // 2. Whitespace
        if b.is_ascii_whitespace() {
            let ws_len = rest.bytes().take_while(|c| c.is_ascii_whitespace()).count();
            push_merged_span(&mut spans, &rest[..ws_len], text_style);
            i += ws_len;
            continue;
        }

        // 3. Comments
        if matches!(
            lang,
            CodeLang::Rust | CodeLang::TypeScript | CodeLang::Go | CodeLang::Json
        ) && rest.starts_with("//")
        {
            push_merged_span(&mut spans, rest, comment_style);
            break;
        }
        if matches!(
            lang,
            CodeLang::Rust | CodeLang::TypeScript | CodeLang::Go | CodeLang::Sql | CodeLang::Json
        ) && rest.starts_with("/*")
        {
            let start = i;
            i += 2;
            let mut depth = 1usize;
            while i < len {
                if lang == CodeLang::Rust && line[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if line[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += line[i..].chars().next().map_or(1, char::len_utf8);
                }
            }
            if depth > 0 {
                *state = CodeHighlightState::BlockComment(depth);
            }
            push_merged_span(&mut spans, &line[start..i], comment_style);
            continue;
        }
        if matches!(
            lang,
            CodeLang::Toml | CodeLang::Yaml | CodeLang::Python | CodeLang::Shell
        ) && b == b'#'
        {
            push_merged_span(&mut spans, rest, comment_style);
            break;
        }
        if matches!(lang, CodeLang::Lua | CodeLang::Sql) && rest.starts_with("--") {
            push_merged_span(&mut spans, rest, comment_style);
            break;
        }

        // 4. Rust attributes: `#[...]` or `#![...]`
        if lang == CodeLang::Rust && (rest.starts_with("#[") || rest.starts_with("#![")) {
            let start = i;
            let mut bracket_depth = 0usize;
            while i < len {
                if bytes[i] == b'[' {
                    bracket_depth += 1;
                } else if bytes[i] == b']' {
                    bracket_depth = bracket_depth.saturating_sub(1);
                    if bracket_depth == 0 {
                        i += 1;
                        break;
                    }
                }
                i += line[i..].chars().next().map_or(1, char::len_utf8);
            }
            push_merged_span(&mut spans, &line[start..i], macro_style);
            continue;
        }

        // 5. Python / TS decorators: `@decorator`
        if matches!(lang, CodeLang::Python | CodeLang::TypeScript)
            && b == b'@'
            && rest[1..].chars().next().is_some_and(is_ident_start)
        {
            let dec_len = 1 + rest[1..]
                .chars()
                .take_while(|&c| is_ident_continue(c) || c == '.')
                .map(char::len_utf8)
                .sum::<usize>();
            push_merged_span(&mut spans, &rest[..dec_len], macro_style);
            i += dec_len;
            continue;
        }

        // 6. Rust raw strings: `r"..."`, `r#"..."#`, `br"..."`, `cr"..."`
        if lang == CodeLang::Rust {
            let raw_prefix_len = if rest.starts_with("br") || rest.starts_with("cr") {
                Some(2)
            } else if rest.starts_with('r') {
                Some(1)
            } else {
                None
            };
            if let Some(p_len) = raw_prefix_len {
                let after_r = &rest[p_len..];
                let hashes = after_r.bytes().take_while(|&c| c == b'#').count();
                if after_r.as_bytes().get(hashes) == Some(&b'"') {
                    let start = i;
                    i += p_len + hashes + 1;
                    let mut closed = false;
                    while i < len {
                        if bytes[i] == b'"' {
                            let h_count = line[i + 1..].bytes().take_while(|&c| c == b'#').count();
                            if h_count >= hashes {
                                i += 1 + hashes;
                                closed = true;
                                break;
                            }
                        }
                        i += line[i..].chars().next().map_or(1, char::len_utf8);
                    }
                    if !closed {
                        *state = CodeHighlightState::MultiLineString {
                            quote: '"',
                            count: 1,
                            raw_hashes: Some(hashes),
                        };
                    }
                    push_merged_span(&mut spans, &line[start..i], str_style);
                    continue;
                }
            }
        }

        // 7. Rust lifetimes and char literals
        if lang == CodeLang::Rust && b == b'\'' {
            let after_quote = &rest[1..];
            if let Some(first_ch) = after_quote.chars().next() {
                if first_ch == '\\' {
                    // Escaped char literal like '\n' or '\''
                    let start = i;
                    i += 2;
                    while i < len {
                        let ch = line[i..].chars().next().unwrap();
                        i += ch.len_utf8();
                        if ch == '\'' {
                            break;
                        }
                    }
                    push_merged_span(&mut spans, &line[start..i], str_style);
                    continue;
                }
                let after_first = &after_quote[first_ch.len_utf8()..];
                if after_first.starts_with('\'') {
                    // Single char literal like 'a'
                    let end = i + 1 + first_ch.len_utf8() + 1;
                    push_merged_span(&mut spans, &line[i..end], str_style);
                    i = end;
                    continue;
                }
                if is_ident_start(first_ch) {
                    // Lifetime like 'a or 'static
                    let ident_len: usize = after_quote
                        .chars()
                        .take_while(|&c| is_ident_continue(c))
                        .map(char::len_utf8)
                        .sum();
                    push_merged_span(&mut spans, &rest[..1 + ident_len], macro_style);
                    i += 1 + ident_len;
                    continue;
                }
            }
        }

        // 8. Triple-quoted strings (TOML, Python)
        if matches!(lang, CodeLang::Toml | CodeLang::Python)
            && (rest.starts_with("\"\"\"") || rest.starts_with("'''"))
        {
            let quote = b as char;
            let delim = &rest[..3];
            let start = i;
            i += 3;
            let mut closed = false;
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1 + line[i + 1..].chars().next().map_or(1, char::len_utf8);
                    continue;
                }
                if line[i..].starts_with(delim) {
                    i += 3;
                    closed = true;
                    break;
                }
                i += line[i..].chars().next().map_or(1, char::len_utf8);
            }
            if !closed {
                *state = CodeHighlightState::MultiLineString {
                    quote,
                    count: 3,
                    raw_hashes: None,
                };
            }
            push_merged_span(&mut spans, &line[start..i], str_style);
            continue;
        }

        // 9. Standard quoted strings (`"..."`, `'...'`, `` `...` ``)
        if b == b'"'
            || (b == b'\'' && lang != CodeLang::Rust)
            || (b == b'`' && matches!(lang, CodeLang::TypeScript | CodeLang::Go | CodeLang::Shell))
        {
            let quote = b;
            let start = i;
            i += 1;
            let mut closed = false;
            while i < len {
                if bytes[i] == b'\\' && quote != b'`' && i + 1 < len {
                    i += 1 + line[i + 1..].chars().next().map_or(1, char::len_utf8);
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    closed = true;
                    break;
                }
                i += line[i..].chars().next().map_or(1, char::len_utf8);
            }
            if !closed
                && ((lang == CodeLang::Rust && quote == b'"')
                    || (lang == CodeLang::Go && quote == b'`')
                    || (lang == CodeLang::TypeScript && quote == b'`'))
            {
                *state = CodeHighlightState::MultiLineString {
                    quote: quote as char,
                    count: 1,
                    raw_hashes: None,
                };
            }
            let token_text = &line[start..i];
            // In JSON, check if this string is followed by `:` (an object key)
            let after_str = line[i..].trim_start();
            let style = if lang == CodeLang::Json && after_str.starts_with(':') {
                key_style
            } else {
                str_style
            };
            push_merged_span(&mut spans, token_text, style);
            continue;
        }

        // 10. Shell variables ($VAR, ${VAR}, $1) and flags (--flag, -f)
        if lang == CodeLang::Shell {
            if b == b'$' && i + 1 < len {
                let start = i;
                i += 1;
                if bytes[i] == b'{' {
                    while i < len && bytes[i] != b'}' {
                        i += line[i..].chars().next().map_or(1, char::len_utf8);
                    }
                    if i < len {
                        i += 1;
                    }
                } else {
                    while i < len
                        && (bytes[i].is_ascii_alphanumeric()
                            || matches!(bytes[i], b'_' | b'?' | b'!' | b'@' | b'*' | b'#'))
                    {
                        i += 1;
                    }
                }
                push_merged_span(&mut spans, &line[start..i], macro_style);
                continue;
            }
            if b == b'-'
                && (i == 0 || bytes[i - 1].is_ascii_whitespace())
                && rest
                    .as_bytes()
                    .get(1)
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'-')
            {
                let flag_len = rest
                    .bytes()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(*c, b'-' | b'_'))
                    .count();
                push_merged_span(&mut spans, &rest[..flag_len], key_style);
                i += flag_len;
                continue;
            }
        }

        // 11. Numbers (integers, floats, hex, binary, octal, TOML dates/times)
        if b.is_ascii_digit()
            || (b == b'-'
                && rest.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_'))
        {
            let start = i;
            if b == b'-' {
                i += 1;
            }
            while i < len
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'.' | b'+' | b'-' | b':'))
            {
                // Stop before range operator `..` in Rust
                if bytes[i] == b'.' && line[i..].starts_with("..") {
                    break;
                }
                i += 1;
            }
            push_merged_span(&mut spans, &line[start..i], num_style);
            continue;
        }

        // 12. Identifiers, keywords, types, functions, macros
        let first_ch = rest.chars().next().unwrap();
        if is_ident_start(first_ch) {
            let start = i;
            i += first_ch.len_utf8();
            while i < len {
                let ch = line[i..].chars().next().unwrap();
                if is_ident_continue(ch) || (lang == CodeLang::Shell && ch == '-') {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            let word = &line[start..i];
            let after_word = &line[i..];

            if lang == CodeLang::Rust && after_word.starts_with('!') {
                i += 1;
                push_merged_span(&mut spans, &line[start..i], macro_style);
                continue;
            }

            let style = if is_code_keyword(lang, word) {
                kw_style
            } else if let Some(s) = is_code_literal_or_builtin_type(lang, word) {
                s
            } else if after_word.trim_start().starts_with('(') {
                fn_style
            } else if lang == CodeLang::Rust && after_word.starts_with("::") {
                type_style
            } else {
                text_style
            };
            push_merged_span(&mut spans, word, style);
            continue;
        }

        // 13. Operators and punctuation
        i += first_ch.len_utf8();
        let style = if first_ch.is_ascii_punctuation() {
            punct_style
        } else {
            text_style
        };
        push_merged_span(&mut spans, &line[i - first_ch.len_utf8()..i], style);
    }

    spans
}

#[derive(Clone)]
struct MarkdownParserState {
    table_buf: Vec<String>,
    list_levels: Vec<(usize, usize)>,
    quote_list_levels: Vec<(usize, usize)>,
    last_quote_depth: Option<usize>,
    code_fence: Option<(char, usize)>,
    in_diff_hunk: bool,
    in_params_block: bool,
    in_code_block: bool,
    code_fence_width: usize,
    code_lang: CodeLang,
    code_hl_state: CodeHighlightState,
    style_code_blocks: bool,
}

impl Default for MarkdownParserState {
    fn default() -> Self {
        Self {
            table_buf: Vec::new(),
            list_levels: Vec::new(),
            quote_list_levels: Vec::new(),
            last_quote_depth: None,
            code_fence: None,
            in_diff_hunk: false,
            in_params_block: false,
            in_code_block: false,
            code_fence_width: 7,
            code_lang: CodeLang::Plain,
            code_hl_state: CodeHighlightState::Normal,
            style_code_blocks: true,
        }
    }
}

fn parse_content_block_lines(
    line: &str,
    base_style: Style,
    list_levels: &mut Vec<(usize, usize)>,
    in_params_block: Option<&mut bool>,
    options: &textwrap::Options<'_>,
) -> Vec<WrappedLine> {
    if let Some((level, hdr_content)) = parse_markdown_header(line) {
        list_levels.clear();
        if let Some(p) = in_params_block {
            *p = false;
        }
        let hdr_opts = textwrap::Options::new(options.width.saturating_sub(6).max(1))
            .break_words(options.break_words);
        return wrap_markdown_line(hdr_content, header_text_style(level), &hdr_opts)
            .into_iter()
            .map(|spans| WrappedLine::Header { level, spans })
            .collect();
    }

    if let Some((raw_indent, kind, item_text)) = parse_list_item(line) {
        if let Some(p) = in_params_block {
            *p = false;
        }
        return wrap_list_item_lines(
            raw_indent,
            kind,
            item_text,
            base_style,
            list_levels,
            options.width,
            options.break_words,
        )
        .into_iter()
        .map(|(indent, maybe_marker, spans)| match maybe_marker {
            Some(marker) => WrappedLine::ListItem {
                indent,
                marker,
                spans,
            },
            None => WrappedLine::ListContinuation { indent, spans },
        })
        .collect();
    }

    if line.trim().is_empty() {
        if let Some(p) = in_params_block {
            *p = false;
        }
        return vec![WrappedLine::Plain(Vec::new())];
    }

    let trimmed = line.trim_start();
    let is_top_level = in_params_block.is_some();
    if let Some(in_params) = in_params_block {
        if trimmed == "Parameters:" {
            *in_params = true;
        }
        if *in_params {
            list_levels.clear();
            let style = if trimmed == "Parameters:" {
                Style::default()
                    .fg(theme::MD_TOOL_PARAM)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::MD_TOOL_PARAM)
            };
            return wrap_raw_line(line, style, options)
                .into_iter()
                .map(WrappedLine::Plain)
                .collect();
        }
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
        return wrap_markdown_line(trimmed, base_style, &cont_opts)
            .into_iter()
            .map(|spans| WrappedLine::ListContinuation {
                indent: hanging_indent,
                spans,
            })
            .collect();
    }

    if is_top_level && trimmed.starts_with("⚙ ") {
        let style = Style::default()
            .fg(theme::MD_TOOL_PARAM)
            .add_modifier(Modifier::ITALIC);
        return wrap_raw_line(line, style, options)
            .into_iter()
            .map(WrappedLine::Plain)
            .collect();
    }

    wrap_markdown_line(line, base_style, options)
        .into_iter()
        .map(WrappedLine::Plain)
        .collect()
}

fn style_content_line(item: &WrappedLine) -> Vec<Span<'static>> {
    match item {
        WrappedLine::Header { level, spans } => style_markdown_header_inner(*level, spans),
        WrappedLine::ListItem {
            indent,
            marker,
            spans: item_spans,
        } => {
            let mut spans = Vec::new();
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
            let mut spans = Vec::new();
            if *indent > 0 {
                spans.push(Span::raw(" ".repeat(*indent)));
            }
            spans.extend(cont_spans.iter().cloned());
            spans
        }
        WrappedLine::Plain(line_spans) => line_spans.clone(),
        _ => Vec::new(),
    }
}

impl MarkdownParserState {
    fn feed_line(
        &mut self,
        line: &str,
        options: &textwrap::Options<'_>,
        out: &mut Vec<WrappedLine>,
    ) {
        let quote_style = Style::default()
            .fg(theme::MD_QUOTE_TEXT)
            .add_modifier(Modifier::ITALIC);

        if let Some((fence_char, fence_len)) = self.code_fence {
            if is_closing_fence(line, fence_char, fence_len) {
                self.code_fence = None;
                out.push(WrappedLine::CodeFence(String::new()));
            } else {
                out.push(WrappedLine::CodeLine(line.to_string()));
            }
            return;
        }

        if let Some((fence_char, fence_len, lang)) = parse_opening_fence(line) {
            self.flush(options, out);
            self.quote_list_levels.clear();
            self.last_quote_depth = None;
            self.in_diff_hunk = false;
            self.in_params_block = false;
            self.code_fence = Some((fence_char, fence_len));
            out.push(WrappedLine::CodeFence(lang.to_string()));
            return;
        }

        if is_diff_hunk_header(line) {
            self.flush(options, out);
            self.list_levels.clear();
            self.quote_list_levels.clear();
            self.last_quote_depth = None;
            self.in_params_block = false;
            self.in_diff_hunk = true;
            for part in wrap_raw_line(line, Style::default(), options) {
                out.push(WrappedLine::Plain(part));
            }
            return;
        }

        if self.in_diff_hunk {
            if is_diff_body_line(line) {
                for part in wrap_raw_line(line, Style::default(), options) {
                    out.push(WrappedLine::Plain(part));
                }
                return;
            }
            self.in_diff_hunk = false;
        }

        if !line.trim_start().starts_with('>') && is_table_line(line) {
            self.quote_list_levels.clear();
            self.last_quote_depth = None;
            self.in_params_block = false;
            self.table_buf.push(line.to_string());
            return;
        }

        self.flush(options, out);

        if let Some((leading_spaces, depth, quote_text)) = parse_blockquote(line) {
            self.in_params_block = false;
            while self
                .list_levels
                .last()
                .is_some_and(|&(raw_indent, _)| raw_indent >= leading_spaces)
            {
                self.list_levels.pop();
            }
            let base_indent = self
                .list_levels
                .last()
                .map_or(0, |&(_, hanging_indent)| hanging_indent);
            if self.last_quote_depth != Some(depth) {
                self.quote_list_levels.clear();
                self.last_quote_depth = Some(depth);
            }

            let inner_width = options.width.saturating_sub(base_indent + depth * 2).max(1);
            let q_opts = textwrap::Options::new(inner_width).break_words(options.break_words);
            for content in parse_content_block_lines(
                quote_text,
                quote_style,
                &mut self.quote_list_levels,
                None,
                &q_opts,
            ) {
                out.push(WrappedLine::Blockquote {
                    indent: base_indent,
                    depth,
                    content: Box::new(content),
                });
            }
            return;
        }

        self.quote_list_levels.clear();
        self.last_quote_depth = None;

        out.extend(parse_content_block_lines(
            line,
            Style::default(),
            &mut self.list_levels,
            Some(&mut self.in_params_block),
            options,
        ));
    }

    fn flush(&mut self, options: &textwrap::Options<'_>, out: &mut Vec<WrappedLine>) {
        flush_table_buffer(&mut self.table_buf, out, options, &mut self.list_levels);
    }

    fn style_wrapped(
        &mut self,
        content_lines: &[WrappedLine],
        is_thought: bool,
        out: &mut Vec<Line<'static>>,
    ) {
        let border_style = Style::default().fg(theme::MD_CODE_BORDER);
        let thought_style = Style::default()
            .fg(theme::MD_THOUGHT)
            .add_modifier(Modifier::ITALIC);

        for item in content_lines {
            let mut formatted = match item {
                WrappedLine::CodeFence(lang) => {
                    let spans = if !self.in_code_block {
                        let lang = lang.trim();
                        if self.style_code_blocks {
                            self.code_lang = CodeLang::from_fence(lang);
                        } else {
                            self.code_lang = CodeLang::Plain;
                        }
                        self.code_hl_state = CodeHighlightState::Normal;
                        let copy_badge = Span::styled(
                            "󰆏",
                            Style::default()
                                .fg(theme::BADGE_CODE_COPY)
                                .add_modifier(Modifier::BOLD),
                        );
                        if lang.is_empty() {
                            self.code_fence_width = 7;
                            vec![
                                Span::raw("  "),
                                Span::styled(
                                    format!("╭{} ", "─".repeat(self.code_fence_width)),
                                    border_style,
                                ),
                                copy_badge,
                            ]
                        } else {
                            self.code_fence_width = display_width(lang) + 6;
                            vec![
                                Span::raw("  "),
                                Span::styled("╭─ ", border_style),
                                Span::styled(
                                    lang.to_string(),
                                    border_style.add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(" ─── ", border_style),
                                copy_badge,
                            ]
                        }
                    } else {
                        self.code_lang = CodeLang::Plain;
                        self.code_hl_state = CodeHighlightState::Normal;
                        vec![
                            Span::raw("  "),
                            Span::styled(
                                format!("╰{}", "─".repeat(self.code_fence_width)),
                                border_style,
                            ),
                        ]
                    };
                    self.in_code_block = !self.in_code_block;
                    spans
                }
                WrappedLine::CodeLine(line) => {
                    let mut spans = vec![Span::styled("  │ ", border_style)];
                    if self.style_code_blocks {
                        spans.extend(highlight_code_line(
                            line,
                            self.code_lang,
                            &mut self.code_hl_state,
                        ));
                    } else {
                        spans.push(Span::styled(
                            line.clone(),
                            Style::default().fg(theme::MD_CODE_TEXT),
                        ));
                    }
                    spans
                }
                WrappedLine::TableBorder(line) => {
                    vec![
                        Span::raw("  "),
                        Span::styled(line.clone(), Style::default().fg(theme::MD_TABLE_BORDER)),
                    ]
                }
                WrappedLine::TableRow(spans) => spans.clone(),
                WrappedLine::Blockquote {
                    indent,
                    depth,
                    content,
                } => {
                    let mut spans = vec![Span::raw(format!("  {}", " ".repeat(*indent)))];
                    spans.extend(style_blockquote_bars(*depth));
                    spans.extend(style_content_line(content));
                    spans
                }
                other => {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(style_content_line(other));
                    spans
                }
            };

            if is_thought {
                for span in &mut formatted {
                    span.style = span.style.patch(thought_style);
                }
            }
            out.push(Line::from(formatted));
        }
    }
}

#[derive(Default)]
pub(crate) struct MarkdownStreamCache {
    committed_bytes: usize,
    committed_line_count: usize,
    pub(super) lines: Vec<Line<'static>>,
    state: MarkdownParserState,
    wrapped_scratch: Vec<WrappedLine>,
}

impl MarkdownStreamCache {
    pub(super) fn clear(&mut self) {
        self.committed_bytes = 0;
        self.committed_line_count = 0;
        self.lines.clear();
        self.state = MarkdownParserState::default();
        self.wrapped_scratch.clear();
    }

    pub(super) fn update(
        &mut self,
        raw_text: &str,
        options: &textwrap::Options<'_>,
        is_thought: bool,
        style_code_blocks: bool,
    ) {
        if raw_text.trim().is_empty() {
            self.lines.clear();
            return;
        }
        if raw_text.len() < self.committed_bytes
            || self.state.style_code_blocks != style_code_blocks
        {
            self.clear();
        }
        self.state.style_code_blocks = style_code_blocks;

        self.lines.truncate(self.committed_line_count);

        let remaining = &raw_text[self.committed_bytes..];
        let mut offset = 0usize;
        while let Some(nl_rel) = remaining[offset..].find('\n') {
            let line_end = offset + nl_rel;
            let raw_line = &remaining[offset..line_end];
            let sanitized = super::render::sanitize_line(raw_line);
            self.wrapped_scratch.clear();
            self.state
                .feed_line(&sanitized, options, &mut self.wrapped_scratch);
            if !self.wrapped_scratch.is_empty() {
                self.state
                    .style_wrapped(&self.wrapped_scratch, is_thought, &mut self.lines);
            }
            offset = line_end + 1;
        }
        self.committed_bytes += offset;
        self.committed_line_count = self.lines.len();

        let tail = &raw_text[self.committed_bytes..];
        let sanitized_tail = super::render::sanitize_line(tail);
        if !sanitized_tail.is_empty() || !self.state.table_buf.is_empty() {
            let mut temp_state = self.state.clone();
            self.wrapped_scratch.clear();
            if !sanitized_tail.is_empty() {
                temp_state.feed_line(&sanitized_tail, options, &mut self.wrapped_scratch);
            }
            temp_state.flush(options, &mut self.wrapped_scratch);
            if !self.wrapped_scratch.is_empty() {
                temp_state.style_wrapped(&self.wrapped_scratch, is_thought, &mut self.lines);
            }
        }
    }
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
    let accent_bold = Style::default()
        .fg(theme::MD_HEADER_ACCENT)
        .add_modifier(Modifier::BOLD);
    let (prefix, accent) = match level {
        1 => ("▌ ", accent_bold),
        2 => ("▎ ", accent_bold),
        3 => ("▸ ", accent_bold),
        _ => ("• ", Style::default().fg(theme::MD_HEADER_SUB_ACCENT)),
    };
    let mut spans = Vec::with_capacity(content_spans.len() + 2);
    spans.push(Span::styled(prefix, accent));
    spans.extend(content_spans.iter().cloned());
    if level == 2 {
        spans.push(Span::styled(
            " ───",
            Style::default().fg(theme::MD_HEADER_ACCENT),
        ));
    }
    spans
}

fn style_blockquote_bars(depth: usize) -> Vec<Span<'static>> {
    (0..depth)
        .map(|level| {
            let color = match level {
                0 => theme::MD_QUOTE_BAR_0,
                1 => theme::MD_QUOTE_BAR_1,
                2 => theme::MD_QUOTE_BAR_2,
                _ => theme::MD_QUOTE_BAR_3,
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
            .fg(theme::MD_LIST_DEPTH_0)
            .add_modifier(Modifier::BOLD),
        1 => Style::default()
            .fg(theme::MD_LIST_DEPTH_1)
            .add_modifier(Modifier::BOLD),
        _ => Style::default()
            .fg(theme::MD_LIST_DEPTH_FALLBACK)
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
                        .fg(theme::MD_LIST_DEPTH_0)
                        .add_modifier(Modifier::BOLD),
                ),
                1 => ("◦ ", Style::default().fg(theme::MD_LIST_DEPTH_1)),
                2 => (
                    "▪ ",
                    Style::default()
                        .fg(theme::MD_LIST_DEPTH_2)
                        .add_modifier(Modifier::BOLD),
                ),
                _ => ("▹ ", Style::default().fg(theme::MD_LIST_DEPTH_FALLBACK)),
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
                        .fg(theme::MD_TASK_CHECKED)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("☐ ", Style::default().fg(theme::MD_TASK_UNCHECKED))
            };
            spans.push(Span::styled(box_glyph, box_style));
            spans
        }
    }
}

fn render_markdown_with_mode(
    content: &str,
    options: &textwrap::Options<'_>,
    is_thought: bool,
    style_code_blocks: bool,
) -> Vec<Line<'static>> {
    let mut state = MarkdownParserState {
        style_code_blocks,
        ..Default::default()
    };
    let mut wrapped = Vec::new();
    for line in content.lines() {
        state.feed_line(line, options, &mut wrapped);
    }
    state.flush(options, &mut wrapped);
    let mut out = Vec::with_capacity(wrapped.len());
    state.style_wrapped(&wrapped, is_thought, &mut out);
    out
}

pub(super) fn render_markdown(
    content: &str,
    options: &textwrap::Options<'_>,
    style_code_blocks: bool,
) -> Vec<Line<'static>> {
    render_markdown_with_mode(content, options, false, style_code_blocks)
}

#[cfg(test)]
fn render_thought_markdown(
    content: &str,
    options: &textwrap::Options<'_>,
    style_code_blocks: bool,
) -> Vec<Line<'static>> {
    render_markdown_with_mode(content, options, true, style_code_blocks)
}

pub(super) fn is_opening_code_fence(line: &Line<'_>) -> bool {
    line.spans
        .get(1)
        .is_some_and(|s| s.content.starts_with('╭'))
        && line.spans.last().is_some_and(|s| s.content.as_ref() == "󰆏")
}

pub(super) fn extract_code_block_from(
    styled_content: &[Line<'static>],
    fence_idx: usize,
) -> String {
    let mut code_lines = Vec::new();
    for line in &styled_content[(fence_idx + 1)..] {
        if line
            .spans
            .first()
            .is_some_and(|s| s.content.as_ref() == "  │ ")
        {
            let text: String = line.spans[1..].iter().map(|s| s.content.as_ref()).collect();
            code_lines.push(text);
        } else {
            break;
        }
    }
    code_lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_backtick_fence_preserves_inner_three_backtick_fence() {
        let md = "````md\n```rs\nfn main() {}\n```\n````";
        let opts = textwrap::Options::new(80);
        let lines = render_markdown(md, &opts, true);

        assert!(is_opening_code_fence(&lines[0]));
        assert_eq!(
            extract_code_block_from(&lines, 0),
            "```rs\nfn main() {}\n```"
        );
    }

    #[test]
    fn closing_fence_requires_no_trailing_text() {
        let md = "```rs\n```not-a-close\nlet x = 1;\n```";
        let opts = textwrap::Options::new(80);
        let lines = render_markdown(md, &opts, true);

        assert_eq!(
            extract_code_block_from(&lines, 0),
            "```not-a-close\nlet x = 1;"
        );
    }

    #[test]
    fn inline_triple_backticks_do_not_open_code_block() {
        assert!(parse_opening_fence("```inline```").is_none());
    }

    #[test]
    fn incremental_stream_cache_matches_full_render_at_every_chunk() {
        let doc = "# Heading 1\r\n\nSome **bold** and `inline code` text.\n\n- Item 1\n  - Nested task:\n    - [x] Done\n    - [ ] Todo\n\n| Col A | Col B |\n| :--- | ---: |\n| 1 | 200 |\n| 3000 | 4 |\n\n> Quoted line\n> > Deeper quote\n\n```rust\nfn demo(x: i32) -> i32 {\n    x + 1\n}\n```\n\nFinal paragraph.";
        let opts = textwrap::Options::new(60);

        for (is_thought, step) in [(false, 1usize), (false, 5), (true, 3)] {
            let mut cache = MarkdownStreamCache::default();
            let mut acc = String::new();
            let bytes = doc.as_bytes();
            let mut idx = 0;

            while idx < bytes.len() {
                let mut next = (idx + step).min(bytes.len());
                while next < bytes.len() && !doc.is_char_boundary(next) {
                    next += 1;
                }
                acc.push_str(&doc[idx..next]);
                idx = next;

                cache.update(&acc, &opts, is_thought, true);
                let sanitized = super::super::render::sanitize_line(&acc);
                let expected = if sanitized.trim().is_empty() {
                    Vec::new()
                } else if is_thought {
                    render_thought_markdown(&sanitized, &opts, true)
                } else {
                    render_markdown(&sanitized, &opts, true)
                };
                assert_eq!(
                    cache.lines,
                    expected,
                    "Mismatch for step={step}, is_thought={is_thought} at prefix len={}",
                    acc.len()
                );
            }
        }
    }

    #[test]
    fn code_block_styling_can_be_disabled_and_preserves_copy_text() {
        let md = "```rust\nfn main() {\n    let x = \"󰆏\";\n}\n```\n```toml\nkey = \"val\"\n```";
        let opts = textwrap::Options::new(80);
        let lines = render_markdown(md, &opts, false);

        assert!(is_opening_code_fence(&lines[0]));
        assert_eq!(
            extract_code_block_from(&lines, 0),
            "fn main() {\n    let x = \"󰆏\";\n}"
        );
        // Box border is preserved, but code inside has uniform MD_CODE_TEXT without syntax highlighting
        assert_eq!(lines[1].spans.len(), 2);
        assert_eq!(lines[1].spans[0].content.as_ref(), "  │ ");
        assert_eq!(
            lines[1].spans[0].style,
            Style::default().fg(theme::MD_CODE_BORDER)
        );
        assert_eq!(lines[1].spans[1].content.as_ref(), "fn main() {");
        assert_eq!(
            lines[1].spans[1].style,
            Style::default().fg(theme::MD_CODE_TEXT)
        );

        // Closing fence still renders box bottom border without copy badge
        assert!(!is_opening_code_fence(&lines[4]));
        assert!(
            lines[4]
                .spans
                .get(1)
                .is_some_and(|s| s.content.starts_with('╰'))
        );

        // Second block still opens and extracts independently
        assert!(is_opening_code_fence(&lines[5]));
        assert_eq!(extract_code_block_from(&lines, 5), "key = \"val\"");

        // Streaming cache matches full render when style_code_blocks is false
        let mut cache = MarkdownStreamCache::default();
        cache.update(md, &opts, false, false);
        assert_eq!(cache.lines, lines);
    }
}
