use crate::{core::config::Config, tools};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use similar::TextDiff;
use tracing::{debug, error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: ToolFunction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: Value,
}

/// Computes a canonical signature for a tool call, such as:
/// - `"read_file"`
/// - `"fs:list"`
/// - `"git:status"`
/// - `"cargo:check"`
/// - `"execute_command:ls"`
pub fn call_signature(call: &ToolCall) -> String {
    let name = call.function.name.as_str();
    let args = &call.function.arguments;

    match name {
        "git" | "cargo" | "fs" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if action.is_empty() {
                name.to_string()
            } else {
                format!("{name}:{action}")
            }
        }
        "execute_command" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            let base = shlex::split(cmd)
                .and_then(|parts| parts.into_iter().next())
                .unwrap_or_default();
            if base.is_empty() {
                "execute_command".to_string()
            } else {
                format!("execute_command:{base}")
            }
        }
        _ => name.to_string(),
    }
}

/// Checks whether a tool call matches any rule in `rules`.
/// Supported patterns:
/// - `"*"` (matches all tools)
/// - `"<tool_name>"` or `"<tool_name>:*"` (matches any invocation of `<tool_name>`)
/// - `"<tool_name>:<sub_action>"` (e.g. `"git:status"`, `"fs:list"`, `"execute_command:ls"`)
pub fn is_auto_approved<'a, I>(call: &ToolCall, rules: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    let name = call.function.name.as_str();
    let sig = call_signature(call);

    rules.into_iter().any(|rule| {
        let r = rule.trim();
        if r.is_empty() {
            return false;
        }
        if r == "*" || r == name || r == sig {
            return true;
        }
        if let Some(prefix) = r.strip_suffix(":*") {
            return name == prefix || sig.starts_with(&format!("{prefix}:"));
        }
        false
    })
}

fn format_kv(k: &str, v: &Value) -> String {
    match v.as_str() {
        Some(s) => format!("  {k}: {s}\n"),
        None => format!("  {k}: {v}\n"),
    }
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

fn format_edit_approval(mut out: String, args: &Value, obj: &Map<String, Value>) -> String {
    let file_path = obj.get("file_path").and_then(Value::as_str).unwrap_or("");

    out.push_str(&format!("  file_path: {file_path}\n"));
    for (k, v) in obj {
        if !matches!(k.as_str(), "file_path" | "edits") {
            out.push_str(&format_kv(k, v));
        }
    }

    match tools::edit_file::apply_edits_from_value(args) {
        Ok((_, original, updated, hunk_count)) => {
            out.push_str(&format!("  hunks: {hunk_count}\n"));
            append_unified_diff(&mut out, file_path, &original, &updated);
        }
        Err(e) => {
            out.push_str(&format!("\n--- WARNING ---\n{e}! Execution will fail.\n"));
            let (raw_search, raw_replace) = tools::edit_file::fallback_hunk_preview(args);
            append_unified_diff(&mut out, file_path, &raw_search, &raw_replace);
        }
    }

    out.trim_end().to_string()
}

fn format_fs_approval(mut out: String, args: &Value, obj: &Map<String, Value>) -> String {
    for (k, v) in obj {
        out.push_str(&format_kv(k, v));
    }

    match tools::fs::preview_action(args) {
        Ok((summary, maybe_diff)) => {
            out.push_str(&format!("\n--- OPERATION ---\n{summary}\n"));
            if let Some((file_path, deleted_content)) = maybe_diff {
                append_unified_diff(&mut out, &file_path, &deleted_content, "");
            }
        }
        Err(e) => {
            out.push_str(&format!("\n--- WARNING ---\n{e}! Execution will fail.\n"));
        }
    }

    out.trim_end().to_string()
}

pub fn format_approval(call: &ToolCall) -> String {
    let name = &call.function.name;
    let args = &call.function.arguments;
    let sig = call_signature(call);

    let Some(obj) = args.as_object() else {
        return format!(
            "Tool: {name} [{sig}]\n{}",
            serde_json::to_string_pretty(args).unwrap_or_default()
        );
    };

    let mut out = format!("Tool: {name} (Rule: {sig})\n");
    if name == "edit_file" {
        return format_edit_approval(out, args, obj);
    }
    if name == "fs" {
        return format_fs_approval(out, args, obj);
    }

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

    out.trim_end().to_string()
}

pub async fn execute(call: &ToolCall, config: &Config, max_size: usize) -> (String, bool) {
    let name = &call.function.name;
    let args = &call.function.arguments;

    info!("Executing tool: {}", name);
    debug!("Tool arguments: {}", args);

    match tools::execute(name, args, config, max_size).await {
        Ok(mut out) => {
            let truncated = out.len() > max_size;
            if truncated {
                let truncate_at = out.floor_char_boundary(max_size);
                let dropped = out.len() - truncate_at;
                out.truncate(truncate_at);
                out.push_str(&format!(
                    "\n\n[TRUNCATED: Output exceeded the {max_size} byte limit — {dropped} bytes were dropped. Content is incomplete.]"
                ));
            }

            info!(
                "Tool '{}' executed successfully. Truncated: {}",
                name, truncated
            );
            let snippet: String = out.replace('\n', " ").chars().take(200).collect();
            debug!("Tool output snippet: {}", snippet);
            (out, truncated)
        }
        Err(e) => {
            error!("Tool '{}' failed: {}", name, e);
            (format!("Error: {e}"), false)
        }
    }
}
