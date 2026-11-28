use super::{
    BlockingTool, ToolContext,
    utils::{
        fs::{format_size, render_directory_listing},
        path::{ensure_parent_dir, has_git_dir_component},
    },
};
use anyhow::{Context, Result, bail};
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct Tool;

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
pub struct FsArgs {
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

fn ensure_not_git_metadata(path: &Path, cwd: &Path) -> Result<()> {
    let rel = diff_paths(path, cwd).unwrap_or_else(|| path.to_path_buf());
    if has_git_dir_component(&rel) {
        bail!("Security violation: cannot modify or delete '.git' repository metadata");
    }
    Ok(())
}

fn ensure_dir_children_allowed(
    src_dir: &Path,
    dst_dir: Option<&Path>,
    check_symlinks: bool,
    ctx: &ToolContext,
) -> Result<()> {
    let (exclude_set, neg_exclude_set) = ctx.build_exclude_globsets(&[], true)?;

    fn walk(
        src: &Path,
        dst: Option<&Path>,
        check_symlinks: bool,
        ctx: &ToolContext,
        exclude_set: &globset::GlobSet,
        neg_exclude_set: &globset::GlobSet,
    ) -> Result<()> {
        for entry in fs::read_dir(src)
            .with_context(|| format!("Failed to read directory {}", src.display()))?
        {
            let entry = entry?;
            let ft = entry.file_type()?;
            let src_path = entry.path();
            if ft.is_symlink() && check_symlinks {
                bail!(
                    "Security violation: copying symlinks is not supported ({})",
                    src_path.display()
                );
            }
            let src_rel = diff_paths(&src_path, &ctx.root).unwrap_or_else(|| src_path.clone());
            ctx.check_rel_path_allowed(&src_rel, exclude_set, neg_exclude_set)?;

            let dst_child = dst.map(|d| d.join(entry.file_name()));
            if let Some(ref dp) = dst_child {
                let dst_rel = diff_paths(dp, &ctx.root).unwrap_or_else(|| dp.clone());
                ctx.check_rel_path_allowed(&dst_rel, exclude_set, neg_exclude_set)?;
            }

            if ft.is_dir() {
                let canon_src = dunce::canonicalize(&src_path).unwrap_or_else(|_| src_path.clone());
                ctx.ensure_within_workspace(&canon_src, "directory entry")?;
                let canon_rel =
                    diff_paths(&canon_src, &ctx.root).unwrap_or_else(|| canon_src.clone());
                ctx.check_rel_path_allowed(&canon_rel, exclude_set, neg_exclude_set)?;
                walk(
                    &src_path,
                    dst_child.as_deref(),
                    check_symlinks,
                    ctx,
                    exclude_set,
                    neg_exclude_set,
                )?;
            }
        }
        Ok(())
    }

    walk(
        src_dir,
        dst_dir,
        check_symlinks,
        ctx,
        &exclude_set,
        &neg_exclude_set,
    )
}

fn validate_transfer_paths(
    args: &FsArgs,
    ctx: &ToolContext,
    verb: &str,
) -> Result<(PathBuf, PathBuf)> {
    let src = ctx.resolve_existing_path(&args.path)?;
    if src == ctx.root {
        bail!("Cannot {verb} workspace root");
    }
    ensure_not_git_metadata(&src, &ctx.root)?;

    let dst = ctx.resolve_new_path(&args.destination)?;
    ensure_not_git_metadata(&dst, &ctx.root)?;
    if dst.starts_with(&src) {
        bail!("Cannot {verb} a directory into a subdirectory of itself");
    }
    if src.is_dir() {
        let is_copy = matches!(args.action, FsAction::Copy);
        ensure_dir_children_allowed(&src, Some(&dst), is_copy, ctx)?;
    }
    Ok((src, dst))
}

fn copy_dir_recursive(src: &Path, dst: &Path, ctx: &ToolContext) -> Result<usize> {
    fs::create_dir_all(dst)
        .with_context(|| format!("Failed to create directory {}", dst.display()))?;
    ctx.ensure_within_workspace(dst, "copied directory")?;

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
        ctx.ensure_not_excluded(&src_path)?;
        let dst_path = dst.join(entry.file_name());
        ctx.ensure_not_excluded(&dst_path)?;
        if file_type.is_dir() {
            copied_files += copy_dir_recursive(&src_path, &dst_path, ctx)?;
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

fn resolve_remove_target(raw: &str, ctx: &ToolContext) -> Result<PathBuf> {
    let target = ctx.resolve_existing_path(raw)?;
    if target == ctx.root {
        bail!("Cannot remove workspace root");
    }
    ensure_not_git_metadata(&target, &ctx.root)?;
    if target.is_dir() {
        ensure_dir_children_allowed(&target, None, false, ctx)?;
    }
    Ok(target)
}

pub fn preview_action(
    args: &FsArgs,
    ctx: &ToolContext,
) -> Result<(String, Option<(String, String)>)> {
    match args.action {
        FsAction::List => {
            let target = ctx.resolve_existing_under_root(&args.path, "path escapes workspace")?;
            Ok((format!("List directory: {}", target.display()), None))
        }
        FsAction::Rename | FsAction::Copy => {
            let is_rename = matches!(args.action, FsAction::Rename);
            let (src, _) =
                validate_transfer_paths(args, ctx, if is_rename { "move" } else { "copy" })?;
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
            let target = resolve_remove_target(&args.path, ctx)?;
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

impl BlockingTool for Tool {
    const NAME: &'static str = "fs";
    const DESCRIPTION: &'static str = "Performs workspace filesystem operations: list directory contents, rename/move paths, copy files/directories, or remove files/directories.";
    const SUBACTIONS: &'static [&'static str] = &["list", "rename", "copy", "remove"];
    const PARAM_HINT: &'static str = "<list|rename|copy|remove> [path] [destination]";
    const SHORT_SUMMARY: &'static str =
        "List directory contents or rename, copy, and remove files/directories in the workspace.";
    type Args = FsArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let src_str = args.path.trim();
        let dst_str = args.destination.trim();

        match args.action {
            FsAction::List => {
                let target = ctx.resolve_existing_under_root(src_str, "path escapes workspace")?;
                let rel_display = diff_paths(&target, &ctx.root)
                    .filter(|p| !p.as_os_str().is_empty())
                    .map_or_else(|| ".".into(), |p| p.to_string_lossy().into_owned());
                let max_depth = args.max_depth.unwrap_or(1).clamp(1, 5);
                let (exclude_set, neg_exclude_set) = ctx.build_exclude_globsets(&[], true)?;
                let neg_exclude_ancestors = ctx.build_neg_exclude_ancestor_globset()?;
                render_directory_listing(
                    &target,
                    &ctx.root,
                    &rel_display,
                    max_depth,
                    ctx.max_output_size,
                    |path| {
                        let rel = diff_paths(path, &ctx.root).unwrap_or_else(|| path.to_path_buf());
                        if path.is_dir() && neg_exclude_ancestors.is_match(&rel) {
                            return true;
                        }
                        ctx.check_rel_path_allowed(&rel, &exclude_set, &neg_exclude_set)
                            .is_ok()
                    },
                )
            }
            FsAction::Rename => {
                let (src, dst) = validate_transfer_paths(&args, ctx, "move")?;
                ensure_parent_dir(&dst, &ctx.root, "destination parent")?;
                fs::rename(&src, &dst)
                    .with_context(|| format!("Failed to rename '{src_str}' to '{dst_str}'"))?;
                Ok(format!("Successfully moved '{src_str}' to '{dst_str}'"))
            }
            FsAction::Copy => {
                let (src, dst) = validate_transfer_paths(&args, ctx, "copy")?;
                ensure_parent_dir(&dst, &ctx.root, "destination parent")?;
                if src.is_dir() {
                    let count = copy_dir_recursive(&src, &dst, ctx)?;
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
                let target = resolve_remove_target(src_str, ctx)?;

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
}
