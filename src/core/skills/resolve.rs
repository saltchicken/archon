//! Skill inheritance: `extends` chain resolution, tool/hook/arg merging,
//! and activation validation.

use super::types::{SkillArgDef, SkillDefinition};
use crate::core::config::Config;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};
use tracing;

fn push_unique_string(vec: &mut Vec<String>, item: &str) {
    if !vec.iter().any(|existing| existing == item) {
        vec.push(item.to_string());
    }
}

fn expand_skill_tool_token(
    token: &str,
    custom_groups: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let trimmed = token.trim();
    if let Some(group_key) = trimmed.strip_prefix('@')
        && let Some(members) = crate::tools::resolve_tool_group(group_key, custom_groups)
    {
        return members
            .into_iter()
            .filter(|m| !m.starts_with('@'))
            .collect();
    }
    vec![trimmed.to_string()]
}

fn auto_approve_rule_covers(broad: &str, target: &str) -> bool {
    if broad == "*" || broad.eq_ignore_ascii_case(target) {
        return true;
    }
    let broad_tool = broad.strip_suffix(":*").unwrap_or(broad);
    let target_tool = target.split_once(':').map_or(target, |(t, _)| t);
    (!broad.contains(':') || broad.ends_with(":*")) && broad_tool.eq_ignore_ascii_case(target_tool)
}

fn apply_positive_auto_approve(merged: &mut Vec<String>, excluded: &mut Vec<String>, rule: &str) {
    let r = rule.trim();
    if r.is_empty() {
        return;
    }
    if r == "*" {
        push_unique_string(merged, "*");
        excluded.clear();
        return;
    }
    push_unique_string(merged, r);
    excluded.retain(|ex| !auto_approve_rule_covers(r, ex));
}

fn apply_negative_auto_approve(merged: &mut Vec<String>, excluded: &mut Vec<String>, neg: &str) {
    let n = neg.trim();
    if n.is_empty() {
        return;
    }
    if n == "*" {
        merged.retain(|r| r != "*");
        excluded.retain(|ex| merged.iter().any(|m| auto_approve_rule_covers(m, ex)));
        return;
    }
    merged.retain(|r| !auto_approve_rule_covers(n, r));
    excluded.retain(|ex| !auto_approve_rule_covers(n, ex));
    let canonical_ex = n.strip_suffix(":*").unwrap_or(n);
    if merged
        .iter()
        .any(|m| auto_approve_rule_covers(m, canonical_ex))
    {
        push_unique_string(excluded, canonical_ex);
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
    custom_groups: &BTreeMap<String, Vec<String>>,
    memo: &mut [Option<SkillDefinition>],
    visiting: &mut Vec<usize>,
) -> SkillDefinition {
    if let Some(ref cached) = memo[idx] {
        return cached.clone();
    }

    let child = &raw[idx];
    if child.extends.is_empty() {
        let mut resolved = child.clone();
        // Strip any stray `!tool` negations from a root skill without parents,
        // and expand any `@group` entries.
        let mut expanded_tools = Vec::new();
        let mut declared_restricted = false;
        for raw_t in &resolved.tools {
            let t = raw_t.trim();
            if t.is_empty() || t.starts_with('!') {
                continue;
            }
            if t != "*" {
                declared_restricted = true;
            }
            for item in expand_skill_tool_token(t, custom_groups) {
                push_unique_string(&mut expanded_tools, &item);
            }
        }
        resolved.tools = expanded_tools;
        let mut deduped_auto_approve = Vec::new();
        for rule in &resolved.auto_approve {
            let r = rule.trim();
            if !r.is_empty() && !r.starts_with('!') {
                push_unique_string(&mut deduped_auto_approve, r);
            }
        }
        resolved.auto_approve = deduped_auto_approve;
        resolved.excluded_auto_approve.clear();
        resolved.excluded_tools.clear();
        resolved.tools_restricted = declared_restricted && !resolved.tools.iter().any(|t| t == "*");
        memo[idx] = Some(resolved.clone());
        return resolved;
    }

    visiting.push(idx);

    let mut merged_tools: Vec<String> = Vec::new();
    let mut excluded_tools: Vec<String> = Vec::new();
    let mut tools_restricted = false;
    let mut merged_auto_approve: Vec<String> = Vec::new();
    let mut excluded_auto_approve: Vec<String> = Vec::new();
    let mut parent_auto_approve: Vec<(Vec<String>, Vec<String>)> = Vec::new();
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

        let parent = resolve_skill_at(
            p_idx,
            raw,
            name_index,
            cmd_index,
            custom_groups,
            memo,
            visiting,
        );
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
        parent_auto_approve.push((
            parent.auto_approve.clone(),
            parent.excluded_auto_approve.clone(),
        ));
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

    // Apply child's tools (supporting `!tool`, `@group`, and `!@group` without expanding `*` into hard requirements).
    for raw_tool in &child.tools {
        let trimmed = raw_tool.trim();
        if let Some(neg) = trimmed.strip_prefix('!') {
            let neg = neg.trim();
            if neg.is_empty() {
                continue;
            }
            for item in expand_skill_tool_token(neg, custom_groups) {
                merged_tools.retain(|t| !t.eq_ignore_ascii_case(&item));
                if !tools_restricted || merged_tools.iter().any(|t| t == "*") {
                    push_unique_string(&mut excluded_tools, &item);
                }
            }
        } else if trimmed == "*" {
            push_unique_string(&mut merged_tools, "*");
            excluded_tools.clear();
        } else if !trimmed.is_empty() {
            tools_restricted = true;
            for item in expand_skill_tool_token(trimmed, custom_groups) {
                push_unique_string(&mut merged_tools, &item);
                excluded_tools.retain(|ex| !ex.eq_ignore_ascii_case(&item));
            }
        }
    }

    // Merge parents' auto_approve rules order-independently: an explicit allow
    // in any parent overrides another parent's exclusion of that rule.
    for (pos_rules, _) in &parent_auto_approve {
        for rule in pos_rules {
            let r = rule.trim();
            if !r.is_empty() {
                push_unique_string(&mut merged_auto_approve, r);
            }
        }
    }
    for (i, (_, ex_rules)) in parent_auto_approve.iter().enumerate() {
        for ex in ex_rules {
            let ex = ex.trim();
            if ex.is_empty() {
                continue;
            }
            let overridden_by_other_parent =
                parent_auto_approve
                    .iter()
                    .enumerate()
                    .any(|(j, (other_pos, other_ex))| {
                        i != j
                            && other_pos
                                .iter()
                                .any(|r| auto_approve_rule_covers(r.trim(), ex))
                            && !other_ex
                                .iter()
                                .any(|oex| auto_approve_rule_covers(oex.trim(), ex))
                    });
            if !overridden_by_other_parent
                && !merged_auto_approve
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(ex))
                && merged_auto_approve
                    .iter()
                    .any(|m| auto_approve_rule_covers(m, ex))
            {
                push_unique_string(&mut excluded_auto_approve, ex);
            }
        }
    }

    // Apply child's auto_approve rules (supporting `!rule` removal and exclusions).
    for raw_rule in &child.auto_approve {
        let trimmed = raw_rule.trim();
        if let Some(neg) = trimmed.strip_prefix('!') {
            apply_negative_auto_approve(&mut merged_auto_approve, &mut excluded_auto_approve, neg);
        } else if !trimmed.is_empty() {
            apply_positive_auto_approve(
                &mut merged_auto_approve,
                &mut excluded_auto_approve,
                trimmed,
            );
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
        auto_approve: merged_auto_approve,
        excluded_auto_approve,
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

/// Resolves `extends` inheritance chains across a vector of parsed skills,
/// expanding built-in and custom `[tool_groups]` entries in `tools`.
pub fn resolve_skills_with_groups(
    raw: Vec<SkillDefinition>,
    custom_groups: &BTreeMap<String, Vec<String>>,
) -> Vec<SkillDefinition> {
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
        .map(|i| {
            resolve_skill_at(
                i,
                &raw,
                &name_index,
                &cmd_index,
                custom_groups,
                &mut memo,
                &mut visiting,
            )
        })
        .collect()
}

/// Resolves `extends` inheritance chains across a vector of parsed skills
/// using built-in `@group` definitions only.
pub fn resolve_skills(raw: Vec<SkillDefinition>) -> Vec<SkillDefinition> {
    resolve_skills_with_groups(raw, &BTreeMap::new())
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
