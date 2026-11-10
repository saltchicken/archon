use crate::tools::{
    function_schema, parse_args, run_workspace_command, validate_relative_path, workspace_root,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;

#[derive(Deserialize)]
struct ExecuteCommandArgs {
    command: String,
}

pub fn schema() -> Value {
    function_schema(
        "execute_command",
        "Executes an allowed command directly without a shell. No pipes or redirects.",
        json!({
            "command": { "type": "string", "description": "The exact command and arguments to execute." }
        }),
        &["command"],
    )
}

fn validate_arg_path(candidate: &str, cwd: &Path) -> Result<()> {
    if candidate.is_empty() {
        return Ok(());
    }
    let joined = cwd.join(validate_relative_path(candidate)?);
    if joined.exists() && !dunce::canonicalize(&joined)?.starts_with(cwd) {
        bail!("Security violation: argument '{candidate}' resolves outside workspace");
    }
    Ok(())
}

pub fn execute(json_args: &Value, allowed_commands: &[String]) -> Result<String> {
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
        validate_arg_path(arg, &cwd)?;
        if let Some((_, val)) = arg.split_once('=') {
            validate_arg_path(val, &cwd)?;
        } else if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            validate_arg_path(&arg[2..], &cwd)?;
        }
    }

    run_workspace_command(base, rest, "Command executed successfully (no output).")
}
