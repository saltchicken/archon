//! Memory scope, item storage, file/SQLite I/O, and path helpers.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
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
    /// Workspace-specific memory stored in `archon.db` (keyed by canonical workspace path).
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
        Self::empty()
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

fn init_memory_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_memory (
             workspace  TEXT PRIMARY KEY,
             enabled    INTEGER NOT NULL DEFAULT 1,
             items      TEXT NOT NULL DEFAULT '',
             updated_at TEXT NOT NULL
         );",
    )
    .context("Failed to create workspace_memory table")?;
    Ok(())
}

fn open_memory_db(raw_history_dir: &str) -> Result<Connection> {
    let conn = crate::db::open_archon_db(raw_history_dir)?;
    init_memory_schema(&conn)?;
    Ok(conn)
}

fn load_workspace_memory_from_conn(conn: &Connection, workspace_key: &str) -> Result<Memory> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT enabled, items FROM workspace_memory WHERE workspace = ?1",
            params![workspace_key],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;

    let Some((enabled_int, items_blob)) = row else {
        return Ok(Memory::empty());
    };

    // `Memory::new` splits each raw entry on newlines and cleans/dedups the lines, so
    // the whole stored blob can be passed as a single element.
    Ok(Memory::new(vec![items_blob], enabled_int != 0))
}

fn save_workspace_memory_to_conn(
    conn: &Connection,
    workspace_key: &str,
    mem: &Memory,
) -> Result<()> {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let items_blob = mem.items.join("\n");

    conn.execute(
        "INSERT INTO workspace_memory (workspace, enabled, items, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace) DO UPDATE SET
             enabled = excluded.enabled,
             items = excluded.items,
             updated_at = excluded.updated_at",
        params![workspace_key, i64::from(mem.enabled), items_blob, now],
    )?;

    Ok(())
}

pub fn load_workspace_memory_db(raw_history_dir: &str, workspace_key: &str) -> Result<Memory> {
    let conn = open_memory_db(raw_history_dir)?;
    load_workspace_memory_from_conn(&conn, workspace_key)
}

pub fn save_workspace_memory_db(
    raw_history_dir: &str,
    workspace_key: &str,
    mem: &Memory,
) -> Result<()> {
    let conn = open_memory_db(raw_history_dir)?;
    save_workspace_memory_to_conn(&conn, workspace_key, mem)
}

pub fn load_scoped_memory(
    scope: MemoryScope,
    history_dir: &str,
    workspace_key: &str,
) -> Result<Memory> {
    match scope {
        MemoryScope::Global => load_scoped_memory_file(&global_memory_path(), true).map(|(_, m)| m),
        MemoryScope::Workspace => load_workspace_memory_db(history_dir, workspace_key),
    }
}

pub fn save_scoped_memory(
    scope: MemoryScope,
    history_dir: &str,
    workspace_key: &str,
    mem: &Memory,
) -> Result<()> {
    match scope {
        MemoryScope::Global => save_memory_file(&global_memory_path(), mem).map(|_| ()),
        MemoryScope::Workspace => save_workspace_memory_db(history_dir, workspace_key, mem),
    }
}

pub fn load_memories(history_dir: &str, workspace_key: &str) -> (Memory, Memory) {
    let global_path = global_memory_path();
    let (_, global_mem) = load_scoped_memory_file(&global_path, true)
        .unwrap_or_else(|_| (String::new(), Memory::default()));

    let workspace_mem =
        load_workspace_memory_db(history_dir, workspace_key).unwrap_or_else(|_| Memory::empty());

    (global_mem, workspace_mem)
}

pub fn reload_memories(current_config: &super::Config) -> (Memory, Memory) {
    let ws_key = current_config.workspace_key();
    load_memories(&current_config.app.history_dir, &ws_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_memory_roundtrip_and_isolation_in_db() {
        let tmp = tempfile::tempdir().unwrap();
        let history_dir = tmp.path().to_string_lossy().into_owned();

        // Missing workspace defaults to empty & enabled.
        let initial = load_workspace_memory_db(&history_dir, "/tmp/ws-a").unwrap();
        assert_eq!(initial, Memory::empty());

        let mem_a = Memory::new(
            vec!["Fact one".into(), "Fact two".into(), "fact one".into()],
            true,
        );
        save_workspace_memory_db(&history_dir, "/tmp/ws-a", &mem_a).unwrap();

        let mem_b = Memory::new(vec!["Other workspace".into()], false);
        save_workspace_memory_db(&history_dir, "/tmp/ws-b", &mem_b).unwrap();

        let loaded_a = load_workspace_memory_db(&history_dir, "/tmp/ws-a").unwrap();
        assert!(loaded_a.enabled);
        assert_eq!(loaded_a.items, vec!["Fact one", "Fact two"]);

        let loaded_b = load_workspace_memory_db(&history_dir, "/tmp/ws-b").unwrap();
        assert!(!loaded_b.enabled);
        assert_eq!(loaded_b.items, vec!["Other workspace"]);

        // Clearing items preserves enabled = false.
        save_workspace_memory_db(&history_dir, "/tmp/ws-b", &Memory::new(vec![], false)).unwrap();
        let cleared_b = load_workspace_memory_db(&history_dir, "/tmp/ws-b").unwrap();
        assert!(!cleared_b.enabled);
        assert!(cleared_b.items.is_empty());
    }
}
