//! Approval formatting: generates human-readable previews and unified diffs
//! for tool calls that require user approval.

use super::{
    BlockingTool, ToolCall, ToolContext, dotfiles, edit_file, fs, memory, parse_args, skill,
};
use crate::core::config::Config;
use anyhow::Result;
use serde_json::{Map, Value};
use similar::TextDiff;
use std::fmt::Write as _;

fn format_kv(k: &str, v: &Value) -> String {
    match v.as_str() {
        Some(s) => format!("  {k}: {s}\n"),
        None => format!("  {k}: {v}\n"),
    }
}

fn append_filtered_kvs(out: &mut String, obj: &Map<String, Value>, skip: &[&str]) {
    for (k, v) in obj.iter().filter(|(k, _)| !skip.contains(&k.as_str())) {
        out.push_str(&format_kv(k, v));
    }
}

fn append_warning(out: &mut String, err: &anyhow::Error) {
    let _ = write!(out, "\n--- WARNING ---\n{err}! Execution will fail.\n");
}

fn append_unified_diff(out: &mut String, file_path: &str, old: &str, new: &str) {
    let diff = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{file_path}"), &format!("b/{file_path}"))
        .to_string();

    out.push_str("\n--- UNIFIED DIFF ---\n");
    out.push_str(if diff.trim().is_empty() {
        "(No differences)\n"
    } else {
        &diff
    });
}

fn append_diff_result(out: &mut String, res: Result<(String, String, String, Option<usize>)>) {
    match res {
        Ok((display, original, updated, hunk_count)) => {
            if let Some(n) = hunk_count.filter(|&n| n > 0) {
                let _ = writeln!(out, "  hunks: {n}");
            }
            append_unified_diff(out, &display, &original, &updated);
        }
        Err(e) => append_warning(out, &e),
    }
}

fn format_edit_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
    active_skill: Option<&str>,
) {
    let file_path = obj.get("file_path").and_then(Value::as_str).unwrap_or("");
    let _ = writeln!(out, "  file_path: {file_path}");
    append_filtered_kvs(out, obj, &["file_path", "edits"]);

    let preview =
        parse_args::<edit_file::EditFileArgs>(args, edit_file::Tool::NAME).and_then(|parsed| {
            edit_file::preview_edits(&parsed, &ToolContext::new(config, active_skill, 0)?)
        });

    match preview {
        Ok((_, original, updated, hunk_count)) => {
            let _ = writeln!(out, "  hunks: {hunk_count}");
            append_unified_diff(out, file_path, &original, &updated);
        }
        Err(e) => {
            append_warning(out, &e);
            let (raw_search, raw_replace) =
                parse_args::<edit_file::EditFileArgs>(args, edit_file::Tool::NAME)
                    .map(|parsed| edit_file::fallback_hunk_preview(&parsed))
                    .unwrap_or_default();
            append_unified_diff(out, file_path, &raw_search, &raw_replace);
        }
    }
}

fn format_fs_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
    active_skill: Option<&str>,
) {
    append_filtered_kvs(out, obj, &[]);
    let preview = parse_args::<fs::FsArgs>(args, fs::Tool::NAME).and_then(|parsed| {
        fs::preview_action(&parsed, &ToolContext::new(config, active_skill, 0)?)
    });
    match preview {
        Ok((summary, maybe_diff)) => {
            let _ = write!(out, "\n--- OPERATION ---\n{summary}\n");
            if let Some((file_path, deleted_content)) = maybe_diff {
                append_unified_diff(out, &file_path, &deleted_content, "");
            }
        }
        Err(e) => append_warning(out, &e),
    }
}

fn format_dotfiles_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
) {
    append_filtered_kvs(out, obj, &["edits", "content"]);

    if let Ok(parsed) = parse_args::<dotfiles::DotfilesArgs>(args, dotfiles::Tool::NAME) {
        match parsed.action {
            dotfiles::DotfilesAction::Edit => append_diff_result(
                out,
                dotfiles::preview_edit(&parsed, config)
                    .map(|(_, d, o, u, n)| (d, o, u, Some(n.max(1)))),
            ),
            dotfiles::DotfilesAction::Create => append_diff_result(
                out,
                dotfiles::preview_create(&parsed, config)
                    .map(|(_, d)| (d, String::new(), parsed.content, None)),
            ),
            dotfiles::DotfilesAction::Restore => match dotfiles::preview_restore(&parsed, config) {
                Ok((file_count, diff)) => {
                    let _ = writeln!(out, "  files: {file_count}");
                    out.push_str("\n--- UNIFIED DIFF ---\n");
                    out.push_str(if diff.trim().is_empty() {
                        "(No differences)\n"
                    } else {
                        &diff
                    });
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                Err(e) => append_warning(out, &e),
            },
            _ => {}
        }
    }
}

fn format_skill_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
    active_skill: Option<&str>,
) {
    append_filtered_kvs(out, obj, &["edits", "content", "prompt"]);

    if let Ok(parsed) = parse_args::<skill::SkillArgs>(args, skill::Tool::NAME)
        && let Ok(ctx) = ToolContext::new(config, active_skill, 0)
    {
        match parsed.action {
            skill::SkillAction::Modify => append_diff_result(
                out,
                skill::preview_modify(&parsed, &ctx).map(|(_, d, o, u, n)| (d, o, u, Some(n))),
            ),
            skill::SkillAction::Create => append_diff_result(
                out,
                skill::preview_create(&parsed, &ctx).map(|(_, d, r)| (d, String::new(), r, None)),
            ),
            _ => {}
        }
    }
}

fn format_memory_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
    active_skill: Option<&str>,
) {
    append_filtered_kvs(out, obj, &[]);

    if let Ok(parsed) = parse_args::<memory::MemoryArgs>(args, memory::Tool::NAME)
        && let Ok(ctx) = ToolContext::new(config, active_skill, 0)
        && !matches!(
            parsed.action,
            memory::MemoryAction::List | memory::MemoryAction::Read
        )
    {
        append_diff_result(
            out,
            memory::preview_mutation(&parsed, &ctx).map(|(_, d, o, u, _)| (d, o, u, None)),
        );
    }
}

pub fn format_approval(call: &ToolCall, config: &Config, active_skill: Option<&str>) -> String {
    let name = &call.function.name;
    let args = &call.function.arguments;
    let sig = super::call_signature(call);

    let Some(obj) = args.as_object() else {
        return format!(
            "Tool: {name} [{sig}]\n{}",
            serde_json::to_string_pretty(args).unwrap_or_default()
        );
    };

    let mut out = format!("Tool: {name} (Rule: {sig})\n");
    match name.as_str() {
        "edit_file" => format_edit_approval(&mut out, args, obj, config, active_skill),
        "fs" => format_fs_approval(&mut out, args, obj, config, active_skill),
        "dotfiles" => format_dotfiles_approval(&mut out, args, obj, config),
        "skill" => format_skill_approval(&mut out, args, obj, config, active_skill),
        "memory" => format_memory_approval(&mut out, args, obj, config, active_skill),
        _ => {
            let mut multiline_blocks = Vec::new();
            for (k, v) in obj {
                match v.as_str() {
                    Some(s) if s.contains('\n') || matches!(k.as_str(), "content" | "message") => {
                        multiline_blocks.push((k, s));
                    }
                    _ => out.push_str(&format_kv(k, v)),
                }
            }
            multiline_blocks.sort_by(|a, b| b.0.cmp(a.0));
            for (k, s) in multiline_blocks {
                out.push_str(&format!("\n--- {} ---\n{s}\n", k.to_uppercase()));
            }
        }
    }

    out.trim_end().to_string()
}
