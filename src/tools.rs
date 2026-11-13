pub mod cargo;
pub mod create_file;
pub mod edit_file;
pub mod execute_command;
pub mod fetch_url;
pub mod fs;
pub mod git;
pub mod read_file;
pub mod repo_map;
pub mod search;
pub mod workspace_snapshot;

use crate::core::config::Config;
use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use similar::TextDiff;
use std::{
    env,
    ffi::OsStr,
    fs::File,
    io::Read as _,
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};
use tracing::{debug, error, info};

const COMMAND_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: ToolFunction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: Value,
}

pub fn call_signature(call: &ToolCall) -> String {
    let name = call.function.name.as_str();
    let args = &call.function.arguments;

    let sub = match name {
        "git" | "cargo" | "fs" | "repo_map" => args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        "execute_command" => {
            shlex::split(args.get("command").and_then(Value::as_str).unwrap_or(""))
                .and_then(|parts| parts.into_iter().next())
                .unwrap_or_default()
        }
        _ => String::new(),
    };

    if sub.is_empty() {
        name.to_string()
    } else {
        format!("{name}:{sub}")
    }
}

pub fn is_auto_approved<'a, I>(call: &ToolCall, rules: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    let name = call.function.name.as_str();
    let sig = call_signature(call);

    rules
        .into_iter()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .any(|r| {
            r == "*"
                || r == name
                || r == sig
                || r.strip_suffix(":*")
                    .is_some_and(|p| name == p || sig.starts_with(&format!("{p}:")))
        })
}

fn format_kv(k: &str, v: &Value) -> String {
    match v.as_str() {
        Some(s) => format!("  {k}: {s}\n"),
        None => format!("  {k}: {v}\n"),
    }
}

fn append_unified_diff(out: &mut String, file_path: &str, old: &str, new: &str) {
    let diff = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{file_path}"), &format!("b/{file_path}"))
        .to_string();

    out.push_str("\n--- UNIFIED DIFF ---\n");
    out.push_str(if diff.trim().is_empty() {
        "(No differences)\n"
    } else {
        &diff
    });
}

fn format_edit_approval(mut out: String, args: &Value, obj: &Map<String, Value>) -> String {
    let file_path = obj.get("file_path").and_then(Value::as_str).unwrap_or("");
    out.push_str(&format!("  file_path: {file_path}\n"));
    for (k, v) in obj
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "file_path" | "edits"))
    {
        out.push_str(&format_kv(k, v));
    }

    match edit_file::apply_edits_from_value(args) {
        Ok((_, original, updated, hunk_count)) => {
            out.push_str(&format!("  hunks: {hunk_count}\n"));
            append_unified_diff(&mut out, file_path, &original, &updated);
        }
        Err(e) => {
            out.push_str(&format!("\n--- WARNING ---\n{e}! Execution will fail.\n"));
            let (raw_search, raw_replace) = edit_file::fallback_hunk_preview(args);
            append_unified_diff(&mut out, file_path, &raw_search, &raw_replace);
        }
    }
    out.trim_end().to_string()
}

fn format_fs_approval(mut out: String, args: &Value, obj: &Map<String, Value>) -> String {
    for (k, v) in obj {
        out.push_str(&format_kv(k, v));
    }
    match fs::preview_action(args) {
        Ok((summary, maybe_diff)) => {
            out.push_str(&format!("\n--- OPERATION ---\n{summary}\n"));
            if let Some((file_path, deleted_content)) = maybe_diff {
                append_unified_diff(&mut out, &file_path, &deleted_content, "");
            }
        }
        Err(e) => out.push_str(&format!("\n--- WARNING ---\n{e}! Execution will fail.\n")),
    }
    out.trim_end().to_string()
}

pub fn format_approval(call: &ToolCall) -> String {
    let name = &call.function.name;
    let args = &call.function.arguments;
    let sig = call_signature(call);

    let Some(obj) = args.as_object() else {
        return format!(
            "Tool: {name} [{sig}]\n{}",
            serde_json::to_string_pretty(args).unwrap_or_default()
        );
    };

    let mut out = format!("Tool: {name} (Rule: {sig})\n");
    if name == "edit_file" {
        return format_edit_approval(out, args, obj);
    }
    if name == "fs" {
        return format_fs_approval(out, args, obj);
    }

    let mut multiline_blocks = Vec::new();
    for (k, v) in obj {
        match v.as_str() {
            Some(s) if s.contains('\n') || matches!(k.as_str(), "content" | "message") => {
                multiline_blocks.push((k, s));
            }
            _ => out.push_str(&format_kv(k, v)),
        }
    }

    multiline_blocks.sort_by(|a, b| b.0.cmp(a.0));
    for (k, s) in multiline_blocks {
        out.push_str(&format!("\n--- {} ---\n{s}\n", k.to_uppercase()));
    }

    out.trim_end().to_string()
}

pub fn schemas() -> Vec<Value> {
    vec![
        cargo::schema(),
        create_file::schema(),
        edit_file::schema(),
        execute_command::schema(),
        fetch_url::schema(),
        fs::schema(),
        git::schema(),
        read_file::schema(),
        repo_map::schema(),
        search::schema(),
        workspace_snapshot::schema(),
    ]
}

async fn spawn_blocking_tool<F>(f: F) -> Result<String>
where
    F: FnOnce() -> Result<String> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("Tool task panicked: {e}"))?
}

async fn dispatch_tool(
    name: &str,
    args: &Value,
    config: &Config,
    max_output_size: usize,
) -> Result<String> {
    let cloned = args.clone();
    let ws = config.workspace_snapshot.clone();
    match name {
        "cargo" => cargo::execute(args, max_output_size).await,
        "repo_map" => {
            spawn_blocking_tool(move || repo_map::execute(&cloned, &ws, max_output_size)).await
        }
        "workspace_snapshot" => {
            spawn_blocking_tool(move || workspace_snapshot::execute(&cloned, &ws, max_output_size))
                .await
        }
        "search" => {
            spawn_blocking_tool(move || search::execute(&cloned, &ws, max_output_size)).await
        }
        "fetch_url" => fetch_url::execute(args, max_output_size).await,
        "fs" => spawn_blocking_tool(move || fs::execute(&cloned, max_output_size)).await,
        "execute_command" => {
            execute_command::execute(args, &config.app.allowed_commands, max_output_size).await
        }
        "git" => git::execute(args, max_output_size).await,
        "create_file" => spawn_blocking_tool(move || create_file::execute(&cloned)).await,
        "edit_file" => spawn_blocking_tool(move || edit_file::execute(&cloned)).await,
        "read_file" => {
            spawn_blocking_tool(move || read_file::execute(&cloned, max_output_size)).await
        }
        _ => bail!("Unknown tool: '{name}'"),
    }
}

pub async fn execute(call: &ToolCall, config: &Config, max_size: usize) -> (String, bool) {
    let name = &call.function.name;
    let args = &call.function.arguments;
    info!("Executing tool: {name}");
    debug!("Tool arguments: {args}");

    match dispatch_tool(name, args, config, max_size).await {
        Ok(mut out) => {
            let truncated = out.len() > max_size;
            if truncated {
                let truncate_at = out.floor_char_boundary(max_size);
                let dropped = out.len() - truncate_at;
                out.truncate(truncate_at);
                out.push_str(&format!(
                    "\n\n[TRUNCATED: Output exceeded the {max_size} byte limit — {dropped} bytes were dropped. Content is incomplete.]"
                ));
            }
            info!("Tool '{name}' executed successfully. Truncated: {truncated}");
            debug!(
                "Tool output snippet: {}",
                out.replace('\n', " ").chars().take(200).collect::<String>()
            );
            (out, truncated)
        }
        Err(e) => {
            error!("Tool '{name}' failed: {e}");
            (format!("Error: {e}"), false)
        }
    }
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
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}

pub fn workspace_root() -> Result<PathBuf> {
    Ok(dunce::canonicalize(env::current_dir()?)?)
}

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
    let stdout = child
        .stdout
        .take()
        .context("Failed to capture child stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("Failed to capture child stderr")?;

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

    Ok(format_process_bytes(
        &out_bytes, &err_bytes, status, empty_msg,
    ))
}

pub fn format_process_bytes(
    stdout: &[u8],
    stderr: &[u8],
    status: ExitStatus,
    empty_msg: &str,
) -> String {
    let mut combined = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str("[STDERR]\n");
        combined.push_str(&String::from_utf8_lossy(stderr));
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

    if combined.trim().is_empty() {
        empty_msg.to_string()
    } else {
        combined
    }
}
