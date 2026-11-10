use crate::tools::{function_schema, parse_args, resolve_new_path};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;

#[derive(Deserialize)]
struct CreateFileArgs {
    file_path: String,
    content: String,
}

pub fn schema() -> Value {
    function_schema(
        "create_file",
        "Create a new file in the workspace.",
        json!({
            "file_path": { "type": "string" },
            "content": { "type": "string" }
        }),
        &["file_path", "content"],
    )
}

pub fn execute(json_args: &Value) -> Result<String> {
    let args: CreateFileArgs = parse_args(json_args, "create_file")?;
    let (cwd, target_path) = resolve_new_path(&args.file_path)?;

    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).context("Error creating parent directories")?;
        if !parent.canonicalize()?.starts_with(&cwd) {
            bail!("Security violation: created path escapes workspace");
        }
    }

    fs::write(&target_path, &args.content).context("Error writing new file")?;
    Ok(format!("Successfully created {}", args.file_path.trim()))
}
