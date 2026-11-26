use super::{BlockingTool, ToolContext};
use crate::core::session::{ChatMessage, SessionManager, truncate_str};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::fmt::Write as _;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    /// List saved sessions (ID, title, message count, model, pinned status, updated timestamp).
    List,
    /// Search message contents and titles across saved sessions.
    Search,
    /// Read messages from a specific session by ID (supports 1-based message slicing).
    Read,
    /// Show aggregate statistics (total sessions, messages, models used) for the workspace or globally.
    Stats,
    /// Rename a saved session's title.
    Rename,
    /// Pin or unpin a saved session (toggles if `pinned` is omitted).
    Pin,
}

#[derive(Deserialize, JsonSchema)]
pub struct SessionArgs {
    /// Action to perform.
    pub action: SessionAction,
    /// Session ID (required for `read`, `rename`, `pin`; e.g. `"20261125_141022_123"`).
    #[serde(default)]
    pub id: Option<String>,
    /// Search query (case-insensitive substring, or regex when `regex: true`) for `search`.
    #[serde(default)]
    pub query: Option<String>,
    /// Treat `query` as a regular expression instead of a case-insensitive substring (default: false).
    #[serde(default)]
    pub regex: bool,
    /// Include sessions across all workspaces instead of only the current workspace (default: false).
    #[serde(default)]
    pub all_workspaces: bool,
    /// Only include pinned sessions in `list` or `search` (default: false).
    #[serde(default)]
    pub pinned_only: bool,
    /// 1-based starting message index for `read` (inclusive, default: 1).
    #[serde(default)]
    pub start_message: Option<usize>,
    /// 1-based ending message index for `read` (inclusive).
    #[serde(default)]
    pub end_message: Option<usize>,
    /// Include full `tool`, `local`, and `hook` outputs when reading a session (default: false).
    #[serde(default)]
    pub include_tool_outputs: bool,
    /// New title for `rename`.
    #[serde(default)]
    pub title: Option<String>,
    /// Explicit pinned state for `pin` (if omitted, toggles the current state).
    #[serde(default)]
    pub pinned: Option<bool>,
    /// Maximum number of sessions or matching sessions to return (default: 20, max: 100).
    #[serde(default)]
    pub limit: Option<usize>,
}

fn require_id<'a>(args: &'a SessionArgs, action_name: &str) -> Result<&'a str> {
    args.id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("'id' is required for session:{action_name}"))
}

fn format_message_for_read(
    idx_1based: usize,
    msg: &ChatMessage,
    include_tool_outputs: bool,
) -> String {
    let mut out = String::new();
    let role_upper = msg.role.to_ascii_uppercase();
    let header_meta = match msg.tool_name.as_deref() {
        Some(name) if !name.is_empty() => format!(" ({name})"),
        _ => String::new(),
    };
    let _ = writeln!(out, "[#{idx_1based} {role_upper}{header_meta}]");

    let is_bulky_role = matches!(msg.role.as_str(), "tool" | "local" | "hook");
    if is_bulky_role && !include_tool_outputs {
        let bytes = msg.content.len();
        let preview = msg
            .content
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| truncate_str(l.trim(), 100))
            .unwrap_or_default();
        if preview.is_empty() {
            let _ = writeln!(
                out,
                "[omitted {bytes} bytes — pass include_tool_outputs=true to view]"
            );
        } else {
            let _ = writeln!(
                out,
                "[omitted {bytes} bytes — pass include_tool_outputs=true to view] Preview: {preview}"
            );
        }
    } else if !msg.content.trim().is_empty() {
        let _ = writeln!(out, "{}", msg.content.trim_end());
    }

    if let Some(ref calls) = msg.tool_calls {
        for tc in calls {
            let _ = writeln!(
                out,
                "  -> call: {}({})",
                tc.function.name, tc.function.arguments
            );
        }
    }

    out
}

impl BlockingTool for Tool {
    const NAME: &'static str = "session";
    const DESCRIPTION: &'static str = "Inspect and search past Archon chat sessions stored in archon.db. Supports listing, searching transcripts, reading message slices, viewing workspace stats, and renaming or pinning sessions.";
    type Args = SessionArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let mgr =
            SessionManager::new(&ctx.config.app.history_dir, ctx.config.workspace.as_deref())?;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        match args.action {
            SessionAction::List => {
                let sessions = mgr.list_filtered(args.all_workspaces, args.pinned_only, limit);
                if sessions.is_empty() {
                    return Ok("No matching sessions found.".into());
                }

                let scope = if args.all_workspaces {
                    "all workspaces"
                } else {
                    "current workspace"
                };
                let mut out = format!("Sessions ({scope}, showing {}):\n", sessions.len());
                for s in sessions {
                    let pin = if s.pinned { " [pinned]" } else { "" };
                    let ws = if args.all_workspaces && !s.workspace.is_empty() {
                        format!(" | ws: {}", s.workspace)
                    } else {
                        String::new()
                    };
                    let _ = writeln!(
                        out,
                        "- {} | \"{}\"{} ({} msgs, {}, {}){}",
                        s.id, s.title, pin, s.messages, s.model, s.updated_at, ws
                    );
                }
                Ok(out.trim_end().to_string())
            }

            SessionAction::Search => {
                let query = args
                    .query
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'query' is required for session:search")?;

                let hits = mgr.search(
                    query,
                    args.regex,
                    args.all_workspaces,
                    args.pinned_only,
                    limit,
                )?;
                if hits.is_empty() {
                    return Ok(format!("No sessions matched query '{query}'."));
                }

                let mut out = format!("Found {} matching session(s):\n", hits.len());
                for hit in hits {
                    let pin = if hit.session.pinned { " [pinned]" } else { "" };
                    let _ = writeln!(
                        out,
                        "\n- {} | \"{}\"{} ({} msgs, {}, {})",
                        hit.session.id,
                        hit.session.title,
                        pin,
                        hit.session.messages,
                        hit.session.model,
                        hit.session.updated_at
                    );
                    if hit.title_matched {
                        let _ = writeln!(out, "  [title matched]");
                    }
                    for snip in &hit.snippets {
                        let _ = writeln!(
                            out,
                            "  #{} [{}]: {}",
                            snip.message_index, snip.role, snip.excerpt
                        );
                    }
                }
                Ok(out.trim_end().to_string())
            }

            SessionAction::Read => {
                let id = require_id(&args, "read")?;
                let data = mgr.load(id)?;
                let total = data.messages.len();
                if total == 0 {
                    return Ok(format!(
                        "Session {} — \"{}\" has no messages.",
                        data.id, data.title
                    ));
                }

                let start_1 = args.start_message.unwrap_or(1).max(1);
                let end_1 = args.end_message.unwrap_or(total).min(total);
                if start_1 > total || start_1 > end_1 {
                    bail!(
                        "Invalid message range [{start_1}..={end_1}] for session '{}' (total messages: {total}).",
                        data.id
                    );
                }

                let pin = if data.pinned { " [pinned]" } else { "" };
                let mut out = format!(
                    "Session: {} — \"{}\"{}\nModel: {} | Messages: {}..{} of {} | Updated: {}\n\n",
                    data.id, data.title, pin, data.model, start_1, end_1, total, data.updated_at
                );

                for (offset, msg) in data.messages[(start_1 - 1)..end_1].iter().enumerate() {
                    out.push_str(&format_message_for_read(
                        start_1 + offset,
                        msg,
                        args.include_tool_outputs,
                    ));
                    out.push('\n');
                }
                Ok(out.trim_end().to_string())
            }

            SessionAction::Stats => {
                let stats = mgr.stats(args.all_workspaces)?;
                let scope = if args.all_workspaces {
                    "all workspaces"
                } else {
                    "current workspace"
                };
                let mut out = format!(
                    "Session stats ({scope}):\n- Total sessions: {}\n- Pinned sessions: {}\n- Total messages: {}\n",
                    stats.total_sessions, stats.pinned_sessions, stats.total_messages
                );
                if !stats.models.is_empty() {
                    out.push_str("- Models:\n");
                    for (model, count) in stats.models {
                        let _ = writeln!(out, "  - {model}: {count} session(s)");
                    }
                }
                Ok(out.trim_end().to_string())
            }

            SessionAction::Rename => {
                let id = require_id(&args, "rename")?;
                let new_title = args
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .context("'title' is required for session:rename")?;
                let final_title = mgr.rename(id, new_title)?;
                Ok(format!("Renamed session '{id}' to \"{final_title}\"."))
            }

            SessionAction::Pin => {
                let id = require_id(&args, "pin")?;
                let new_state = match args.pinned {
                    Some(state) => mgr.set_pinned(id, state)?,
                    None => mgr.toggle_pinned(id)?,
                };
                let verb = if new_state { "Pinned" } else { "Unpinned" };
                Ok(format!("{verb} session '{id}'."))
            }
        }
    }
}
