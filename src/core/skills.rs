//! Skill system: parsing, discovery, inheritance, typed arguments,
//! template rendering, and hook management.

mod args;
mod hooks;
mod parse;
mod resolve;
mod template;
mod types;

pub use args::{
    activate_skill_for_turn, apply_at_skill_to_turn, format_arg_value, resolve_skill_inputs,
    validate_arg_json_value, validate_turn_skill,
};
pub use hooks::{
    collect_after_turn_hooks, collect_due_hooks, format_hook_summary, interpolate_hook_args,
    preview_due_hooks,
};
pub use parse::{
    DetailedSkillParse, discover_skills, find_skill, parse_at_command, parse_skill_markdown,
    parse_skill_markdown_detailed, scan_skills_dir, split_toml_frontmatter,
};
pub use resolve::{
    check_skill_activation, missing_skill_tools, replace_super_placeholder, resolve_skills,
};
pub use template::{TemplateContext, inspect_template, render_template};
pub use types::{
    HookTrigger, KNOWN_FRONTMATTER_KEYS, RESERVED_ARG_NAMES, SkillArgDef, SkillArgType,
    SkillDefinition, SkillHook,
};

use crate::core::config::Config;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Composes the full system prompt for a turn, incorporating the base prompt,
/// memory blocks, and the active skill's rendered prompt.
pub fn compose_system_prompt(
    base_system_prompt: &str,
    active_skill: Option<&str>,
    skill_args: Option<&str>,
    raw_skill_args: Option<&str>,
    skill_inputs: &BTreeMap<String, Value>,
    session_id: Option<&str>,
    config: &Config,
) -> String {
    let mut sections = Vec::new();
    if !base_system_prompt.trim().is_empty() {
        sections.push(base_system_prompt.trim().to_string());
    }

    if let Some(mem_block) = config.effective_memory().format_for_prompt() {
        sections.push(mem_block);
    }

    if let Some(sid) = session_id
        && let Some(task_block) = crate::tools::task::format_active_tasks_for_prompt(config, sid)
    {
        sections.push(task_block);
    }

    if let Some(skill_name) = active_skill
        && let Some(skill) = find_skill(config, skill_name)
    {
        let args = skill_args.unwrap_or("").trim();
        let raw_args = raw_skill_args.or(skill_args).unwrap_or("").trim();
        let tmpl_ctx = TemplateContext {
            skill_name: &skill.name,
            args,
            raw_args,
            last_response: None,
            inputs: skill_inputs,
        };
        let (referenced_vars, _) = inspect_template(&skill.prompt);
        let prompt_body = render_template(&skill.prompt, &tmpl_ctx).trim().to_string();

        let mut block = String::new();

        let unreferenced_inputs: Vec<String> = skill_inputs
            .iter()
            .filter(|(k, _)| !referenced_vars.iter().any(|v| v.eq_ignore_ascii_case(k)))
            .map(|(k, v)| format!("{k}={}", format_arg_value(v)))
            .collect();
        if !unreferenced_inputs.is_empty() {
            let _ = writeln!(block, "Skill inputs: {}", unreferenced_inputs.join(", "));
        }

        if !prompt_body.is_empty() {
            let _ = writeln!(block, "{prompt_body}");
        } else if args.is_empty() && unreferenced_inputs.is_empty() {
            let _ = writeln!(block, "Active skill: {}", skill.description);
        }

        let prompt_used_args = referenced_vars
            .iter()
            .any(|v| v == "args" || v == "raw_args");
        if !args.is_empty() && !prompt_used_args {
            let _ = writeln!(block, "\nUser arguments / instructions: {args}");
        }
        sections.push(format!(
            "<skill name=\"{}\">\n{}\n</skill>",
            skill.name,
            block.trim_end()
        ));
    }

    sections.join("\n\n")
}
