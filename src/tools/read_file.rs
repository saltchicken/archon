use crate::tools::{auto_schema, parse_args, resolve_existing_path};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{fs::File, io::Read};

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

pub fn execute(json_args: &Value, max_output_size: usize) -> Result<String> {
    let args: ReadFileArgs = parse_args(json_args, "read_file")?;
    let (_, target_path) = resolve_existing_path(&args.file_path)?;

    if !target_path.is_file() {
        bail!("Target is not a file: {}", args.file_path.trim());
    }

    let file = File::open(&target_path).context("Error opening file")?;
    let cap = (max_output_size as u64).saturating_add(1);
    let mut buf = Vec::new();
    file.take(cap)
        .read_to_end(&mut buf)
        .context("Error reading file")?;

    Ok(String::from_utf8_lossy(&buf).into_owned())
}
