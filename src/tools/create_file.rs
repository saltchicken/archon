use crate::tools::{auto_schema, ensure_within_workspace, parse_args, resolve_new_path};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fs;

#[derive(Deserialize, JsonSchema)]
struct CreateFileArgs {
    file_path: String,
    content: String,
}

pub fn schema() -> Value {
    auto_schema::<CreateFileArgs>(
        "create_file",
        "Create a new file in the workspace.",
    )
}

pub fn execute(json_args: &Value) -> Result<String> {
    let args: CreateFileArgs = parse_args(json_args, "create_file")?;
    let (cwd, target_path) = resolve_new_path(&args.file_path)?;

    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).context("Error creating parent directories")?;
        ensure_within_workspace(parent, &cwd, "created path")?;
    }

    fs::write(&target_path, &args.content).context("Error writing new file")?;
    Ok(format!("Successfully created {}", args.file_path.trim()))
}
