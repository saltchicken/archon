use super::paths::workspace_root;
use anyhow::{Context, Result};
use std::{
    ffi::OsStr,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

pub const COMMAND_TIMEOUT_SECS: u64 = 30;

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
