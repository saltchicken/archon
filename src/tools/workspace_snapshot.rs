use crate::{
    core::config::{WorkspaceSnapshotSettings, expand_tilde},
    tools::utils::{
        auto_schema, build_exclude_globset, build_globset, build_workspace_walker, is_binary_bytes,
        parse_args, resolve_scan_roots,
    },
};
use anyhow::Result;
use globset::GlobSet;
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
};

#[derive(Deserialize, JsonSchema)]
struct WorkspaceSnapshotArgs {
    /// Optional relative subdirectory or file path within the workspace (defaults to workspace root).
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
}

pub fn schema() -> Value {
    auto_schema::<WorkspaceSnapshotArgs>(
        "workspace_snapshot",
        "Generates a full workspace directory tree and dumps complete file contents (including configs like Cargo.toml and non-code files). Use `tree_only: true` to inspect project structure, or scope with `target_path` / `include` for small modules. For navigating medium-to-large codebases without exhausting context, prefer `repo_map`.",
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

enum FileOutcome {
    Content(String),
    Skipped(&'static str),
    Error(String),
}

fn shrink_tilde(path: &Path) -> String {
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(home) = dirs::home_dir()
        && let Ok(stripped) = canonical.strip_prefix(&home)
    {
        let s = stripped.to_string_lossy();
        return if s.is_empty() {
            "~".into()
        } else {
            format!("~/{s}")
        };
    }
    canonical.to_string_lossy().into_owned()
}

fn read_to_outcome(
    path: &Path,
    display_len: usize,
    used_bytes: &mut usize,
    max_output_size: usize,
) -> FileOutcome {
    if *used_bytes > max_output_size {
        return FileOutcome::Skipped("Workspace snapshot output size limit reached.");
    }

    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return FileOutcome::Error(format!("Error reading file: {e}")),
    };
    let file_size = file.metadata().map_or(0, |m| m.len() as usize);

    let mut chunk = [0u8; 8192];
    let n = match file.read(&mut chunk) {
        Ok(0) => return FileOutcome::Content(String::new()),
        Ok(n) => n,
        Err(e) => return FileOutcome::Error(format!("Error reading file: {e}")),
    };

    if is_binary_bytes(&chunk[..n]) {
        return FileOutcome::Skipped("Binary file detected.");
    }

    let mut buffer = Vec::with_capacity(file_size);
    buffer.extend_from_slice(&chunk[..n]);
    if let Err(e) = file.read_to_end(&mut buffer) {
        return FileOutcome::Error(format!("Error reading file: {e}"));
    }

    match String::from_utf8(buffer) {
        Ok(content) => {
            *used_bytes = used_bytes.saturating_add(content.len() + display_len + 32);
            FileOutcome::Content(content)
        }
        Err(_) => FileOutcome::Skipped("Non-UTF-8 text / binary file detected."),
    }
}

fn get_fence(content: &str) -> String {
    let max_backticks = content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(max_backticks.max(2) + 1)
}

fn archon_config_dir() -> Option<PathBuf> {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("archon");
    dunce::canonicalize(&dir).ok().or(Some(dir))
}

fn validate_extra_file(
    raw_file: &str,
    expanded: &Path,
    base_root: &Path,
    exclude_set: &GlobSet,
    trusted_global_files: &HashSet<PathBuf>,
    config_dir: Option<&Path>,
) -> std::result::Result<PathBuf, String> {
    let candidate = if expanded.is_relative() {
        base_root.join(expanded)
    } else {
        expanded.to_path_buf()
    };
    let canon = dunce::canonicalize(&candidate).unwrap_or(candidate);

    if trusted_global_files.contains(&canon)
        || trusted_global_files.contains(expanded)
        || config_dir.is_some_and(|cfg| canon.starts_with(cfg))
    {
        return Ok(canon);
    }

    if canon.starts_with(base_root) {
        if diff_paths(&canon, base_root).is_some_and(|rel| exclude_set.is_match(&rel)) {
            return Err(format!(
                "Security violation: '{raw_file}' matches an excluded pattern."
            ));
        }
        return Ok(canon);
    }

    Err(format!(
        "Security violation: local .archon.toml cannot read external file '{raw_file}' outside the workspace."
    ))
}

pub fn execute(
    json_args: &Value,
    settings: &WorkspaceSnapshotSettings,
    max_output_size: usize,
) -> Result<String> {
    let args: WorkspaceSnapshotArgs = parse_args(json_args, "workspace_snapshot")?;
    let (base_root, scan_root) = resolve_scan_roots(
        settings.git_root,
        &args.target_path,
        "target_path escapes workspace root",
    )?;

    let workspace_name = base_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    let include_patterns = if !args.include.is_empty() {
        args.include
    } else if !settings.include.is_empty() {
        settings.include.clone()
    } else {
        vec!["**/*".into()]
    };

    let include_set = build_globset(&include_patterns, false)?;
    let exclude_set = build_exclude_globset(&settings.exclude, &args.exclude, false)?;
    let tree_only_set = build_globset(&settings.include_in_tree, false)?;

    let mut entries = Vec::new();
    for entry in build_workspace_walker(&scan_root, settings.git_root) {
        if entries.len() >= settings.max_files {
            break;
        }
        let path = entry.path();
        if path == base_root || path.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let Some(relative) = diff_paths(path, &base_root).filter(|r| !r.as_os_str().is_empty())
        else {
            continue;
        };

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
    let mut files: Vec<(String, FileOutcome)> = Vec::new();

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
    }

    let mut used_bytes = tree_out.len() + 128;

    if !args.tree_only {
        for entry in entries.iter().filter(|e| e.include_content) {
            let outcome = if entry.exceeds_size {
                FileOutcome::Error("File exceeds maximum size limit.".into())
            } else {
                read_to_outcome(
                    &entry.path,
                    entry.display_path.len(),
                    &mut used_bytes,
                    max_output_size,
                )
            };
            files.push((entry.display_path.clone(), outcome));
        }
    }

    if args.target_path.trim().is_empty() {
        let trusted_global_files: HashSet<PathBuf> = settings
            .global_files
            .iter()
            .flat_map(|f| {
                let exp = expand_tilde(f);
                let canon = dunce::canonicalize(&exp).unwrap_or_else(|_| exp.clone());
                [exp, canon]
            })
            .collect();
        let cfg_dir = archon_config_dir();
        let mut seen = HashSet::new();

        for raw_file in &settings.file {
            let expanded = expand_tilde(raw_file);
            if !seen.insert(expanded.clone()) {
                continue;
            }

            match validate_extra_file(
                raw_file,
                &expanded,
                &base_root,
                &exclude_set,
                &trusted_global_files,
                cfg_dir.as_deref(),
            ) {
                Ok(valid_path) => {
                    let display_path = shrink_tilde(&valid_path);
                    if !valid_path.exists() {
                        let _ = writeln!(tree_out, "{display_path} (not found)");
                        if !args.tree_only {
                            files.push((
                                display_path,
                                FileOutcome::Error("File does not exist.".into()),
                            ));
                        }
                    } else if valid_path.is_dir() {
                        let _ = writeln!(tree_out, "{display_path}/");
                    } else {
                        let _ = writeln!(tree_out, "{display_path}");
                        if !args.tree_only {
                            let exceeds = valid_path
                                .metadata()
                                .is_ok_and(|m| m.len() > settings.max_size);
                            let outcome = if exceeds {
                                FileOutcome::Error("File exceeds maximum size limit.".into())
                            } else {
                                read_to_outcome(
                                    &valid_path,
                                    display_path.len(),
                                    &mut used_bytes,
                                    max_output_size,
                                )
                            };
                            files.push((display_path, outcome));
                        }
                    }
                }
                Err(sec_err) => {
                    let display_path = shrink_tilde(&expanded);
                    let _ = writeln!(tree_out, "{display_path} (blocked)");
                    if !args.tree_only {
                        files.push((display_path, FileOutcome::Error(sec_err)));
                    }
                }
            }
        }
    }

    let mut out = format!(
        "# Workspace: {workspace_name}\n\n## Directory Structure\n\n```text\n{}\n```\n\n",
        tree_out.trim_end()
    );

    if !files.is_empty() {
        out.push_str("## Files\n\n");
        for (path, outcome) in &files {
            let _ = writeln!(out, "### File: `{path}`\n");
            match outcome {
                FileOutcome::Error(err) => {
                    let _ = writeln!(out, "**Error:** {err}\n");
                }
                FileOutcome::Skipped(skip) => {
                    let _ = writeln!(out, "**Skipped:** {skip}\n");
                }
                FileOutcome::Content(content) => {
                    let lang = Path::new(path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("text");
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
    }

    Ok(out.trim_end().to_string())
}
