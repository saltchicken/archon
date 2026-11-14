use super::{
    BlockingTool, ToolContext,
    utils::{atomic_write_file, ensure_parent_dir},
};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct CreateFileArgs {
    file_path: String,
    content: String,
}

impl BlockingTool for Tool {
    const NAME: &'static str = "create_file";
    const DESCRIPTION: &'static str = "Create a new file in the workspace.";
    type Args = CreateFileArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let target_path = ctx.resolve_new_path(&args.file_path)?;
        ensure_parent_dir(&target_path, &ctx.root, "created path")?;
        atomic_write_file(&target_path, &args.content).context("Error writing new file")?;
        Ok(format!("Successfully created {}", args.file_path.trim()))
    }
}
