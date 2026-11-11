use crate::tools::{
    auto_schema, parse_args, resolve_existing_path, run_workspace_command,
};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct ContextArgs {
    /// Optional relative path (defaults to current directory).
    #[serde(default)]
    target_path: String,
}

pub fn schema() -> Value {
    auto_schema::<ContextArgs>(
        "context",
        "Retrieves project directory structures and file contents. Use this to explore the codebase.",
    )
}

pub async fn execute(json_args: &Value, max_output_size: usize) -> Result<String> {
    let args: ContextArgs = parse_args(json_args, "context")?;
    let target = args.target_path.trim();

    let cmd_args: Vec<&str> = if target.is_empty() {
        vec![]
    } else {
        resolve_existing_path(target)?;
        vec![target]
    };

    run_workspace_command(
        "context",
        cmd_args,
        "Context executed successfully (no output).",
        max_output_size,
    )
    .await
}
