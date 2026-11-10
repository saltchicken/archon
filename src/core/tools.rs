use crate::core::config::Config;
use crate::tools;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
        let mut multiline_blocks = Vec::new();

        for (k, v) in obj {
            match v.as_str() {
                Some(s)
                    if s.contains('\n')
                        || k == "search_string"
                        || k == "replace_string"
                        || k == "content" =>
                {
                    multiline_blocks.push((k, s));
                }
                Some(s) => out.push_str(&format!("  {k}: {s}\n")),
                None => out.push_str(&format!("  {k}: {v}\n")),
            }
        }

        // Sort reverse-alphabetically so `search_string` appears before `replace_string`
        multiline_blocks.sort_by(|a, b| b.0.cmp(a.0));

        for (k, s) in multiline_blocks {
            let header = k.to_uppercase();
            out.push_str(&format!("\n--- {header} ---\n{s}\n"));
        }

        out.trim_end().to_string()
    }

    pub async fn execute(&self, call: &ToolCall) -> (String, String, bool) {
        let name = call.function.name.clone();
        let args = call.function.arguments.clone();
        let allowed = self.allowed_commands.clone();
        let max_size = self.max_output_size;

        info!("Executing tool: {}", name);
        debug!("Tool arguments: {}", args);

        let name_clone = name.clone();
        let res = tokio::task::spawn_blocking(move || tools::execute(&name_clone, &args, &allowed))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("Tool task panicked: {e}")));

        match res {
            Ok(mut out) => {
                let mut truncated = false;
                if out.len() > max_size {
                    truncated = true;
                    let mut truncate_at = max_size;
                    while truncate_at > 0 && !out.is_char_boundary(truncate_at) {
                        truncate_at -= 1;
                    }
                    let dropped = out.len() - truncate_at;
                    out.truncate(truncate_at);
                    out.push_str(&format!(
                        "\n\n[TRUNCATED: Output exceeded the {max_size} byte limit — {dropped} bytes were dropped. Content is incomplete.]"
                    ));
                }

                info!("Tool '{}' executed successfully. Truncated: {}", name, truncated);
                debug!("Tool output snippet: {:.200}", out.replace('\n', " "));
                (out.clone(), format!("[Tool Result: {name}]\n{out}"), truncated)
            },
            Err(e) => {
                error!("Tool '{}' failed: {}", name, e);
                (format!("Error: {e}"), format!("[Tool Error: {name}]\n{e}"), false)
            },
        }
    }
}
