use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    path::{Component, Path},
    process,
};

pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "context",
            "description": "Retrieves project directory structures and file contents. Use this to explore the codebase.",
            "parameters": {
                "type": "object",
                "properties": {
                    "target_path": { "type": "string", "description": "Optional relative path (defaults to current directory)." }
                }
            }
        }
    })
}

pub fn execute(json_args: &Value) -> Result<String> {
    let target = json_args
        .get("target_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    let cwd = env::current_dir()?.canonicalize()?;
    let mut cmd = process::Command::new("context");
    cmd.current_dir(&cwd);

    if !target.is_empty() {
        let rel_path = Path::new(target);
        if rel_path.is_absolute() {
            bail!("Path traversal blocked for target '{}'", target);
        }
        for comp in rel_path.components() {
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                _ => bail!("Path traversal blocked for target '{}'", target),
            }
        }
        let resolved = cwd.join(rel_path);
        if !resolved.exists() {
            bail!("Target path does not exist: '{}'", target);
        }
        let canon = resolved.canonicalize()?;
        if !canon.starts_with(&cwd) {
            bail!("Security violation: target '{}' escapes workspace", target);
        }
        cmd.arg(target);
    }

    let out = cmd.output().context("Error executing context")?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str("[STDERR]\n");
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
    }

    if combined.trim().is_empty() {
        Ok("Context executed successfully (no output).".into())
    } else {
        Ok(combined)
    }
}
