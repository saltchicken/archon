use crate::core::config::Config;
use crate::tools::{context, create_file, edit_file, execute_command};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, env, fs, path::PathBuf, process::Stdio};
use tokio::{io::AsyncWriteExt, process::Command};

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
        let allowed_commands = config.app.allowed_commands.clone();
        let cached_schemas = Self::discover_schemas();
        Self {
            allowed_commands,
            cached_schemas,
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

        // 1. Load built-in compiled Rust tools directly from memory
        for name in ["context", "create_file", "edit_file", "execute_command"] {
            if let Some(schema) = Self::builtin_schema(name) {
                schemas.push(schema);
                loaded_tools.insert(name.to_string());
            }
        }

        // 2. Discover external scripts/binaries (e.g., Python tools)
        for dir in Self::tool_dirs() {
            fs::create_dir_all(&dir).ok();

            if let Ok(entries) = fs::read_dir(&dir) {
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
                        .to_string();

                    let (base_name, is_python) =
                        if let Some(stripped) = file_name.strip_suffix(".py") {
                            (stripped.to_string(), true)
                        } else if let Some(stripped) = file_name.strip_suffix(".exe") {
                            (stripped.to_string(), false)
                        } else {
                            (file_name.clone(), false)
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

                    cmd.arg("--schema");

                    if let Ok(out) = cmd.output() {
                        if out.status.success() {
                            if let Ok(schema) = serde_json::from_slice::<Value>(&out.stdout) {
                                schemas.push(schema);
                                loaded_tools.insert(base_name);
                            }
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
        format!(
            "Dynamic Tool [{}]:\n{}",
            call.function.name,
            serde_json::to_string_pretty(&call.function.arguments).unwrap_or_default()
        )
    }

    pub async fn execute(&self, call: &ToolCall) -> (String, String) {
        let name = call.function.name.clone();
        let args = call.function.arguments.clone();
        let allowed = self.allowed_commands.clone();

        // 1. Check and run built-in Rust tools first
        if Self::builtin_schema(&name).is_some() {
            let name_clone = name.clone();
            let res = tokio::task::spawn_blocking(move || {
                Self::execute_builtin(&name_clone, &args, &allowed).unwrap()
            })
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("Tool task panicked: {e}")));

            return match res {
                Ok(out) => (out.clone(), format!("[Tool Result: {}]\n{}", name, out)),
                Err(e) => (
                    format!("Error: {e}"),
                    format!("[Tool Error: {}]\n{e}", name),
                ),
            };
        }

        // 2. Otherwise, look for external Python/binary tools
        let mut found_target = None;
        for dir in Self::tool_dirs() {
            let unix_cand = dir.join(&name);
            let win_cand = dir.join(format!("{name}.exe"));
            let py_cand = dir.join(format!("{name}.py"));

            if unix_cand.exists() && unix_cand.is_file() {
                found_target = Some((unix_cand, false));
                break;
            } else if win_cand.exists() && win_cand.is_file() {
                found_target = Some((win_cand, false));
                break;
            } else if py_cand.exists() && py_cand.is_file() {
                found_target = Some((py_cand, true));
                break;
            }
        }

        let res = if let Some((bin_path, is_python)) = found_target {
            self.exec_dynamic(&bin_path, is_python, &call.function.arguments)
                .await
        } else {
            Err(anyhow::anyhow!(
                "Unknown tool: '{name}'. Could not find built-in tool or external script."
            ))
        };

        match res {
            Ok(out) => (out.clone(), format!("[Tool Result: {}]\n{}", name, out)),
            Err(e) => (
                format!("Error: {e}"),
                format!("[Tool Error: {}]\n{e}", name),
            ),
        }
    }

    async fn exec_dynamic(
        &self,
        bin_path: &PathBuf,
        is_python: bool,
        args: &Value,
    ) -> Result<String> {
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

        // Pass payload over stdin to avoid OS ARG_MAX limits on large file contents
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(payload_json.as_bytes()).await?;
        }

        let out = child.wait_with_output().await?;

        let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str("[STDERR]\n");
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
        }

        if combined.trim().is_empty() {
            Ok("Tool executed successfully (no output).".to_string())
        } else {
            Ok(combined)
        }
    }
}
