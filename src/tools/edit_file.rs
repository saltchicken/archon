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
            "name": "edit_file",
            "description": "Edit a file by replacing an exact, unique search string with a replacement string.",
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
    let trimmed = rel.trim();
    if trimmed.is_empty() {
        bail!("Security violation: path must be non-empty and relative to current working directory");
    }

    let rel_path = Path::new(trimmed);
    if rel_path.is_absolute() {
        bail!("Security violation: path must be relative to current working directory");
    }

    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("Security violation: path traversal is not allowed"),
        }
    }

    let cwd = env::current_dir()?.canonicalize()?;
    let target = cwd.join(rel_path);

    if !target.exists() {
        bail!("File not found: {}", trimmed);
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

    if search_str.is_empty() {
        bail!("search_string cannot be empty");
    }

    let target_path = resolve_secure_path(path_str)?;
    let content = fs::read_to_string(&target_path).context("Error reading file")?;

    let match_count = content.matches(search_str).count();
    if match_count == 0 {
        bail!("Exact search_string not found in {}", path_str);
    }
    if match_count > 1 {
        bail!(
            "search_string matched {} times in {}. Provide more surrounding context to make it unique.",
            match_count,
            path_str
        );
    }

    let updated = content.replacen(search_str, replace_str, 1);
    fs::write(&target_path, updated).context("Error writing file")?;

    Ok(format!("Successfully edited {}", path_str))
}
