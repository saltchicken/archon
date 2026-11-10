use crate::tools::{
    format_process_output, function_schema, parse_args, resolve_existing_path, workspace_root,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::process;

#[derive(Deserialize)]
struct ContextArgs {
    #[serde(default)]
    target_path: String,
}

pub fn schema() -> Value {
    function_schema(
        "context",
        "Retrieves project directory structures and file contents. Use this to explore the codebase.",
        json!({
            "target_path": {
                "type": "string",
                "description": "Optional relative path (defaults to current directory)."
            }
        }),
        &[],
    )
}

pub fn execute(json_args: &Value) -> Result<String> {
    let args: ContextArgs = parse_args(json_args, "context")?;
    let target = args.target_path.trim();

    let cwd = workspace_root()?;
    let mut cmd = process::Command::new("context");
    cmd.current_dir(&cwd);

    if !target.is_empty() {
        resolve_existing_path(target)?;
        cmd.arg(target);
    }

    let out = cmd.output().context("Error executing context")?;
    Ok(format_process_output(
        &out,
        "Context executed successfully (no output).",
    ))
}
