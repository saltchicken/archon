use anyhow::{Result, bail};
use std::{
    env,
    path::{Component, Path, PathBuf},
};

pub fn workspace_root() -> Result<PathBuf> {
    Ok(dunce::canonicalize(env::current_dir()?)?)
}

pub fn ensure_within_workspace(path: &Path, cwd: &Path, label: &str) -> Result<PathBuf> {
    let canon = dunce::canonicalize(path)?;
    if !canon.starts_with(cwd) {
        bail!("Security violation: '{label}' escapes workspace");
    }
    Ok(canon)
}

pub fn ensure_path_or_ancestor_within_workspace(
    target: &Path,
    cwd: &Path,
    label: &str,
) -> Result<()> {
    if let Some(existing) = target.ancestors().find(|p| p.exists()) {
        ensure_within_workspace(existing, cwd, label)?;
    }
    Ok(())
}

pub fn validate_relative_path(raw: &str) -> Result<&Path> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Security violation: path cannot be empty");
    }
    let rel_path = Path::new(trimmed);
    if rel_path.is_absolute() {
        bail!(
            "Security violation: path must be relative to current working directory: '{trimmed}'"
        );
    }
    if rel_path
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("Security violation: path traversal blocked for '{trimmed}'");
    }
    Ok(rel_path)
}

fn resolve_workspace_path(raw: &str) -> Result<(PathBuf, PathBuf)> {
    let rel_path = validate_relative_path(raw)?;
    let cwd = workspace_root()?;
    let target = cwd.join(rel_path);
    Ok((cwd, target))
}

pub fn resolve_existing_path(raw: &str) -> Result<(PathBuf, PathBuf)> {
    let (cwd, target) = resolve_workspace_path(raw)?;
    let trimmed = raw.trim();
    if !target.exists() {
        bail!("Target path does not exist: '{trimmed}'");
    }
    let canon = ensure_within_workspace(&target, &cwd, trimmed)?;
    Ok((cwd, canon))
}

pub fn resolve_new_path(raw: &str) -> Result<(PathBuf, PathBuf)> {
    let (cwd, target) = resolve_workspace_path(raw)?;
    let trimmed = raw.trim();
    if target.exists() {
        bail!("File already exists: {trimmed}. Use edit_file instead.");
    }
    ensure_path_or_ancestor_within_workspace(&target, &cwd, trimmed)?;
    Ok((cwd, target))
}
