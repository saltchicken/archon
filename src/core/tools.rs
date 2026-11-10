use crate::core::config::Config;
use crate::tools;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

impl ToolRegistry {
    pub fn new(config: &Config) -> Self {
        Self {
            allowed_commands: config.app.allowed_commands.clone(),
            schemas: tools::schemas(),
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

    pub async fn execute(&self, call: &ToolCall) -> (String, String) {
        let name = call.function.name.clone();
        let args = call.function.arguments.clone();
        let allowed = self.allowed_commands.clone();

        let name_clone = name.clone();
        let res = tokio::task::spawn_blocking(move || tools::execute(&name_clone, &args, &allowed))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("Tool task panicked: {e}")));

        match res {
            Ok(out) => (out.clone(), format!("[Tool Result: {name}]\n{out}")),
            Err(e) => (format!("Error: {e}"), format!("[Tool Error: {name}]\n{e}")),
        }
    }
}
