use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder};
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    env,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

pub const COMMAND_TIMEOUT_SECS: u64 = 30;

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

pub enum FileOutcome {
    Content(String),
    Skipped(&'static str),
    Error(String),
}

pub fn parse_args<T: DeserializeOwned>(json_args: &Value, tool_name: &str) -> Result<T> {
    serde_json::from_value(json_args.clone())
        .with_context(|| format!("Invalid arguments provided to {tool_name}"))
}

pub fn auto_schema<T: JsonSchema>(name: &str, description: &str) -> Value {
    let mut parameters = serde_json::to_value(schemars::schema_for!(T)).unwrap();
    if let Some(obj) = parameters.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    json!({
        "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters }
    })
}

pub fn workspace_root() -> Result<PathBuf> {
    Ok(dunce::canonicalize(env::current_dir()?)?)
}

pub fn has_git_dir_component(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
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

pub fn resolve_existing_under_root(
    base_root: &Path,
    raw: &str,
    escape_label: &str,
) -> Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." {
        return Ok(base_root.to_path_buf());
    }
    let candidate = base_root.join(validate_relative_path(trimmed)?);
    if !candidate.exists() {
        bail!("Target path does not exist: '{trimmed}'");
    }
    ensure_within_workspace(&candidate, base_root, escape_label)
}

pub fn resolve_existing_path(raw: &str) -> Result<(PathBuf, PathBuf)> {
    let cwd = workspace_root()?;
    let trimmed = raw.trim();
    let target = cwd.join(validate_relative_path(trimmed)?);
    if !target.exists() {
        bail!("Target path does not exist: '{trimmed}'");
    }
    let canon = ensure_within_workspace(&target, &cwd, trimmed)?;
    Ok((cwd, canon))
}

pub fn resolve_new_path(raw: &str) -> Result<(PathBuf, PathBuf)> {
    let cwd = workspace_root()?;
    let trimmed = raw.trim();
    let target = cwd.join(validate_relative_path(trimmed)?);
    if target.exists() {
        bail!("File already exists: {trimmed}. Use edit_file instead.");
    }
    ensure_path_or_ancestor_within_workspace(&target, &cwd, trimmed)?;
    Ok((cwd, target))
}

/// Atomically writes `content` to `target_path` via a sibling temporary file,
/// preserving existing file permissions when overwriting an existing file.
pub fn atomic_write_file(target_path: &Path, content: impl AsRef<[u8]>) -> Result<()> {
    let parent = target_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let existing_perms = fs::metadata(target_path).ok().map(|m| m.permissions());

    let file_name = target_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let pid = std::process::id();

    let (mut tmp_file, tmp_path) = (0..64u32)
        .find_map(|attempt| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let candidate = parent.join(format!(".{file_name}.archon-{pid}-{nanos}-{attempt}.tmp"));
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
                .ok()
                .map(|f| (f, candidate))
        })
        .context("Failed to create temporary file for atomic write")?;

    let write_res = (|| -> Result<()> {
        tmp_file
            .write_all(content.as_ref())
            .context("Error writing to temporary file")?;
        tmp_file
            .sync_all()
            .context("Error syncing temporary file to disk")?;
        drop(tmp_file);

        if let Some(perms) = existing_perms {
            fs::set_permissions(&tmp_path, perms)
                .context("Error preserving file permissions on temporary file")?;
        }

        fs::rename(&tmp_path, target_path).context("Error committing atomic file write")?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }

    write_res
}

async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    (&mut reader).take(cap).read_to_end(&mut buf).await?;
    Ok(buf)
}

pub async fn run_workspace_command<I, S>(
    program: &str,
    args: I,
    empty_msg: &str,
    max_output_size: usize,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_workspace_command_with_timeout(
        program,
        args,
        empty_msg,
        max_output_size,
        COMMAND_TIMEOUT_SECS,
    )
    .await
}

pub async fn run_workspace_command_with_timeout<I, S>(
    program: &str,
    args: I,
    empty_msg: &str,
    max_output_size: usize,
    timeout_secs: u64,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let cwd = workspace_root()?;
    let mut child = Command::new(program)
        .current_dir(&cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Error executing {program}"))?;

    let cap = (max_output_size as u64).saturating_add(1);
    let stdout = child.stdout.take().context("Failed to capture stdout")?;
    let stderr = child.stderr.take().context("Failed to capture stderr")?;

    let (out_bytes, err_bytes, status) = timeout(Duration::from_secs(timeout_secs), async {
        let (r1, r2, status) = tokio::join!(
            read_capped(stdout, cap),
            read_capped(stderr, cap),
            child.wait()
        );
        Ok::<_, std::io::Error>((r1?, r2?, status?))
    })
    .await
    .map_err(|_| anyhow::anyhow!("Command '{program}' timed out after {timeout_secs} seconds"))?
    .with_context(|| format!("Error reading output from {program}"))?;

    let mut combined = String::from_utf8_lossy(&out_bytes).into_owned();
    if !err_bytes.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str("[STDERR]\n");
        combined.push_str(&String::from_utf8_lossy(&err_bytes));
    }
    if !status.success() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        match status.code() {
            Some(code) => combined.push_str(&format!("[Exit code: {code}]")),
            None => combined.push_str("[Process terminated by signal]"),
        }
    }

    Ok(if combined.trim().is_empty() {
        empty_msg.to_string()
    } else {
        combined
    })
}

pub fn resolve_scan_roots(
    use_git_root: bool,
    target_path: &str,
    escape_label: &str,
) -> Result<(PathBuf, PathBuf)> {
    let cwd = workspace_root()?;
    let base_root = if use_git_root {
        cwd.ancestors()
            .find(|a| a.join(".git").exists())
            .map(Path::to_path_buf)
            .unwrap_or(cwd)
    } else {
        cwd
    };
    let base_root = dunce::canonicalize(&base_root).unwrap_or(base_root);
    let scan_root = resolve_existing_under_root(&base_root, target_path, escape_label)?;
    Ok((base_root, scan_root))
}

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

pub fn build_exclude_globset(
    config_exclude: &[String],
    call_exclude: &[String],
    allow_recursive_shorthand: bool,
) -> Result<GlobSet> {
    let patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
        .iter()
        .map(|&s| String::from(s))
        .chain(config_exclude.iter().cloned())
        .chain(call_exclude.iter().cloned())
        .collect();
    build_globset(&patterns, allow_recursive_shorthand)
}

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

    if scan_root.is_file() {
        let rel = diff_paths(scan_root, base_root).unwrap_or_else(|| scan_root.to_path_buf());
        if exclude_set.is_match(&rel) {
            bail!(
                "Security violation: '{}' matches an excluded pattern",
                rel.display()
            );
        }
        return Ok(vec![(
            scan_root.to_path_buf(),
            rel.to_string_lossy().into_owned(),
        )]);
    }

    let mut files = Vec::new();
    for entry in build_workspace_walker(scan_root, use_git_ignore) {
        if files.len() >= max_files {
            break;
        }
        let path = entry.path();
        if !path.is_file() || has_git_dir_component(path) {
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

pub fn read_text_file_inspected(path: &Path, max_file_size: u64) -> FileOutcome {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return FileOutcome::Error(format!("Error reading file: {e}")),
    };
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => return FileOutcome::Error(format!("Error reading file: {e}")),
    };
    if len == 0 {
        return FileOutcome::Content(String::new());
    }
    if len > max_file_size {
        return FileOutcome::Error("File exceeds maximum size limit.".into());
    }

    let mut chunk = [0u8; 8192];
    let n = match file.read(&mut chunk) {
        Ok(0) => return FileOutcome::Content(String::new()),
        Ok(n) => n,
        Err(e) => return FileOutcome::Error(format!("Error reading file: {e}")),
    };
    let sample = &chunk[..n];
    let ctrl_count = sample
        .iter()
        .filter(|&&b| b < 32 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0C))
        .count();
    if sample.contains(&0) || (ctrl_count as f32 / sample.len() as f32) > 0.3 {
        return FileOutcome::Skipped("Binary file detected.");
    }

    let mut buffer = Vec::with_capacity(len as usize);
    buffer.extend_from_slice(sample);
    if let Err(e) = file.read_to_end(&mut buffer) {
        return FileOutcome::Error(format!("Error reading file: {e}"));
    }

    match String::from_utf8(buffer) {
        Ok(s) => FileOutcome::Content(s),
        Err(_) => FileOutcome::Skipped("Non-UTF-8 text / binary file detected."),
    }
}

pub fn read_utf8_capped(path: &Path, max_file_size: u64) -> Option<String> {
    match read_text_file_inspected(path, max_file_size) {
        FileOutcome::Content(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}
