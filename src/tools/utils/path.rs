use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
};

pub fn workspace_root() -> Result<PathBuf> {
    Ok(dunce::canonicalize(env::current_dir()?)?)
}

pub fn ensure_within_root(path: &Path, root: &Path, label: &str) -> Result<PathBuf> {
    let canon = dunce::canonicalize(path)?;
    if !canon.starts_with(root) {
        bail!("Security violation: '{label}' escapes workspace");
    }
    Ok(canon)
}

pub fn ensure_parent_dir(target: &Path, root: &Path, label: &str) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).context("Error creating parent directories")?;
        ensure_within_root(parent, root, label)?;
    }
    Ok(())
}

pub fn validate_relative_path(raw: &str) -> Result<&Path> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Security violation: path cannot be empty");
    }
    let rel = Path::new(trimmed);
    if rel.is_absolute() {
        bail!(
            "Security violation: path must be relative to current working directory: '{trimmed}'"
        );
    }
    if rel
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("Security violation: path traversal blocked for '{trimmed}'");
    }
    Ok(rel)
}

pub fn resolve_existing_under(base_root: &Path, raw: &str, escape_label: &str) -> Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." {
        return Ok(base_root.to_path_buf());
    }
    let candidate = base_root.join(validate_relative_path(trimmed)?);
    if !candidate.exists() {
        bail!("Target path does not exist: '{trimmed}'");
    }
    ensure_within_root(&candidate, base_root, escape_label)
}

pub fn has_git_dir_component(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

pub fn build_globset(patterns: &[String], allow_recursive_shorthand: bool) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(Glob::new(pat).with_context(|| format!("Invalid glob pattern: {pat}"))?);
        if allow_recursive_shorthand && !pat.contains(['/', '\\']) {
            let rec = format!("**/{pat}");
            builder.add(Glob::new(&rec).with_context(|| format!("Invalid glob pattern: {rec}"))?);
        }
    }
    Ok(builder.build()?)
}
