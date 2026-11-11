pub mod context;
pub mod create_file;
pub mod edit_file;
pub mod execute_command;
pub mod read_file;

use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{
    env,
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

const COMMAND_TIMEOUT_SECS: u64 = 30;

pub fn schemas() -> Vec<Value> {
    vec![
        context::schema(),
        create_file::schema(),
        edit_file::schema(),
        execute_command::schema(),
        read_file::schema(),
    ]
}

pub async fn execute(
    name: &str,
    args: &Value,
    allowed_commands: &[String],
    max_output_size: usize,
) -> Result<String> {
    match name {
        "context" => context::execute(args, max_output_size).await,
        "execute_command" => {
            execute_command::execute(args, allowed_commands, max_output_size).await
        }
        "create_file" => {
            let args = args.clone();
            tokio::task::spawn_blocking(move || create_file::execute(&args))
                .await
                .map_err(|e| anyhow::anyhow!("Tool task panicked: {e}"))?
        }
        "edit_file" => {
            let args = args.clone();
            tokio::task::spawn_blocking(move || edit_file::execute(&args))
                .await
                .map_err(|e| anyhow::anyhow!("Tool task panicked: {e}"))?
        }
        "read_file" => {
            let args = args.clone();
            tokio::task::spawn_blocking(move || read_file::execute(&args))
                .await
                .map_err(|e| anyhow::anyhow!("Tool task panicked: {e}"))?
        }
        _ => bail!("Unknown tool: '{name}'"),
    }
}

pub fn parse_args<T: DeserializeOwned>(json_args: &Value, tool_name: &str) -> Result<T> {
    serde_json::from_value(json_args.clone())
        .with_context(|| format!("Invalid arguments provided to {tool_name}"))
}

pub fn auto_schema<T: JsonSchema>(name: &str, description: &str) -> Value {
    let schema = schemars::schema_for!(T);
    let mut parameters = serde_json::to_value(&schema).unwrap();

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
    Ok((cwd.clone(), cwd.join(rel_path)))
}

pub fn resolve_existing_path(raw: &str) -> Result<(PathBuf, PathBuf)> {
    let (cwd, target) = resolve_workspace_path(raw)?;
    if !target.exists() {
        bail!("Target path does not exist: '{}'", raw.trim());
    }
    let canon = dunce::canonicalize(&target)?;
    if !canon.starts_with(&cwd) {
        bail!(
            "Security violation: target '{}' escapes workspace",
            raw.trim()
        );
    }
    Ok((cwd, canon))
}

pub fn resolve_new_path(raw: &str) -> Result<(PathBuf, PathBuf)> {
    let (cwd, target) = resolve_workspace_path(raw)?;
    if target.exists() {
        bail!(
            "File already exists: {}. Use edit_file instead.",
            raw.trim()
        );
    }

    if let Some(existing_ancestor) = target.ancestors().skip(1).find(|p| p.exists()) {
        if !dunce::canonicalize(existing_ancestor)?.starts_with(&cwd) {
            bail!("Security violation: target directory escapes workspace via symlink");
        }
    }

    Ok((cwd, target))
}

/// Reads up to `cap` bytes into memory, then drops `reader` immediately so the
/// write end of the OS pipe closes (preventing the child process from deadlocking
/// on a full pipe buffer when output exceeds `max_output_size`).
async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    (&mut reader).take(cap).read_to_end(&mut buf).await?;
    drop(reader);
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

    // Cap reads at max_output_size + 1 so ToolRegistry::execute can still detect truncation
    let cap = (max_output_size as u64).saturating_add(1);
    let stdout = child
        .stdout
        .take()
        .context("Failed to capture child stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("Failed to capture child stderr")?;

    let (out_bytes, err_bytes) = timeout(Duration::from_secs(COMMAND_TIMEOUT_SECS), async {
        let (r1, r2, status) = tokio::join!(
            read_capped(stdout, cap),
            read_capped(stderr, cap),
            child.wait()
        );
        let out_buf = r1?;
        let err_buf = r2?;
        let _ = status?;
        Ok::<_, std::io::Error>((out_buf, err_buf))
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("Command '{program}' timed out after {COMMAND_TIMEOUT_SECS} seconds")
    })?
    .with_context(|| format!("Error reading output from {program}"))?;

    Ok(format_process_bytes(&out_bytes, &err_bytes, empty_msg))
}

pub fn format_process_bytes(stdout: &[u8], stderr: &[u8], empty_msg: &str) -> String {
    let mut combined = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str("[STDERR]\n");
        combined.push_str(&String::from_utf8_lossy(stderr));
    }
    if combined.trim().is_empty() {
        empty_msg.to_string()
    } else {
        combined
    }
}
