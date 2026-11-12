use crate::tools::{
    auto_schema, ensure_within_workspace, parse_args, resolve_existing_path, resolve_new_path,
    validate_relative_path, workspace_root,
};
use anyhow::{bail, Context, Result};
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{
    fmt::Write as _,
    fs,
    path::Path,
};

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsAction {
    /// List files and directories at `path` (defaults to workspace root) with sizes and entry counts.
    List,
    /// Move or rename a file or directory from `path` to `destination` within the workspace.
    Rename,
    /// Copy a file or directory from `path` to `destination` within the workspace.
    Copy,
    /// Remove a file or directory at `path` within the workspace. Non-empty directories require `recursive: true`.
    Remove,
}

#[derive(Deserialize, JsonSchema)]
struct FsArgs {
    /// The filesystem operation to perform.
    action: FsAction,
    /// Relative source path in the workspace (defaults to "." when `action` is `list`; required for `rename`, `copy`, and `remove`).
    #[serde(default)]
    path: String,
    /// Relative destination path in the workspace (required when `action` is `rename` or `copy`).
    #[serde(default)]
    destination: String,
    /// When `action` is `list`, maximum directory traversal depth (default: 1, max: 5).
    #[serde(default)]
    max_depth: Option<usize>,
    /// When `action` is `remove`, set to `true` to allow deleting a non-empty directory recursively.
    #[serde(default)]
    recursive: bool,
}

pub fn schema() -> Value {
    auto_schema::<FsArgs>(
        "fs",
        "Performs workspace filesystem operations: list directory contents, rename/move paths, copy files/directories, or remove files/directories.",
    )
}

fn ensure_not_git_metadata(path: &Path, cwd: &Path) -> Result<()> {
    let rel = diff_paths(path, cwd).unwrap_or_else(|| path.to_path_buf());
    if rel.components().any(|c| c.as_os_str() == ".git") {
        bail!("Security violation: cannot modify or delete '.git' repository metadata");
    }
    Ok(())
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path, cwd: &Path) -> Result<usize> {
    fs::create_dir_all(dst)
        .with_context(|| format!("Failed to create directory {}", dst.display()))?;
    ensure_within_workspace(dst, cwd, "copied directory")?;

    let mut copied_files = 0usize;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "Security violation: copying symlinks is not supported ({})",
                entry.path().display()
            );
        }
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copied_files += copy_dir_recursive(&src_path, &dst_path, cwd)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
            copied_files += 1;
        }
    }
    Ok(copied_files)
}

fn count_dir_items(dir: &Path) -> (usize, usize, u64) {
    let mut files = 0usize;
    let mut dirs = 0usize;
    let mut total_bytes = 0u64;

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                dirs += 1;
                let (f, d, b) = count_dir_items(&entry.path());
                files += f;
                dirs += d;
                total_bytes = total_bytes.saturating_add(b);
            } else if ft.is_file() {
                files += 1;
                if let Ok(meta) = entry.metadata() {
                    total_bytes = total_bytes.saturating_add(meta.len());
                }
            }
        }
    }
    (files, dirs, total_bytes)
}

/// Validates the requested `fs` operation without mutating disk so the approval modal
/// can display an accurate summary or warning if the call will fail.
pub fn preview_action(json_args: &Value) -> Result<(String, Option<(String, String)>)> {
    let args: FsArgs = parse_args(json_args, "fs")?;

    match args.action {
        FsAction::List => {
            let raw = if args.path.trim().is_empty() {
                "."
            } else {
                args.path.trim()
            };
            let (_, target) = resolve_existing_path(raw)?;
            Ok((format!("List directory: {}", target.display()), None))
        }
        FsAction::Rename => {
            let (cwd, src) = resolve_existing_path(&args.path)?;
            if src == cwd {
                bail!("Cannot rename workspace root");
            }
            ensure_not_git_metadata(&src, &cwd)?;
            let (_, dst) = resolve_new_path(&args.destination)?;
            ensure_not_git_metadata(&dst, &cwd)?;
            if dst.starts_with(&src) {
                bail!("Cannot move a directory into a subdirectory of itself");
            }
            let kind = if src.is_dir() { "directory" } else { "file" };
            Ok((
                format!(
                    "Move {kind} '{}' -> '{}'",
                    args.path.trim(),
                    args.destination.trim()
                ),
                None,
            ))
        }
        FsAction::Copy => {
            let (cwd, src) = resolve_existing_path(&args.path)?;
            if src == cwd {
                bail!("Cannot copy workspace root");
            }
            ensure_not_git_metadata(&src, &cwd)?;
            let (_, dst) = resolve_new_path(&args.destination)?;
            ensure_not_git_metadata(&dst, &cwd)?;
            if dst.starts_with(&src) {
                bail!("Cannot copy a directory into a subdirectory of itself");
            }
            let kind = if src.is_dir() { "directory" } else { "file" };
            Ok((
                format!(
                    "Copy {kind} '{}' -> '{}'",
                    args.path.trim(),
                    args.destination.trim()
                ),
                None,
            ))
        }
        FsAction::Remove => {
            let (cwd, target) = resolve_existing_path(&args.path)?;
            if target == cwd {
                bail!("Cannot remove workspace root");
            }
            ensure_not_git_metadata(&target, &cwd)?;

            if target.is_dir() {
                let (files, dirs, bytes) = count_dir_items(&target);
                if (files > 0 || dirs > 0) && !args.recursive {
                    bail!(
                        "Directory '{}' is not empty ({files} files, {dirs} subdirs). Set `recursive: true` to remove it",
                        args.path.trim()
                    );
                }
                Ok((
                    format!(
                        "Remove directory '{}' ({files} file(s), {dirs} subdir(s), {})",
                        args.path.trim(),
                        format_size(bytes)
                    ),
                    None,
                ))
            } else {
                let size = target.metadata().map(|m| m.len()).unwrap_or(0);
                let diff_preview = if size <= 64 * 1024 {
                    fs::read_to_string(&target)
                        .ok()
                        .map(|content| (args.path.trim().to_string(), content))
                } else {
                    None
                };
                Ok((
                    format!("Remove file '{}' ({})", args.path.trim(), format_size(size)),
                    diff_preview,
                ))
            }
        }
    }
}

fn list_dir_recursive(
    dir: &Path,
    base_root: &Path,
    current_depth: usize,
    max_depth: usize,
    out: &mut String,
    max_output_size: usize,
    count: &mut usize,
) -> Result<()> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory {}", dir.display()))?
        .flatten()
        .collect();

    entries.sort_by(|a, b| {
        let a_dir = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let b_dir = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
        b_dir.cmp(&a_dir).then_with(|| a.file_name().cmp(&b.file_name()))
    });

    let indent = "  ".repeat(current_depth);
    for entry in entries {
        if out.len() > max_output_size {
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        *count += 1;

        if meta.is_dir() {
            let child_count = fs::read_dir(&path).map(|rd| rd.count()).unwrap_or(0);
            let _ = writeln!(out, "{indent}{name}/ ({child_count} items)");
            if current_depth < max_depth && path.starts_with(base_root) {
                list_dir_recursive(
                    &path,
                    base_root,
                    current_depth + 1,
                    max_depth,
                    out,
                    max_output_size,
                    count,
                )?;
            }
        } else {
            let _ = writeln!(out, "{indent}{name} ({})", format_size(meta.len()));
        }
    }
    Ok(())
}

pub fn execute(json_args: &Value, max_output_size: usize) -> Result<String> {
    let args: FsArgs = parse_args(json_args, "fs")?;

    match args.action {
        FsAction::List => {
            let cwd = workspace_root()?;
            let target = if args.path.trim().is_empty() || args.path.trim() == "." {
                cwd.clone()
            } else {
                let rel = validate_relative_path(&args.path)?;
                let candidate = cwd.join(rel);
                if !candidate.exists() {
                    bail!("Target path does not exist: '{}'", args.path.trim());
                }
                ensure_within_workspace(&candidate, &cwd, &args.path)?
            };

            let rel_display = diff_paths(&target, &cwd)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".to_string());

            if target.is_file() {
                let len = target.metadata()?.len();
                return Ok(format!("{rel_display} (file, {})", format_size(len)));
            }

            let max_depth = args.max_depth.unwrap_or(1).clamp(1, 5);
            let mut out = format!("Directory listing for '{rel_display}' (max_depth: {max_depth}):\n");
            let mut count = 0usize;
            list_dir_recursive(&target, &cwd, 1, max_depth, &mut out, max_output_size, &mut count)?;

            if count == 0 {
                return Ok(format!("Directory '{rel_display}' is empty."));
            }
            Ok(out.trim_end().to_string())
        }
        FsAction::Rename => {
            let (cwd, src) = resolve_existing_path(&args.path)?;
            if src == cwd {
                bail!("Cannot rename workspace root");
            }
            ensure_not_git_metadata(&src, &cwd)?;

            let (_, dst) = resolve_new_path(&args.destination)?;
            ensure_not_git_metadata(&dst, &cwd)?;
            if dst.starts_with(&src) {
                bail!("Cannot move a directory into a subdirectory of itself");
            }

            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).context("Error creating destination parent directories")?;
                ensure_within_workspace(parent, &cwd, "destination parent")?;
            }

            fs::rename(&src, &dst).with_context(|| {
                format!(
                    "Failed to rename '{}' to '{}'",
                    args.path.trim(),
                    args.destination.trim()
                )
            })?;

            Ok(format!(
                "Successfully moved '{}' to '{}'",
                args.path.trim(),
                args.destination.trim()
            ))
        }
        FsAction::Copy => {
            let (cwd, src) = resolve_existing_path(&args.path)?;
            if src == cwd {
                bail!("Cannot copy workspace root");
            }
            ensure_not_git_metadata(&src, &cwd)?;

            let (_, dst) = resolve_new_path(&args.destination)?;
            ensure_not_git_metadata(&dst, &cwd)?;
            if dst.starts_with(&src) {
                bail!("Cannot copy a directory into a subdirectory of itself");
            }

            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).context("Error creating destination parent directories")?;
                ensure_within_workspace(parent, &cwd, "destination parent")?;
            }

            if src.is_dir() {
                let count = copy_dir_recursive(&src, &dst, &cwd)?;
                Ok(format!(
                    "Successfully copied directory '{}' to '{}' ({count} file(s))",
                    args.path.trim(),
                    args.destination.trim()
                ))
            } else {
                let bytes = fs::copy(&src, &dst).with_context(|| {
                    format!(
                        "Failed to copy '{}' to '{}'",
                        args.path.trim(),
                        args.destination.trim()
                    )
                })?;
                Ok(format!(
                    "Successfully copied '{}' to '{}' ({})",
                    args.path.trim(),
                    args.destination.trim(),
                    format_size(bytes)
                ))
            }
        }
        FsAction::Remove => {
            let (cwd, target) = resolve_existing_path(&args.path)?;
            if target == cwd {
                bail!("Cannot remove workspace root");
            }
            ensure_not_git_metadata(&target, &cwd)?;

            if target.is_dir() {
                let is_non_empty = fs::read_dir(&target)?.next().is_some();
                if is_non_empty && !args.recursive {
                    bail!(
                        "Directory '{}' is not empty. Pass `recursive: true` to delete non-empty directories.",
                        args.path.trim()
                    );
                }
                if args.recursive {
                    fs::remove_dir_all(&target).with_context(|| {
                        format!("Failed to recursively remove directory '{}'", args.path.trim())
                    })?;
                } else {
                    fs::remove_dir(&target).with_context(|| {
                        format!("Failed to remove empty directory '{}'", args.path.trim())
                    })?;
                }
                Ok(format!("Successfully removed directory '{}'", args.path.trim()))
            } else {
                fs::remove_file(&target)
                    .with_context(|| format!("Failed to remove file '{}'", args.path.trim()))?;
                Ok(format!("Successfully removed file '{}'", args.path.trim()))
            }
        }
    }
}
