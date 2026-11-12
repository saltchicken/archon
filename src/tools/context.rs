use crate::{
    core::config::{expand_tilde, ContextSettings},
    tools::{auto_schema, parse_args, validate_relative_path, workspace_root},
};
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashSet,
    fmt::Write as _,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize, JsonSchema)]
struct ContextArgs {
    /// Optional relative subdirectory or file path within the workspace (defaults to project root).
    #[serde(default)]
    target_path: String,
    /// If true, only output the directory tree structure without file contents.
    #[serde(default)]
    tree_only: bool,
    /// Optional glob patterns to filter included files for this call (overrides config `include` when non-empty).
    #[serde(default)]
    include: Vec<String>,
    /// Optional additional glob patterns to exclude for this call.
    #[serde(default)]
    exclude: Vec<String>,
    /// Optional git ref or branch to include a `git diff` section (e.g. "HEAD", "main", "--staged"). Leave empty for no diff.
    #[serde(default)]
    diff: String,
}

pub fn schema() -> Value {
    auto_schema::<ContextArgs>(
        "context",
        "Generates a formatted project snapshot including directory tree, file contents, and optional git diff. Respects .gitignore and .archon.toml [context] settings.",
    )
}

struct FileEntry {
    path: PathBuf,
    display_path: String,
    depth: usize,
    is_dir: bool,
    include_content: bool,
    exceeds_size: bool,
}

struct FileData {
    path: String,
    content: Option<String>,
    error: Option<String>,
    skipped: Option<String>,
}

enum FileReadResult {
    Text(String),
    Binary,
    NonUtf8,
}

fn find_git_root(start_path: &Path) -> Option<PathBuf> {
    start_path
        .ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

fn shrink_tilde(path: &Path) -> String {
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(home) = dirs::home_dir()
        && let Ok(stripped) = canonical.strip_prefix(&home)
    {
        let s = stripped.to_string_lossy();
        return if s.is_empty() {
            "~".to_string()
        } else {
            format!("~/{s}")
        };
    }
    canonical.to_string_lossy().into_owned()
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(Glob::new(pat).with_context(|| format!("Invalid glob pattern: {pat}"))?);
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

fn read_text_file(path: &Path) -> std::io::Result<FileReadResult> {
    let mut file = fs::File::open(path)?;
    let file_size = file.metadata()?.len() as usize;

    let mut chunk = [0u8; 8192];
    let n = file.read(&mut chunk)?;
    if n == 0 {
        return Ok(FileReadResult::Text(String::new()));
    }
    if is_binary_bytes(&chunk[..n]) {
        return Ok(FileReadResult::Binary);
    }

    let mut buffer = Vec::with_capacity(file_size);
    buffer.extend_from_slice(&chunk[..n]);
    file.read_to_end(&mut buffer)?;

    match String::from_utf8(buffer) {
        Ok(s) => Ok(FileReadResult::Text(s)),
        Err(_) => Ok(FileReadResult::NonUtf8),
    }
}

fn read_to_file_data(path: &Path, display_path: String) -> FileData {
    match read_text_file(path) {
        Ok(FileReadResult::Text(content)) => FileData {
            path: display_path,
            content: Some(content),
            error: None,
            skipped: None,
        },
        Ok(FileReadResult::Binary) => FileData {
            path: display_path,
            content: None,
            error: None,
            skipped: Some("Binary file detected.".into()),
        },
        Ok(FileReadResult::NonUtf8) => FileData {
            path: display_path,
            content: None,
            error: None,
            skipped: Some("Non-UTF-8 text / binary file detected.".into()),
        },
        Err(e) => FileData {
            path: display_path,
            content: None,
            error: Some(format!("Error reading file: {e}")),
            skipped: None,
        },
    }
}

fn get_language(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("text")
}

fn get_fence(content: &str) -> String {
    let mut max_backticks = 0usize;
    let mut current = 0usize;
    for c in content.chars() {
        if c == '`' {
            current += 1;
            max_backticks = max_backticks.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(max_backticks.max(2) + 1)
}

fn run_git_diff(git_root: &Path, target: &str) -> Result<Option<(String, String)>> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.starts_with('-') && trimmed != "--staged" && trimmed != "--cached" {
        bail!("Invalid diff target '{trimmed}': only '--staged' or a valid git ref is allowed");
    }

    let output = Command::new("git")
        .current_dir(git_root)
        .args(["--no-pager", "diff", "--no-ext-diff", trimmed])
        .output()
        .context("Failed to execute git diff")?;

    if !output.status.success() {
        bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let diff_str = String::from_utf8_lossy(&output.stdout).into_owned();
    if diff_str.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some((trimmed.to_string(), diff_str)))
    }
}

pub fn execute(
    json_args: &Value,
    settings: &ContextSettings,
    _max_output_size: usize,
) -> Result<String> {
    let args: ContextArgs = parse_args(json_args, "context")?;
    let cwd = workspace_root()?;

    let base_root = if settings.git_root {
        find_git_root(&cwd).unwrap_or_else(|| cwd.clone())
    } else {
        cwd.clone()
    };

    let scan_root = if args.target_path.trim().is_empty() || args.target_path.trim() == "." {
        base_root.clone()
    } else {
        let rel = validate_relative_path(&args.target_path)?;
        let candidate = base_root.join(rel);
        if !candidate.exists() {
            bail!("Target path does not exist: '{}'", args.target_path.trim());
        }
        let canon = dunce::canonicalize(&candidate)?;
        if !canon.starts_with(&base_root) {
            bail!("Security violation: target_path escapes project root");
        }
        canon
    };

    let project_name = base_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();

    let include_patterns: Vec<String> = if !args.include.is_empty() {
        args.include
    } else if !settings.include.is_empty() {
        settings.include.clone()
    } else {
        vec!["**/*".into()]
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

    let include_set = build_globset(&include_patterns)?;
    let exclude_set = build_globset(&exclude_patterns)?;
    let tree_only_set = build_globset(&settings.include_in_tree)?;

    let mut entries = Vec::new();
    let walker = WalkBuilder::new(&scan_root)
        .hidden(false)
        .git_ignore(settings.git_root)
        .git_global(settings.git_root)
        .git_exclude(settings.git_root)
        .build();

    for result in walker {
        if entries.len() >= settings.max_files {
            break;
        }
        let Ok(entry) = result else { continue };
        let path = entry.path();
        if path == base_root {
            continue;
        }
        if path.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }

        let Some(relative) = diff_paths(path, &base_root) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }

        let matches_tree = tree_only_set.is_match(&relative);
        if exclude_set.is_match(&relative) && !matches_tree {
            continue;
        }

        let Ok(metadata) = path.metadata() else {
            continue;
        };
        let is_dir = metadata.is_dir();
        let matches_include = include_set.is_match(&relative);

        if !is_dir && !matches_include && !matches_tree {
            continue;
        }

        entries.push(FileEntry {
            path: path.to_path_buf(),
            display_path: relative.to_string_lossy().into_owned(),
            depth: relative.components().count(),
            is_dir,
            include_content: !is_dir && matches_include && !matches_tree,
            exceeds_size: metadata.len() > settings.max_size,
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let mut tree_out = String::new();
    let mut files = Vec::new();

    for entry in &entries {
        let indent = "    ".repeat(entry.depth.saturating_sub(1));
        let name = entry
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&entry.display_path);

        let marker = if entry.is_dir {
            "/"
        } else if !entry.include_content {
            " (content excluded)"
        } else {
            ""
        };

        let _ = writeln!(tree_out, "{indent}{name}{marker}");

        if !args.tree_only && entry.include_content {
            if entry.exceeds_size {
                files.push(FileData {
                    path: entry.display_path.clone(),
                    content: None,
                    error: Some("File exceeds maximum size limit.".into()),
                    skipped: None,
                });
            } else {
                files.push(read_to_file_data(&entry.path, entry.display_path.clone()));
            }
        }
    }

    if args.target_path.trim().is_empty() {
        let mut seen = HashSet::new();
        for raw_file in &settings.file {
            let expanded = expand_tilde(raw_file);
            if !seen.insert(expanded.clone()) {
                continue;
            }
            let display_path = shrink_tilde(&expanded);
            if !expanded.exists() {
                let _ = writeln!(tree_out, "{display_path} (not found)");
                if !args.tree_only {
                    files.push(FileData {
                        path: display_path,
                        content: None,
                        error: Some("File does not exist.".into()),
                        skipped: None,
                    });
                }
            } else if expanded.is_dir() {
                let _ = writeln!(tree_out, "{display_path}/");
            } else {
                let _ = writeln!(tree_out, "{display_path}");
                if !args.tree_only {
                    files.push(read_to_file_data(&expanded, display_path));
                }
            }
        }
    }

    let git_diff = run_git_diff(&base_root, &args.diff)?;

    let mut out = String::new();
    let _ = writeln!(out, "# Project: {project_name}\n");
    out.push_str("## Directory Structure\n\n```text\n");
    out.push_str(tree_out.trim_end());
    out.push_str("\n```\n\n");

    if !files.is_empty() {
        out.push_str("## Files\n\n");
        for f in &files {
            let _ = writeln!(out, "### File: `{}`\n", f.path);
            if let Some(err) = &f.error {
                let _ = writeln!(out, "**Error:** {err}\n");
            } else if let Some(skip) = &f.skipped {
                let _ = writeln!(out, "**Skipped:** {skip}\n");
            } else if let Some(content) = &f.content {
                let lang = get_language(&f.path);
                let fence = get_fence(content);
                let _ = writeln!(out, "{fence}{lang}");
                out.push_str(content);
                if !content.ends_with('\n') {
                    out.push('\n');
                }
                let _ = writeln!(out, "{fence}\n");
            }
        }
    }

    if let Some((target, diff)) = git_diff {
        let _ = writeln!(out, "## Git Diff (Target: {target})\n");
        let fence = get_fence(&diff);
        let _ = writeln!(out, "{fence}diff");
        out.push_str(&diff);
        if !diff.ends_with('\n') {
            out.push('\n');
        }
        let _ = writeln!(out, "{fence}\n");
    }

    Ok(out.trim_end().to_string())
}
