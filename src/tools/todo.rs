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
pub enum TodoAction {
    /// List tasks for the current workspace.
    #[default]
    List,
    /// Add a single task (`title`) or batch-add multiple tasks (`items`).
    Add,
    /// Update an existing task's `status`, `title`, `priority`, or `notes` by `id`.
    Update,
    /// Delete a task by `id`.
    Remove,
    /// Remove all tasks for the current workspace.
    Clear,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Backlog / not started.
    #[default]
    Pending,
    /// Actively being worked on.
    InProgress,
    /// Waiting on user input or an external decision (pauses `todo_autoloop` unless ignored).
    Blocked,
}

impl TodoStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "in_progress" => Self::InProgress,
            "blocked" => Self::Blocked,
            _ => Self::Pending,
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Blocked => "[!]",
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
    /// Short, actionable task description.
    pub title: String,
    /// Optional priority (`low`, `medium`, `high`). Defaults to `medium`.
    #[serde(default)]
    pub priority: TodoPriority,
    /// Optional initial status (`pending`, `in_progress`, `blocked`). Defaults to `pending`.
    #[serde(default)]
    pub status: TodoStatus,
    /// Optional details, file paths, or acceptance notes.
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TodoArgs {
    /// Operation to perform: `list` (default), `add`, `update`, `remove`, or `clear`.
    #[serde(default)]
    pub action: TodoAction,
    /// Task ID (for `update` or `remove`).
    #[serde(default)]
    pub id: Option<i64>,
    /// Task title (for single `add` or `update`).
    #[serde(default)]
    pub title: Option<String>,
    /// Batch of tasks to add in a single call (for `add`).
    #[serde(default)]
    pub items: Vec<NewTodoItem>,
    /// Task status (`pending`, `in_progress`, `blocked`) for `add`, `update`, or filtering `list`.
    #[serde(default)]
    pub status: Option<TodoStatus>,
    /// Task priority (`low`, `medium`, `high`) for `add` or `update`.
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

fn canonical_workspace(config: &Config) -> String {
    config
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
        .unwrap_or_default()
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

    // The `done` status no longer exists; drop any legacy rows so finished
    // tasks can't resurface as active ones.
    conn.execute("DELETE FROM todos WHERE status = 'done'", params![])?;

    Ok(conn)
}

fn fetch_todos(
    conn: &Connection,
    workspace: &str,
    status_filter: Option<TodoStatus>,
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

    let iter = stmt.query_map(params![workspace], |row| {
        Ok(TodoRow {
            id: row.get(0)?,
            title: row.get(1)?,
            status: TodoStatus::from_db(&row.get::<_, String>(2)?),
            priority: TodoPriority::from_db(&row.get::<_, String>(3)?),
            notes: row.get(4)?,
        })
    })?;

    for item in iter.flatten() {
        if let Some(filter) = status_filter
            && item.status != filter
        {
            continue;
        }
        rows.push(item);
    }
    Ok(rows)
}

fn format_todo_list(rows: &[TodoRow]) -> String {
    if rows.is_empty() {
        return "No tasks found for this workspace.".to_string();
    }

    let in_prog = rows
        .iter()
        .filter(|r| r.status == TodoStatus::InProgress)
        .count();
    let blocked = rows
        .iter()
        .filter(|r| r.status == TodoStatus::Blocked)
        .count();
    let blocked_suffix = if blocked > 0 {
        format!(", {blocked} blocked")
    } else {
        String::new()
    };
    let mut out = format!(
        "Workspace tasks ({} total, {in_prog} in progress{blocked_suffix}):\n",
        rows.len()
    );

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

/// Formats active workspace tasks for injection into the system prompt so
/// multi-step plans survive `/compact`.
pub fn format_active_todos_for_prompt(config: &Config) -> Option<String> {
    let conn = open_todo_db(config).ok()?;
    let ws = canonical_workspace(config);
    let active = fetch_todos(&conn, &ws, None).ok()?;
    if active.is_empty() {
        return None;
    }

    let mut lines = Vec::with_capacity(active.len());
    for r in &active {
        let note_suffix = if r.notes.trim().is_empty() {
            String::new()
        } else {
            format!(" — {}", r.notes.trim())
        };
        lines.push(format!(
            "- #{} {} [{}] {}{}",
            r.id,
            r.status.glyph(),
            r.priority.badge(),
            r.title,
            note_suffix
        ));
    }

    let instruction = if config.app.todo_autoloop && !config.app.todo_autoloop_ignore_blocked {
        "Active workspace task plan (update with the `todo` tool as you make progress; if you need user input or clarification before continuing, set the relevant task's status to `blocked` with a note and ask your question):"
    } else {
        "Active workspace task plan (update with the `todo` tool as you make progress):"
    };

    Some(format!(
        "<todos>\n{instruction}\n{}\n</todos>",
        lines.join("\n")
    ))
}

/// Returns `true` if there is at least one active task for the current workspace.
pub fn has_active_todos(config: &Config) -> bool {
    let Ok(conn) = open_todo_db(config) else {
        return false;
    };
    let ws = canonical_workspace(config);
    fetch_todos(&conn, &ws, None).is_ok_and(|rows| !rows.is_empty())
}

/// Returns `true` if `todo_autoloop` should continue into another turn.
///
/// When `app.todo_autoloop_ignore_blocked` is `false` (default), any `blocked`
/// task pauses the loop so the model can wait for user input.
pub fn should_autoloop(config: &Config) -> bool {
    let Ok(conn) = open_todo_db(config) else {
        return false;
    };
    let ws = canonical_workspace(config);
    let Ok(rows) = fetch_todos(&conn, &ws, None) else {
        return false;
    };
    if rows.is_empty() {
        return false;
    }
    if config.app.todo_autoloop_ignore_blocked {
        return true;
    }
    !rows.iter().any(|r| r.status == TodoStatus::Blocked)
}

impl BlockingTool for Tool {
    const NAME: &'static str = "todo";
    const DESCRIPTION: &'static str = "Manage a persistent workspace task checklist (`list`, `add`, `update`, `remove`, `clear`) to track multi-step plans across turns and compactions.";
    type Args = TodoArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let conn = open_todo_db(&ctx.config)?;
        let ws = canonical_workspace(&ctx.config);
        let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

        match args.action {
            TodoAction::List => {
                let rows = fetch_todos(&conn, &ws, args.status)?;
                Ok(format_todo_list(&rows))
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
                        bail!("Task `title` cannot be empty.");
                    }
                    // Per-workspace dense numbering: an empty list restarts
                    // at #1 instead of continuing the global counter.
                    let next_id: i64 = conn.query_row(
                        "SELECT COALESCE(MAX(id), 0) + 1 FROM todos WHERE workspace = ?1",
                        params![ws],
                        |r| r.get(0),
                    )?;
                    conn.execute(
                        "INSERT INTO todos (workspace, id, title, status, priority, notes, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            ws,
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

                let rows = fetch_todos(&conn, &ws, None)?;
                let id_list = added_ids
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(format!("Added {id_list}.\n\n{}", format_todo_list(&rows)))
            }
            TodoAction::Update => {
                let id = args.id.context("`todo:update` requires `id`.")?;
                let existing: Option<(String, String, String, String)> = conn
                    .query_row(
                        "SELECT title, status, priority, notes FROM todos WHERE id = ?1 AND workspace = ?2",
                        params![id, ws],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .ok();
                let Some((cur_title, cur_status, cur_priority, cur_notes)) = existing else {
                    bail!("Task #{id} not found in this workspace.");
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
                    params![new_title, new_status, new_priority, new_notes, now, id, ws],
                )?;

                let rows = fetch_todos(&conn, &ws, None)?;
                Ok(format!(
                    "Updated task #{id}.\n\n{}",
                    format_todo_list(&rows)
                ))
            }
            TodoAction::Remove => {
                let id = args.id.context("`todo:remove` requires `id`.")?;
                let deleted = conn.execute(
                    "DELETE FROM todos WHERE id = ?1 AND workspace = ?2",
                    params![id, ws],
                )?;
                if deleted == 0 {
                    bail!("Task #{id} not found in this workspace.");
                }
                let rows = fetch_todos(&conn, &ws, None)?;
                Ok(format!(
                    "Removed task #{id}.\n\n{}",
                    format_todo_list(&rows)
                ))
            }
            TodoAction::Clear => {
                let deleted =
                    conn.execute("DELETE FROM todos WHERE workspace = ?1", params![ws])?;
                Ok(format!("Cleared {deleted} task(s)."))
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
            config: cfg,
        }
    }

    #[test]
    fn add_update_remove_and_clear_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&tmp);

        let added = Tool::execute(
            TodoArgs {
                action: TodoAction::Add,
                id: None,
                title: None,
                items: vec![
                    NewTodoItem {
                        title: "Add todo tool".into(),
                        priority: TodoPriority::High,
                        status: TodoStatus::InProgress,
                        notes: "src/tools/todo.rs".into(),
                    },
                    NewTodoItem {
                        title: "Register in tools.rs".into(),
                        priority: TodoPriority::Medium,
                        status: TodoStatus::Pending,
                        notes: String::new(),
                    },
                ],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(added.contains("#1 [~] [HIGH] Add todo tool"));
        assert!(added.contains("#2 [ ] [med] Register in tools.rs"));

        let prompt_block = format_active_todos_for_prompt(&ctx.config).unwrap();
        assert!(prompt_block.contains("<todos>"));
        assert!(prompt_block.contains("Add todo tool"));

        let removed = Tool::execute(
            TodoArgs {
                action: TodoAction::Remove,
                id: Some(1),
                title: None,
                items: vec![],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(removed.contains("Removed task #1."));
        assert!(removed.contains("1 total"));

        let cleared = Tool::execute(
            TodoArgs {
                action: TodoAction::Clear,
                id: None,
                title: None,
                items: vec![],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(cleared.contains("Cleared 1 task(s)."));

        // With an empty list, the next task must restart at #1 rather than
        // continuing the previous numbering.
        let readded = Tool::execute(
            TodoArgs {
                action: TodoAction::Add,
                id: None,
                title: Some("Restart numbering".into()),
                items: vec![],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(readded.contains("Added #1."));
        assert!(readded.contains("#1 [ ] [med] Restart numbering"));
    }

    #[test]
    fn blocked_status_pauses_autoloop_unless_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(&tmp);

        Tool::execute(
            TodoArgs {
                action: TodoAction::Add,
                id: None,
                title: Some("Clarify API contract".into()),
                items: vec![],
                status: Some(TodoStatus::Blocked),
                priority: Some(TodoPriority::High),
                notes: Some("Waiting on user".into()),
            },
            &ctx,
        )
        .unwrap();

        ctx.config.app.todo_autoloop_ignore_blocked = false;
        assert!(!should_autoloop(&ctx.config));

        ctx.config.app.todo_autoloop_ignore_blocked = true;
        assert!(should_autoloop(&ctx.config));
    }
}
