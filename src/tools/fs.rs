use crate::tools::utils::{
    auto_schema, ensure_within_workspace, has_git_dir_component, parse_args, resolve_existing_path,
    resolve_existing_under_root, resolve_new_path, workspace_root,
};
use anyhow::{Context, Result, bail};
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
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
    if has_git_dir_component(&rel) {
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

fn validate_transfer_paths(args: &FsArgs, verb: &str) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let (cwd, src) = resolve_existing_path(&args.path)?;
    if src == cwd {
        bail!("Cannot {verb} workspace root");
    }
    ensure_not_git_metadata(&src, &cwd)?;

    let (_, dst) = resolve_new_path(&args.destination)?;
    ensure_not_git_metadata(&dst, &cwd)?;
    if dst.starts_with(&src) {
        bail!("Cannot {verb} a directory into a subdirectory of itself");
    }
    Ok((cwd, src, dst))
}

fn ensure_dst_parent(dst: &Path, cwd: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).context("Error creating destination parent directories")?;
        ensure_within_workspace(parent, cwd, "destination parent")?;
    }
    Ok(())
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
    let mut stats = (0usize, 0usize, 0u64);
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            let (f, d, b) = count_dir_items(&entry.path());
            stats = (stats.0 + f, stats.1 + 1 + d, stats.2.saturating_add(b));
        } else if ft.is_file() {
            let len = entry.metadata().map_or(0, |m| m.len());
            stats = (stats.0 + 1, stats.1, stats.2.saturating_add(len));
        }
    }
    stats
}

pub fn preview_action(json_args: &Value) -> Result<(String, Option<(String, String)>)> {
    let args: FsArgs = parse_args(json_args, "fs")?;
    match args.action {
        FsAction::List => {
            let cwd = workspace_root()?;
            let target = resolve_existing_under_root(&cwd, &args.path, "path escapes workspace")?;
            Ok((format!("List directory: {}", target.display()), None))
        }
        FsAction::Rename | FsAction::Copy => {
            let is_rename = matches!(args.action, FsAction::Rename);
            let (_, src, _) =
                validate_transfer_paths(&args, if is_rename { "move" } else { "copy" })?;
            let kind = if src.is_dir() { "directory" } else { "file" };
            let title = if is_rename { "Move" } else { "Copy" };
            Ok((
                format!(
                    "{title} {kind} '{}' -> '{}'",
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
            let path_str = args.path.trim();

            if target.is_dir() {
                let (files, dirs, bytes) = count_dir_items(&target);
                if (files > 0 || dirs > 0) && !args.recursive {
                    bail!(
                        "Directory '{path_str}' is not empty ({files} files, {dirs} subdirs). Set `recursive: true` to remove it"
                    );
                }
                Ok((
                    format!(
                        "Remove directory '{path_str}' ({files} file(s), {dirs} subdir(s), {})",
                        format_size(bytes)
                    ),
                    None,
                ))
            } else {
                let size = target.metadata().map_or(0, |m| m.len());
                let diff_preview = (size <= 64 * 1024)
                    .then(|| {
                        fs::read_to_string(&target)
                            .ok()
                            .map(|c| (path_str.to_string(), c))
                    })
                    .flatten();
                Ok((
                    format!("Remove file '{path_str}' ({})", format_size(size)),
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
        let a_dir = a.file_type().is_ok_and(|t| t.is_dir());
        let b_dir = b.file_type().is_ok_and(|t| t.is_dir());
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.file_name().cmp(&b.file_name()))
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
        let Ok(meta) = entry.metadata() else { continue };
        *count += 1;

        if meta.is_dir() {
            let child_count = fs::read_dir(&path).map_or(0, Iterator::count);
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
    let src_str = args.path.trim();
    let dst_str = args.destination.trim();

    match args.action {
        FsAction::List => {
            let cwd = workspace_root()?;
            let target = resolve_existing_under_root(&cwd, src_str, "path escapes workspace")?;

            let rel_display = diff_paths(&target, &cwd)
                .filter(|p| !p.as_os_str().is_empty())
                .map_or_else(|| ".".into(), |p| p.to_string_lossy().into_owned());

            if target.is_file() {
                return Ok(format!(
                    "{rel_display} (file, {})",
                    format_size(target.metadata()?.len())
                ));
            }

            let max_depth = args.max_depth.unwrap_or(1).clamp(1, 5);
            let mut out =
                format!("Directory listing for '{rel_display}' (max_depth: {max_depth}):\n");
            let mut count = 0usize;
            list_dir_recursive(
                &target,
                &cwd,
                1,
                max_depth,
                &mut out,
                max_output_size,
                &mut count,
            )?;

            if count == 0 {
                Ok(format!("Directory '{rel_display}' is empty."))
            } else {
                Ok(out.trim_end().to_string())
            }
        }
        FsAction::Rename => {
            let (cwd, src, dst) = validate_transfer_paths(&args, "move")?;
            ensure_dst_parent(&dst, &cwd)?;
            fs::rename(&src, &dst)
                .with_context(|| format!("Failed to rename '{src_str}' to '{dst_str}'"))?;
            Ok(format!("Successfully moved '{src_str}' to '{dst_str}'"))
        }
        FsAction::Copy => {
            let (cwd, src, dst) = validate_transfer_paths(&args, "copy")?;
            ensure_dst_parent(&dst, &cwd)?;
            if src.is_dir() {
                let count = copy_dir_recursive(&src, &dst, &cwd)?;
                Ok(format!(
                    "Successfully copied directory '{src_str}' to '{dst_str}' ({count} file(s))"
                ))
            } else {
                let bytes = fs::copy(&src, &dst)
                    .with_context(|| format!("Failed to copy '{src_str}' to '{dst_str}'"))?;
                Ok(format!(
                    "Successfully copied '{src_str}' to '{dst_str}' ({})",
                    format_size(bytes)
                ))
            }
        }
        FsAction::Remove => {
            let (cwd, target) = resolve_existing_path(src_str)?;
            if target == cwd {
                bail!("Cannot remove workspace root");
            }
            ensure_not_git_metadata(&target, &cwd)?;

            if target.is_dir() {
                if fs::read_dir(&target)?.next().is_some() && !args.recursive {
                    bail!(
                        "Directory '{src_str}' is not empty. Pass `recursive: true` to delete non-empty directories."
                    );
                }
                let res = if args.recursive {
                    fs::remove_dir_all(&target)
                } else {
                    fs::remove_dir(&target)
                };
                res.with_context(|| format!("Failed to remove directory '{src_str}'"))?;
                Ok(format!("Successfully removed directory '{src_str}'"))
            } else {
                fs::remove_file(&target)
                    .with_context(|| format!("Failed to remove file '{src_str}'"))?;
                Ok(format!("Successfully removed file '{src_str}'"))
            }
        }
    }
}
