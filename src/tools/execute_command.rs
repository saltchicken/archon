use crate::tools::{auto_schema, parse_args, run_workspace_command, workspace_root};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Component, Path};

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

    let path = Path::new(trimmed);
    if path.is_absolute() {
        bail!("Security violation: absolute path '{trimmed}' is not allowed");
    }

    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_) | Component::RootDir))
    {
        bail!("Security violation: path traversal ('..') blocked in '{trimmed}'");
    }

    let joined = cwd.join(path);
    if joined.exists() {
        let canon = dunce::canonicalize(&joined)?;
        if !canon.starts_with(cwd) {
            bail!("Security violation: '{trimmed}' resolves outside workspace");
        }
    } else if let Some(ancestor) = joined.ancestors().skip(1).find(|p| p.exists()) {
        let canon = dunce::canonicalize(ancestor)?;
        if !canon.starts_with(cwd) {
            bail!("Security violation: '{trimmed}' parent resolves outside workspace");
        }
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
            // Allow standard flags (e.g. -rn, --color), but still block traversal or absolute paths
            // embedded inside flag arguments like -f../secret or -I/etc
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
