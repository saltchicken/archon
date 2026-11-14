use super::{BlockingTool, ToolContext, utils::fs::read_file_contents};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Path to the file in the workspace (or inside a skill directory).
    file_path: String,
    /// Optional 1-based starting line number (inclusive). If set, output lines are prefixed with line numbers.
    #[serde(default)]
    start_line: Option<usize>,
    /// Optional 1-based ending line number (inclusive). If set, output lines are prefixed with line numbers.
    #[serde(default)]
    end_line: Option<usize>,
}

impl BlockingTool for Tool {
    const NAME: &'static str = "read_file";
    const DESCRIPTION: &'static str = "Read the contents of a file in the workspace or a skill reference file, optionally restricted to a 1-based line range [start_line, end_line].";
    type Args = ReadFileArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let target_path = ctx.resolve_readable_path(&args.file_path)?;
        read_file_contents(
            &target_path,
            args.file_path.trim(),
            args.start_line,
            args.end_line,
            ctx.max_output_size,
        )
    }
}
