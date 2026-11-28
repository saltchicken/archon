use super::{AsyncTool, ToolContext};
use crate::core::processes::{ProcessRegistry, SpawnOptions};
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

/// A single action button on the notification.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotifyAction {
    /// Action identifier — the string printed to stdout when the user clicks this button.
    /// Use short, lowercase identifiers (e.g. `"retry"`, `"show_log"`, `"dismiss"`).
    pub name: String,
    /// Display label shown on the button in dunst (e.g. `"Retry"`, `"Show Log"`).
    pub label: String,
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
    /// Optional action buttons displayed in dunst. When present, the notification
    /// blocks until the user clicks a button or dismisses it. The clicked action's
    /// `name` is retrieved via the `process` tool (`action: "poll"`).
    #[serde(default)]
    pub actions: Vec<NotifyAction>,
}

pub struct Tool;

impl AsyncTool for Tool {
    const NAME: &'static str = "notify";
    const DESCRIPTION: &'static str = "Send a desktop notification via notify-send (dunst). Supports optional action buttons (e.g. Retry, Dismiss) that block until the user clicks one; the selected action name is retrievable via the `process` tool (action=poll).";
    const PARAM_HINT: &'static str = "[low|normal|critical] <summary> [body]";
    const SHORT_SUMMARY: &'static str = "Send a desktop notification via notify-send (dunst), optionally with interactive action buttons.";
    type Args = NotifyArgs;

    fn positional_keys(
        token: &str,
        _map: &serde_json::Map<String, serde_json::Value>,
    ) -> &'static [&'static str] {
        if matches!(token, "low" | "normal" | "critical") {
            &["urgency", "summary", "body"]
        } else {
            &["summary", "body"]
        }
    }

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

        let body_opt = body.filter(|_| summary.is_some());

        if args.actions.is_empty() {
            // --- Simple fire-and-forget notification (no actions) ---
            let mut cmd = Command::new("notify-send");
            cmd.arg("-a")
                .arg("archon")
                .arg("-u")
                .arg(args.urgency.as_str());

            if let Some(timeout_ms) = args.timeout_ms {
                cmd.arg("-t").arg(timeout_ms.to_string());
            }

            cmd.arg("--").arg(&effective_summary);

            if let Some(b) = &body_opt {
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
        } else {
            // --- Notification with action buttons (background job) ---
            // `notify-send --action=NAME=Label` blocks until the user clicks a
            // button or dismisses the notification. The clicked action's NAME
            // is printed to stdout, which the ProcessRegistry captures.
            let labels: Vec<String> = args.actions.iter().map(|a| a.label.clone()).collect();

            let mut reg_args: Vec<String> = vec![
                "-a".to_string(),
                "archon".to_string(),
                "-u".to_string(),
                args.urgency.as_str().to_string(),
            ];

            if let Some(timeout_ms) = args.timeout_ms {
                reg_args.push("-t".to_string());
                reg_args.push(timeout_ms.to_string());
            }

            for a in &args.actions {
                reg_args.push("--action".to_string());
                reg_args.push(format!("{}={}", a.name, a.label));
            }

            reg_args.push("--".to_string());
            reg_args.push(effective_summary.clone());
            if let Some(b) = &body_opt {
                reg_args.push(b.to_string());
            }

            let reg = ProcessRegistry::global();
            let job = reg
                .start_with(&SpawnOptions {
                    command: "notify-send".into(),
                    args: reg_args,
                    cwd: None,
                    env: vec![],
                    source: "notify".into(),
                })
                .await?;

            Ok(format!(
                "Notification sent with actions [{}] (job {}), waiting for user input.\n\n\
                 Use `process` (action: \"poll\", id: \"{}\") to check which action was clicked.\n\
                 The action name will appear in the output when the user makes a selection.",
                labels.join(", "),
                job.id,
                job.id,
            ))
        }
    }
}
