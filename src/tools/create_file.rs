use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
};

pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "create_file",
            "description": "Create a new file in the workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["file_path", "content"]
            }
        }
    })
}

fn resolve_new_secure_path(rel: &str) -> Result<PathBuf> {
    let trimmed = rel.trim();
    if trimmed.is_empty() {
        bail!("Security violation: file_path cannot be empty");
    }

    let rel_path = Path::new(trimmed);
    if rel_path.is_absolute() {
        bail!("Security violation: path must be relative to current working directory");
    }

    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("Security violation: path traversal components are not allowed"),
        }
    }

    let cwd = env::current_dir()?.canonicalize()?;
    let target = cwd.join(rel_path);

    if target.exists() {
        bail!("File already exists: {trimmed}. Use edit_file instead.");
    }

    // Verify the deepest existing ancestor resides within the workspace (prevents symlink escapes)
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

    Ok(target)
}

pub fn execute(json_args: &Value) -> Result<String> {
    let path_str = json_args
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let content = json_args
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let target_path = resolve_new_secure_path(path_str)?;
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).context("Error creating parent directories")?;
    }

    // Double-check parent canonical path after creation
    let cwd = env::current_dir()?.canonicalize()?;
    if let Some(parent) = target_path.parent() {
        let canon_parent = parent.canonicalize()?;
        if !canon_parent.starts_with(&cwd) {
            bail!("Security violation: created path escapes workspace");
        }
    }

    fs::write(&target_path, content).context("Error writing new file")?;
    Ok(format!("Successfully created {}", path_str.trim()))
}
