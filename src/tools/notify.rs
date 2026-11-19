use super::{AsyncTool, ToolContext};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{process::Stdio, time::Duration};
use tokio::{process::Command, time::timeout};

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotifyUrgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl NotifyUrgency {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotifyArgs {
    /// Short notification title / summary text. If omitted, the body text is used as the summary.
    #[serde(default)]
    pub summary: Option<String>,
    /// Notification body text. Used as the summary if `summary` is not provided.
    #[serde(default)]
    pub body: Option<String>,
    /// Notification urgency level: "low", "normal" (default), or "critical".
    #[serde(default)]
    pub urgency: NotifyUrgency,
    /// Optional expire timeout in milliseconds (e.g. 5000 for 5s, 0 for persistent).
    #[serde(default)]
    pub timeout_ms: Option<u32>,
}

pub struct Tool;

impl AsyncTool for Tool {
    const NAME: &'static str = "notify";
    const DESCRIPTION: &'static str = "Send a desktop notification via notify-send (mako). Useful for alerting the user when a task, check, or scheduled job finishes.";
    type Args = NotifyArgs;

    async fn execute(args: Self::Args, _ctx: &ToolContext) -> Result<String> {
        let summary = args
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let body = args
            .body
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // `notify-send` requires a summary; fall back to body if no summary given.
        let effective_summary = match (summary, body) {
            (Some(s), _) => s.to_string(),
            (None, Some(b)) => b.to_string(),
            (None, None) => bail!("Notification requires either a `summary` or a `body`."),
        };

        let mut cmd = Command::new("notify-send");
        cmd.arg("-a")
            .arg("archon")
            .arg("-u")
            .arg(args.urgency.as_str());

        if let Some(timeout_ms) = args.timeout_ms {
            cmd.arg("-t").arg(timeout_ms.to_string());
        }

        cmd.arg("--").arg(&effective_summary);

        // Pass body as a second argument only if it was distinct from the summary.
        if summary.is_none() && body.is_some() {
            // body was used as the summary — nothing extra to pass
        } else if let Some(b) = body {
            cmd.arg(b);
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = timeout(Duration::from_secs(5), cmd.output())
            .await
            .context("`notify-send` timed out after 5s")?
            .context("Failed to execute `notify-send`")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            };
            bail!("`notify-send` failed: {detail}");
        }

        Ok(format!(
            "Notification sent ({}): {effective_summary}",
            args.urgency.as_str()
        ))
    }
}
