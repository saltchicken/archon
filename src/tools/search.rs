use crate::{
    core::config::WorkspaceSnapshotSettings,
    tools::utils::{
        auto_schema, parse_args, read_utf8_capped, resolve_scan_roots, walk_workspace_files,
    },
};
use anyhow::{Context, Result, bail};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write as _;

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// Regular expression (or literal string when `fixed_strings: true`) to search for across files.
    pattern: String,
    /// Optional relative directory or file path within the workspace to restrict the search scope (defaults to workspace root).
    #[serde(default)]
    path: String,
    /// Optional glob patterns to filter which files are searched (e.g. `["*.rs", "src/**/*.toml"]`).
    #[serde(default)]
    include: Vec<String>,
    /// Optional glob patterns to exclude from search.
    #[serde(default)]
    exclude: Vec<String>,
    /// If true, treats `pattern` as a literal string instead of a regular expression.
    #[serde(default)]
    fixed_strings: bool,
    /// If true, returns only the list of matching file paths with their match counts (sorted by match count descending) instead of individual lines, avoiding early line-limit truncation on broad queries.
    #[serde(default)]
    files_with_matches: bool,
    /// Optional case sensitivity override. Defaults to smart-case (case-insensitive unless `pattern` contains uppercase characters).
    #[serde(default)]
    case_sensitive: Option<bool>,
    /// Number of surrounding context lines to show before and after each match (0 to 5, default: 0). Ignored when `files_with_matches: true`.
    #[serde(default)]
    context_lines: Option<usize>,
    /// Maximum number of matching lines (or matching files when `files_with_matches: true`) to return across the workspace (default: 100, max: 500).
    #[serde(default)]
    max_results: Option<usize>,
}

pub fn schema() -> Value {
    auto_schema::<SearchArgs>(
        "search",
        "Searches file contents in the workspace using regular expressions or literal strings. Use `files_with_matches` to survey all matching files and counts across large codebases. For AST symbol definitions or call sites, prefer `repo_map` (`action: \"definition\"` or `\"references\"`). Respects .gitignore and outputs 1-based line numbers.",
    )
}

fn truncate_long_line(line: &str, max_chars: usize) -> String {
    match line.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}... [line truncated]", &line[..idx]),
        None => line.to_string(),
    }
}

pub fn execute(
    json_args: &Value,
    settings: &WorkspaceSnapshotSettings,
    max_output_size: usize,
) -> Result<String> {
    let args: SearchArgs = parse_args(json_args, "search")?;
    if args.pattern.is_empty() {
        bail!("'pattern' cannot be empty");
    }

    let case_sensitive = args
        .case_sensitive
        .unwrap_or_else(|| args.pattern.chars().any(char::is_uppercase));
    let effective_pattern = if args.fixed_strings {
        regex::escape(&args.pattern)
    } else {
        args.pattern.clone()
    };

    let matcher = RegexBuilder::new(&effective_pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .with_context(|| format!("Invalid regex pattern: '{}'", args.pattern))?;

    let context_radius = args.context_lines.unwrap_or(0).min(5);
    let max_results = args.max_results.unwrap_or(100).clamp(1, 500);
    let (base_root, scan_root) =
        resolve_scan_roots(settings.git_root, &args.path, "path escapes workspace root")?;

    let candidate_files = walk_workspace_files(
        &base_root,
        &scan_root,
        settings.git_root,
        &args.include,
        &settings.exclude,
        &args.exclude,
        settings.max_files,
    )?;

    if args.files_with_matches {
        let mut file_hits: Vec<(String, usize, usize)> = Vec::new();
        let mut total_matches = 0usize;

        for (abs_path, display_path) in candidate_files {
            let Some(content) = read_utf8_capped(&abs_path, settings.max_size) else {
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
            return Ok(format!("No matches found for pattern '{}'.", args.pattern));
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
        let Some(content) = read_utf8_capped(&abs_path, settings.max_size) else {
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

        if context_radius == 0 {
            for &idx in &match_indices {
                let _ = writeln!(
                    out,
                    "{:>4}: {}",
                    idx + 1,
                    truncate_long_line(lines[idx], 300)
                );
                if out.len() > max_output_size {
                    hit_limit = true;
                    break;
                }
            }
        } else {
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
                if r_idx > 0 {
                    let _ = writeln!(out, "  --");
                }
                for idx in start..=end {
                    let sep = if match_indices.binary_search(&idx).is_ok() {
                        ':'
                    } else {
                        '-'
                    };
                    let _ = writeln!(
                        out,
                        "{:>4}{sep} {}",
                        idx + 1,
                        truncate_long_line(lines[idx], 300)
                    );
                }
                if out.len() > max_output_size {
                    hit_limit = true;
                    break;
                }
            }
        }
    }

    if total_matches == 0 {
        return Ok(format!("No matches found for pattern '{}'.", args.pattern));
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
