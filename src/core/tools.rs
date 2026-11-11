use crate::tools;
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

fn format_edit_approval(mut out: String, obj: &Map<String, Value>) -> String {
    let get_str = |key| obj.get(key).and_then(Value::as_str).unwrap_or("");
    let file_path = get_str("file_path");
    let search = get_str("search_string");
    let replace = get_str("replace_string");

    out.push_str(&format!("  file_path: {file_path}\n"));
    for (k, v) in obj {
        if !matches!(k.as_str(), "file_path" | "search_string" | "replace_string") {
            out.push_str(&format_kv(k, v));
        }
    }

    match tools::edit_file::apply_edit(file_path, search, replace) {
        Ok((_, original, updated)) => append_unified_diff(&mut out, file_path, &original, &updated),
        Err(e) => {
            out.push_str(&format!("\n--- WARNING ---\n{e}! Execution will fail.\n"));
            append_unified_diff(&mut out, file_path, search, replace);
        }
    }

    out.trim_end().to_string()
}

pub fn format_approval(call: &ToolCall) -> String {
    let name = &call.function.name;
    let args = &call.function.arguments;

    let Some(obj) = args.as_object() else {
        return format!(
            "Tool: {name}\n{}",
            serde_json::to_string_pretty(args).unwrap_or_default()
        );
    };

    let mut out = format!("Tool: {name}\n");
    if name == "edit_file" {
        return format_edit_approval(out, obj);
    }

    let mut multiline_blocks = Vec::new();
    for (k, v) in obj {
        match v.as_str() {
            Some(s)
                if s.contains('\n')
                    || matches!(k.as_str(), "search_string" | "replace_string" | "content") =>
            {
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

pub async fn execute(
    call: &ToolCall,
    allowed_commands: &[String],
    max_size: usize,
) -> (String, bool) {
    let name = &call.function.name;
    let args = &call.function.arguments;

    info!("Executing tool: {}", name);
    debug!("Tool arguments: {}", args);

    match tools::execute(name, args, allowed_commands, max_size).await {
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
