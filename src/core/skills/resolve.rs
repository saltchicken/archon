//! Skill inheritance: `extends` chain resolution, tool/hook/arg merging,
//! and activation validation.

use super::types::{SkillArgDef, SkillDefinition};
use crate::core::config::Config;
use std::{
    collections::HashMap,
    path::PathBuf,
};
use tracing;

fn push_unique_string(vec: &mut Vec<String>, item: &str) {
    if !vec.iter().any(|existing| existing == item) {
        vec.push(item.to_string());
    }
}

fn merge_arg_def(vec: &mut Vec<SkillArgDef>, incoming: SkillArgDef) {
    if let Some(existing) = vec
        .iter_mut()
        .find(|a| a.name.eq_ignore_ascii_case(&incoming.name))
    {
        *existing = incoming;
    } else {
        vec.push(incoming);
    }
}

fn resolve_skill_at(
    idx: usize,
    raw: &[SkillDefinition],
    name_index: &HashMap<String, usize>,
    cmd_index: &HashMap<String, usize>,
    memo: &mut [Option<SkillDefinition>],
    visiting: &mut Vec<usize>,
) -> SkillDefinition {
    if let Some(ref cached) = memo[idx] {
        return cached.clone();
    }

    let child = &raw[idx];
    if child.extends.is_empty() {
        let mut resolved = child.clone();
        // Strip any stray `!tool` negations from a root skill without parents.
        resolved.tools = resolved
            .tools
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty() && !t.starts_with('!'))
            .map(String::from)
            .collect();
        resolved.excluded_tools.clear();
        resolved.tools_restricted =
            !resolved.tools.is_empty() && !resolved.tools.iter().any(|t| t == "*");
        memo[idx] = Some(resolved.clone());
        return resolved;
    }

    visiting.push(idx);

    let mut merged_tools: Vec<String> = Vec::new();
    let mut excluded_tools: Vec<String> = Vec::new();
    let mut tools_restricted = false;
    let mut merged_hooks: Vec<super::types::SkillHook> = Vec::new();
    let mut merged_args: Vec<SkillArgDef> = Vec::new();
    let mut inherited_dirs: Vec<PathBuf> = Vec::new();
    let mut base_prompts: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for parent_ref in &child.extends {
        let key = parent_ref
            .trim()
            .trim_start_matches('@')
            .to_ascii_lowercase();
        let parent_idx = name_index
            .get(&key)
            .copied()
            .or_else(|| cmd_index.get(&key).copied());

        let Some(p_idx) = parent_idx else {
            errors.push(format!(
                "Skill '{}' extends unknown parent skill '{}'",
                child.name, parent_ref
            ));
            continue;
        };

        if let Some(cycle_pos) = visiting.iter().position(|&v| v == p_idx) {
            let mut chain: Vec<&str> = visiting[cycle_pos..]
                .iter()
                .map(|&i| raw[i].name.as_str())
                .collect();
            chain.push(raw[p_idx].name.as_str());
            errors.push(format!(
                "Circular skill inheritance detected: {}",
                chain.join(" -> ")
            ));
            continue;
        }

        let parent = resolve_skill_at(p_idx, raw, name_index, cmd_index, memo, visiting);
        for err in &parent.resolution_errors {
            push_unique_string(&mut errors, err);
        }
        if parent.tools_restricted {
            tools_restricted = true;
        }
        for tool in &parent.tools {
            let t = tool.trim();
            if !t.is_empty() {
                push_unique_string(&mut merged_tools, t);
                excluded_tools.retain(|ex| !ex.eq_ignore_ascii_case(t));
            }
        }
        for ex in &parent.excluded_tools {
            if !merged_tools.iter().any(|t| t.eq_ignore_ascii_case(ex)) {
                push_unique_string(&mut excluded_tools, ex);
            }
        }
        for hook in &parent.hooks {
            if !merged_hooks.contains(hook) {
                merged_hooks.push(hook.clone());
            }
        }
        for arg in &parent.args {
            merge_arg_def(&mut merged_args, arg.clone());
        }
        for dir in parent.all_dirs() {
            if !inherited_dirs.iter().any(|d| d == dir) {
                inherited_dirs.push(dir.to_path_buf());
            }
        }
        if !parent.prompt.trim().is_empty() {
            base_prompts.push(parent.prompt.clone());
        }
    }

    visiting.pop();

    // Apply child's tools (supporting `!tool` removal without expanding `*` into hard requirements).
    for raw_tool in &child.tools {
        let trimmed = raw_tool.trim();
        if let Some(neg) = trimmed.strip_prefix('!') {
            let neg = neg.trim();
            if neg.is_empty() {
                continue;
            }
            merged_tools.retain(|t| !t.eq_ignore_ascii_case(neg));
            if !tools_restricted || merged_tools.iter().any(|t| t == "*") {
                push_unique_string(&mut excluded_tools, neg);
            }
        } else if trimmed == "*" {
            push_unique_string(&mut merged_tools, "*");
            excluded_tools.clear();
        } else if !trimmed.is_empty() {
            tools_restricted = true;
            push_unique_string(&mut merged_tools, trimmed);
            excluded_tools.retain(|ex| !ex.eq_ignore_ascii_case(trimmed));
        }
    }

    // Apply child's hooks after inherited hooks, deduplicating exact matches.
    for hook in &child.hooks {
        if !merged_hooks.contains(hook) {
            merged_hooks.push(hook.clone());
        }
    }

    // Apply child's typed args (override by name or append).
    for arg in &child.args {
        merge_arg_def(&mut merged_args, arg.clone());
    }

    // Compose prompt: child can place parent prompt via `{{super}}`, otherwise base precedes child.
    let combined_base = base_prompts.join("\n\n");
    let child_prompt = child.prompt.trim();
    let final_prompt = if combined_base.is_empty() {
        child_prompt.to_string()
    } else if child_prompt.is_empty() {
        combined_base
    } else if let Some(replaced) = replace_super_placeholder(child_prompt, &combined_base) {
        replaced
    } else {
        format!("{combined_base}\n\n{child_prompt}")
    };

    if !errors.is_empty() {
        for err in &errors {
            tracing::warn!("{err}");
        }
    }

    let resolved = SkillDefinition {
        name: child.name.clone(),
        description: child.description.clone(),
        command: child.command.clone(),
        extends: child.extends.clone(),
        is_abstract: child.is_abstract,
        tools: merged_tools,
        excluded_tools,
        tools_restricted,
        hooks: merged_hooks,
        args: merged_args,
        prompt: final_prompt,
        dir_path: child.dir_path.clone(),
        inherited_dirs,
        scope: child.scope,
        overrides_global: child.overrides_global,
        resolution_errors: errors,
    };

    memo[idx] = Some(resolved.clone());
    resolved
}

pub fn replace_super_placeholder(prompt: &str, base: &str) -> Option<String> {
    if !prompt.contains("{{") {
        return None;
    }
    let mut out = String::with_capacity(prompt.len() + base.len());
    let mut pos = 0usize;
    let mut found = false;
    while let Some(rel_open) = prompt[pos..].find("{{") {
        let open = pos + rel_open;
        let Some(rel_close) = prompt[open + 2..].find("}}") else {
            break;
        };
        let close = open + 2 + rel_close;
        if prompt[open + 2..close].trim() == "super" {
            out.push_str(&prompt[pos..open]);
            out.push_str(base);
            pos = close + 2;
            found = true;
        } else {
            out.push_str(&prompt[pos..open + 1]);
            pos = open + 1;
        }
    }
    if !found {
        return None;
    }
    out.push_str(&prompt[pos..]);
    Some(out)
}

/// Resolves `extends` inheritance chains across a vector of parsed skills.
pub fn resolve_skills(raw: Vec<SkillDefinition>) -> Vec<SkillDefinition> {
    let mut name_index = HashMap::with_capacity(raw.len());
    let mut cmd_index = HashMap::new();
    for (idx, skill) in raw.iter().enumerate() {
        name_index.insert(skill.name.to_ascii_lowercase(), idx);
        if !skill.command.is_empty() {
            cmd_index
                .entry(skill.command.to_ascii_lowercase())
                .or_insert(idx);
        }
    }

    let mut memo: Vec<Option<SkillDefinition>> = vec![None; raw.len()];
    let mut visiting = Vec::new();
    (0..raw.len())
        .map(|i| resolve_skill_at(i, &raw, &name_index, &cmd_index, &mut memo, &mut visiting))
        .collect()
}

pub fn missing_skill_tools(skill: &SkillDefinition, config: &Config) -> Vec<String> {
    let mut missing = Vec::new();
    for raw in skill
        .tools
        .iter()
        .map(String::as_str)
        .chain(skill.hooks.iter().map(|h| h.tool.as_str()))
    {
        let name = raw.trim();
        if name.is_empty() || name == "*" || name.starts_with('!') {
            continue;
        }
        if !crate::tools::is_tool_enabled(name, config) && !missing.iter().any(|m| m == name) {
            missing.push(name.to_string());
        }
    }
    missing
}

pub fn check_skill_activation(
    skill_name: &str,
    enable_tools: bool,
    config: &Config,
) -> Result<String, String> {
    let clean_name = skill_name.trim().trim_start_matches('@');
    let Some(skill) = super::parse::find_skill(config, clean_name) else {
        return Err(format!("Skill '@{clean_name}' not found."));
    };
    if skill.is_abstract {
        return Err(format!(
            "Cannot activate skill '@{}': it is declared `abstract = true` and can only be extended by other skills.",
            skill.name
        ));
    }
    if !skill.resolution_errors.is_empty() {
        return Err(format!(
            "Cannot activate skill '@{}': {}",
            skill.name,
            skill.resolution_errors.join("; ")
        ));
    }
    if (!skill.tools.is_empty() || !skill.hooks.is_empty()) && !enable_tools {
        return Err(format!(
            "Cannot activate skill '@{}': tools are currently disabled (press ^T to enable tools).",
            skill.name
        ));
    }
    let missing = missing_skill_tools(skill, config);
    if !missing.is_empty() {
        return Err(format!(
            "Cannot activate skill '@{}': required tool(s) [{}] are not in `app.enabled_tools` in config.toml.",
            skill.name,
            missing.join(", ")
        ));
    }
    Ok(skill.name.clone())
}
