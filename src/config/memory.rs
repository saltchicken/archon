//! Memory scope, item storage, file I/O, and path helpers.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Global `~/.config/archon/MEMORY.md` (shared across all workspaces).
    #[default]
    Global,
    /// Workspace-specific `~/.config/archon/workspaces/<ws>/MEMORY.md`.
    Workspace,
}

impl MemoryScope {
    pub fn toggle(self) -> Self {
        match self {
            Self::Global => Self::Workspace,
            Self::Workspace => Self::Global,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub enabled: bool,
    pub items: Vec<String>,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            enabled: true,
            items: vec!["The user is running CachyOS.".into()],
        }
    }
}

pub fn clean_memory_item(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.starts_with('#') {
        return "";
    }
    let unbulleted = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("• "))
        .map_or(trimmed, str::trim);
    if unbulleted.starts_with('#') {
        return "";
    }
    unbulleted
}

impl Memory {
    pub fn empty() -> Self {
        Self {
            enabled: true,
            items: Vec::new(),
        }
    }

    pub fn new(raw_items: Vec<String>, enabled: bool) -> Self {
        let mut items: Vec<String> = Vec::new();
        for raw in raw_items {
            for line in raw.lines().map(clean_memory_item) {
                if !line.is_empty()
                    && !items
                        .iter()
                        .any(|existing| existing.eq_ignore_ascii_case(line))
                {
                    items.push(line.to_string());
                }
            }
        }
        Self { enabled, items }
    }

    pub fn count(&self) -> usize {
        self.items.len()
    }

    pub fn to_editor_text(&self) -> String {
        self.items.join("\n")
    }

    pub fn from_editor_text(raw: &str, enabled: bool) -> Self {
        Self::new(vec![raw.to_string()], enabled)
    }

    pub fn from_markdown(raw: &str) -> Self {
        let (fm_opt, body) = crate::skills::split_toml_frontmatter(raw);
        let enabled = fm_opt
            .and_then(|fm| toml::from_str::<toml::Table>(fm).ok())
            .and_then(|t| t.get("enabled")?.as_bool())
            .unwrap_or(true);
        Self::new(vec![body.to_string()], enabled)
    }

    pub fn to_markdown_file(&self) -> String {
        let mut out = if self.enabled {
            String::new()
        } else {
            String::from("+++\nenabled = false\n+++\n\n")
        };
        for item in &self.items {
            out.push_str("- ");
            out.push_str(item);
            out.push('\n');
        }
        out
    }

    pub fn format_for_prompt(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let today = chrono::Local::now().format("%A, %B %-d, %Y");
        let mut bullets = vec![format!("- Current date: {today}")];
        bullets.extend(self.items.iter().map(|s| format!("- {s}")));
        Some(format!(
            "<memory>\nPersistent user context and environment facts (always apply):\n{}\n</memory>",
            bullets.join("\n")
        ))
    }
}

pub fn archon_config_root() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("archon")
}

pub fn global_memory_path() -> PathBuf {
    archon_config_root().join("MEMORY.md")
}

pub fn workspace_memory_path(ws_dir: &Path) -> PathBuf {
    ws_dir.join("MEMORY.md")
}

pub fn load_scoped_memory_file(mem_path: &Path, is_global: bool) -> Result<(String, Memory)> {
    if !mem_path.exists() {
        let mem = if is_global {
            Memory::default()
        } else {
            Memory::empty()
        };
        return Ok((String::new(), mem));
    }

    let raw = fs::read_to_string(mem_path)
        .with_context(|| format!("Failed to read {}", mem_path.display()))?;
    let mem = Memory::from_markdown(&raw);
    Ok((raw, mem))
}

pub fn save_memory_file(mem_path: &Path, mem: &Memory) -> Result<String> {
    if let Some(parent) = mem_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }

    let rendered = mem.to_markdown_file();
    crate::tools::executor::utils::fs::atomic_write_file(mem_path, &rendered)
        .with_context(|| format!("Failed to write {}", mem_path.display()))?;
    Ok(rendered)
}

pub fn save_scoped_memory(
    scope: MemoryScope,
    ws_dir: Option<&Path>,
    mem: &Memory,
) -> Result<String> {
    let path = match scope {
        MemoryScope::Global => global_memory_path(),
        MemoryScope::Workspace => {
            let dir = match ws_dir {
                Some(d) => d.to_path_buf(),
                None => {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    super::workspace_config_dir_for(&cwd)
                }
            };
            workspace_memory_path(&dir)
        }
    };
    save_memory_file(&path, mem)
}

pub fn load_memories(ws_dir: Option<&Path>) -> (Memory, Memory) {
    let global_path = global_memory_path();
    let (_, global_mem) = load_scoped_memory_file(&global_path, true)
        .unwrap_or_else(|_| (String::new(), Memory::default()));

    let workspace_mem = ws_dir
        .map(workspace_memory_path)
        .and_then(|ws_path| load_scoped_memory_file(&ws_path, false).ok())
        .map(|(_, m)| m)
        .unwrap_or_else(Memory::empty);

    (global_mem, workspace_mem)
}

pub fn reload_memories(current_config: &super::Config) -> (Memory, Memory) {
    let ws_dir = current_config.workspace_config_dir();
    load_memories(ws_dir.as_deref())
}
