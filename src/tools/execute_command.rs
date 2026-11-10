use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::process;

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

pub fn execute(json_args: &Value, allowed_commands: &[String]) -> Result<String> {
    let raw_cmd = json_args
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    if raw_cmd.is_empty() {
        bail!("Empty command");
    }

    for bad in [";", "&", "|", "`", "$", "<", ">"] {
        if raw_cmd.contains(bad) {
            bail!("Forbidden shell operator '{}'", bad);
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

    for arg in rest {
        if arg.starts_with('/') || arg.contains("..") {
            bail!("Path traversal blocked for argument '{}'", arg);
        }
    }

    let out = process::Command::new(base)
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
