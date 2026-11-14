use super::fs::read_utf8_capped;
use anyhow::{Context, Result, bail};
use regex::RegexBuilder;
use std::{fmt::Write as _, path::PathBuf};

fn truncate_long_line(line: &str, max_chars: usize) -> String {
    match line.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}... [line truncated]", &line[..idx]),
        None => line.to_string(),
    }
}

pub struct SearchExecutionParams<'a> {
    pub pattern: &'a str,
    pub fixed_strings: bool,
    pub files_with_matches: bool,
    pub case_sensitive: Option<bool>,
    pub context_lines: Option<usize>,
    pub max_results: Option<usize>,
    pub max_file_size: u64,
    pub max_output_size: usize,
}

pub fn search_candidate_files(
    candidate_files: Vec<(PathBuf, String)>,
    params: &SearchExecutionParams<'_>,
) -> Result<String> {
    if params.pattern.is_empty() {
        bail!("'pattern' cannot be empty");
    }

    let case_sensitive = params
        .case_sensitive
        .unwrap_or_else(|| params.pattern.chars().any(char::is_uppercase));
    let effective_pattern = if params.fixed_strings {
        regex::escape(params.pattern)
    } else {
        params.pattern.to_string()
    };

    let matcher = RegexBuilder::new(&effective_pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .with_context(|| format!("Invalid regex pattern: '{}'", params.pattern))?;

    let context_radius = params.context_lines.unwrap_or(0).min(5);
    let max_results = params.max_results.unwrap_or(100).clamp(1, 500);
    let max_file_size = params.max_file_size;
    let max_output_size = params.max_output_size;

    if params.files_with_matches {
        let mut file_hits: Vec<(String, usize, usize)> = Vec::new();
        let mut total_matches = 0usize;

        for (abs_path, display_path) in candidate_files {
            let Some(content) = read_utf8_capped(&abs_path, max_file_size) else {
                continue;
            };
            let mut count = 0usize;
            let mut first_line = 0usize;
            for (idx, line) in content.lines().enumerate() {
                if matcher.is_match(line) {
                    if count == 0 {
                        first_line = idx + 1;
                    }
                    count += 1;
                }
            }
            if count > 0 {
                total_matches += count;
                file_hits.push((display_path, count, first_line));
            }
        }

        if file_hits.is_empty() {
            return Ok(format!(
                "No matches found for pattern '{}'.",
                params.pattern
            ));
        }

        file_hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let total_matched_files = file_hits.len();
        let hit_limit = total_matched_files > max_results;
        let mut out = String::new();

        for (display_path, count, first_line) in file_hits.into_iter().take(max_results) {
            let plural = if count == 1 { "match" } else { "matches" };
            let _ = writeln!(
                out,
                "{display_path} ({count} {plural}, first at L{first_line})"
            );
            if out.len() > max_output_size {
                break;
            }
        }

        let suffix = if hit_limit {
            format!(" — showing top {max_results} files")
        } else {
            String::new()
        };
        let _ = write!(
            out,
            "\n[Found {total_matches} match(es) across {total_matched_files} file(s){suffix}]"
        );
        return Ok(out);
    }

    let mut out = String::new();
    let mut total_matches = 0usize;
    let mut matched_files = 0usize;
    let mut hit_limit = false;

    for (abs_path, display_path) in candidate_files {
        if hit_limit {
            break;
        }
        let Some(content) = read_utf8_capped(&abs_path, max_file_size) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        let mut match_indices = Vec::new();

        for (idx, line) in lines.iter().enumerate() {
            if matcher.is_match(line) {
                match_indices.push(idx);
                total_matches += 1;
                if total_matches >= max_results {
                    hit_limit = true;
                    break;
                }
            }
        }

        if match_indices.is_empty() {
            continue;
        }

        matched_files += 1;
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(out, "=== {display_path} ===");

        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &idx in &match_indices {
            let start = idx.saturating_sub(context_radius);
            let end = (idx + context_radius).min(lines.len().saturating_sub(1));
            if let Some(last) = ranges.last_mut().filter(|l| start <= l.1 + 1) {
                last.1 = last.1.max(end);
            } else {
                ranges.push((start, end));
            }
        }

        for (r_idx, (start, end)) in ranges.into_iter().enumerate() {
            if context_radius > 0 && r_idx > 0 {
                let _ = writeln!(out, "  --");
            }
            for (idx, &line) in lines.iter().enumerate().take(end + 1).skip(start) {
                let sep = if match_indices.binary_search(&idx).is_ok() {
                    ':'
                } else {
                    '-'
                };
                let _ = writeln!(out, "{:>4}{sep} {}", idx + 1, truncate_long_line(line, 300));
            }
            if out.len() > max_output_size {
                hit_limit = true;
                break;
            }
        }
    }

    if total_matches == 0 {
        return Ok(format!(
            "No matches found for pattern '{}'.",
            params.pattern
        ));
    }

    let suffix = if hit_limit {
        format!(" — stopped at limit ({max_results})")
    } else {
        String::new()
    };
    let _ = write!(
        out,
        "\n[Found {total_matches} match(es) across {matched_files} file(s){suffix}]"
    );
    Ok(out)
}
