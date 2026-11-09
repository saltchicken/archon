use crate::core::config::Config;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, fs, path::PathBuf};
use tokio::process::Command;

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
}

impl ToolRegistry {
    pub fn new(config: &Config) -> Self {
        Self {
            allowed_commands: config.app.allowed_commands.clone(),
        }
    }

    pub fn schemas(&self) -> Vec<Value> {
        let allowed = self.allowed_commands.join(", ");
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "execute_command",
                    "description": format!("Executes an allowed shell command. Allowed base commands: {allowed}. No pipes or redirects."),
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string", "description": "The exact command to execute." }
                        },
                        "required": ["command"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "context",
                    "description": "Retrieves project directory structures and file contents. Use this to explore the codebase.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "target_path": { "type": "string", "description": "Optional relative path (defaults to current directory)." }
                        }
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "edit_file",
                    "description": "Edit a file by replacing an exact search string with a replacement string.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "file_path": { "type": "string" },
                            "search_string": { "type": "string" },
                            "replace_string": { "type": "string" }
                        },
                        "required": ["file_path", "search_string", "replace_string"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "create_file",
                    "description": "Create a new file in the workspace.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "file_path": { "type": "string" },
                            "content": { "type": "string" }
                        },
                        "required": ["file_path", "content"]
                    }
                }
            }),
        ]
    }

    pub fn format_approval(&self, call: &ToolCall) -> String {
        let args = &call.function.arguments;
        match call.function.name.as_str() {
            "execute_command" => format!(
                "$ {}",
                args.get("command").and_then(|v| v.as_str()).unwrap_or("")
            ),
            "context" => format!(
                "$ context {}",
                args.get("target_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ),
            "edit_file" => format!(
                "Edit: {}\n--- SEARCH ---\n{}\n--- REPLACE ---\n{}",
                args.get("file_path").and_then(|v| v.as_str()).unwrap_or(""),
                args.get("search_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                args.get("replace_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ),
            "create_file" => format!(
                "Create: {}\n--- CONTENT ---\n{}",
                args.get("file_path").and_then(|v| v.as_str()).unwrap_or(""),
                args.get("content").and_then(|v| v.as_str()).unwrap_or("")
            ),
            _ => serde_json::to_string_pretty(args).unwrap_or_default(),
        }
    }

    pub async fn execute(&self, call: &ToolCall) -> (String, String) {
        let res = match call.function.name.as_str() {
            "execute_command" => self.exec_cmd(&call.function.arguments).await,
            "context" => self.context_cmd(&call.function.arguments).await,
            "edit_file" => self.edit_file(&call.function.arguments),
            "create_file" => self.create_file(&call.function.arguments),
            other => Err(anyhow::anyhow!("Unknown tool: {other}")),
        };

        match res {
            Ok(out) => (
                out.clone(),
                format!("[Tool Result: {}]\n{}", call.function.name, out),
            ),
            Err(e) => (
                format!("Error: {e}"),
                format!("[Tool Error: {}]\n{e}", call.function.name),
            ),
        }
    }

    async fn exec_cmd(&self, args: &Value) -> Result<String> {
        let raw = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if raw.is_empty() {
            bail!("Empty command");
        }
        for bad in [';', '&', '|', '`', '$', '<', '>'] {
            if raw.contains(bad) {
                bail!("Forbidden shell operator '{bad}'");
            }
        }
        let parts = shlex::split(raw).context("Malformed command string")?;
        let Some((base, rest)) = parts.split_first() else {
            bail!("Empty parsed command");
        };
        if !self.allowed_commands.contains(base) {
            bail!("Command '{base}' is not in allowlist");
        }
        for arg in rest {
            if arg.starts_with('/') || arg.contains("..") {
                bail!("Path traversal blocked for argument '{arg}'");
            }
        }

        let out = Command::new(base).args(rest).output().await?;
        let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.stderr.is_empty() {
            combined.push_str("\n[STDERR]\n");
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        if combined.trim().is_empty() {
            Ok("Command executed successfully (no output).".into())
        } else {
            Ok(combined)
        }
    }

    async fn context_cmd(&self, args: &Value) -> Result<String> {
        let target = args
            .get("target_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if target.starts_with('/') || target.contains("..") {
            bail!(
                "Path traversal blocked for target '{target}'. Must operate in current directory."
            );
        }

        let mut cmd = Command::new("context");
        if !target.is_empty() {
            cmd.arg(target);
        }
        let out = cmd.output().await?;
        let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.stderr.is_empty() {
            combined.push_str("\n[STDERR]\n");
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        if combined.trim().is_empty() {
            Ok("Context executed successfully (no output).".into())
        } else {
            Ok(combined)
        }
    }

    fn edit_file(&self, args: &Value) -> Result<String> {
        let path_str = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
        let search = args
            .get("search_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let replace = args
            .get("replace_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let path = resolve_secure_path(path_str, true)?;
        let content = fs::read_to_string(&path)?;
        if !content.contains(search) {
            bail!("Exact search_string not found in {}", path_str);
        }
        let updated = content.replacen(search, replace, 1);
        fs::write(&path, updated)?;
        Ok(format!("Successfully edited {path_str}"))
    }

    fn create_file(&self, args: &Value) -> Result<String> {
        let path_str = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");

        let path = resolve_secure_path(path_str, false)?;
        if path.exists() {
            bail!("File already exists. Use edit_file instead.");
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content)?;
        Ok(format!("Successfully created {path_str}"))
    }
}

fn resolve_secure_path(rel: &str, must_exist: bool) -> Result<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains("..") {
        bail!("Security violation: path must be relative to current working directory");
    }
    let cwd = env::current_dir()?.canonicalize()?;
    let joined = cwd.join(rel);
    if must_exist {
        let canon = joined.canonicalize()?;
        if !canon.starts_with(&cwd) {
            bail!("Security violation: path escapes workspace");
        }
        Ok(canon)
    } else {
        Ok(joined)
    }
}
