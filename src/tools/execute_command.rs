use crate::tools::{
    auto_schema, ensure_within_workspace, parse_args, run_workspace_command,
    validate_relative_path, workspace_root,
};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

#[derive(Deserialize, JsonSchema)]
struct ExecuteCommandArgs {
    /// The exact command and arguments to execute.
    command: String,
}

pub fn schema() -> Value {
    auto_schema::<ExecuteCommandArgs>(
        "execute_command",
        "Executes an allowed command directly without a shell. No pipes or redirects.",
    )
}

fn check_path_candidate(candidate: &str, cwd: &Path) -> Result<()> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    if trimmed.starts_with('~') {
        bail!("Security violation: home directory expansion ('~') is not allowed in '{trimmed}'");
    }

    let path = validate_relative_path(trimmed)?;
    let joined = cwd.join(path);

    if joined.exists() {
        ensure_within_workspace(&joined, cwd, trimmed)?;
    } else if let Some(ancestor) = joined.ancestors().skip(1).find(|p| p.exists()) {
        ensure_within_workspace(ancestor, cwd, trimmed)?;
    }

    Ok(())
}

pub async fn execute(
    json_args: &Value,
    allowed_commands: &[String],
    max_output_size: usize,
) -> Result<String> {
    let args: ExecuteCommandArgs = parse_args(json_args, "execute_command")?;
    let raw_cmd = args.command.trim();

    if raw_cmd.is_empty() {
        bail!("Empty command");
    }

    let parts = shlex::split(raw_cmd).context("Malformed command string (unclosed quotes)")?;
    let Some((base, rest)) = parts.split_first() else {
        bail!("Empty parsed command");
    };

    if !allowed_commands.iter().any(|c| c == base) {
        bail!("Command '{base}' is not in allowlist");
    }

    let cwd = workspace_root()?;
    for arg in rest {
        if let Some((_, val)) = arg.split_once('=') {
            check_path_candidate(val, &cwd)?;
        } else if arg.starts_with('-') {
            if arg.contains("..") || arg.contains('/') || arg.contains('\\') || arg.contains('~') {
                let stripped = arg.trim_start_matches('-');
                check_path_candidate(stripped, &cwd)?;
                if stripped.len() > 1 {
                    check_path_candidate(&stripped[1..], &cwd)?;
                }
            }
        } else {
            check_path_candidate(arg, &cwd)?;
        }
    }

    run_workspace_command(
        base,
        rest,
        "Command executed successfully (no output).",
        max_output_size,
    )
    .await
}
