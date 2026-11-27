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
pub enum TaskAction {
    /// List active execution tasks for the current session.
    #[default]
    List,
    /// Add a single task (`title`) or batch-add multiple tasks (`items`).
    Add,
    /// Update an existing task's `status`, `title`, `priority`, or `notes` by `id`.
    Update,
    /// Remove a finished or unneeded task by `id`.
    Remove,
    /// Clear all tasks for the current session.
    Clear,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Queued / not started.
    #[default]
    Pending,
    /// Actively being worked on.
    InProgress,
    /// Waiting on user input or an external decision (pauses `task_autoloop` unless ignored).
    Blocked,
}

impl TaskStatus {
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
pub enum TaskPriority {
    Low,
    #[default]
    Medium,
    High,
}

impl TaskPriority {
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
pub struct NewTaskItem {
    /// Short, actionable step description.
    pub title: String,
    /// Optional priority (`low`, `medium`, `high`). Defaults to `medium`.
    #[serde(default)]
    pub priority: TaskPriority,
    /// Optional initial status (`pending`, `in_progress`, `blocked`). Defaults to `pending`.
    #[serde(default)]
    pub status: TaskStatus,
    /// Optional details, file paths, or acceptance notes.
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskArgs {
    /// Operation to perform: `list` (default), `add`, `update`, `remove`, or `clear`.
    #[serde(default)]
    pub action: TaskAction,
    /// Task ID (for `update` or `remove`).
    #[serde(default)]
    pub id: Option<i64>,
    /// Task title (for single `add` or `update`).
    #[serde(default)]
    pub title: Option<String>,
    /// Batch of tasks to add in a single call (for `add`).
    #[serde(default)]
    pub items: Vec<NewTaskItem>,
    /// Task status (`pending`, `in_progress`, `blocked`) for `add`, `update`, or filtering `list`.
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// Task priority (`low`, `medium`, `high`) for `add` or `update`.
    #[serde(default)]
    pub priority: Option<TaskPriority>,
    /// Optional notes for `add` or `update`.
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone)]
struct TaskRow {
    id: i64,
    title: String,
    status: TaskStatus,
    priority: TaskPriority,
    notes: String,
}

fn open_task_db(config: &Config) -> Result<Connection> {
    let history_dir: PathBuf = expand_tilde(&config.app.history_dir);
    fs::create_dir_all(&history_dir)?;
    let db_path = history_dir.join("archon.db");
    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open SQLite database at {}", db_path.display()))?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;

         CREATE TABLE IF NOT EXISTS tasks (
             session_id TEXT NOT NULL,
             id INTEGER NOT NULL,
             title TEXT NOT NULL,
             status TEXT NOT NULL DEFAULT 'pending',
             priority TEXT NOT NULL DEFAULT 'medium',
             notes TEXT NOT NULL DEFAULT '',
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             PRIMARY KEY (session_id, id)
         );

         CREATE INDEX IF NOT EXISTS idx_tasks_session_status
             ON tasks(session_id, status, id);",
    )
    .context("Failed to initialize tasks table")?;

    Ok(conn)
}

fn fetch_tasks(
    conn: &Connection,
    session_id: &str,
    status_filter: Option<TaskStatus>,
) -> Result<Vec<TaskRow>> {
    let mut rows = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id, title, status, priority, notes
         FROM tasks
         WHERE session_id = ?1
         ORDER BY
             CASE status WHEN 'in_progress' THEN 0 WHEN 'pending' THEN 1 ELSE 2 END,
             CASE priority WHEN 'high' THEN 0 WHEN 'medium' THEN 1 ELSE 2 END,
             id ASC",
    )?;

    let iter = stmt.query_map(params![session_id], |row| {
        Ok(TaskRow {
            id: row.get(0)?,
            title: row.get(1)?,
            status: TaskStatus::from_db(&row.get::<_, String>(2)?),
            priority: TaskPriority::from_db(&row.get::<_, String>(3)?),
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

fn format_task_list(rows: &[TaskRow]) -> String {
    if rows.is_empty() {
        return "No active tasks for this session.".to_string();
    }

    let in_prog = rows
        .iter()
        .filter(|r| r.status == TaskStatus::InProgress)
        .count();
    let blocked = rows
        .iter()
        .filter(|r| r.status == TaskStatus::Blocked)
        .count();
    let blocked_suffix = if blocked > 0 {
        format!(", {blocked} blocked")
    } else {
        String::new()
    };
    let mut out = format!(
        "Session tasks ({} total, {in_prog} in progress{blocked_suffix}):\n",
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

/// Formats active session tasks for injection into the system prompt so
/// multi-step execution plans survive `/compact`.
pub fn format_active_tasks_for_prompt(config: &Config, session_id: &str) -> Option<String> {
    if session_id.trim().is_empty() {
        return None;
    }
    let conn = open_task_db(config).ok()?;
    let active = fetch_tasks(&conn, session_id, None).ok()?;
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

    let instruction = if config.app.task_autoloop && !config.app.task_autoloop_ignore_blocked {
        "Active session task plan (update with the `task` tool as you make progress; remove finished tasks; if you need user input before continuing, set the task's status to `blocked` with a note and ask your question):"
    } else {
        "Active session task plan (update with the `task` tool as you make progress; remove finished tasks):"
    };

    Some(format!(
        "<tasks>\n{instruction}\n{}\n</tasks>",
        lines.join("\n")
    ))
}

/// Returns `true` if `task_autoloop` should continue into another turn.
pub fn should_autoloop(config: &Config, session_id: &str) -> bool {
    if session_id.trim().is_empty() {
        return false;
    }
    let Ok(conn) = open_task_db(config) else {
        return false;
    };
    let Ok(rows) = fetch_tasks(&conn, session_id, None) else {
        return false;
    };
    if rows.is_empty() {
        return false;
    }
    if config.app.task_autoloop_ignore_blocked {
        return true;
    }
    !rows.iter().any(|r| r.status == TaskStatus::Blocked)
}

/// Clones all active tasks from one session into a forked session.
pub fn clone_session_tasks(
    config: &Config,
    from_session_id: &str,
    to_session_id: &str,
) -> Result<()> {
    let conn = open_task_db(config)?;
    conn.execute(
        "INSERT OR REPLACE INTO tasks (session_id, id, title, status, priority, notes, created_at, updated_at)
         SELECT ?2, id, title, status, priority, notes, created_at, updated_at
         FROM tasks
         WHERE session_id = ?1",
        params![from_session_id, to_session_id],
    )?;
    Ok(())
}

/// Removes all tasks belonging to a deleted session.
pub fn delete_session_tasks(config: &Config, session_id: &str) -> Result<()> {
    let conn = open_task_db(config)?;
    conn.execute("DELETE FROM tasks WHERE session_id = ?1", params![session_id])?;
    Ok(())
}

impl BlockingTool for Tool {
    const NAME: &'static str = "task";
    const DESCRIPTION: &'static str = "Manage a session-scoped execution plan (`list`, `add`, `update`, `remove`, `clear`) to track active multi-step work across turns and compactions.";
    type Args = TaskArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let session_id = ctx
            .session_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("default");
        let conn = open_task_db(&ctx.config)?;
        let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

        match args.action {
            TaskAction::List => {
                let rows = fetch_tasks(&conn, session_id, args.status)?;
                Ok(format_task_list(&rows))
            }
            TaskAction::Add => {
                let mut to_insert = args.items;
                if let Some(title) = args.title.map(|t| t.trim().to_string())
                    && !title.is_empty()
                {
                    to_insert.push(NewTaskItem {
                        title,
                        priority: args.priority.unwrap_or_default(),
                        status: args.status.unwrap_or_default(),
                        notes: args.notes.unwrap_or_default(),
                    });
                }
                if to_insert.is_empty() {
                    bail!("`task:add` requires `title` or a non-empty `items` array.");
                }

                let mut added_ids = Vec::with_capacity(to_insert.len());
                for item in to_insert {
                    let title = item.title.trim();
                    if title.is_empty() {
                        bail!("Task `title` cannot be empty.");
                    }
                    let next_id: i64 = conn.query_row(
                        "SELECT COALESCE(MAX(id), 0) + 1 FROM tasks WHERE session_id = ?1",
                        params![session_id],
                        |r| r.get(0),
                    )?;
                    conn.execute(
                        "INSERT INTO tasks (session_id, id, title, status, priority, notes, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            session_id,
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

                let rows = fetch_tasks(&conn, session_id, None)?;
                let id_list = added_ids
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(format!("Added {id_list}.\n\n{}", format_task_list(&rows)))
            }
            TaskAction::Update => {
                let id = args.id.context("`task:update` requires `id`.")?;
                let existing: Option<(String, String, String, String)> = conn
                    .query_row(
                        "SELECT title, status, priority, notes FROM tasks WHERE id = ?1 AND session_id = ?2",
                        params![id, session_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .ok();
                let Some((cur_title, cur_status, cur_priority, cur_notes)) = existing else {
                    bail!("Task #{id} not found in this session.");
                };

                let new_title = args
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&cur_title);
                let new_status = args.status.map(TaskStatus::as_str).unwrap_or(&cur_status);
                let new_priority = args
                    .priority
                    .map(TaskPriority::as_str)
                    .unwrap_or(&cur_priority);
                let new_notes = args.notes.as_deref().map(str::trim).unwrap_or(&cur_notes);

                conn.execute(
                    "UPDATE tasks SET title = ?1, status = ?2, priority = ?3, notes = ?4, updated_at = ?5
                     WHERE id = ?6 AND session_id = ?7",
                    params![new_title, new_status, new_priority, new_notes, now, id, session_id],
                )?;

                let rows = fetch_tasks(&conn, session_id, None)?;
                Ok(format!(
                    "Updated task #{id}.\n\n{}",
                    format_task_list(&rows)
                ))
            }
            TaskAction::Remove => {
                let id = args.id.context("`task:remove` requires `id`.")?;
                let deleted = conn.execute(
                    "DELETE FROM tasks WHERE id = ?1 AND session_id = ?2",
                    params![id, session_id],
                )?;
                if deleted == 0 {
                    bail!("Task #{id} not found in this session.");
                }
                let rows = fetch_tasks(&conn, session_id, None)?;
                Ok(format!(
                    "Removed task #{id}.\n\n{}",
                    format_task_list(&rows)
                ))
            }
            TaskAction::Clear => {
                let deleted =
                    conn.execute("DELETE FROM tasks WHERE session_id = ?1", params![session_id])?;
                Ok(format!("Cleared {deleted} task(s)."))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(tmp: &tempfile::TempDir, session_id: &str) -> ToolContext {
        let mut cfg = Config::default();
        cfg.app.history_dir = tmp.path().to_string_lossy().into_owned();
        cfg.workspace = Some(tmp.path().to_string_lossy().into_owned());
        ToolContext {
            root: tmp.path().to_path_buf(),
            max_output_size: 32_768,
            active_skill: None,
            session_id: Some(session_id.to_string()),
            config: cfg,
        }
    }

    #[test]
    fn add_update_remove_clear_and_session_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx_a = test_ctx(&tmp, "sess-a");
        let ctx_b = test_ctx(&tmp, "sess-b");

        let added = Tool::execute(
            TaskArgs {
                action: TaskAction::Add,
                id: None,
                title: None,
                items: vec![
                    NewTaskItem {
                        title: "Implement task tool".into(),
                        priority: TaskPriority::High,
                        status: TaskStatus::InProgress,
                        notes: "src/tools/task.rs".into(),
                    },
                    NewTaskItem {
                        title: "Register in tools.rs".into(),
                        priority: TaskPriority::Medium,
                        status: TaskStatus::Pending,
                        notes: String::new(),
                    },
                ],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx_a,
        )
        .unwrap();
        assert!(added.contains("#1 [~] [HIGH] Implement task tool"));
        assert!(added.contains("#2 [ ] [med] Register in tools.rs"));

        let prompt_a = format_active_tasks_for_prompt(&ctx_a.config, "sess-a").unwrap();
        assert!(prompt_a.contains("<tasks>"));
        assert!(prompt_a.contains("Implement task tool"));
        assert!(format_active_tasks_for_prompt(&ctx_b.config, "sess-b").is_none());

        clone_session_tasks(&ctx_a.config, "sess-a", "sess-b").unwrap();
        assert!(format_active_tasks_for_prompt(&ctx_b.config, "sess-b").is_some());

        let removed = Tool::execute(
            TaskArgs {
                action: TaskAction::Remove,
                id: Some(1),
                title: None,
                items: vec![],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx_a,
        )
        .unwrap();
        assert!(removed.contains("Removed task #1."));
        assert!(removed.contains("1 total"));

        let cleared = Tool::execute(
            TaskArgs {
                action: TaskAction::Clear,
                id: None,
                title: None,
                items: vec![],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx_a,
        )
        .unwrap();
        assert!(cleared.contains("Cleared 1 task(s)."));

        let readded = Tool::execute(
            TaskArgs {
                action: TaskAction::Add,
                id: None,
                title: Some("Restart numbering".into()),
                items: vec![],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx_a,
        )
        .unwrap();
        assert!(readded.contains("Added #1."));
    }

    #[test]
    fn blocked_status_pauses_autoloop_unless_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(&tmp, "sess-block");

        Tool::execute(
            TaskArgs {
                action: TaskAction::Add,
                id: None,
                title: Some("Clarify API contract".into()),
                items: vec![],
                status: Some(TaskStatus::Blocked),
                priority: Some(TaskPriority::High),
                notes: Some("Waiting on user".into()),
            },
            &ctx,
        )
        .unwrap();

        ctx.config.app.task_autoloop_ignore_blocked = false;
        assert!(!should_autoloop(&ctx.config, "sess-block"));

        ctx.config.app.task_autoloop_ignore_blocked = true;
        assert!(should_autoloop(&ctx.config, "sess-block"));
    }
}
