use super::{AsyncTool, ToolCall, ToolContext};
use crate::core::{
    EngineEvent, config::Config, ollama, provider::Provider, session::ChatMessage,
    skills::find_skill,
};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write as _;
use tokio::sync::mpsc;
use tracing::{info, warn};

const DEFAULT_MAX_STEPS: usize = 8;
const MAX_STEPS_CAP: usize = 15;

const SUB_AGENT_SYSTEM_PROMPT: &str = "\
You are a fast, focused local sub-agent worker running inside Archon.
Complete the delegated task directly and concisely using the provided workspace tools:
- Run requested commands or checks (`cargo`, `git`, `search`, `repo_map`, `read_file`, `fs`, `dotfiles`).
- If asked to format or fix warnings/errors, apply minimal, exact hunks via `edit_file` and re-verify with `cargo`.
- Do not ask follow-up questions. Once the task is finished, output a concise summary of actions taken and final verification results.";

const DEFAULT_ALLOWED_SUB_TOOLS: &[&str] = &[
    "cargo",
    "read_file",
    "edit_file",
    "create_file",
    "search",
    "repo_map",
    "fs",
    "git",
    "dotfiles",
    "system",
    "execute_command",
];

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
pub struct DelegateTaskArgs {
    /// Clear, self-contained instructions for the local sub-agent (e.g. "Run cargo fmt and cargo clippy, fix any warnings, and report the result").
    pub task: String,
    /// Optional skill name (e.g. `"codebase-architect"`, `"cachyos-optimizer"`, `"git-commit"`) to equip the sub-agent with specialized instructions and scoped tools.
    #[serde(default)]
    pub skill: String,
    /// Optional local Ollama model override. Defaults to `app.sub_agent_model` (or `app.default_model` if it is an Ollama model).
    #[serde(default)]
    pub model: String,
    /// Optional list of tool names to expose to the sub-agent (e.g. `["cargo", "read_file", "edit_file"]`). Overrides skill tools when non-empty.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Maximum number of autonomous tool-calling turns allowed before stopping (default: 8, max: 15).
    #[serde(default)]
    pub max_steps: Option<usize>,
}

fn resolve_sub_agent_model(arg_model: &str, config: &Config) -> String {
    let candidates = [
        arg_model.trim(),
        config.app.sub_agent_model.trim(),
        config.app.default_model.trim(),
    ];

    for cand in candidates {
        if !cand.is_empty() && Provider::from_model(cand) == Provider::Ollama {
            return cand.to_string();
        }
    }

    config
        .models
        .iter()
        .find(|m| Provider::from_model(&m.name) == Provider::Ollama)
        .map_or_else(|| "qwen3.8:27b".into(), |m| m.name.clone())
}

fn filter_sub_agent_tools(
    requested: &[String],
    skill_tools: &[String],
    active_skills: &[String],
    config: &Config,
) -> Vec<Value> {
    let allow_list: Vec<&str> = if !requested.is_empty() {
        requested
            .iter()
            .map(String::as_str)
            .filter(|&name| name != "delegate_task")
            .collect()
    } else if !skill_tools.is_empty() {
        skill_tools
            .iter()
            .map(String::as_str)
            .filter(|&name| name != "delegate_task")
            .collect()
    } else {
        DEFAULT_ALLOWED_SUB_TOOLS.to_vec()
    };

    super::schemas_for_turn(config, active_skills)
        .into_iter()
        .filter(|schema| {
            schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|name| name != "delegate_task" && allow_list.contains(&name))
        })
        .collect()
}

async fn run_ollama_turn(
    client: &reqwest::Client,
    base_url: &str,
    payload: Value,
) -> Result<(String, Vec<ToolCall>, u64, u64)> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    ollama::stream_chat(client.clone(), base_url.to_string(), payload, tx).await;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut prompt_tokens = 0u64;
    let mut eval_tokens = 0u64;

    while let Some(ev) = rx.recv().await {
        match ev {
            EngineEvent::StreamChunk(chunk) => text.push_str(&chunk),
            EngineEvent::StreamDone {
                prompt_tokens: pt,
                eval_tokens: et,
                tool_calls: tc,
                ..
            } => {
                prompt_tokens = pt;
                eval_tokens = et;
                tool_calls = tc;
            }
            EngineEvent::StreamError(err) => bail!("Sub-agent Ollama stream error: {err}"),
            _ => {}
        }
    }

    Ok((text, tool_calls, prompt_tokens, eval_tokens))
}

impl AsyncTool for Tool {
    const NAME: &'static str = "delegate_task";
    const DESCRIPTION: &'static str = "Delegates routine or mechanical workspace tasks (such as running `cargo fmt`, `cargo clippy`, `cargo check`, fixing simple lints, or inspecting files) to a fast local Ollama sub-agent so the primary model saves tokens and context.";
    type Args = DelegateTaskArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let task = args.task.trim();
        if task.is_empty() {
            bail!("'task' cannot be empty");
        }

        let model = resolve_sub_agent_model(&args.model, &ctx.config);
        if Provider::from_model(&model) != Provider::Ollama {
            bail!("Sub-agent model '{model}' must be a local Ollama model (not a cloud provider).");
        }

        ollama::run_preflight(&model, &ctx.config).await?;

        let client = Provider::Ollama.build_client(&ctx.config)?;
        let num_ctx = ctx.config.context_for_model(&model);
        let max_tool_out = ctx
            .config
            .max_tool_output_for_model(&model)
            .min(ctx.max_output_size);
        let max_steps = args
            .max_steps
            .unwrap_or(DEFAULT_MAX_STEPS)
            .clamp(1, MAX_STEPS_CAP);

        let resolved_skill = if args.skill.trim().is_empty() {
            None
        } else {
            let Some(s) = find_skill(&ctx.config, &args.skill) else {
                let available: Vec<&str> =
                    ctx.config.skills.iter().map(|k| k.name.as_str()).collect();
                bail!(
                    "Unknown sub-agent skill '{}'. Available skills: {}",
                    args.skill.trim(),
                    available.join(", ")
                );
            };
            Some(s.clone())
        };

        let (skill_tools, active_skill_names, sub_system_prompt) = match &resolved_skill {
            Some(s) => (
                s.tools.clone(),
                vec![s.name.clone()],
                format!(
                    "{SUB_AGENT_SYSTEM_PROMPT}\n\n<active_skill name=\"{}\">\n{}\n</active_skill>",
                    s.name,
                    s.prompt.trim()
                ),
            ),
            None => (Vec::new(), Vec::new(), SUB_AGENT_SYSTEM_PROMPT.to_string()),
        };

        let sub_tools =
            filter_sub_agent_tools(&args.tools, &skill_tools, &active_skill_names, &ctx.config);
        let allowed_tool_names: Vec<String> = sub_tools
            .iter()
            .filter_map(|s| s.get("function")?.get("name")?.as_str().map(String::from))
            .collect();

        info!(
            "Starting sub-agent '{}' (skill: {:?}, ctx: {}, max_steps: {}, tools: {:?})",
            model,
            resolved_skill.as_ref().map(|s| s.name.as_str()),
            num_ctx,
            max_steps,
            allowed_tool_names
        );

        let mut messages = vec![ChatMessage::new("user", task)];
        let mut execution_log = Vec::new();
        let mut total_prompt_tokens = 0u64;
        let mut total_eval_tokens = 0u64;
        let mut final_response = String::new();

        for step in 1..=max_steps {
            let tools_opt = (!sub_tools.is_empty()).then(|| sub_tools.clone());
            let payload = ollama::build_payload(
                &model,
                &messages,
                &sub_system_prompt,
                num_ctx,
                tools_opt,
            );

            let (assistant_text, tool_calls, pt, et) =
                run_ollama_turn(&client, &ctx.config.ollama.base_url, payload).await?;
            total_prompt_tokens = pt;
            total_eval_tokens += et;

            if !assistant_text.trim().is_empty() {
                final_response = assistant_text.trim().to_string();
            }

            if tool_calls.is_empty() {
                break;
            }

            messages.push(ChatMessage {
                tool_calls: Some(tool_calls.clone()),
                ..ChatMessage::new("assistant", assistant_text)
            });

            for call in tool_calls {
                let sig = super::call_signature(&call);
                if call.function.name == Self::NAME
                    || !allowed_tool_names.contains(&call.function.name)
                {
                    warn!("Sub-agent attempted disallowed tool call: {sig}");
                    execution_log.push(format!(
                        "[Step {step}] {sig} -> [Blocked: tool not allowed]"
                    ));
                    messages.push(ChatMessage::new(
                        "tool",
                        format!(
                            "Error: Tool '{}' is not permitted in sub-agent.",
                            call.function.name
                        ),
                    ));
                    continue;
                }

                info!("Sub-agent ({model}) step {step}/{max_steps} executing: {sig}");
                let (output, truncated) =
                    Box::pin(super::execute(&call, &ctx.config, max_tool_out)).await;
                let first_line = output.lines().next().unwrap_or("(no output)").trim();
                execution_log.push(format!(
                    "[Step {step}] {sig} -> {}{}",
                    crate::core::session::truncate_str(first_line, 100),
                    if truncated { " [truncated]" } else { "" }
                ));

                messages.push(ChatMessage {
                    truncated,
                    ..ChatMessage::new("tool", output)
                });
            }

            if step == max_steps {
                let _ = write!(
                    final_response,
                    "\n\n[Sub-agent reached max_steps ({max_steps})]"
                );
            }
        }

        let mut report = format!(
            "=== Sub-Agent Report (model: `{model}`, ctx: {total_prompt_tokens}/{num_ctx}, generated: {total_eval_tokens} tokens) ===\n"
        );
        if !execution_log.is_empty() {
            report.push_str("\n--- Actions Executed ---\n");
            for entry in &execution_log {
                let _ = writeln!(report, "• {entry}");
            }
        }
        report.push_str("\n--- Summary ---\n");
        if final_response.trim().is_empty() {
            report.push_str("(Sub-agent completed tool execution without additional commentary.)");
        } else {
            report.push_str(final_response.trim());
        }

        Ok(report)
    }
}
