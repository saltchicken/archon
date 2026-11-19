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

pub fn ensure_ancestor_within_root(target: &Path, root: &Path, label: &str) -> Result<PathBuf> {
    if let Some(existing) = target
        .ancestors()
        .find(|p| p.exists() || p.symlink_metadata().is_ok())
    {
        let canon_anc = ensure_within_root(existing, root, label)?;
        if let Ok(rem) = target.strip_prefix(existing) {
            return Ok(canon_anc.join(rem));
        }
        return Ok(canon_anc);
    }
    Ok(target.to_path_buf())
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

pub fn has_hidden_component(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c,
            Component::Normal(os) if os.as_encoded_bytes().first() == Some(&b'.')
        )
    })
}

pub fn partition_negated_patterns<'a, I>(patterns: I) -> (Vec<String>, Vec<String>)
where
    I: IntoIterator<Item = &'a str>,
{
    let mut neg = Vec::new();
    let mut pos = Vec::new();
    for pat in patterns
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (target, raw) = match pat.strip_prefix('!') {
            Some(s) => (&mut neg, s.trim()),
            None => (&mut pos, pat),
        };
        let cleaned = raw.trim_start_matches('/');
        if !cleaned.is_empty() {
            target.push(cleaned.to_string());
        }
    }
    (neg, pos)
}

pub fn build_globset(patterns: &[String], allow_recursive_shorthand: bool) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(Glob::new(pat).with_context(|| format!("Invalid glob pattern: {pat}"))?);
        if let Some(dir_prefix) = pat.strip_suffix("/**").filter(|p| !p.is_empty()) {
            builder.add(
                Glob::new(dir_prefix)
                    .with_context(|| format!("Invalid glob pattern: {dir_prefix}"))?,
            );
            if allow_recursive_shorthand && !dir_prefix.contains(['/', '\\']) {
                let rec_dir = format!("**/{dir_prefix}");
                builder.add(
                    Glob::new(&rec_dir)
                        .with_context(|| format!("Invalid glob pattern: {rec_dir}"))?,
                );
            }
        }
        if allow_recursive_shorthand && !pat.contains(['/', '\\']) {
            let rec = format!("**/{pat}");
            builder.add(Glob::new(&rec).with_context(|| format!("Invalid glob pattern: {rec}"))?);
        }
    }
    Ok(builder.build()?)
}

pub fn build_ancestor_dir_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let mut cur = pat.trim_end_matches("/**").trim_end_matches('/');
        while let Some((parent, _)) = cur.rsplit_once('/') {
            let parent = parent.trim_end_matches('/');
            if parent.is_empty() || parent == "**" {
                break;
            }
            builder
                .add(Glob::new(parent).with_context(|| format!("Invalid glob pattern: {parent}"))?);
            cur = parent;
        }
    }
    Ok(builder.build()?)
}
