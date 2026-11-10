use crate::core::config::Config;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, env, fs, path::PathBuf};
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

    /// Returns the tool directories in order of priority:
    /// 1. Project-specific ./tools/python
    /// 2. Project-specific ./tools/rust
    /// 3. Legacy flat ./tools
    /// 4. Global ~/.local/share/archon/tools
    pub fn tool_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(cwd) = env::current_dir() {
            dirs.push(cwd.join("tools").join("python"));
            dirs.push(cwd.join("tools").join("rust"));
            dirs.push(cwd.join("tools"));
        }
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".local/share/archon/tools"));
        }
        dirs
    }

    pub fn schemas(&self) -> Vec<Value> {
        let mut schemas = Vec::new();
        let mut loaded_tools = HashSet::new();

        for dir in Self::tool_dirs() {
            fs::create_dir_all(&dir).ok();

            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    
                    if path.is_dir() {
                        continue;
                    }

                    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                    if ext == "json" || ext == "toml" || ext == "md" {
                        continue;
                    }

                    let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    
                    // Determine tool name and language backend
                    let (base_name, is_python, is_rust) = if file_name.ends_with(".py") {
                        (file_name.strip_suffix(".py").unwrap().to_string(), true, false)
                    } else if file_name.ends_with(".rs") {
                        (file_name.strip_suffix(".rs").unwrap().to_string(), false, true)
                    } else if file_name.ends_with(".exe") {
                        (file_name.strip_suffix(".exe").unwrap().to_string(), false, false)
                    } else {
                        (file_name.clone(), false, false)
                    };

                    if loaded_tools.contains(&base_name) {
                        continue;
                    }

                    let mut cmd = if is_python {
                        let mut c = std::process::Command::new("python3");
                        c.arg(&path);
                        c
                    } else if is_rust {
                        // Resolve the compiled binary path instead of trying to run the .rs source file
                        let cwd = env::current_dir().unwrap_or_default();
                        let debug_bin = cwd.join("target").join("debug").join(&base_name);
                        let release_bin = cwd.join("target").join("release").join(&base_name);
                        
                        let compiled_path = if debug_bin.exists() {
                            debug_bin
                        } else if debug_bin.with_extension("exe").exists() {
                            debug_bin.with_extension("exe")
                        } else if release_bin.exists() {
                            release_bin
                        } else if release_bin.with_extension("exe").exists() {
                            release_bin.with_extension("exe")
                        } else {
                            continue; // Skip if it hasn't been compiled yet
                        };
                        std::process::Command::new(compiled_path)
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

    pub fn format_approval(&self, call: &ToolCall) -> String {
        format!(
            "Dynamic Tool [{}]:\n{}",
            call.function.name,
            serde_json::to_string_pretty(&call.function.arguments).unwrap_or_default()
        )
    }

    pub async fn execute(&self, call: &ToolCall) -> (String, String) {
        let name = &call.function.name;
        
        let mut found_target = None;
        for dir in Self::tool_dirs() {
            let unix_cand = dir.join(name);
            let win_cand = dir.join(format!("{name}.exe"));
            let py_cand = dir.join(format!("{name}.py"));
            let rs_cand = dir.join(format!("{name}.rs"));
            
            if unix_cand.exists() && unix_cand.is_file() {
                found_target = Some((unix_cand, false));
                break;
            } else if win_cand.exists() && win_cand.is_file() {
                found_target = Some((win_cand, false));
                break;
            } else if py_cand.exists() && py_cand.is_file() {
                found_target = Some((py_cand, true));
                break;
            } else if rs_cand.exists() && rs_cand.is_file() {
                // Find corresponding compiled rust binary
                let cwd = env::current_dir().unwrap_or_default();
                let debug_bin = cwd.join("target").join("debug").join(name);
                let release_bin = cwd.join("target").join("release").join(name);
                
                if debug_bin.exists() {
                    found_target = Some((debug_bin, false));
                    break;
                } else if debug_bin.with_extension("exe").exists() {
                    found_target = Some((debug_bin.with_extension("exe"), false));
                    break;
                } else if release_bin.exists() {
                    found_target = Some((release_bin, false));
                    break;
                } else if release_bin.with_extension("exe").exists() {
                    found_target = Some((release_bin.with_extension("exe"), false));
                    break;
                }
            }
        }

        let res = if let Some((bin_path, is_python)) = found_target {
            self.exec_dynamic(&bin_path, is_python, &call.function.arguments).await
        } else {
            Err(anyhow::anyhow!("Unknown tool: '{name}'. Could not find compiled binary or .py script in tool directories."))
        };

        match res {
            Ok(out) => (
                out.clone(),
                format!("[Tool Result: {}]\n{}", name, out),
            ),
            Err(e) => (
                format!("Error: {e}"),
                format!("[Tool Error: {}]\n{e}", name),
            ),
        }
    }

    async fn exec_dynamic(&self, bin_path: &PathBuf, is_python: bool, args: &Value) -> Result<String> {
        let args_json = serde_json::to_string(args)?;
        let cwd = env::current_dir()?;
        let allowed_cmds_str = self.allowed_commands.join(",");

        let mut cmd = if is_python {
            let mut c = Command::new("python3");
            c.arg(bin_path);
            c
        } else {
            Command::new(bin_path)
        };

        // Inject allowed commands into the environment so scripts can still respect them
        cmd.current_dir(cwd)
           .env("ARCHON_ALLOWED_COMMANDS", allowed_cmds_str)
           .arg(&args_json);

        let out = cmd.output().await?;

        let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str("[STDERR]\n");
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
        }

        if combined.trim().is_empty() {
            Ok(format!("Tool executed successfully (no output)."))
        } else {
            Ok(combined)
        }
    }
}
