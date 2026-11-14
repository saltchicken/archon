use super::{AsyncTool, ToolContext};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct ExecuteCommandArgs {
    /// The exact command and arguments to execute.
    command: String,
}

fn check_path_candidate(candidate: &str, ctx: &ToolContext) -> Result<()> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if trimmed.starts_with('~') {
        bail!("Security violation: home directory expansion ('~') is not allowed in '{trimmed}'");
    }

    ctx.validate_scoped_relative_path(trimmed)?;
    Ok(())
}

impl AsyncTool for Tool {
    const NAME: &'static str = "execute_command";
    const DESCRIPTION: &'static str =
        "Executes an allowed command directly without a shell. No pipes or redirects.";
    type Args = ExecuteCommandArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let raw_cmd = args.command.trim();
        if raw_cmd.is_empty() {
            bail!("Empty command");
        }

        let parts = shlex::split(raw_cmd).context("Malformed command string (unclosed quotes)")?;
        let Some((base, rest)) = parts.split_first() else {
            bail!("Empty parsed command");
        };

        if !ctx
            .execute_command
            .allowed_commands
            .iter()
            .any(|c| c == base)
        {
            bail!("Command '{base}' is not in allowlist");
        }

        for arg in rest {
            if let Some((_, val)) = arg.split_once('=') {
                check_path_candidate(val, ctx)?;
            } else if !arg.starts_with('-') {
                check_path_candidate(arg, ctx)?;
            } else if arg.contains("..") || arg.contains(['/', '\\', '~']) {
                let stripped = arg.trim_start_matches('-');
                check_path_candidate(stripped, ctx)?;
                if stripped.len() > 1 {
                    check_path_candidate(&stripped[1..], ctx)?;
                }
            }
        }

        ctx.run_command(base, rest, "Command executed successfully (no output).")
            .await
    }
}
