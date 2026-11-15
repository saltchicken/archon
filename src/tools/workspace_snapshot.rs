use super::{
    BlockingTool, ToolContext,
    utils::{
        fs::{FileOutcome, read_text_file_inspected},
        path::{build_globset, has_git_dir_component},
    },
};
use crate::core::config::{archon_config_root, expand_tilde, format_tilde_path};
use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    collections::HashSet,
    fmt::Write as _,
    path::{Component, Path, PathBuf},
};

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct WorkspaceSnapshotArgs {
    /// If true, only output the workspace directory tree without reading or dumping file contents.
    #[serde(default)]
    pub tree_only: Option<bool>,
}

struct FileEntry {
    path: PathBuf,
    display_path: String,
    depth: usize,
    is_dir: bool,
    include_content: bool,
}

fn shrink_tilde(path: &Path) -> String {
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    format_tilde_path(&canonical)
}

fn read_to_outcome(
    path: &Path,
    display_len: usize,
    max_file_size: u64,
    used_bytes: &mut usize,
    max_output_size: usize,
) -> FileOutcome {
    if *used_bytes > max_output_size {
        return FileOutcome::Skipped("Workspace snapshot output size limit reached.");
    }
    let outcome = read_text_file_inspected(path, max_file_size);
    if let FileOutcome::Content(ref content) = outcome
        && !content.is_empty()
    {
        *used_bytes = used_bytes.saturating_add(content.len() + display_len + 32);
    }
    outcome
}

fn get_fence(content: &str) -> String {
    let max_backticks = content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(max_backticks.max(2) + 1)
}

fn archon_config_dir() -> Option<PathBuf> {
    let dir = archon_config_root();
    dunce::canonicalize(&dir).ok().or(Some(dir))
}

trait PartitionPatterns<'a>: Iterator<Item = &'a str> + Sized {
    fn partition_map_pat(self, normalize: &impl Fn(&str) -> String) -> (Vec<String>, Vec<String>) {
        let mut neg = Vec::new();
        let mut pos = Vec::new();
        for pat in self {
            if let Some(stripped) = pat.strip_prefix('!') {
                let s = normalize(stripped.trim());
                if !s.is_empty() {
                    neg.push(s);
                }
            } else {
                let s = normalize(pat);
                if !s.is_empty() {
                    pos.push(s);
                }
            }
        }
        (neg, pos)
    }
}

impl<'a, I: Iterator<Item = &'a str>> PartitionPatterns<'a> for I {}

fn has_glob_meta(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

fn split_static_glob_prefix(path: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(os) => {
                if os.to_str().is_some_and(has_glob_meta) {
                    break;
                }
                prefix.push(os);
            }
            _ => prefix.push(comp.as_os_str()),
        }
    }
    if prefix.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        prefix
    }
}

fn expand_extra_entry(
    raw_file: &str,
    base_root: &Path,
) -> std::result::Result<Vec<PathBuf>, String> {
    let expanded = expand_tilde(raw_file);
    let candidate = if expanded.is_relative() {
        base_root.join(&expanded)
    } else {
        expanded
    };

    let candidate_str = candidate.to_string_lossy();
    if !has_glob_meta(&candidate_str) {
        return Ok(vec![candidate]);
    }

    let search_root = split_static_glob_prefix(&candidate);
    if !search_root.exists() {
        return Ok(Vec::new());
    }
    let canon_root = dunce::canonicalize(&search_root).unwrap_or_else(|_| search_root.clone());

    let rel_pattern = candidate
        .strip_prefix(&search_root)
        .unwrap_or(&candidate)
        .to_string_lossy();
    let full_canon_pattern = canon_root
        .join(rel_pattern.as_ref())
        .to_string_lossy()
        .into_owned();

    let mut builder = GlobSetBuilder::new();
    builder.add(
        Glob::new(&candidate_str).map_err(|e| format!("Invalid glob pattern '{raw_file}': {e}"))?,
    );
    if full_canon_pattern != candidate_str
        && let Ok(g) = Glob::new(&full_canon_pattern)
    {
        builder.add(g);
    }
    let matcher = builder
        .build()
        .map_err(|e| format!("Invalid glob pattern '{raw_file}': {e}"))?;

    let mut matched = Vec::new();
    for entry in WalkBuilder::new(&canon_root)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .build()
        .flatten()
    {
        let p = entry.path();
        if !p.is_file() || has_git_dir_component(p) {
            continue;
        }
        if matcher.is_match(p) {
            matched.push(p.to_path_buf());
        }
    }

    matched.sort();
    matched.dedup();
    Ok(matched)
}

fn validate_extra_file(
    raw_file: &str,
    candidate: &Path,
    base_root: &Path,
    exclude_set: &GlobSet,
    trusted_global_files: &HashSet<PathBuf>,
    trusted_global_globs: &GlobSet,
    config_dir: Option<&Path>,
) -> std::result::Result<PathBuf, String> {
    let canon = dunce::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());

    if trusted_global_files.contains(&canon)
        || trusted_global_files.contains(candidate)
        || trusted_global_globs.is_match(&canon)
        || trusted_global_globs.is_match(candidate)
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
        "Security violation: local .archon/config.toml cannot read external file '{raw_file}' outside the workspace."
    ))
}

impl BlockingTool for Tool {
    const NAME: &'static str = "workspace_snapshot";
    const DESCRIPTION: &'static str = "Generates a workspace directory tree and optionally dumps file contents as configured in config.toml (pass `tree_only: true` to only show the directory tree). For navigating medium-to-large codebases with code signatures, prefer `repo_map`.";
    type Args = WorkspaceSnapshotArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let settings = &ctx.config.workspace_snapshot;
        let tree_only = args.tree_only.unwrap_or(settings.tree_only);
        let max_output_size = ctx.max_output_size;
        let (base_root, scan_root) =
            ctx.resolve_scan_roots("", "target_path escapes workspace root")?;

        let workspace_name = base_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let include_patterns = if !settings.include.is_empty() {
            settings.include.clone()
        } else {
            vec!["**/*".into()]
        };

        let normalize_pat = |p: &str| p.trim_start_matches('/').to_string();

        let (neg_include, pos_include): (Vec<String>, Vec<String>) = include_patterns
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .partition_map_pat(&normalize_pat);

        let (neg_exclude, pos_exclude): (Vec<String>, Vec<String>) = settings
            .exclude
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .partition_map_pat(&normalize_pat);

        let (neg_tree, pos_tree): (Vec<String>, Vec<String>) = settings
            .include_in_tree
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .partition_map_pat(&normalize_pat);

        let include_set = build_globset(&pos_include, false)?;
        let neg_include_set = build_globset(&neg_include, false)?;
        let exclude_set = ctx.build_exclude_globset(&pos_exclude, false)?;
        let neg_exclude_set = build_globset(&neg_exclude, false)?;
        let tree_only_set = build_globset(&pos_tree, false)?;
        let neg_tree_only_set = build_globset(&neg_tree, false)?;

        let mut entries = Vec::new();
        for entry in ctx.build_walker(&scan_root) {
            if entries.len() >= settings.max_files {
                break;
            }
            let path = entry.path();
            if path == base_root || has_git_dir_component(path) {
                continue;
            }
            let Some(relative) = diff_paths(path, &base_root).filter(|r| !r.as_os_str().is_empty())
            else {
                continue;
            };

            let matches_tree =
                tree_only_set.is_match(&relative) && !neg_tree_only_set.is_match(&relative);
            let is_excluded =
                exclude_set.is_match(&relative) && !neg_exclude_set.is_match(&relative);
            if is_excluded && !matches_tree {
                continue;
            }

            let Ok(metadata) = path.metadata() else {
                continue;
            };
            let is_dir = metadata.is_dir();
            let matches_include =
                include_set.is_match(&relative) && !neg_include_set.is_match(&relative);
            if !is_dir && !matches_include && !matches_tree {
                continue;
            }

            entries.push(FileEntry {
                path: path.to_path_buf(),
                display_path: relative.to_string_lossy().into_owned(),
                depth: relative.components().count(),
                is_dir,
                include_content: !is_dir && matches_include && !matches_tree,
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

        if !tree_only {
            for entry in entries.iter().filter(|e| e.include_content) {
                let outcome = read_to_outcome(
                    &entry.path,
                    entry.display_path.len(),
                    settings.max_size,
                    &mut used_bytes,
                    max_output_size,
                );
                files.push((entry.display_path.clone(), outcome));
            }
        }

        let mut trusted_global_files = HashSet::new();
        let mut global_glob_builder = GlobSetBuilder::new();
        for f in &settings.global_files {
            let exp = expand_tilde(f);
            let abs = if exp.is_relative() {
                base_root.join(&exp)
            } else {
                exp
            };
            let abs_str = abs.to_string_lossy();
            if has_glob_meta(&abs_str) {
                if let Ok(g) = Glob::new(&abs_str) {
                    global_glob_builder.add(g);
                }
                let prefix = split_static_glob_prefix(&abs);
                if let Ok(canon_prefix) = dunce::canonicalize(&prefix)
                    && let Ok(rel) = abs.strip_prefix(&prefix)
                    && let Ok(g) = Glob::new(&canon_prefix.join(rel).to_string_lossy())
                {
                    global_glob_builder.add(g);
                }
            } else {
                let canon = dunce::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
                trusted_global_files.insert(abs);
                trusted_global_files.insert(canon);
            }
        }
        let trusted_global_globs = global_glob_builder
            .build()
            .unwrap_or_else(|_| GlobSet::empty());

        let cfg_dir = archon_config_dir();
        let mut seen: HashSet<PathBuf> = entries
            .iter()
            .map(|e| dunce::canonicalize(&e.path).unwrap_or_else(|_| e.path.clone()))
            .collect();

        let mut record_extra = |display: String, marker: &str, outcome: Option<FileOutcome>| {
            let _ = writeln!(tree_out, "{display}{marker}");
            if let Some(o) = outcome {
                files.push((display, o));
            }
        };

        for raw_file in &settings.file {
            let candidates = match expand_extra_entry(raw_file, &base_root) {
                Ok(list) => list,
                Err(glob_err) => {
                    record_extra(
                        raw_file.clone(),
                        " (invalid glob)",
                        (!tree_only).then_some(FileOutcome::Error(glob_err)),
                    );
                    continue;
                }
            };

            for candidate in candidates {
                let dedup_key =
                    dunce::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
                if !seen.insert(dedup_key) {
                    continue;
                }

                match validate_extra_file(
                    raw_file,
                    &candidate,
                    &base_root,
                    &exclude_set,
                    &trusted_global_files,
                    &trusted_global_globs,
                    cfg_dir.as_deref(),
                ) {
                    Ok(valid_path) => {
                        let display_path = shrink_tilde(&valid_path);
                        if !valid_path.exists() {
                            record_extra(
                                display_path,
                                " (not found)",
                                (!tree_only)
                                    .then(|| FileOutcome::Error("File does not exist.".into())),
                            );
                        } else if valid_path.is_dir() {
                            record_extra(display_path, "/", None);
                        } else if tree_only {
                            record_extra(display_path, "", None);
                        } else {
                            let outcome = read_to_outcome(
                                &valid_path,
                                display_path.len(),
                                settings.max_size,
                                &mut used_bytes,
                                max_output_size,
                            );
                            record_extra(display_path, "", Some(outcome));
                        }
                    }
                    Err(sec_err) => {
                        record_extra(
                            shrink_tilde(&candidate),
                            " (blocked)",
                            (!tree_only).then_some(FileOutcome::Error(sec_err)),
                        );
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
}
