pub mod context;
pub mod create_file;
pub mod edit_file;
pub mod execute_command;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{
    env,
    path::{Component, Path, PathBuf},
    process::Output,
};

pub fn parse_args<T: DeserializeOwned>(json_args: &Value, tool_name: &str) -> Result<T> {
    serde_json::from_value(json_args.clone())
        .with_context(|| format!("Invalid arguments provided to {tool_name}"))
}

pub fn function_schema(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> Value {
    let mut params = json!({
        "type": "object",
        "properties": properties,
    });
    if !required.is_empty() {
        params["required"] = json!(required);
    }
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": params,
        }
    })
}

pub fn workspace_root() -> Result<PathBuf> {
    Ok(env::current_dir()?.canonicalize()?)
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
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("Security violation: path traversal blocked for '{trimmed}'"),
        }
    }
    Ok(rel_path)
}

pub fn resolve_existing_path(raw: &str) -> Result<(PathBuf, PathBuf)> {
    let rel_path = validate_relative_path(raw)?;
    let cwd = workspace_root()?;
    let target = cwd.join(rel_path);

    if !target.exists() {
        bail!("Target path does not exist: '{}'", raw.trim());
    }

    let canon = target.canonicalize()?;
    if !canon.starts_with(&cwd) {
        bail!(
            "Security violation: target '{}' escapes workspace",
            raw.trim()
        );
    }
    Ok((cwd, canon))
}

pub fn resolve_new_path(raw: &str) -> Result<(PathBuf, PathBuf)> {
    let rel_path = validate_relative_path(raw)?;
    let cwd = workspace_root()?;
    let target = cwd.join(rel_path);

    if target.exists() {
        bail!(
            "File already exists: {}. Use edit_file instead.",
            raw.trim()
        );
    }

    let mut ancestor = target.parent();
    while let Some(p) = ancestor {
        if p.exists() {
            let canon_parent = p.canonicalize()?;
            if !canon_parent.starts_with(&cwd) {
                bail!("Security violation: target directory escapes workspace via symlink");
            }
            break;
        }
        ancestor = p.parent();
    }

    Ok((cwd, target))
}

pub fn format_process_output(out: &Output, empty_msg: &str) -> String {
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str("[STDERR]\n");
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    if combined.trim().is_empty() {
        empty_msg.to_string()
    } else {
        combined
    }
}
