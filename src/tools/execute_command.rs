use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    env,
    path::{Component, Path},
    process,
};

#[derive(Deserialize)]
struct ExecuteCommandArgs {
    command: String,
}

pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "execute_command",
            "description": "Executes an allowed shell command. No pipes or redirects.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The exact command to execute." }
                },
                "required": ["command"]
            }
        }
    })
}

fn validate_arg_path(candidate: &str, cwd: &Path) -> Result<()> {
    if candidate.is_empty() {
        return Ok(());
    }
    let path = Path::new(candidate);
    if path.is_absolute() {
        bail!("Absolute path blocked in argument '{}'", candidate);
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("Path traversal ('..') blocked in argument '{}'", candidate);
    }

    // If the argument resolves to an existing filesystem entry, ensure it doesn't symlink outside workspace
    let joined = cwd.join(path);
    if joined.exists() {
        let canon = joined.canonicalize()?;
        if !canon.starts_with(cwd) {
            bail!(
                "Security violation: argument '{}' resolves outside workspace",
                candidate
            );
        }
    }
    Ok(())
}

pub fn execute(json_args: &Value, allowed_commands: &[String]) -> Result<String> {
    let args: ExecuteCommandArgs = serde_json::from_value(json_args.clone())
        .context("Invalid arguments provided to execute_command")?;
        
    let raw_cmd = args.command.trim();

    if raw_cmd.is_empty() {
        bail!("Empty command");
    }

    for bad in [";", "&", "|", "`", "$", "<", ">", "\n", "\r"] {
        if raw_cmd.contains(bad) {
            bail!("Forbidden shell operator '{}'", bad.escape_default());
        }
    }

    let parts = shlex::split(raw_cmd).context("Malformed command string")?;
    if parts.is_empty() {
        bail!("Empty parsed command");
    }

    let base = &parts[0];
    let rest = &parts[1..];

    if !allowed_commands.iter().any(|c| c == base) {
        bail!("Command '{}' is not in allowlist", base);
    }

    let cwd = env::current_dir()?.canonicalize()?;

    for arg in rest {
        validate_arg_path(arg, &cwd)?;

        // Also inspect inline flag values like `--file=/etc/passwd` or `-f../secret`
        if let Some((_, val)) = arg.split_once('=') {
            validate_arg_path(val, &cwd)?;
        } else if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            validate_arg_path(&arg[2..], &cwd)?;
        }
    }

    let out = process::Command::new(base)
        .current_dir(&cwd)
        .args(rest)
        .output()
        .context("Error executing command")?;

    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str("[STDERR]\n");
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
    }

    if combined.trim().is_empty() {
        Ok("Command executed successfully (no output).".into())
    } else {
        Ok(combined)
    }
}
