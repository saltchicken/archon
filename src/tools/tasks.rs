//! Shared engine behind the `plan` and `todo` tools.
//!
//! Both tools are CRUD managers for tasks stored in a single unified `tasks`
//! table; they differ only in:
//!   * **scope key** — `plan` keys by session id, `todo` keys by workspace path
//!     (or the empty string for the global backlog), and
//!   * **priority** — surfaced by `todo`, ignored by `plan`.
//!
//! The `plan` tool additionally drives autoloop and system-prompt injection via
//! the helpers in [`crate::tools::plan`].

use crate::core::config::Config;
use anyhow::{Context, Result, bail};
use chrono::Local;
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::Deserialize;
use std::fmt::Write as _;

/// Which logical collection a task belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// Session-scoped execution plan (the `plan` tool).
    Plan,
    /// Persistent workspace/global backlog (the `todo` tool).
    Todo,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Todo => "todo",
        }
    }
}

/// CRUD operation shared by both tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// List tasks.
    #[default]
    List,
    /// Add a single task (`title`) or batch-add multiple tasks (`tasks`).
    Add,
    /// Update an existing task's `status`, `title`, `priority`, or `notes` by `id`.
    Update,
    /// Remove a task by `id`.
    Remove,
    /// Clear tasks (pass `status` to clear only that status, or omit to clear all).
    Clear,
}

/// Lifecycle status shared by both tools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Queued / not started.
    #[default]
    Pending,
    /// Actively being worked on.
    InProgress,
    /// Waiting on input or an external dependency.
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

    /// Bracketed display label (e.g. `[in_progress]`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "[pending]",
            Self::InProgress => "[in_progress]",
            Self::Blocked => "[blocked]",
        }
    }
}

/// Priority (surfaced by `todo`; ignored by `plan`).
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

    /// Compact display badge (e.g. `HIGH`).
    pub fn badge(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "med",
            Self::High => "HIGH",
        }
    }
}

/// A single task to insert (used by both tools' `add`).
#[derive(Debug, Clone)]
pub struct NewTask {
    pub title: String,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub notes: String,
}

/// A task row read back from the database.
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: i64,
    pub title: String,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub notes: String,
}

/// Fully-resolved parameters for a single CRUD operation.
pub struct ExecParams {
    pub kind: TaskKind,
    /// Scope key: session id for `plan`, workspace path (or `""` for global) for `todo`.
    pub scope_key: String,
    pub action: Action,
    pub id: Option<i64>,
    pub title: Option<String>,
    pub tasks: Vec<NewTask>,
    pub status: Option<TaskStatus>,
    pub priority: Option<TaskPriority>,
    pub notes: Option<String>,
    /// Whether to render the priority badge in list output.
    pub show_priority: bool,
    /// Human-readable collection label for headers (e.g. "Session plan").
    pub header_label: &'static str,
    /// Message rendered when the list is empty.
    pub empty_message: &'static str,
    /// Noun used in confirmation/error messages (e.g. "task", "todo").
    pub noun: &'static str,
}

/// Opens the shared database and ensures the unified `tasks` table exists.
pub fn open_tasks_db(config: &Config) -> Result<Connection> {
    let conn = crate::core::db::open_archon_db(&config.app.history_dir)?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
             kind TEXT NOT NULL,
             scope_key TEXT NOT NULL,
             id INTEGER NOT NULL,
             title TEXT NOT NULL,
             status TEXT NOT NULL DEFAULT 'pending',
             priority TEXT NOT NULL DEFAULT 'medium',
             notes TEXT NOT NULL DEFAULT '',
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             PRIMARY KEY (kind, scope_key, id)
         );

         CREATE INDEX IF NOT EXISTS idx_tasks_scope_status
             ON tasks(kind, scope_key, status, id);",
    )
    .context("Failed to initialize tasks table")?;

    Ok(conn)
}

/// Fetches tasks for a `(kind, scope_key)` pair, ordered by status then priority.
pub fn fetch_tasks(
    conn: &Connection,
    kind: TaskKind,
    scope_key: &str,
    status_filter: Option<TaskStatus>,
) -> Result<Vec<TaskRow>> {
    let mut rows = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id, title, status, priority, notes
         FROM tasks
         WHERE kind = ?1 AND scope_key = ?2
         ORDER BY
             CASE status WHEN 'in_progress' THEN 0 WHEN 'pending' THEN 1 ELSE 2 END,
             CASE priority WHEN 'high' THEN 0 WHEN 'medium' THEN 1 ELSE 2 END,
             id ASC",
    )?;

    let iter = stmt.query_map(params![kind.as_str(), scope_key], |row| {
        Ok(TaskRow {
            id: row.get(0)?,
            title: row.get(1)?,
            status: TaskStatus::from_db(&row.get::<_, String>(2)?),
            priority: TaskPriority::from_db(&row.get::<_, String>(3)?),
            notes: row.get(4)?,
        })
    })?;

    for task in iter.flatten() {
        if let Some(filter) = status_filter
            && task.status != filter
        {
            continue;
        }
        rows.push(task);
    }
    Ok(rows)
}

/// Renders a human-readable list of tasks.
pub fn format_task_list(
    rows: &[TaskRow],
    header_label: &str,
    empty_message: &str,
    show_priority: bool,
) -> String {
    if rows.is_empty() {
        return empty_message.to_string();
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
        "{header_label} ({} total, {in_prog} in progress{blocked_suffix}):\n",
        rows.len()
    );

    for r in rows {
        if show_priority {
            let _ = writeln!(
                out,
                "  #{} {} [{}] {}",
                r.id,
                r.status.label(),
                r.priority.badge(),
                r.title
            );
        } else {
            let _ = writeln!(out, "  #{} {} {}", r.id, r.status.label(), r.title);
        }
        if !r.notes.trim().is_empty() {
            let _ = writeln!(out, "      note: {}", r.notes.trim());
        }
    }
    out.trim_end().to_string()
}

/// Executes a single CRUD operation against the unified `tasks` table.
pub fn execute(config: &Config, p: ExecParams) -> Result<String> {
    let conn = open_tasks_db(config)?;
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let kind = p.kind.as_str();

    match p.action {
        Action::List => {
            let rows = fetch_tasks(&conn, p.kind, &p.scope_key, p.status)?;
            Ok(format_task_list(
                &rows,
                p.header_label,
                p.empty_message,
                p.show_priority,
            ))
        }
        Action::Add => {
            let mut to_insert = p.tasks;
            if let Some(title) = p.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                to_insert.push(NewTask {
                    title: title.to_string(),
                    status: p.status.unwrap_or_default(),
                    priority: p.priority.unwrap_or_default(),
                    notes: p.notes.unwrap_or_default(),
                });
            }
            if to_insert.is_empty() {
                bail!("`{kind}:add` requires `title` or a non-empty `tasks` array.");
            }

            let tx = conn.unchecked_transaction()?;
            let mut added_ids = Vec::with_capacity(to_insert.len());
            for task in to_insert {
                let title = task.title.trim();
                if title.is_empty() {
                    bail!("Task `title` cannot be empty.");
                }
                let next_id: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(id), 0) + 1 FROM tasks WHERE kind = ?1 AND scope_key = ?2",
                    params![kind, p.scope_key],
                    |r| r.get(0),
                )?;
                tx.execute(
                    "INSERT INTO tasks (kind, scope_key, id, title, status, priority, notes, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        kind,
                        p.scope_key,
                        next_id,
                        title,
                        task.status.as_str(),
                        task.priority.as_str(),
                        task.notes.trim(),
                        now,
                        now,
                    ],
                )?;
                added_ids.push(next_id);
            }
            tx.commit()?;

            let rows = fetch_tasks(&conn, p.kind, &p.scope_key, None)?;
            let id_list = added_ids
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!(
                "Added {id_list}.\n\n{}",
                format_task_list(&rows, p.header_label, p.empty_message, p.show_priority)
            ))
        }
        Action::Update => {
            let id = p.id.context(format!("`{kind}:update` requires `id`."))?;
            let existing: Option<(String, String, String, String)> = conn
                .query_row(
                    "SELECT title, status, priority, notes FROM tasks WHERE kind = ?1 AND scope_key = ?2 AND id = ?3",
                    params![kind, p.scope_key, id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .ok();
            let Some((cur_title, cur_status, cur_priority, cur_notes)) = existing else {
                bail!("{noun} #{id} not found.", noun = p.noun);
            };

            let new_title = p
                .title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&cur_title);
            let new_status = p.status.map(TaskStatus::as_str).unwrap_or(&cur_status);
            let new_priority = p
                .priority
                .map(TaskPriority::as_str)
                .unwrap_or(&cur_priority);
            let new_notes = p.notes.as_deref().map(str::trim).unwrap_or(&cur_notes);

            conn.execute(
                "UPDATE tasks SET title = ?1, status = ?2, priority = ?3, notes = ?4, updated_at = ?5
                 WHERE kind = ?6 AND scope_key = ?7 AND id = ?8",
                params![
                    new_title,
                    new_status,
                    new_priority,
                    new_notes,
                    now,
                    kind,
                    p.scope_key,
                    id
                ],
            )?;

            let rows = fetch_tasks(&conn, p.kind, &p.scope_key, None)?;
            Ok(format!(
                "Updated {noun} #{id}.\n\n{}",
                format_task_list(&rows, p.header_label, p.empty_message, p.show_priority),
                noun = p.noun
            ))
        }
        Action::Remove => {
            let id = p.id.context(format!("`{kind}:remove` requires `id`."))?;
            let deleted = conn.execute(
                "DELETE FROM tasks WHERE kind = ?1 AND scope_key = ?2 AND id = ?3",
                params![kind, p.scope_key, id],
            )?;
            if deleted == 0 {
                bail!("{noun} #{id} not found.", noun = p.noun);
            }
            let rows = fetch_tasks(&conn, p.kind, &p.scope_key, None)?;
            Ok(format!(
                "Removed {noun} #{id}.\n\n{}",
                format_task_list(&rows, p.header_label, p.empty_message, p.show_priority),
                noun = p.noun
            ))
        }
        Action::Clear => {
            let deleted = if let Some(status) = p.status {
                conn.execute(
                    "DELETE FROM tasks WHERE kind = ?1 AND scope_key = ?2 AND status = ?3",
                    params![kind, p.scope_key, status.as_str()],
                )?
            } else {
                conn.execute(
                    "DELETE FROM tasks WHERE kind = ?1 AND scope_key = ?2",
                    params![kind, p.scope_key],
                )?
            };
            Ok(format!("Cleared {deleted} {noun}(s).", noun = p.noun))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_and_todo_kinds_are_isolated_in_unified_table() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.app.history_dir = tmp.path().to_string_lossy().into_owned();

        execute(
            &cfg,
            ExecParams {
                kind: TaskKind::Plan,
                scope_key: "shared-scope".into(),
                action: Action::Add,
                id: None,
                title: Some("Plan step".into()),
                tasks: vec![],
                status: None,
                priority: None,
                notes: None,
                show_priority: false,
                header_label: "Session plan",
                empty_message: "No active tasks in this session plan.",
                noun: "task",
            },
        )
        .unwrap();

        execute(
            &cfg,
            ExecParams {
                kind: TaskKind::Todo,
                scope_key: "shared-scope".into(),
                action: Action::Add,
                id: None,
                title: Some("Todo item".into()),
                tasks: vec![],
                status: None,
                priority: Some(TaskPriority::High),
                notes: None,
                show_priority: true,
                header_label: "Workspace todos",
                empty_message: "No workspace todos found.",
                noun: "todo",
            },
        )
        .unwrap();

        let conn = open_tasks_db(&cfg).unwrap();
        let plan_rows = fetch_tasks(&conn, TaskKind::Plan, "shared-scope", None).unwrap();
        let todo_rows = fetch_tasks(&conn, TaskKind::Todo, "shared-scope", None).unwrap();

        assert_eq!(plan_rows.len(), 1);
        assert_eq!(plan_rows[0].title, "Plan step");
        assert_eq!(todo_rows.len(), 1);
        assert_eq!(todo_rows[0].title, "Todo item");
        assert_eq!(todo_rows[0].priority, TaskPriority::High);
    }
}
