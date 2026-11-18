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

fn format_markdown_table(raw_lines: &[&str], available_width: usize) -> Option<Vec<WrappedLine>> {
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
        .fg(theme::MD_QUOTE_TEXT)
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
                        .fg(theme::MD_TOOL_PARAM)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::MD_TOOL_PARAM)
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
            } else if trimmed.starts_with("⚙ ") {
                let style = Style::default()
                    .fg(theme::MD_TOOL_PARAM)
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

fn style_markdown_header(level: usize, content_spans: &[Span<'static>]) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("  ")];
    spans.extend(style_markdown_header_inner(level, content_spans));
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

fn style_lines(content_lines: &[WrappedLine]) -> Vec<Line<'static>> {
    let mut in_code_block = false;
    let mut code_fence_width = 7usize;
    let mut out = Vec::with_capacity(content_lines.len());
    let border_style = Style::default().fg(theme::MD_CODE_BORDER);

    for item in content_lines {
        let formatted = match item {
            WrappedLine::CodeFence(trimmed) => {
                let spans = if !in_code_block {
                    let lang = trimmed.trim_start_matches('`').trim();
                    let copy_badge = Span::styled(
                        "󰆏",
                        Style::default()
                            .fg(theme::BADGE_CODE_COPY)
                            .add_modifier(Modifier::BOLD),
                    );
                    if lang.is_empty() {
                        code_fence_width = 7;
                        vec![
                            Span::raw("  "),
                            Span::styled(
                                format!("╭{} ", "─".repeat(code_fence_width)),
                                border_style,
                            ),
                            copy_badge,
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
                            Span::styled(" ─── ", border_style),
                            copy_badge,
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
                    Span::styled(line.clone(), Style::default().fg(theme::MD_CODE_TEXT)),
                ]
            }
            WrappedLine::TableBorder(line) => {
                vec![
                    Span::raw("  "),
                    Span::styled(line.clone(), Style::default().fg(theme::MD_TABLE_BORDER)),
                ]
            }
            WrappedLine::TableRow(spans) => spans.clone(),
            WrappedLine::Header { level, spans } => style_markdown_header(*level, spans),
            WrappedLine::ListItem {
                indent,
                marker,
                spans: item_spans,
            } => {
                let mut spans = vec![Span::raw(format!("  {}", " ".repeat(*indent)))];
                spans.extend(style_list_marker(marker));
                spans.extend(item_spans.iter().cloned());
                spans
            }
            WrappedLine::ListContinuation {
                indent,
                spans: cont_spans,
            } => {
                let mut spans = vec![Span::raw(format!("  {}", " ".repeat(*indent)))];
                spans.extend(cont_spans.iter().cloned());
                spans
            }
            WrappedLine::Blockquote {
                indent,
                depth,
                content,
            } => {
                let mut spans = vec![Span::raw(format!("  {}", " ".repeat(*indent)))];
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

pub(super) fn render_markdown(
    content: &str,
    options: &textwrap::Options<'_>,
) -> Vec<Line<'static>> {
    style_lines(&wrap_text(content, options))
}

pub(super) fn render_thought_markdown(
    content: &str,
    options: &textwrap::Options<'_>,
) -> Vec<Line<'static>> {
    let thought_style = Style::default()
        .fg(theme::MD_THOUGHT)
        .add_modifier(Modifier::ITALIC);
    render_markdown(content, options)
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
            let text = line.spans.get(1).map_or("", |s| s.content.as_ref());
            code_lines.push(text);
        } else {
            break;
        }
    }
    code_lines.join("\n")
}
