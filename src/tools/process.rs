//! `process` tool: manage background jobs started by other tools.
//!
//! Delegates to the process-lifetime [`crate::core::processes::ProcessRegistry`],
//! which owns the spawned children. This tool is **management-only** —
//! the LLM can `list`, `output`, `poll`, `kill`, and `clean` jobs, but
//! cannot start new ones. Spawning is reserved for purpose-built tools that
//! call [`ProcessRegistry::start_with`] directly, ensuring the LLM never
//! specifies arbitrary shell commands.
//!
//! `kill` and `clean` are gated behind approval; `list`, `output`, and `poll`
//! are read-only / follow actions (auto-approved by default).

use super::{AsyncTool, ToolContext};
use crate::core::processes::ProcessRegistry;
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ProcessAction {
    /// List all background jobs (running and finished) with their status.
    List,
    /// Peek at the last N lines of a job's recorded output (does not consume it).
    Output,
    /// Wait up to `wait_ms` for the job to finish or emit new output, then return the new output since the last poll.
    Poll,
    /// Terminate a running job (SIGKILL its process group).
    Kill,
    /// Terminate all running jobs and clear the job table.
    Clean,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProcessArgs {
    /// The process action to run.
    action: ProcessAction,
    /// Job id (action=output|poll|kill). Get ids from `list`.
    #[serde(default)]
    id: String,
    /// Number of trailing output lines to return (action=output). 0 = the entire recorded output. Default 50.
    #[serde(default)]
    tail_lines: Option<usize>,
    /// How long to wait for new output/exit before returning (action=poll), in milliseconds. Default 5000.
    #[serde(default)]
    wait_ms: Option<u64>,
}

fn require_id(args: &ProcessArgs) -> Result<String> {
    let id = args.id.trim();
    if id.is_empty() {
        bail!("'id' is required for this action. Use action=\"list\" to see active jobs.");
    }
    Ok(id.to_string())
}

impl AsyncTool for Tool {
    const NAME: &'static str = "process";
    const DESCRIPTION: &'static str = "Manage background jobs started by purpose-built tools (list, peek at output, poll for new output, kill, clean). This tool cannot start new processes.";
    const SUBACTIONS: &'static [&'static str] = &["list", "output", "poll", "kill", "clean"];
    const DEFAULT_ACTION: Option<&'static str> = Some("list");
    const PARAM_HINT: &'static str = "<list|output|poll|kill|clean> [id]";
    const SHORT_SUMMARY: &'static str =
        "List, peek output, poll, kill, or clean up background jobs started by other tools.";
    type Args = ProcessArgs;

    fn positional_keys(
        token: &str,
        map: &serde_json::Map<String, serde_json::Value>,
    ) -> &'static [&'static str] {
        if !map.contains_key("action") && Self::SUBACTIONS.contains(&token) {
            &["action"]
        } else {
            &["id"]
        }
    }

    async fn execute(args: Self::Args, _ctx: &ToolContext) -> Result<String> {
        let reg = ProcessRegistry::global();
        match args.action {
            ProcessAction::List => {
                let jobs = reg.list().await;
                if jobs.is_empty() {
                    return Ok("No background jobs (none started, or all cleaned up).".into());
                }
                let mut out = format!("{} background job(s):\n", jobs.len());
                for j in &jobs {
                    let state = if j.finished {
                        format!(
                            "finished, exit {}",
                            j.exit_code
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "signal".into())
                        )
                    } else {
                        format!("running, {}s", j.elapsed_secs)
                    };
                    let argv = if j.args.is_empty() {
                        j.command.clone()
                    } else {
                        format!("{} {}", j.command, j.args.join(" "))
                    };
                    let unserved = if j.unserved_bytes > 0 {
                        format!(", {} bytes new output waiting", j.unserved_bytes)
                    } else {
                        String::new()
                    };
                    out.push_str(&format!(
                        "  {} (pid {}) — {}{}\n    cmd: {}\n    src: {}",
                        j.id, j.pid, state, unserved, argv, j.source
                    ));
                }
                Ok(out)
            }
            ProcessAction::Output => {
                let id = require_id(&args)?;
                let tail = args.tail_lines.unwrap_or(50);
                reg.output(&id, tail).await
            }
            ProcessAction::Poll => {
                let id = require_id(&args)?;
                let wait = args.wait_ms.unwrap_or(5000).min(120_000);
                reg.poll(&id, wait).await
            }
            ProcessAction::Kill => {
                let id = require_id(&args)?;
                reg.kill(&id).await
            }
            ProcessAction::Clean => reg.clean().await,
        }
    }
}
