use crate::{
    core::config::WorkspaceSnapshotSettings,
    tools::{auto_schema, parse_args, validate_relative_path, workspace_root},
};
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use pathdiff::diff_paths;
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{
    fmt::Write as _,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

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
    /// Optional case sensitivity override. Defaults to smart-case (case-insensitive unless `pattern` contains uppercase characters).
    #[serde(default)]
    case_sensitive: Option<bool>,
    /// Number of surrounding context lines to show before and after each match (0 to 5, default: 0).
    #[serde(default)]
    context_lines: Option<usize>,
    /// Maximum number of matching lines to return across the workspace (default: 100, max: 500).
    #[serde(default)]
    max_results: Option<usize>,
}

pub fn schema() -> Value {
    auto_schema::<SearchArgs>(
        "search",
        "Searches file contents in the workspace using regular expressions or literal strings. Respects .gitignore and outputs 1-based line numbers.",
    )
}

fn find_git_root(start_path: &Path) -> Option<PathBuf> {
    start_path
        .ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if pat.contains('/') || pat.contains('\\') {
            builder
                .add(Glob::new(pat).with_context(|| format!("Invalid glob pattern: {pat}"))?);
        } else {
            // Allow shorthand like "*.rs" to match at any directory depth
            builder
                .add(Glob::new(pat).with_context(|| format!("Invalid glob pattern: {pat}"))?);
            let recursive = format!("**/{pat}");
            builder.add(
                Glob::new(&recursive)
                    .with_context(|| format!("Invalid glob pattern: {recursive}"))?,
            );
        }
    }
    Ok(builder.build()?)
}

fn is_binary_bytes(data: &[u8]) -> bool {
    if data.contains(&0) {
        return true;
    }
    let control_chars = data
        .iter()
        .filter(|&&b| b < 32 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0C))
        .count();
    (control_chars as f32 / data.len() as f32) > 0.3
}

fn read_searchable_text(path: &Path, max_file_size: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 || len > max_file_size {
        return None;
    }

    let mut chunk = [0u8; 8192];
    let n = file.read(&mut chunk).ok()?;
    if n == 0 || is_binary_bytes(&chunk[..n]) {
        return None;
    }

    let mut buffer = Vec::with_capacity(len as usize);
    buffer.extend_from_slice(&chunk[..n]);
    file.read_to_end(&mut buffer).ok()?;
    String::from_utf8(buffer).ok()
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
    let raw_pattern = if args.fixed_strings {
        regex::escape(&args.pattern)
    } else {
        args.pattern.clone()
    };

    let matcher = RegexBuilder::new(&raw_pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .with_context(|| format!("Invalid regex pattern: '{}'", args.pattern))?;

    let context_radius = args.context_lines.unwrap_or(0).min(5);
    let max_results = args.max_results.unwrap_or(100).clamp(1, 500);

    let cwd = workspace_root()?;
    let base_root = if settings.git_root {
        find_git_root(&cwd).unwrap_or_else(|| cwd.clone())
    } else {
        cwd.clone()
    };
    let base_root = dunce::canonicalize(&base_root).unwrap_or(base_root);

    let scan_root = if args.path.trim().is_empty() || args.path.trim() == "." {
        base_root.clone()
    } else {
        let rel = validate_relative_path(&args.path)?;
        let candidate = base_root.join(rel);
        if !candidate.exists() {
            bail!("Target path does not exist: '{}'", args.path.trim());
        }
        let canon = dunce::canonicalize(&candidate)?;
        if !canon.starts_with(&base_root) {
            bail!("Security violation: path escapes workspace root");
        }
        canon
    };

    let include_set = if args.include.is_empty() {
        None
    } else {
        Some(build_globset(&args.include)?)
    };

    let mut exclude_patterns = vec![
        "**/.git/**".to_string(),
        "**/*.venv/**".to_string(),
        "**/.env*".to_string(),
        "**/*.pem".to_string(),
        "**/*.key".to_string(),
        "**/id_rsa*".to_string(),
        "**/secrets.json".to_string(),
    ];
    exclude_patterns.extend(settings.exclude.iter().cloned());
    exclude_patterns.extend(args.exclude);
    let exclude_set = build_globset(&exclude_patterns)?;

    let mut candidate_files = Vec::new();
    if scan_root.is_file() {
        let rel = diff_paths(&scan_root, &base_root).unwrap_or_else(|| scan_root.clone());
        if exclude_set.is_match(&rel) {
            bail!(
                "Security violation: '{}' matches an excluded pattern",
                rel.display()
            );
        }
        candidate_files.push((scan_root, rel.to_string_lossy().into_owned()));
    } else {
        let walker = WalkBuilder::new(&scan_root)
            .hidden(false)
            .git_ignore(settings.git_root)
            .git_global(settings.git_root)
            .git_exclude(settings.git_root)
            .build();

        for entry in walker.flatten() {
            if candidate_files.len() >= settings.max_files {
                break;
            }
            let path = entry.path();
            if !path.is_file() || path.components().any(|c| c.as_os_str() == ".git") {
                continue;
            }
            let Some(relative) = diff_paths(path, &base_root) else {
                continue;
            };
            if exclude_set.is_match(&relative) {
                continue;
            }
            if let Some(inc) = &include_set
                && !inc.is_match(&relative)
            {
                continue;
            }
            candidate_files.push((path.to_path_buf(), relative.to_string_lossy().into_owned()));
        }
        candidate_files.sort_by(|a, b| a.0.cmp(&b.0));
    }

    let mut out = String::new();
    let mut total_matches = 0usize;
    let mut matched_files = 0usize;
    let mut hit_limit = false;

    for (abs_path, display_path) in candidate_files {
        if hit_limit {
            break;
        }
        let Some(content) = read_searchable_text(&abs_path, settings.max_size) else {
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
                let line_no = idx + 1;
                let text = truncate_long_line(lines[idx], 300);
                let _ = writeln!(out, "{line_no:>4}: {text}");
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
                if let Some(last) = ranges.last_mut()
                    && start <= last.1 + 1
                {
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
                    let line_no = idx + 1;
                    let is_match = match_indices.binary_search(&idx).is_ok();
                    let sep = if is_match { ':' } else { '-' };
                    let text = truncate_long_line(lines[idx], 300);
                    let _ = writeln!(out, "{line_no:>4}{sep} {text}");
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

    let _ = write!(
        out,
        "\n[Found {total_matches} match(es) across {matched_files} file(s){}]",
        if hit_limit {
            format!(" — stopped at limit ({max_results})")
        } else {
            String::new()
        }
    );

    Ok(out)
}

fn truncate_long_line(line: &str, max_chars: usize) -> String {
    match line.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}... [line truncated]", &line[..idx]),
        None => line.to_string(),
    }
}
