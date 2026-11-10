use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{env, fs, path::PathBuf};

pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "edit_file",
            "description": "Edit a file by replacing an exact search string with a replacement string.",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_path": { "type": "string" },
                    "search_string": { "type": "string" },
                    "replace_string": { "type": "string" }
                },
                "required": ["file_path", "search_string", "replace_string"]
            }
        }
    })
}

fn resolve_secure_path(rel: &str) -> Result<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains("..") {
        bail!("Security violation: path must be relative to current working directory");
    }
    let cwd = env::current_dir()?.canonicalize()?;
    let target = cwd.join(rel);

    if !target.exists() {
        bail!("File not found: {}", rel);
    }

    let canon = target.canonicalize()?;
    if !canon.starts_with(&cwd) {
        bail!("Security violation: path escapes workspace");
    }
    Ok(canon)
}

pub fn execute(json_args: &Value) -> Result<String> {
    let path_str = json_args
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let search_str = json_args
        .get("search_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let replace_str = json_args
        .get("replace_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let target_path = resolve_secure_path(path_str)?;
    let content = fs::read_to_string(&target_path).context("Error reading file")?;

    if !content.contains(search_str) {
        bail!("Exact search_string not found in {}", path_str);
    }

    let updated = content.replacen(search_str, replace_str, 1);
    fs::write(&target_path, updated).context("Error writing file")?;

    Ok(format!("Successfully edited {}", path_str))
}
