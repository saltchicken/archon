//! Approval formatting: generates human-readable previews and unified diffs
//! for tool calls that require user approval.

use super::ToolCall;
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

pub(super) fn append_filtered_kvs(out: &mut String, obj: &Map<String, Value>, skip: &[&str]) {
    for (k, v) in obj.iter().filter(|(k, _)| !skip.contains(&k.as_str())) {
        out.push_str(&format_kv(k, v));
    }
}

pub(super) fn append_warning(out: &mut String, err: &anyhow::Error) {
    let _ = write!(out, "\n--- WARNING ---\n{err}! Execution will fail.\n");
}

pub(super) fn append_unified_diff(out: &mut String, file_path: &str, old: &str, new: &str) {
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

pub(super) fn append_diff_result(
    out: &mut String,
    res: Result<(String, String, String, Option<usize>)>,
) {
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

pub(super) fn format_generic_approval(out: &mut String, obj: &Map<String, Value>) {
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
    super::format_tool_approval_body(name, &mut out, args, obj, config, active_skill);

    out.trim_end().to_string()
}
