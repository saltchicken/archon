//! Skill hook collection, interpolation, and execution key management.

use super::template::TemplateContext;
use super::types::{HookTrigger, SkillHook, SkillDefinition};
use crate::{
    core::{
        config::Config,
        session::ChatMessage,
    },
    tools::{ToolCall, ToolFunction},
};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub fn format_hook_summary(hook: &SkillHook) -> String {
    format_hook_summary_with_args(&hook.tool, hook.args.clone())
}

fn format_hook_summary_with_args(tool: &str, arguments: Value) -> String {
    let call = ToolCall {
        function: ToolFunction {
            name: tool.to_string(),
            arguments,
        },
        thought_signature: None,
    };
    crate::tools::format_call_summary(&call)
}

pub fn hook_execution_key(
    skill_name: &str,
    idx: usize,
    hook: &SkillHook,
    resolved_args: &Value,
) -> String {
    format!(
        "{}::{idx}::{}::{}",
        skill_name.to_ascii_lowercase(),
        hook.tool,
        resolved_args
    )
}

pub fn is_hook_already_in_session(
    key: &str,
    summary: &str,
    executed_hooks: &HashSet<String>,
    messages: &[ChatMessage],
) -> bool {
    executed_hooks.contains(key)
        || messages.iter().any(|m| {
            m.include_in_context
                && matches!(m.role.as_str(), "hook" | "local")
                && m.tool_name.as_deref() == Some(summary)
        })
}

pub fn preview_due_hooks(
    config: &Config,
    active_skill: Option<&str>,
    skill_args: Option<&str>,
    raw_skill_args: Option<&str>,
    skill_inputs: &BTreeMap<String, Value>,
    executed_hooks: &HashSet<String>,
    messages: &[ChatMessage],
) -> Vec<String> {
    let mut temp = executed_hooks.clone();
    collect_due_hooks(
        config,
        active_skill,
        skill_args,
        raw_skill_args,
        skill_inputs,
        &mut temp,
        messages,
    )
    .into_iter()
    .map(|(summary, _)| summary)
    .collect()
}

fn last_assistant_response(messages: &[ChatMessage]) -> &str {
    messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant" && !m.content.trim().is_empty())
        .map(|m| m.content.trim())
        .unwrap_or("")
}

fn single_template_ident(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix("{{")
        .and_then(|r| r.strip_suffix("}}"))
        .map(str::trim)?;
    (!inner.contains("{{") && !inner.contains("}}") && super::template::is_valid_template_ident(inner))
        .then_some(inner)
}

fn interpolate_hook_value(args: &Value, ctx: &TemplateContext<'_>) -> Option<Value> {
    match args {
        Value::String(s) if s.contains("{{") => {
            if let Some(inner) = single_template_ident(s) {
                if let Some(typed_val) = ctx.inputs.get(inner).or_else(|| {
                    ctx.inputs
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(inner))
                        .map(|(_, v)| v)
                }) {
                    return Some(typed_val.clone());
                }
                if !matches!(inner, "args" | "raw_args" | "skill.name" | "last_response") {
                    // Optional declared arg omitted by caller: omit field/element instead of passing "".
                    return None;
                }
            }
            Some(Value::String(super::template::render_template(s, ctx)))
        }
        Value::Array(arr) => Some(Value::Array(
            arr.iter()
                .filter_map(|v| interpolate_hook_value(v, ctx))
                .collect(),
        )),
        Value::Object(map) => Some(Value::Object(
            map.iter()
                .filter_map(|(k, v)| interpolate_hook_value(v, ctx).map(|rv| (k.clone(), rv)))
                .collect(),
        )),
        other => Some(other.clone()),
    }
}

pub fn interpolate_hook_args(args: &Value, ctx: &TemplateContext<'_>) -> Value {
    interpolate_hook_value(args, ctx).unwrap_or(Value::Null)
}

fn build_hook_ctx<'a>(
    config: &'a Config,
    active_skill: Option<&str>,
    skill_args: Option<&'a str>,
    raw_skill_args: Option<&'a str>,
    skill_inputs: &'a BTreeMap<String, Value>,
    last_response: &'a str,
) -> Option<(
    &'a SkillDefinition,
    TemplateContext<'a>,
    TemplateContext<'a>,
)> {
    let skill = active_skill.and_then(|name| super::parse::find_skill(config, name))?;
    let tmpl = TemplateContext {
        skill_name: &skill.name,
        args: skill_args.unwrap_or(""),
        raw_args: raw_skill_args.or(skill_args).unwrap_or(""),
        last_response: Some(last_response),
        inputs: skill_inputs,
    };
    let summary = TemplateContext {
        last_response: Some("{{last_response}}"),
        ..tmpl
    };
    Some((skill, tmpl, summary))
}

pub fn collect_due_hooks(
    config: &Config,
    active_skill: Option<&str>,
    skill_args: Option<&str>,
    raw_skill_args: Option<&str>,
    skill_inputs: &BTreeMap<String, Value>,
    executed_hooks: &mut HashSet<String>,
    messages: &[ChatMessage],
) -> Vec<(String, ToolCall)> {
    let Some((skill, tmpl_ctx, summary_ctx)) = build_hook_ctx(
        config,
        active_skill,
        skill_args,
        raw_skill_args,
        skill_inputs,
        last_assistant_response(messages),
    ) else {
        return Vec::new();
    };

    let mut due_hooks = Vec::new();
    for (idx, hook) in skill.hooks.iter().enumerate() {
        let resolved_args = interpolate_hook_args(&hook.args, &tmpl_ctx);
        let summary = format_hook_summary_with_args(
            &hook.tool,
            interpolate_hook_args(&hook.args, &summary_ctx),
        );
        let key = hook_execution_key(&skill.name, idx, hook, &resolved_args);
        let is_due = match hook.trigger {
            HookTrigger::OnActivate => {
                let already = is_hook_already_in_session(&key, &summary, executed_hooks, messages);
                executed_hooks.insert(key);
                !already
            }
            HookTrigger::EveryTurn => true,
            HookTrigger::AfterTurn => false,
        };
        if is_due {
            due_hooks.push((
                summary,
                ToolCall {
                    function: ToolFunction {
                        name: hook.tool.clone(),
                        arguments: resolved_args,
                    },
                    thought_signature: None,
                },
            ));
        }
    }
    due_hooks
}

pub fn collect_after_turn_hooks(
    config: &Config,
    active_skill: Option<&str>,
    skill_args: Option<&str>,
    raw_skill_args: Option<&str>,
    skill_inputs: &BTreeMap<String, Value>,
    last_response: &str,
) -> Vec<(String, ToolCall)> {
    let Some((skill, tmpl_ctx, summary_ctx)) = build_hook_ctx(
        config,
        active_skill,
        skill_args,
        raw_skill_args,
        skill_inputs,
        last_response.trim(),
    ) else {
        return Vec::new();
    };

    skill
        .hooks
        .iter()
        .filter(|hook| hook.trigger == HookTrigger::AfterTurn)
        .map(|hook| {
            (
                format_hook_summary_with_args(
                    &hook.tool,
                    interpolate_hook_args(&hook.args, &summary_ctx),
                ),
                ToolCall {
                    function: ToolFunction {
                        name: hook.tool.clone(),
                        arguments: interpolate_hook_args(&hook.args, &tmpl_ctx),
                    },
                    thought_signature: None,
                },
            )
        })
        .collect()
}
