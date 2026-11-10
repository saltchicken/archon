use crate::tools::{auto_schema, parse_args, resolve_existing_path};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fs;

#[derive(Deserialize, JsonSchema)]
struct ReadFileArgs {
    file_path: String,
}

pub fn schema() -> Value {
    auto_schema::<ReadFileArgs>(
        "read_file",
        "Read the exact contents of a file in the workspace.",
    )
}

pub fn execute(json_args: &Value) -> Result<String> {
    let args: ReadFileArgs = parse_args(json_args, "read_file")?;
    let (_, target_path) = resolve_existing_path(&args.file_path)?;

    if !target_path.is_file() {
        bail!("Target is not a file: {}", args.file_path.trim());
    }

    fs::read_to_string(&target_path).context("Error reading file")
}
