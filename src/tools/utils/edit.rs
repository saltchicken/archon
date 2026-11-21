use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::borrow::Cow;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditHunk {
    /// The exact, unique block of text to search for.
    pub search_string: String,
    /// The replacement text to substitute in place of `search_string`.
    pub replace_string: String,
}

fn normalize_eol(s: &str, use_crlf: bool) -> String {
    let lf = s.replace("\r\n", "\n");
    if use_crlf {
        lf.replace('\n', "\r\n")
    } else {
        lf
    }
}

fn locate_hunk_span(
    content: &str,
    file_path: &str,
    search: &str,
    replace: &str,
    hunk_label: &str,
) -> Result<(usize, usize, String)> {
    if search.is_empty() {
        bail!("{hunk_label}: search_string cannot be empty");
    }

    let mut matches: Vec<usize> = content.match_indices(search).map(|(i, _)| i).collect();
    let mut effective_search = Cow::Borrowed(search);
    let mut effective_replace = Cow::Borrowed(replace);

    if matches.is_empty() && search.contains('\n') {
        let target_crlf = content.contains("\r\n");
        let alt_search = normalize_eol(search, target_crlf);
        let alt_matches: Vec<usize> = content.match_indices(&alt_search).map(|(i, _)| i).collect();
        if !alt_matches.is_empty() {
            matches = alt_matches;
            effective_search = Cow::Owned(alt_search);
            effective_replace = Cow::Owned(normalize_eol(replace, target_crlf));
        }
    }

    // Fallback: whitespace-insensitive line matching for multi-line searches.
    // Handles trailing-whitespace and minor indentation differences that
    // commonly occur when an LLM reconstructs code from memory.
    if matches.is_empty()
        && search.contains('\n')
        && let Some((start, end)) = find_whitespace_insensitive_match(content, search)
    {
        let matched_text = &content[start..end];
        let target_crlf = matched_text.contains("\r\n");
        return Ok((start, end, normalize_eol(replace, target_crlf)));
    }

    match matches.as_slice() {
        [] => {
            let hint = nearest_match_hint(content, search);
            bail!("{hunk_label}: exact search_string not found in {file_path}.{hint}")
        }
        &[start] => Ok((
            start,
            start + effective_search.len(),
            effective_replace.into_owned(),
        )),
        n => bail!(
            "{hunk_label}: search_string matched {} times in {file_path}. Provide more surrounding context to make it unique.",
            n.len()
        ),
    }
}

/// Finds a multi-line search string in content by comparing lines with
/// trailing whitespace ignored. Returns the byte span of the matched region.
fn find_whitespace_insensitive_match(content: &str, search: &str) -> Option<(usize, usize)> {
    let search_lines: Vec<&str> = search.lines().collect();
    if search_lines.len() < 2 {
        return None;
    }

    // Build (byte_offset, line_text_without_newline) for each content line.
    let mut content_lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        content_lines.push((offset, text));
        offset += line.len();
    }

    if content_lines.len() < search_lines.len() {
        return None;
    }

    let search_trimmed: Vec<String> = search_lines
        .iter()
        .map(|l| l.trim_end().to_string())
        .collect();

    let mut candidates: Vec<usize> = Vec::new();
    let window = search_lines.len();
    for i in 0..=(content_lines.len() - window) {
        let all_match =
            (0..window).all(|j| content_lines[i + j].1.trim_end() == search_trimmed[j].as_str());
        if all_match {
            candidates.push(i);
        }
    }

    let [start_line] = candidates.as_slice() else {
        return None;
    };

    let start_byte = content_lines[*start_line].0;
    let end_line_idx = start_line + window - 1;
    let end_byte = content_lines[end_line_idx].0 + content_lines[end_line_idx].1.len();

    Some((start_byte, end_byte))
}

/// Produces a short hint for the error message by finding the closest
/// line-based match, helping the model correct its search_string.
fn nearest_match_hint(content: &str, search: &str) -> String {
    let search_lines: Vec<&str> = search.lines().collect();
    if search_lines.len() < 2 {
        return String::new();
    }

    let content_lines: Vec<&str> = content.lines().collect();
    if content_lines.len() < search_lines.len() {
        return String::new();
    }

    let search_trimmed: Vec<String> = search_lines
        .iter()
        .map(|l| l.trim_end().to_string())
        .collect();
    let window = search_lines.len();

    let mut best_score = 0usize;
    let mut best_start = 0usize;
    for i in 0..=(content_lines.len() - window) {
        let score = (0..window)
            .filter(|&j| content_lines[i + j].trim_end() == search_trimmed[j].as_str())
            .count();
        if score > best_score {
            best_score = score;
            best_start = i;
        }
    }

    if best_score >= window.saturating_sub(1) && best_score < window {
        // Very close match (off by 1 line) — show a snippet.
        let snippet: String = content_lines[best_start..best_start + window]
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n  Closest match found at line {} ({} of {} lines matched).\n  Did you mean:\n    {}",
            best_start + 1,
            best_score,
            window,
            snippet
                .lines()
                .map(|l| format!("    {}", l))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        String::new()
    }
}

pub fn apply_hunks_to_str(content: &str, file_path: &str, edits: &[EditHunk]) -> Result<String> {
    if edits.is_empty() {
        bail!("'edits' array cannot be empty");
    }

    let total = edits.len();
    let label_for = |idx: usize| {
        if total == 1 {
            "Edit".into()
        } else {
            format!("Hunk {}/{total}", idx + 1)
        }
    };

    let mut resolved: Vec<(usize, usize, usize, String)> = Vec::with_capacity(total);
    for (idx, hunk) in edits.iter().enumerate() {
        let (start, end, replacement) = locate_hunk_span(
            content,
            file_path,
            &hunk.search_string,
            &hunk.replace_string,
            &label_for(idx),
        )?;
        for &(prev_idx, prev_start, prev_end, _) in &resolved {
            if start < prev_end && prev_start < end {
                bail!(
                    "{} and {} overlap in {file_path}. Edits must be disjoint.",
                    label_for(prev_idx),
                    label_for(idx)
                );
            }
        }
        resolved.push((idx, start, end, replacement));
    }

    resolved.sort_by_key(|&(_, start, _, _)| start);

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0;
    for (_, start, end, replacement) in resolved {
        out.push_str(&content[cursor..start]);
        out.push_str(&replacement);
        cursor = end;
    }
    out.push_str(&content[cursor..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_still_works() {
        let content = "fn foo() {\n    let x = 1;\n}\n";
        let hunk = EditHunk {
            search_string: "let x = 1;".into(),
            replace_string: "let x = 2;".into(),
        };
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]).unwrap();
        assert_eq!(result, "fn foo() {\n    let x = 2;\n}\n");
    }

    #[test]
    fn trailing_whitespace_fallback_matches() {
        // File has trailing spaces on line 2, but search_string doesn't.
        let content = "fn foo() {\n    let x = 1;   \n}\n";
        let hunk = EditHunk {
            search_string: "fn foo() {\n    let x = 1;\n}".into(),
            replace_string: "fn bar() {\n    let y = 2;\n}".into(),
        };
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]).unwrap();
        assert_eq!(result, "fn bar() {\n    let y = 2;\n}\n");
    }

    #[test]
    fn extra_trailing_whitespace_in_search_is_ignored() {
        // Search has trailing spaces that aren't in the file.
        let content = "fn foo() {\n    let x = 1;\n}\n";
        let hunk = EditHunk {
            search_string: "fn foo() {\n    let x = 1;   \n}".into(),
            replace_string: "fn bar() {\n    let y = 2;\n}".into(),
        };
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]).unwrap();
        assert_eq!(result, "fn bar() {\n    let y = 2;\n}\n");
    }

    #[test]
    fn ambiguous_whitespace_match_fails() {
        // Two identical blocks — whitespace-insensitive match is ambiguous.
        let content = "let a = 1;\nlet b = 2;\nlet a = 1;\nlet b = 2;\n";
        let hunk = EditHunk {
            search_string: "let a = 1;\nlet b = 2;".into(),
            replace_string: "let c = 3;\nlet d = 4;".into(),
        };
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]);
        assert!(result.is_err());
    }

    #[test]
    fn single_line_search_still_requires_exact() {
        // Single-line searches don't use the fallback.
        let content = "let x = 1;   \n";
        let hunk = EditHunk {
            search_string: "let x = 1;".into(),
            replace_string: "let x = 2;".into(),
        };
        // Exact match works fine here.
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]).unwrap();
        assert_eq!(result, "let x = 2;   \n");
    }
}
