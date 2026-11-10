use crate::tools::{
    function_schema, parse_args, resolve_existing_path, run_workspace_command,
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

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
    )
}
