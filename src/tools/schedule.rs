use super::{BlockingTool, ToolContext};
use crate::{cli::ScheduleCommand, core::schedule};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;

const DEFAULT_LOG_LINES: usize = 50;
const MAX_LOG_LINES: usize = 500;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAction {
    /// List configured schedules and active systemd user timers.
    List,
    /// Show journalctl logs for a specific schedule or all schedules in scope.
    Logs,
    /// Trigger a scheduled systemd service immediately.
    Run,
    /// Generate and sync systemd user units from config.toml.
    Sync,
    /// Disable and remove all managed systemd units for this scope.
    Clean,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScheduleArgs {
    /// Action to perform: `list`, `logs`, `run`, `sync`, or `clean`.
    pub action: ScheduleAction,
    /// Schedule name (required for `run`, optional filter for `logs`).
    #[serde(default)]
    pub name: Option<String>,
    /// Number of journalctl log lines to return for `logs` (default: 50, max: 500).
    #[serde(default)]
    pub lines: Option<usize>,
    /// Operate on global `~/.config/archon/config.toml` schedules instead of workspace schedules (default: false).
    #[serde(default)]
    pub global: bool,
}

impl BlockingTool for Tool {
    const NAME: &'static str = "schedule";
    const DESCRIPTION: &'static str = "Manage Archon's systemd user schedules: list configured timers, inspect journalctl logs, trigger a schedule immediately, sync unit files from config.toml, or clean managed units.";
    const SUBACTIONS: &'static [&'static str] = &["list", "logs", "run", "sync", "clean"];
    const PARAM_HINT: &'static str = "<list|logs|run|sync|clean> [name=...] [global=true]";
    const SHORT_SUMMARY: &'static str =
        "List, inspect logs, trigger, sync, or clean Archon systemd user schedules.";
    type Args = ScheduleArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let clean_name = args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        let cmd = match args.action {
            ScheduleAction::List => ScheduleCommand::List {
                global: args.global,
            },
            ScheduleAction::Logs => ScheduleCommand::Logs {
                name: clean_name,
                lines: args
                    .lines
                    .unwrap_or(DEFAULT_LOG_LINES)
                    .clamp(1, MAX_LOG_LINES),
                global: args.global,
            },
            ScheduleAction::Run => ScheduleCommand::Run {
                name: clean_name.context("'name' is required for schedule:run")?,
                global: args.global,
            },
            ScheduleAction::Sync => ScheduleCommand::Sync {
                global: args.global,
            },
            ScheduleAction::Clean => ScheduleCommand::Clean {
                global: args.global,
            },
        };

        schedule::execute_command(cmd, &ctx.config)
    }
}
