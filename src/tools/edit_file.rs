use super::{
    BlockingTool, ToolContext,
    utils::{
        edit::{EditHunk, apply_hunks_to_str},
        fs::atomic_write_file,
    },
};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{fs, path::PathBuf};

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Relative path to the file in the workspace.
    file_path: String,
    /// Disjoint search/replace hunks to apply atomically to the file in a single pass.
    edits: Vec<EditHunk>,
}

pub fn preview_edits(
    args: &EditFileArgs,
    ctx: &ToolContext,
) -> Result<(PathBuf, String, String, usize)> {
    if args.edits.is_empty() {
        bail!("'edits' array cannot be empty");
    }

    let target_path = ctx.resolve_existing_path(&args.file_path)?;
    let original = fs::read_to_string(&target_path).context("Error reading file")?;
    let updated = apply_hunks_to_str(&original, args.file_path.trim(), &args.edits)?;

    Ok((target_path, original, updated, args.edits.len()))
}

pub fn fallback_hunk_preview(args: &EditFileArgs) -> (String, String) {
    let join_field = |f: fn(&EditHunk) -> &str| {
        args.edits
            .iter()
            .map(f)
            .collect::<Vec<_>>()
            .join("\n// ---\n")
    };
    (
        join_field(|h| &h.search_string),
        join_field(|h| &h.replace_string),
    )
}

impl BlockingTool for Tool {
    const NAME: &'static str = "edit_file";
    const DESCRIPTION: &'static str = "Edit a file by applying one or more disjoint, exact, unique search/replace hunks atomically.";
    type Args = EditFileArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let (target_path, _, updated, hunk_count) = preview_edits(&args, ctx)?;
        atomic_write_file(&target_path, updated).context("Error writing file")?;

        let path = args.file_path.trim();
        if hunk_count == 1 {
            Ok(format!("Successfully edited {path}"))
        } else {
            Ok(format!("Successfully applied {hunk_count} edits to {path}"))
        }
    }
}
