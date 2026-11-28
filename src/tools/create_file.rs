use super::{
    BlockingTool, ToolContext,
    executor::utils::{fs::atomic_write_file, path::ensure_parent_dir},
};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct CreateFileArgs {
    /// Relative path for the brand-new file in the workspace (must not already exist).
    file_path: String,
    /// Complete initial contents of the new file.
    content: String,
}

impl BlockingTool for Tool {
    const NAME: &'static str = "create_file";
    const DESCRIPTION: &'static str = "Create a brand-new file in the workspace. Fails if the file already exists—always use `edit_file` to modify or overwrite existing files, and check with `read_file` or `search` first if unsure whether a file exists.";
    const PARAM_HINT: &'static str = "<file_path> <content>";
    const SHORT_SUMMARY: &'static str =
        "Create a brand-new file in the workspace (fails if it already exists).";
    type Args = CreateFileArgs;

    fn positional_keys(
        _token: &str,
        _map: &serde_json::Map<String, serde_json::Value>,
    ) -> &'static [&'static str] {
        &["file_path", "content"]
    }

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let target_path = ctx.resolve_new_path(&args.file_path)?;
        ensure_parent_dir(&target_path, &ctx.root, "created path")?;
        atomic_write_file(&target_path, &args.content).context("Error writing new file")?;
        Ok(format!("Successfully created {}", args.file_path.trim()))
    }
}
