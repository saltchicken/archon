//! `process` tool: run long-running commands in the background and follow or
//! stop them across turns.
//!
//! Delegates to the process-lifetime [`crate::core::processes::ProcessRegistry`],
//! which owns the spawned children. `start` and `kill` are gated behind
//! approval; `list`, `output`, `poll`, and `clean` are read-only / follow
//! actions (auto-approved by default).

use super::{AsyncTool, ToolContext};
use crate::core::processes::ProcessRegistry;
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::HashMap;

pub struct Tool;

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ProcessAction {
    /// Start a long-running command in the background and get its job id.
    Start,
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
    /// Executable to run (action=start). No shell is involved; pass `args` separately. e.g. `"node"`, `"cargo"`.
    #[serde(default)]
    command: String,
    /// Arguments for the executable (action=start). e.g. `["server.js"]` or `["watch", "-w", ".", "run"]`.
    #[serde(default)]
    args: Vec<String>,
    /// Working directory (action=start); relative to the workspace root, or absolute. Defaults to the workspace root.
    #[serde(default)]
    cwd: String,
    /// Extra environment variables to set (action=start), as `{NAME: value}`. The ambient environment (PATH, HOME, …) is inherited.
    #[serde(default)]
    env: HashMap<String, String>,
    /// Job id (action=output|poll|kill). Get ids from `start` or `list`.
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
    const DESCRIPTION: &'static str = "Run long-running commands in the background (a dev server, a build, a watcher, a soak test) and follow or stop them across turns: start, list, output (peek at the tail), poll (wait + new output), kill, clean.";
    type Args = ProcessArgs;

    async fn execute(args: Self::Args, _ctx: &ToolContext) -> Result<String> {
        let reg = ProcessRegistry::global();
        match args.action {
            ProcessAction::Start => {
                if args.command.trim().is_empty() {
                    bail!("'command' is required for process:start");
                }
                let cwd = if args.cwd.trim().is_empty() {
                    None
                } else {
                    Some(args.cwd.trim())
                };
                let env: Vec<(String, String)> = args.env.into_iter().collect();
                let spawned = reg.start(&args.command, &args.args, cwd, &env).await?;
                let argv = if args.args.is_empty() {
                    args.command.trim().to_string()
                } else {
                    format!("{} {}", args.command.trim(), args.args.join(" "))
                };
                let cwd_line = cwd.map(|c| format!("\n  cwd: {c}")).unwrap_or_default();
                Ok(format!(
                    "Started background job {} (pid {}): {}{}\nFollow it with action=\"poll\" (id=\"{}\", wait_ms≈5000) or peek with action=\"output\" (id=\"{}\", tail_lines≈50). Stop it with action=\"kill\" (id=\"{}\"), or action=\"clean\" to stop all.",
                    spawned.id, spawned.pid, argv, cwd_line, spawned.id, spawned.id, spawned.id
                ))
            }
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
                        "  {} (pid {}) — {}{}\n    cmd: {}",
                        j.id, j.pid, state, unserved, argv
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
