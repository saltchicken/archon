use super::{BlockingTool, ToolContext};
use crate::core::config::{Config, expand_tilde};
use anyhow::{Context, Result, bail};
use chrono::Local;
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{fmt::Write as _, fs, path::PathBuf};

pub struct Tool;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoScope {
    /// Current workspace backlog (default).
    #[default]
    Workspace,
    /// Global user backlog shared across all workspaces.
    Global,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoAction {
    /// List todo items (hides `done` items unless `include_done = true` or `status = "done"`).
    #[default]
    List,
    /// Add a single todo (`title`) or batch-add multiple todos (`items`).
    Add,
    /// Update an existing todo's `status`, `title`, `priority`, or `notes` by `id`.
    Update,
    /// Mark a todo as `done` by `id`.
    Done,
    /// Delete a todo permanently by `id`.
    Remove,
    /// Clear todos (pass `status = "done"` to clear only completed items, or omit to clear all).
    Clear,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    #[default]
    Pending,
    /// In progress.
    InProgress,
    /// Completed.
    Done,
}

impl TodoStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "in_progress" => Self::InProgress,
            "done" => Self::Done,
            _ => Self::Pending,
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Done => "[x]",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoPriority {
    Low,
    #[default]
    Medium,
    High,
}

impl TodoPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "low" => Self::Low,
            "high" => Self::High,
            _ => Self::Medium,
        }
    }

    fn badge(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "med",
            Self::High => "HIGH",
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct NewTodoItem {
    /// Short description of the todo item.
    pub title: String,
    /// Optional priority (`low`, `medium`, `high`). Defaults to `medium`.
    #[serde(default)]
    pub priority: TodoPriority,
    /// Optional initial status (`pending`, `in_progress`, `done`). Defaults to `pending`.
    #[serde(default)]
    pub status: TodoStatus,
    /// Optional notes or context.
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoArgs {
    /// Operation to perform: `list` (default), `add`, `update`, `done`, `remove`, or `clear`.
    #[serde(default)]
    pub action: TodoAction,
    /// Backlog scope: `workspace` (default) or `global`.
    #[serde(default)]
    pub scope: TodoScope,
    /// Todo ID (for `update`, `done`, or `remove`).
    #[serde(default)]
    pub id: Option<i64>,
    /// Todo title (for single `add` or `update`).
    #[serde(default)]
    pub title: Option<String>,
    /// Batch of todos to add in a single call (for `add`).
    #[serde(default)]
    pub items: Vec<NewTodoItem>,
    /// Todo status (`pending`, `in_progress`, `done`) for `add`, `update`, `list`, or `clear`.
    #[serde(default)]
    pub status: Option<TodoStatus>,
    /// Include completed (`done`) items when running `list` without a `status` filter.
    #[serde(default)]
    pub include_done: bool,
    /// Todo priority (`low`, `medium`, `high`) for `add` or `update`.
    #[serde(default)]
    pub priority: Option<TodoPriority>,
    /// Optional notes for `add` or `update`.
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone)]
struct TodoRow {
    id: i64,
    title: String,
    status: TodoStatus,
    priority: TodoPriority,
    notes: String,
}

fn scope_key(config: &Config, scope: TodoScope) -> String {
    match scope {
        TodoScope::Global => String::new(),
        TodoScope::Workspace => config
            .workspace
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(expand_tilde)
            .or_else(|| std::env::current_dir().ok())
            .map(|p| {
                dunce::canonicalize(&p)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default(),
    }
}

fn open_todo_db(config: &Config) -> Result<Connection> {
    let history_dir: PathBuf = expand_tilde(&config.app.history_dir);
    fs::create_dir_all(&history_dir)?;
    let db_path = history_dir.join("archon.db");
    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open SQLite database at {}", db_path.display()))?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;

         CREATE TABLE IF NOT EXISTS todos (
             workspace TEXT NOT NULL,
             id INTEGER NOT NULL,
             title TEXT NOT NULL,
             status TEXT NOT NULL DEFAULT 'pending',
             priority TEXT NOT NULL DEFAULT 'medium',
             notes TEXT NOT NULL DEFAULT '',
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             PRIMARY KEY (workspace, id)
         );

         CREATE INDEX IF NOT EXISTS idx_todos_workspace_status
             ON todos(workspace, status, id);",
    )
    .context("Failed to initialize todos table")?;

    Ok(conn)
}

fn fetch_todos(
    conn: &Connection,
    ws_key: &str,
    status_filter: Option<TodoStatus>,
    include_done: bool,
) -> Result<Vec<TodoRow>> {
    let mut rows = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id, title, status, priority, notes
         FROM todos
         WHERE workspace = ?1
         ORDER BY
             CASE status WHEN 'in_progress' THEN 0 WHEN 'pending' THEN 1 ELSE 2 END,
             CASE priority WHEN 'high' THEN 0 WHEN 'medium' THEN 1 ELSE 2 END,
             id ASC",
    )?;

    let iter = stmt.query_map(params![ws_key], |row| {
        Ok(TodoRow {
            id: row.get(0)?,
            title: row.get(1)?,
            status: TodoStatus::from_db(&row.get::<_, String>(2)?),
            priority: TodoPriority::from_db(&row.get::<_, String>(3)?),
            notes: row.get(4)?,
        })
    })?;

    for item in iter.flatten() {
        if let Some(filter) = status_filter {
            if item.status != filter {
                continue;
            }
        } else if !include_done && item.status == TodoStatus::Done {
            continue;
        }
        rows.push(item);
    }
    Ok(rows)
}

fn format_todo_list(rows: &[TodoRow], scope: TodoScope) -> String {
    let scope_label = match scope {
        TodoScope::Workspace => "Workspace",
        TodoScope::Global => "Global",
    };
    if rows.is_empty() {
        return format!("No {scope_label} todos found.");
    }

    let open = rows.iter().filter(|r| r.status != TodoStatus::Done).count();
    let done = rows.iter().filter(|r| r.status == TodoStatus::Done).count();
    let done_suffix = if done > 0 {
        format!(", {done} done")
    } else {
        String::new()
    };
    let mut out = format!("{scope_label} todos ({open} open{done_suffix}):\n");

    for r in rows {
        let _ = writeln!(
            out,
            "  #{} {} [{}] {}",
            r.id,
            r.status.glyph(),
            r.priority.badge(),
            r.title
        );
        if !r.notes.trim().is_empty() {
            let _ = writeln!(out, "      note: {}", r.notes.trim());
        }
    }
    out.trim_end().to_string()
}

impl BlockingTool for Tool {
    const NAME: &'static str = "todo";
    const DESCRIPTION: &'static str = "Manage a persistent workspace or global todo backlog (`list`, `add`, `update`, `done`, `remove`, `clear`). Passive tracker — does not trigger autoloop or inject into the system prompt.";
    type Args = TodoArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let conn = open_todo_db(&ctx.config)?;
        let ws_key = scope_key(&ctx.config, args.scope);
        let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

        match args.action {
            TodoAction::List => {
                let rows = fetch_todos(&conn, &ws_key, args.status, args.include_done)?;
                Ok(format_todo_list(&rows, args.scope))
            }
            TodoAction::Add => {
                let mut to_insert = args.items;
                if let Some(title) = args.title.map(|t| t.trim().to_string())
                    && !title.is_empty()
                {
                    to_insert.push(NewTodoItem {
                        title,
                        priority: args.priority.unwrap_or_default(),
                        status: args.status.unwrap_or_default(),
                        notes: args.notes.unwrap_or_default(),
                    });
                }
                if to_insert.is_empty() {
                    bail!("`todo:add` requires `title` or a non-empty `items` array.");
                }

                let mut added_ids = Vec::with_capacity(to_insert.len());
                for item in to_insert {
                    let title = item.title.trim();
                    if title.is_empty() {
                        bail!("Todo `title` cannot be empty.");
                    }
                    let next_id: i64 = conn.query_row(
                        "SELECT COALESCE(MAX(id), 0) + 1 FROM todos WHERE workspace = ?1",
                        params![ws_key],
                        |r| r.get(0),
                    )?;
                    conn.execute(
                        "INSERT INTO todos (workspace, id, title, status, priority, notes, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            ws_key,
                            next_id,
                            title,
                            item.status.as_str(),
                            item.priority.as_str(),
                            item.notes.trim(),
                            now,
                            now,
                        ],
                    )?;
                    added_ids.push(next_id);
                }

                let rows = fetch_todos(&conn, &ws_key, None, false)?;
                let id_list = added_ids
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(format!(
                    "Added {id_list}.\n\n{}",
                    format_todo_list(&rows, args.scope)
                ))
            }
            TodoAction::Done => {
                let id = args.id.context("`todo:done` requires `id`.")?;
                let updated = conn.execute(
                    "UPDATE todos SET status = 'done', updated_at = ?1 WHERE id = ?2 AND workspace = ?3",
                    params![now, id, ws_key],
                )?;
                if updated == 0 {
                    bail!("Todo #{id} not found.");
                }
                let rows = fetch_todos(&conn, &ws_key, None, false)?;
                Ok(format!(
                    "Marked todo #{id} as done.\n\n{}",
                    format_todo_list(&rows, args.scope)
                ))
            }
            TodoAction::Update => {
                let id = args.id.context("`todo:update` requires `id`.")?;
                let existing: Option<(String, String, String, String)> = conn
                    .query_row(
                        "SELECT title, status, priority, notes FROM todos WHERE id = ?1 AND workspace = ?2",
                        params![id, ws_key],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .ok();
                let Some((cur_title, cur_status, cur_priority, cur_notes)) = existing else {
                    bail!("Todo #{id} not found.");
                };

                let new_title = args
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&cur_title);
                let new_status = args.status.map(TodoStatus::as_str).unwrap_or(&cur_status);
                let new_priority = args
                    .priority
                    .map(TodoPriority::as_str)
                    .unwrap_or(&cur_priority);
                let new_notes = args.notes.as_deref().map(str::trim).unwrap_or(&cur_notes);

                conn.execute(
                    "UPDATE todos SET title = ?1, status = ?2, priority = ?3, notes = ?4, updated_at = ?5
                     WHERE id = ?6 AND workspace = ?7",
                    params![new_title, new_status, new_priority, new_notes, now, id, ws_key],
                )?;

                let rows = fetch_todos(&conn, &ws_key, None, args.include_done)?;
                Ok(format!(
                    "Updated todo #{id}.\n\n{}",
                    format_todo_list(&rows, args.scope)
                ))
            }
            TodoAction::Remove => {
                let id = args.id.context("`todo:remove` requires `id`.")?;
                let deleted = conn.execute(
                    "DELETE FROM todos WHERE id = ?1 AND workspace = ?2",
                    params![id, ws_key],
                )?;
                if deleted == 0 {
                    bail!("Todo #{id} not found.");
                }
                let rows = fetch_todos(&conn, &ws_key, None, false)?;
                Ok(format!(
                    "Removed todo #{id}.\n\n{}",
                    format_todo_list(&rows, args.scope)
                ))
            }
            TodoAction::Clear => {
                let deleted = if let Some(status) = args.status {
                    conn.execute(
                        "DELETE FROM todos WHERE workspace = ?1 AND status = ?2",
                        params![ws_key, status.as_str()],
                    )?
                } else {
                    conn.execute("DELETE FROM todos WHERE workspace = ?1", params![ws_key])?
                };
                Ok(format!("Cleared {deleted} todo(s)."))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(tmp: &tempfile::TempDir) -> ToolContext {
        let mut cfg = Config::default();
        cfg.app.history_dir = tmp.path().to_string_lossy().into_owned();
        cfg.workspace = Some(tmp.path().to_string_lossy().into_owned());
        ToolContext {
            root: tmp.path().to_path_buf(),
            max_output_size: 32_768,
            active_skill: None,
            session_id: None,
            config: cfg,
        }
    }

    #[test]
    fn backlog_add_done_filter_and_clear_done() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&tmp);

        let added = Tool::execute(
            TodoArgs {
                action: TodoAction::Add,
                scope: TodoScope::Workspace,
                id: None,
                title: None,
                items: vec![
                    NewTodoItem {
                        title: "Refactor markdown parser".into(),
                        priority: TodoPriority::High,
                        status: TodoStatus::Pending,
                        notes: "src/ui/markdown.rs".into(),
                    },
                    NewTodoItem {
                        title: "Add Windows clipboard".into(),
                        priority: TodoPriority::Low,
                        status: TodoStatus::Pending,
                        notes: String::new(),
                    },
                ],
                status: None,
                include_done: false,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(added.contains("#1 [ ] [HIGH] Refactor markdown parser"));
        assert!(added.contains("#2 [ ] [low] Add Windows clipboard"));

        let done_res = Tool::execute(
            TodoArgs {
                action: TodoAction::Done,
                scope: TodoScope::Workspace,
                id: Some(1),
                title: None,
                items: vec![],
                status: None,
                include_done: false,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(done_res.contains("Marked todo #1 as done."));
        assert!(!done_res.contains("Refactor markdown parser"));

        let listed_all = Tool::execute(
            TodoArgs {
                action: TodoAction::List,
                scope: TodoScope::Workspace,
                id: None,
                title: None,
                items: vec![],
                status: None,
                include_done: true,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(listed_all.contains("#1 [x] [HIGH] Refactor markdown parser"));
        assert!(listed_all.contains("1 open, 1 done"));

        let cleared_done = Tool::execute(
            TodoArgs {
                action: TodoAction::Clear,
                scope: TodoScope::Workspace,
                id: None,
                title: None,
                items: vec![],
                status: Some(TodoStatus::Done),
                include_done: false,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(cleared_done.contains("Cleared 1 todo(s)."));
    }
}
