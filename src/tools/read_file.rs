use super::{BlockingTool, ToolContext, executor::utils::fs::read_file_contents};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Path to the file in the workspace (or inside a skill directory). Also
    /// accepts an absolute path into a directory configured via
    /// `app.read_roots` (e.g. `~/.cargo/registry/src`).
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
    const DESCRIPTION: &'static str = "Read the contents of a file in the workspace or inside a skill directory (e.g. SKILL.md), optionally restricted to a 1-based line range [start_line, end_line].";
    const PARAM_HINT: &'static str = "<file_path> [start_line] [end_line]";
    const SHORT_SUMMARY: &'static str =
        "Read a workspace, active-skill, or read_roots file with optional 1-based line range.";
    type Args = ReadFileArgs;

    fn apply_positional_arg(token: &str, map: &mut serde_json::Map<String, serde_json::Value>) {
        if map.contains_key("file_path") {
            if let Ok(n) = token.parse::<usize>()
                && let Some(&k) = ["start_line", "end_line"]
                    .iter()
                    .find(|&&k| !map.contains_key(k))
            {
                map.insert(k.into(), serde_json::Value::Number(n.into()));
            }
        } else {
            map.insert(
                "file_path".into(),
                serde_json::Value::String(token.to_string()),
            );
        }
    }

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
