//! The `plan` tool: a session-scoped execution plan.
//!
//! CRUD is delegated to the shared [`super::tasks`] engine; this module adds the
//! session-specific behavior — system-prompt injection, autoloop gating, and
//! plan cloning/cleanup on session fork/delete.

use super::tasks::{self, Action, NewTask, TaskKind, TaskPriority, TaskStatus};
use super::{BlockingTool, ToolContext};
use crate::config::Config;
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct NewPlanTask {
    /// Short, actionable step description.
    pub title: String,
    /// Optional initial status (`pending`, `in_progress`, `blocked`). Defaults to `pending`.
    #[serde(default)]
    pub status: TaskStatus,
    /// Optional details, file paths, or acceptance notes.
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PlanArgs {
    /// Operation to perform: `list` (default), `add`, `update`, `remove`, or `clear`.
    #[serde(default)]
    pub action: Action,
    /// Task ID (for `update` or `remove`).
    #[serde(default)]
    pub id: Option<i64>,
    /// Task title (for single `add` or `update`).
    #[serde(default)]
    pub title: Option<String>,
    /// Batch of tasks to add in a single call (for `add`).
    #[serde(default)]
    pub tasks: Vec<NewPlanTask>,
    /// Task status (`pending`, `in_progress`, `blocked`) for `add`, `update`, or filtering `list`.
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// Optional notes for `add` or `update`.
    #[serde(default)]
    pub notes: Option<String>,
}

/// Formats active session plan tasks for injection into the system prompt so
/// multi-step execution plans survive `/compact`.
pub fn format_active_plan_for_prompt(config: &Config, session_id: &str) -> Option<String> {
    if session_id.trim().is_empty() {
        return None;
    }
    let conn = tasks::open_tasks_db(config).ok()?;
    let active = tasks::fetch_tasks(&conn, TaskKind::Plan, session_id, None).ok()?;
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
            "- #{} {} {}{}",
            r.id,
            r.status.label(),
            r.title,
            note_suffix
        ));
    }

    let instruction = if config.app.plan_autoloop && !config.app.plan_autoloop_ignore_blocked {
        "Active session plan (update with the `plan` tool as you make progress; valid statuses are only `pending`, `in_progress`, `blocked` — there is NO completed/done status, so when a task finishes remove it with `action: \"remove\"` and its `id`; if you need user input before continuing, set the task's status to `blocked` with a note and ask your question):"
    } else {
        "Active session plan (update with the `plan` tool as you make progress; valid statuses are only `pending`, `in_progress`, `blocked` — there is NO completed/done status, so when a task finishes remove it with `action: \"remove\"` and its `id`):"
    };

    Some(format!(
        "<plan>\n{instruction}\n{}\n</plan>",
        lines.join("\n")
    ))
}

/// Returns `true` if `plan_autoloop` should continue into another turn.
pub fn should_autoloop(config: &Config, session_id: &str) -> bool {
    if session_id.trim().is_empty() {
        return false;
    }
    let Ok(conn) = tasks::open_tasks_db(config) else {
        return false;
    };
    let Ok(rows) = tasks::fetch_tasks(&conn, TaskKind::Plan, session_id, None) else {
        return false;
    };
    if rows.is_empty() {
        return false;
    }
    if config.app.plan_autoloop_ignore_blocked {
        return true;
    }
    !rows.iter().any(|r| r.status == TaskStatus::Blocked)
}

/// Clones all active plan tasks from one session into a forked session.
pub fn clone_session_plan(
    config: &Config,
    from_session_id: &str,
    to_session_id: &str,
) -> Result<()> {
    let conn = tasks::open_tasks_db(config)?;
    conn.execute(
        "INSERT OR REPLACE INTO tasks (kind, scope_key, id, title, status, priority, notes, created_at, updated_at)
         SELECT 'plan', ?2, id, title, status, priority, notes, created_at, updated_at
         FROM tasks
         WHERE kind = 'plan' AND scope_key = ?1",
        rusqlite::params![from_session_id, to_session_id],
    )?;
    Ok(())
}

/// Removes all plan tasks belonging to a deleted session.
pub fn delete_session_plan(config: &Config, session_id: &str) -> Result<()> {
    let conn = tasks::open_tasks_db(config)?;
    conn.execute(
        "DELETE FROM tasks WHERE kind = 'plan' AND scope_key = ?1",
        rusqlite::params![session_id],
    )?;
    Ok(())
}

impl BlockingTool for Tool {
    const NAME: &'static str = "plan";
    const DESCRIPTION: &'static str = "Manage a session-scoped execution plan (`list`, `add`, `update`, `remove`, `clear`) to track active multi-step work across turns and compactions. Valid statuses are only `pending`, `in_progress`, `blocked`. There is NO completed/done status: when a task finishes, remove it with `action: \"remove\"` and its `id`.";
    const SUBACTIONS: &'static [&'static str] = &["list", "add", "update", "remove", "clear"];
    const DEFAULT_ACTION: Option<&'static str> = Some("list");
    const PARAM_HINT: &'static str =
        "[list|add|update|remove|clear] [title=...] [id=...] [status=...]";
    const SHORT_SUMMARY: &'static str =
        "Manage a session-scoped execution plan of tasks across turns and compactions.";

    type Args = PlanArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let session_id = ctx
            .session_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("default");
        let tasks: Vec<NewTask> = args
            .tasks
            .into_iter()
            .map(|t| NewTask {
                title: t.title,
                status: t.status,
                priority: TaskPriority::default(),
                notes: t.notes,
            })
            .collect();

        tasks::execute(
            &ctx.config,
            tasks::ExecParams {
                kind: TaskKind::Plan,
                scope_key: session_id.to_string(),
                action: args.action,
                id: args.id,
                title: args.title,
                tasks,
                status: args.status,
                priority: None,
                notes: args.notes,
                show_priority: false,
                header_label: "Session plan",
                empty_message: "No active tasks in this session plan.",
                noun: "task",
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_ctx(tmp: &tempfile::TempDir, session_id: &str) -> ToolContext {
        let mut cfg = Config::default();
        cfg.app.history_dir = tmp.path().to_string_lossy().into_owned();
        ToolContext {
            root: tmp.path().to_path_buf(),
            max_output_size: 4096,
            active_skill: None,
            session_id: Some(session_id.to_string()),
            session_tools: crate::tools::SessionToolState::default(),
            config: cfg,
        }
    }

    #[test]
    fn add_update_remove_clear_and_session_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx_a = test_ctx(&tmp, "sess-a");
        let ctx_b = test_ctx(&tmp, "sess-b");

        let added = Tool::execute(
            PlanArgs {
                action: Action::Add,
                id: None,
                title: None,
                tasks: vec![
                    NewPlanTask {
                        title: "Implement plan tool".into(),
                        status: TaskStatus::InProgress,
                        notes: "src/tools/plan.rs".into(),
                    },
                    NewPlanTask {
                        title: "Register in tools.rs".into(),
                        status: TaskStatus::Pending,
                        notes: String::new(),
                    },
                ],
                status: None,
                notes: None,
            },
            &ctx_a,
        )
        .unwrap();
        assert!(added.contains("#1 [in_progress] Implement plan tool"));
        assert!(added.contains("#2 [pending] Register in tools.rs"));

        let prompt_a = format_active_plan_for_prompt(&ctx_a.config, "sess-a").unwrap();
        assert!(prompt_a.contains("<plan>"));
        assert!(prompt_a.contains("Implement plan tool"));
        assert!(format_active_plan_for_prompt(&ctx_b.config, "sess-b").is_none());

        clone_session_plan(&ctx_a.config, "sess-a", "sess-b").unwrap();
        assert!(format_active_plan_for_prompt(&ctx_b.config, "sess-b").is_some());

        let removed = Tool::execute(
            PlanArgs {
                action: Action::Remove,
                id: Some(1),
                title: None,
                tasks: vec![],
                status: None,
                notes: None,
            },
            &ctx_a,
        )
        .unwrap();
        assert!(removed.contains("Removed task #1."));
        assert!(removed.contains("1 total"));

        let cleared = Tool::execute(
            PlanArgs {
                action: Action::Clear,
                id: None,
                title: None,
                tasks: vec![],
                status: None,
                notes: None,
            },
            &ctx_a,
        )
        .unwrap();
        assert!(cleared.contains("Cleared 1 task(s)."));

        let readded = Tool::execute(
            PlanArgs {
                action: Action::Add,
                id: None,
                title: Some("Restart numbering".into()),
                tasks: vec![],
                status: None,
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
            PlanArgs {
                action: Action::Add,
                id: None,
                title: Some("Clarify API contract".into()),
                tasks: vec![],
                status: Some(TaskStatus::Blocked),
                notes: Some("Waiting on user".into()),
            },
            &ctx,
        )
        .unwrap();

        ctx.config.app.plan_autoloop_ignore_blocked = false;
        assert!(!should_autoloop(&ctx.config, "sess-block"));

        ctx.config.app.plan_autoloop_ignore_blocked = true;
        assert!(should_autoloop(&ctx.config, "sess-block"));
    }
}
