use crate::core::config::Config;
use crate::tools::{context, create_file, edit_file, execute_command, format_process_output};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{io::AsyncWriteExt, process::Command};

const BUILTIN_TOOLS: [&str; 4] = ["context", "create_file", "edit_file", "execute_command"];

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
    cached_schemas: Vec<Value>,
}

impl ToolRegistry {
    pub fn new(config: &Config) -> Self {
        Self {
            allowed_commands: config.app.allowed_commands.clone(),
            cached_schemas: Self::discover_schemas(),
        }
    }

    /// Returns the external tool directories in order of priority:
    /// 1. Project-specific ./tools/python
    /// 2. Legacy flat ./tools
    /// 3. Global ~/.local/share/archon/tools
    pub fn tool_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(cwd) = env::current_dir() {
            dirs.push(cwd.join("tools").join("python"));
            dirs.push(cwd.join("tools"));
        }
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".local/share/archon/tools"));
        }
        dirs
    }

    pub fn builtin_schema(name: &str) -> Option<Value> {
        match name {
            "context" => Some(context::schema()),
            "create_file" => Some(create_file::schema()),
            "edit_file" => Some(edit_file::schema()),
            "execute_command" => Some(execute_command::schema()),
            _ => None,
        }
    }

    pub fn execute_builtin(
        name: &str,
        args: &Value,
        allowed_commands: &[String],
    ) -> Option<Result<String>> {
        match name {
            "context" => Some(context::execute(args)),
            "create_file" => Some(create_file::execute(args)),
            "edit_file" => Some(edit_file::execute(args)),
            "execute_command" => Some(execute_command::execute(args, allowed_commands)),
            _ => None,
        }
    }

    fn discover_schemas() -> Vec<Value> {
        let mut schemas = Vec::new();
        let mut loaded_tools = HashSet::new();

        for name in BUILTIN_TOOLS {
            if let Some(schema) = Self::builtin_schema(name) {
                schemas.push(schema);
                loaded_tools.insert(name.to_string());
            }
        }

        for dir in Self::tool_dirs() {
            fs::create_dir_all(&dir).ok();
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };

            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    continue;
                }

                let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                if matches!(ext, "json" | "toml" | "md" | "rs") {
                    continue;
                }

                let file_name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();

                let (base_name, is_python) = if let Some(s) = file_name.strip_suffix(".py") {
                    (s.to_string(), true)
                } else if let Some(s) = file_name.strip_suffix(".exe") {
                    (s.to_string(), false)
                } else {
                    (file_name, false)
                };

                if loaded_tools.contains(&base_name) {
                    continue;
                }

                let mut cmd = if is_python {
                    let mut c = std::process::Command::new("python3");
                    c.arg(&path);
                    c
                } else {
                    std::process::Command::new(&path)
                };

                if let Ok(out) = cmd.arg("--schema").output() {
                    if out.status.success() {
                        if let Ok(schema) = serde_json::from_slice::<Value>(&out.stdout) {
                            schemas.push(schema);
                            loaded_tools.insert(base_name);
                        }
                    }
                }
            }
        }
        schemas
    }

    pub fn schemas(&self) -> Vec<Value> {
        self.cached_schemas.clone()
    }

    pub fn format_approval(&self, call: &ToolCall) -> String {
        let name = call.function.name.as_str();
        let args = &call.function.arguments;

        // TODO: This hardcoded approach needs to be fixed
        match name {
            "edit_file" => {
                let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("<unknown>");
                let search = args.get("search_string").and_then(|v| v.as_str()).unwrap_or("");
                let replace = args.get("replace_string").and_then(|v| v.as_str()).unwrap_or("");
                format!(
                    "Tool: edit_file\nFile: {path}\n\n--- SEARCH ---\n{search}\n\n+++ REPLACE +++\n{replace}"
                )
            }
            "create_file" => {
                let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("<unknown>");
                let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                format!(
                    "Tool: create_file\nFile: {path}\n\n+++ CONTENT +++\n{content}"
                )
            }
            "execute_command" => {
                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                format!("Tool: execute_command\nCommand:\n  $ {cmd}")
            }
            _ => {
                if let Some(obj) = args.as_object() {
                    let mut out = format!("Tool [{name}]:\n");
                    for (k, v) in obj {
                        match v.as_str() {
                            Some(s) if s.contains('\n') => {
                                out.push_str(&format!("\n{k}:\n{s}\n"));
                            }
                            Some(s) => out.push_str(&format!("  {k}: {s}\n")),
                            None => out.push_str(&format!("  {k}: {v}\n")),
                        }
                    }
                    out.trim_end().to_string()
                } else {
                    format!(
                        "Tool [{name}]:\n{}",
                        serde_json::to_string_pretty(args).unwrap_or_default()
                    )
                }
            }
        }
    }

    fn find_external_tool(name: &str) -> Option<(PathBuf, bool)> {
        let candidates = [
            (name.to_string(), false),
            (format!("{name}.exe"), false),
            (format!("{name}.py"), true),
        ];

        for dir in Self::tool_dirs() {
            for (filename, is_py) in &candidates {
                let path = dir.join(filename);
                if path.is_file() {
                    return Some((path, *is_py));
                }
            }
        }
        None
    }

    pub async fn execute(&self, call: &ToolCall) -> (String, String) {
        let name = call.function.name.clone();
        let args = call.function.arguments.clone();
        let allowed = self.allowed_commands.clone();

        let res = if Self::builtin_schema(&name).is_some() {
            let name_clone = name.clone();
            tokio::task::spawn_blocking(move || {
                Self::execute_builtin(&name_clone, &args, &allowed).unwrap()
            })
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("Tool task panicked: {e}")))
        } else if let Some((bin_path, is_python)) = Self::find_external_tool(&name) {
            self.exec_dynamic(&bin_path, is_python, &args).await
        } else {
            Err(anyhow::anyhow!(
                "Unknown tool: '{name}'. Could not find built-in tool or external script."
            ))
        };

        match res {
            Ok(out) => (out.clone(), format!("[Tool Result: {name}]\n{out}")),
            Err(e) => (format!("Error: {e}"), format!("[Tool Error: {name}]\n{e}")),
        }
    }

    async fn exec_dynamic(&self, bin_path: &Path, is_python: bool, args: &Value) -> Result<String> {
        let cwd = env::current_dir()?;
        let envelope = json!({
            "arguments": args,
            "context": {
                "allowed_commands": self.allowed_commands,
                "workspace_root": cwd.to_string_lossy(),
            }
        });
        let payload_json = serde_json::to_string(&envelope)?;

        let mut cmd = if is_python {
            let mut c = Command::new("python3");
            c.arg(bin_path);
            c
        } else {
            Command::new(bin_path)
        };

        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload_json.as_bytes()).await?;
        }

        let out = child.wait_with_output().await?;
        Ok(format_process_output(
            &out,
            "Tool executed successfully (no output).",
        ))
    }
}
