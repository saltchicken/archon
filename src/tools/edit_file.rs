use super::{
    BlockingTool, SessionToolState, ToolContext, approval, parse_args,
    utils::{
        edit::{EditHunk, apply_hunks_to_str},
        fs::atomic_write_file,
    },
};
use crate::core::config::Config;
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{fmt::Write as _, fs, path::PathBuf};

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Relative path to the file in the workspace.
    file_path: String,
    /// Disjoint search/replace hunks to apply atomically to the file in a single pass.
    edits: Vec<EditHunk>,
}

fn preview_edits(
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

fn fallback_hunk_preview(args: &EditFileArgs) -> (String, String) {
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
    const DESCRIPTION: &'static str = "Modify an existing file in the workspace by applying one or more disjoint, exact, unique search/replace hunks atomically. Always use this instead of `create_file` when the file already exists.";
    const PARAM_HINT: &'static str = "{\"file_path\": \"...\", \"edits\": [...]}";
    const SHORT_SUMMARY: &'static str =
        "Apply exact search/replace edits to an existing workspace file.";
    type Args = EditFileArgs;

    fn format_approval(
        out: &mut String,
        args: &serde_json::Value,
        obj: &serde_json::Map<String, serde_json::Value>,
        config: &Config,
        active_skill: Option<&str>,
    ) {
        let file_path = obj
            .get("file_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let _ = writeln!(out, "  file_path: {file_path}");
        approval::append_filtered_kvs(out, obj, &["file_path", "edits"]);

        let preview = parse_args::<EditFileArgs>(args, Self::NAME).and_then(|parsed| {
            preview_edits(
                &parsed,
                &ToolContext::new(config, active_skill, None, &SessionToolState::default(), 0)?,
            )
        });

        match preview {
            Ok((_, original, updated, hunk_count)) => {
                let _ = writeln!(out, "  hunks: {hunk_count}");
                approval::append_unified_diff(out, file_path, &original, &updated);
            }
            Err(e) => {
                approval::append_warning(out, &e);
                let (raw_search, raw_replace) = parse_args::<EditFileArgs>(args, Self::NAME)
                    .map(|parsed| fallback_hunk_preview(&parsed))
                    .unwrap_or_default();
                approval::append_unified_diff(out, file_path, &raw_search, &raw_replace);
            }
        }
    }

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
