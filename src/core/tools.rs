use crate::core::config::Config;
use crate::tools;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use similar::TextDiff;
use tracing::{debug, error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: Value,
}

pub struct ToolRegistry {
    allowed_commands: Vec<String>,
    schemas: Vec<Value>,
    max_output_size: usize,
}

impl ToolRegistry {
    pub fn new(config: &Config) -> Self {
        Self {
            allowed_commands: config.app.allowed_commands.clone(),
            schemas: tools::schemas(),
            max_output_size: config.app.max_tool_output_size,
        }
    }

    pub fn schemas(&self) -> Vec<Value> {
        self.schemas.clone()
    }

    pub fn format_approval(&self, call: &ToolCall) -> String {
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
            let file_path = obj.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let search = obj
                .get("search_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let replace = obj
                .get("replace_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            out.push_str(&format!("  file_path: {file_path}\n"));
            for (k, v) in obj {
                if !matches!(
                    k.as_str(),
                    "file_path" | "search_string" | "replace_string"
                ) {
                    match v.as_str() {
                        Some(s) => out.push_str(&format!("  {k}: {s}\n")),
                        None => out.push_str(&format!("  {k}: {v}\n")),
                    }
                }
            }

            let diff = TextDiff::from_lines(search, replace)
                .unified_diff()
                .context_radius(3)
                .header(&format!("a/{file_path}"), &format!("b/{file_path}"))
                .to_string();

            out.push_str("\n--- UNIFIED DIFF ---\n");
            if diff.trim().is_empty() {
                out.push_str("(No differences)\n");
            } else {
                out.push_str(&diff);
            }
            return out.trim_end().to_string();
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
                Some(s) => out.push_str(&format!("  {k}: {s}\n")),
                None => out.push_str(&format!("  {k}: {v}\n")),
            }
        }

        multiline_blocks.sort_by(|a, b| b.0.cmp(a.0));

        for (k, s) in multiline_blocks {
            out.push_str(&format!("\n--- {} ---\n{s}\n", k.to_uppercase()));
        }

        out.trim_end().to_string()
    }

    pub async fn execute(&self, call: &ToolCall) -> (String, bool) {
        let name = call.function.name.clone();
        let args = call.function.arguments.clone();
        let allowed = self.allowed_commands.clone();
        let max_size = self.max_output_size;

        info!("Executing tool: {}", name);
        debug!("Tool arguments: {}", args);

        let res = tools::execute(&name, &args, &allowed, max_size).await;

        match res {
            Ok(mut out) => {
                let truncated = out.len() > max_size;
                if truncated {
                    let truncate_at = (0..=max_size)
                        .rev()
                        .find(|&i| out.is_char_boundary(i))
                        .unwrap_or(0);
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
}
