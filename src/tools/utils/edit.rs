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

    // Fallback: whitespace-insensitive line matching under `canonical_line`.
    // Tolerates leading indentation, internal-whitespace, and stray outer
    // blank-line differences that commonly occur when an LLM reconstructs code
    // from memory. Applies to single-line searches too; ambiguous windows are
    // still rejected (see `find_canonical_match`).
    if matches.is_empty()
        && let Some((start, end)) = find_canonical_match(content, search)
    {
        let matched_text = &content[start..end];
        let target_crlf = matched_text.contains("\r\n");
        return Ok((start, end, normalize_eol(replace, target_crlf)));
    }

    match matches.as_slice() {
        [] => {
            let hint = nearest_match_hint(content, search);
            bail!("{hunk_label}: search_string not found in {file_path}.{hint}")
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

/// Canonical form of a line used for *matching only* (never for the
/// replacement): both ends trimmed and internal whitespace runs collapsed to
/// single spaces. Two lines are "equal" under this form when their non-whitespace
/// content and token order are identical, which tolerates leading indentation,
/// tabs-vs-spaces, and extra internal spacing.
fn canonical_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Finds a search string in content by comparing lines under [`canonical_line`].
/// Tolerates leading indentation, internal-whitespace, and leading/trailing
/// blank-line differences that commonly occur when an LLM reconstructs code
/// from memory. Works for single-line searches too.
///
/// The matched region spans whole lines (including their original leading
/// indentation) and excludes the final newline; callers splice the replacement
/// in verbatim, so the replacement should include any indentation it wants to
/// preserve. Ambiguous windows (more than one candidate) are rejected by
/// returning `None`, so a wrong region is never silently edited.
fn find_canonical_match(content: &str, search: &str) -> Option<(usize, usize)> {
    // Canonical search lines, dropping leading/trailing blank lines so a stray
    // blank line in the model's search string does not break matching.
    let mut search_lines: Vec<String> = search.lines().map(canonical_line).collect();
    while search_lines.first().is_some_and(|l| l.is_empty()) {
        search_lines.remove(0);
    }
    while search_lines.last().is_some_and(|l| l.is_empty()) {
        search_lines.pop();
    }
    let window = search_lines.len();
    if window == 0 {
        return None;
    }

    // (byte_offset, trimmed_len, canonical) per content line. Using
    // split_inclusive keeps byte offsets stable so spans map back into `content`.
    let mut content_lines: Vec<(usize, usize, String)> = Vec::new();
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        content_lines.push((offset, text.len(), canonical_line(text)));
        offset += line.len();
    }

    if content_lines.len() < window {
        return None;
    }

    let mut candidates: Vec<usize> = Vec::new();
    for i in 0..=(content_lines.len() - window) {
        let all_match = (0..window).all(|j| content_lines[i + j].2 == search_lines[j]);
        if all_match {
            candidates.push(i);
        }
    }

    let [start_line] = candidates.as_slice() else {
        return None;
    };

    let start_byte = content_lines[*start_line].0;
    let end_line_idx = start_line + window - 1;
    let end_byte = content_lines[end_line_idx].0 + content_lines[end_line_idx].1;

    Some((start_byte, end_byte))
}

/// Produces an actionable hint for the error message by locating the closest
/// line-based match and showing the real file content with line numbers and a
/// per-line match marker, so the model can copy the exact text (including
/// indentation) and retry in a single step. Always shown when any line is
/// partially similar; empty only when nothing in the file resembles the search.
fn nearest_match_hint(content: &str, search: &str) -> String {
    let mut search_lines: Vec<String> = search.lines().map(canonical_line).collect();
    while search_lines.first().is_some_and(|l| l.is_empty()) {
        search_lines.remove(0);
    }
    while search_lines.last().is_some_and(|l| l.is_empty()) {
        search_lines.pop();
    }
    let window = search_lines.len();
    if window == 0 {
        return String::new();
    }

    let raw_lines: Vec<&str> = content.lines().collect();
    if raw_lines.len() < window {
        return String::new();
    }

    let canon_lines: Vec<String> = raw_lines.iter().map(|l| canonical_line(l)).collect();

    let mut best_score = 0usize;
    let mut best_start = 0usize;
    for i in 0..=(raw_lines.len() - window) {
        let score = (0..window)
            .filter(|&j| canon_lines[i + j] == search_lines[j])
            .count();
        if score > best_score {
            best_score = score;
            best_start = i;
        }
    }

    if best_score == 0 {
        return String::new();
    }

    let show = window.min(8);
    let mut out = String::new();
    out.push_str(&format!(
        "\n  Closest region near line {} ({} of {} line(s) matched under whitespace normalization).\n",
        best_start + 1,
        best_score,
        window
    ));
    out.push_str("  File lines (copy the exact text, including indentation, and retry):\n");
    for (j, expected) in search_lines.iter().take(show).enumerate() {
        let idx = best_start + j;
        let matched = canon_lines[idx] == *expected;
        let marker = if matched { "  " } else { "!=" };
        out.push_str(&format!(
            "    {:>4} {} {}\n",
            idx + 1,
            marker,
            raw_lines[idx]
        ));
    }
    if window > 8 {
        out.push_str("      ... (window truncated)\n");
    }
    out
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
    fn single_line_exact_substring_match() {
        // Single-line searches that are exact substrings still match (Tier 0).
        let content = "let x = 1;   \n";
        let hunk = EditHunk {
            search_string: "let x = 1;".into(),
            replace_string: "let x = 2;".into(),
        };
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]).unwrap();
        assert_eq!(result, "let x = 2;   \n");
    }

    #[test]
    fn single_line_internal_whitespace_matches_via_canonical() {
        // Internal-whitespace differences no longer block single-line edits.
        let content = "fn foo() {\n    let x = 1;\n}\n";
        let hunk = EditHunk {
            search_string: "let x   =   1;".into(),
            replace_string: "    let x = 2;".into(),
        };
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]).unwrap();
        assert_eq!(result, "fn foo() {\n    let x = 2;\n}\n");
    }

    #[test]
    fn indentation_drift_multiline_matches() {
        // File uses 4-space indent; search uses 2-space indent.
        let content = "fn foo() {\n    let x = 1;\n    let y = 2;\n}\n";
        let hunk = EditHunk {
            search_string: "fn foo() {\n  let x = 1;\n  let y = 2;\n}".into(),
            replace_string: "fn bar() {\n    let x = 9;\n    let y = 9;\n}".into(),
        };
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]).unwrap();
        assert_eq!(result, "fn bar() {\n    let x = 9;\n    let y = 9;\n}\n");
    }

    #[test]
    fn outer_blank_lines_in_search_are_ignored() {
        // A leading blank line in the search string is tolerated.
        let content = "fn foo() {\n    let x = 1;\n}\n";
        let hunk = EditHunk {
            search_string: "\nfn foo() {\n    let x = 1;\n}".into(),
            replace_string: "fn bar() {\n    let y = 2;\n}".into(),
        };
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]).unwrap();
        assert_eq!(result, "fn bar() {\n    let y = 2;\n}\n");
    }

    #[test]
    fn ambiguous_single_line_fails() {
        // Two identical lines → match is ambiguous → rejected (no silent edit).
        let content = "let a = 1;\nlet a = 1;\n";
        let hunk = EditHunk {
            search_string: "let a = 1;".into(),
            replace_string: "let a = 2;".into(),
        };
        let result = apply_hunks_to_str(content, "test.rs", &[hunk]);
        assert!(result.is_err());
    }

    #[test]
    fn canonical_match_preserves_file_line_ending() {
        // CRLF file with an LF/indentation-drifted search must not corrupt endings.
        let content = "fn foo() {\r\n    let x = 1;\r\n}\r\n";
        let hunk = EditHunk {
            search_string: "fn foo() {\n  let x = 1;\n}".into(),
            replace_string: "fn bar() {\n    let y = 2;\n}".into(),
        };
        let result = apply_hunks_to_str(content, "crlf.rs", &[hunk]).unwrap();
        assert_eq!(result, "fn bar() {\r\n    let y = 2;\r\n}\r\n");
    }

    #[test]
    fn hint_shows_numbered_region_with_markers() {
        let content = "fn foo() {\n    let x = 1;\n    let z = 0;\n}\n";
        // One line's indentation differs → partial match, expect a hint.
        let hint = nearest_match_hint(content, "fn foo() {\nlet x = 1;\n}");
        assert!(
            hint.contains("line 1"),
            "hint should reference line numbers: {hint}"
        );
        assert!(
            hint.contains("fn foo() {"),
            "hint should show file content: {hint}"
        );
        assert!(
            hint.contains("!="),
            "hint should mark a mismatched line: {hint}"
        );
    }

    #[test]
    fn hint_empty_when_no_similarity() {
        let content = "fn foo() {\n    let x = 1;\n}\n";
        let hint = nearest_match_hint(content, "completely unrelated text here");
        assert_eq!(hint, String::new());
    }
}
