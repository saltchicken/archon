use super::paths::{validate_relative_path, workspace_root};
use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder};
use pathdiff::diff_paths;
use std::{
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
};

pub const DEFAULT_EXCLUDE_PATTERNS: &[&str] = &[
    "**/.git/**",
    "**/*.venv/**",
    "**/node_modules/**",
    "**/target/**",
    "**/.env*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/secrets.json",
];

pub fn find_git_root(start_path: &Path) -> Option<PathBuf> {
    start_path
        .ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Resolves `(base_root, scan_root)` for workspace-scanning tools (`repo_map`, `search`, `workspace_snapshot`).
pub fn resolve_scan_roots(
    use_git_root: bool,
    target_path: &str,
    escape_label: &str,
) -> Result<(PathBuf, PathBuf)> {
    let cwd = workspace_root()?;
    let base_root = if use_git_root {
        find_git_root(&cwd).unwrap_or(cwd)
    } else {
        cwd
    };
    let base_root = dunce::canonicalize(&base_root).unwrap_or(base_root);

    let trimmed = target_path.trim();
    if trimmed.is_empty() || trimmed == "." {
        return Ok((base_root.clone(), base_root));
    }

    let rel = validate_relative_path(trimmed)?;
    let candidate = base_root.join(rel);
    if !candidate.exists() {
        bail!("Target path does not exist: '{trimmed}'");
    }
    let canon = dunce::canonicalize(&candidate)?;
    if !canon.starts_with(&base_root) {
        bail!("Security violation: {escape_label}");
    }
    Ok((base_root, canon))
}

/// Builds a configured `ignore::Walk` iterator over `scan_root`.
pub fn build_workspace_walker(
    scan_root: &Path,
    use_git_ignore: bool,
) -> impl Iterator<Item = DirEntry> {
    WalkBuilder::new(scan_root)
        .hidden(false)
        .git_ignore(use_git_ignore)
        .git_global(use_git_ignore)
        .git_exclude(use_git_ignore)
        .build()
        .flatten()
}

/// Builds a `GlobSet`. When `allow_recursive_shorthand` is true, bare patterns without `/` or `\`
/// also match recursively via `**/<pattern>`.
pub fn build_globset(patterns: &[String], allow_recursive_shorthand: bool) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(Glob::new(pat).with_context(|| format!("Invalid glob pattern: {pat}"))?);
        if allow_recursive_shorthand && !pat.contains(['/', '\\']) {
            let recursive = format!("**/{pat}");
            builder.add(
                Glob::new(&recursive)
                    .with_context(|| format!("Invalid glob pattern: {recursive}"))?,
            );
        }
    }
    Ok(builder.build()?)
}

/// Builds the combined exclude `GlobSet` merging `DEFAULT_EXCLUDE_PATTERNS`, config excludes, and per-call excludes.
pub fn build_exclude_globset(
    config_exclude: &[String],
    call_exclude: &[String],
    allow_recursive_shorthand: bool,
) -> Result<GlobSet> {
    let mut patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
        .iter()
        .map(|&s| String::from(s))
        .collect();
    patterns.extend_from_slice(config_exclude);
    patterns.extend_from_slice(call_exclude);
    build_globset(&patterns, allow_recursive_shorthand)
}

/// Walks `scan_root` (handling both single-file and directory targets) and returns `(abs_path, rel_display)`
/// pairs filtered by `include` and `exclude` globsets up to `max_files`.
pub fn walk_workspace_files(
    base_root: &Path,
    scan_root: &Path,
    use_git_ignore: bool,
    include_patterns: &[String],
    config_exclude: &[String],
    call_exclude: &[String],
    max_files: usize,
) -> Result<Vec<(PathBuf, String)>> {
    let include_set = (!include_patterns.is_empty())
        .then(|| build_globset(include_patterns, true))
        .transpose()?;
    let exclude_set = build_exclude_globset(config_exclude, call_exclude, true)?;

    let mut files = Vec::new();
    if scan_root.is_file() {
        let rel = diff_paths(scan_root, base_root).unwrap_or_else(|| scan_root.to_path_buf());
        if exclude_set.is_match(&rel) {
            bail!(
                "Security violation: '{}' matches an excluded pattern",
                rel.display()
            );
        }
        files.push((scan_root.to_path_buf(), rel.to_string_lossy().into_owned()));
        return Ok(files);
    }

    for entry in build_workspace_walker(scan_root, use_git_ignore) {
        if files.len() >= max_files {
            break;
        }
        let path = entry.path();
        if !path.is_file() || path.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let Some(relative) = diff_paths(path, base_root) else {
            continue;
        };
        if exclude_set.is_match(&relative)
            || include_set
                .as_ref()
                .is_some_and(|inc| !inc.is_match(&relative))
        {
            continue;
        }
        files.push((path.to_path_buf(), relative.to_string_lossy().into_owned()));
    }

    files.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(files)
}

pub fn is_binary_bytes(data: &[u8]) -> bool {
    if data.contains(&0) {
        return true;
    }
    let control_chars = data
        .iter()
        .filter(|&&b| b < 32 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0C))
        .count();
    (control_chars as f32 / data.len() as f32) > 0.3
}

pub fn read_utf8_capped(path: &Path, max_file_size: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
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
