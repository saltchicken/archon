use crate::core::provider::Provider;
use crate::tools::{AsyncTool, ToolContext};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubagentArgs {
    /// The task to delegate to the subagent. Be specific about what you need accomplished, including any constraints, context, or expected output format.
    pub task: String,
    /// Optional system prompt to guide the subagent's behavior. If omitted, a generic focused-subagent prompt is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Optional list of tool names the subagent may use. If omitted, all enabled tools (except "subagent") are available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

pub struct Tool;

impl AsyncTool for Tool {
    const NAME: &'static str = "subagent";
    const DESCRIPTION: &'static str = "Delegate a task to a fresh subagent that runs in its own isolated context. The subagent can use tools across multiple turns and returns its final response. Use this for complex multi-step tasks, research, analysis, or work that benefits from a clean context separate from the main conversation.";
    type Args = SubagentArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let model = ctx.config.startup_model();
        let provider = Provider::from_model(&model);
        let client = provider.build_client(&ctx.config)?;
        let num_ctx = ctx.config.context_for_model(&model);

        let system_prompt = args
            .system_prompt
            .unwrap_or_else(|| {
                "You are a focused subagent. Complete the given task efficiently using the available tools. Be thorough but concise. Your final response should directly address the task with all relevant findings.".to_string()
            });

        let allowed_tools = args
            .tools
            .unwrap_or_else(|| ctx.config.app.enabled_tools.clone());

        let result = provider
            .run_subagent(
                client,
                model,
                ctx.config.clone(),
                num_ctx,
                &system_prompt,
                &args.task,
                allowed_tools,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Subagent failed: {e}"))?;

        Ok(result)
    }
}
