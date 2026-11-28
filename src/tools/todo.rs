//! The `todo` tool: a persistent workspace or global backlog.
//!
//! CRUD is delegated to the shared [`super::tasks`] engine; this module adds the
//! backlog-specific behavior — `workspace`/`global` scoping and priority.

use super::tasks::{self, Action, NewTask, TaskKind, TaskPriority, TaskStatus};
use super::{BlockingTool, ToolContext};
use crate::config::{Config, expand_tilde};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;

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

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct NewTodoTask {
    /// Short description of the todo task.
    pub title: String,
    /// Optional priority (`low`, `medium`, `high`). Defaults to `medium`.
    #[serde(default)]
    pub priority: TaskPriority,
    /// Optional initial status (`pending`, `in_progress`, `blocked`). Defaults to `pending`.
    #[serde(default)]
    pub status: TaskStatus,
    /// Optional notes or context.
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TodoArgs {
    /// Operation to perform: `list` (default), `add`, `update`, `remove`, or `clear`.
    #[serde(default)]
    pub action: Action,
    /// Backlog scope: `workspace` (default) or `global`.
    #[serde(default)]
    pub scope: TodoScope,
    /// Todo ID (for `update` or `remove`).
    #[serde(default)]
    pub id: Option<i64>,
    /// Todo title (for single `add` or `update`).
    #[serde(default)]
    pub title: Option<String>,
    /// Batch of todo tasks to add in a single call (for `add`).
    #[serde(default)]
    pub tasks: Vec<NewTodoTask>,
    /// Todo status (`pending`, `in_progress`, `blocked`) for `add`, `update`, `list`, or `clear`.
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// Todo priority (`low`, `medium`, `high`) for `add` or `update`.
    #[serde(default)]
    pub priority: Option<TaskPriority>,
    /// Optional notes for `add` or `update`.
    #[serde(default)]
    pub notes: Option<String>,
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

impl BlockingTool for Tool {
    const NAME: &'static str = "todo";
    const DESCRIPTION: &'static str = "Manage a persistent workspace or global todo backlog (`list`, `add`, `update`, `remove`, `clear`). Passive tracker — does not trigger autoloop or inject into the system prompt.";
    const SUBACTIONS: &'static [&'static str] = &["list", "add", "update", "remove", "clear"];
    const PARAM_HINT: &'static str =
        "<list|add|update|remove|clear> [scope=workspace|global] [id=...] [title=...]";
    const SHORT_SUMMARY: &'static str = "Manage a persistent workspace or global todo backlog.";
    type Args = TodoArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let scope_key = scope_key(&ctx.config, args.scope);
        let (header_label, empty_message) = match args.scope {
            TodoScope::Workspace => ("Workspace todos", "No workspace todos found."),
            TodoScope::Global => ("Global todos", "No global todos found."),
        };
        let tasks: Vec<NewTask> = args
            .tasks
            .into_iter()
            .map(|t| NewTask {
                title: t.title,
                status: t.status,
                priority: t.priority,
                notes: t.notes,
            })
            .collect();

        tasks::execute(
            &ctx.config,
            tasks::ExecParams {
                kind: TaskKind::Todo,
                scope_key,
                action: args.action,
                id: args.id,
                title: args.title,
                tasks,
                status: args.status,
                priority: args.priority,
                notes: args.notes,
                show_priority: true,
                header_label,
                empty_message,
                noun: "todo",
            },
        )
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
            session_tools: crate::tools::SessionToolState::default(),
            config: cfg,
        }
    }

    #[test]
    fn backlog_add_update_remove_and_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&tmp);

        let added = Tool::execute(
            TodoArgs {
                action: Action::Add,
                scope: TodoScope::Workspace,
                id: None,
                title: None,
                tasks: vec![
                    NewTodoTask {
                        title: "Refactor markdown parser".into(),
                        priority: TaskPriority::High,
                        status: TaskStatus::Pending,
                        notes: "src/ui/markdown.rs".into(),
                    },
                    NewTodoTask {
                        title: "Add Windows clipboard".into(),
                        priority: TaskPriority::Low,
                        status: TaskStatus::Pending,
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
        assert!(added.contains("#1 [pending] [HIGH] Refactor markdown parser"));
        assert!(added.contains("#2 [pending] [low] Add Windows clipboard"));

        let updated = Tool::execute(
            TodoArgs {
                action: Action::Update,
                scope: TodoScope::Workspace,
                id: Some(1),
                title: None,
                tasks: vec![],
                status: Some(TaskStatus::Blocked),
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(updated.contains("#1 [blocked] [HIGH] Refactor markdown parser"));
        assert!(updated.contains("1 blocked"));

        let removed = Tool::execute(
            TodoArgs {
                action: Action::Remove,
                scope: TodoScope::Workspace,
                id: Some(1),
                title: None,
                tasks: vec![],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(removed.contains("Removed todo #1."));
        assert!(!removed.contains("Refactor markdown parser"));

        let cleared = Tool::execute(
            TodoArgs {
                action: Action::Clear,
                scope: TodoScope::Workspace,
                id: None,
                title: None,
                tasks: vec![],
                status: None,
                priority: None,
                notes: None,
            },
            &ctx,
        )
        .unwrap();
        assert!(cleared.contains("Cleared 1 todo(s)."));
    }
}
