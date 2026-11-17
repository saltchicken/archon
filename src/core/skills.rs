use crate::{
    core::{
        config::{Config, expand_tilde, format_tilde_path},
        session::ChatMessage,
    },
    tools::{ToolCall, ToolFunction},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

fn default_hook_args() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookTrigger {
    /// Run once when the skill is first activated in a session (default).
    #[default]
    #[serde(alias = "once", alias = "activate")]
    OnActivate,
    /// Run before every user turn while this skill is active.
    #[serde(alias = "always", alias = "turn")]
    EveryTurn,
}

impl HookTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnActivate => "on_activate",
            Self::EveryTurn => "every_turn",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct SkillHook {
    /// When to execute the hook: `on_activate` (default, once per session) or `every_turn`.
    #[serde(default)]
    pub trigger: HookTrigger,
    /// Built-in tool name to execute (e.g. `"workspace_snapshot"`, `"repo_map"`, `"git"`).
    pub tool: String,
    /// Tool arguments object (e.g. `{"tree_only": true}`).
    #[serde(default = "default_hook_args")]
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub command: String,
    pub tools: Vec<String>,
    pub hooks: Vec<SkillHook>,
    pub prompt: String,
    pub dir_path: Option<PathBuf>,
    pub is_trusted: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SkillFrontmatter {
    name: String,
    description: String,
    command: String,
    tools: Vec<String>,
    hooks: Vec<SkillHook>,
}

pub(crate) fn split_toml_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let trimmed = raw.trim_start_matches('\u{feff}').trim_start();
    let Some(rest) = trimmed
        .strip_prefix("+++\n")
        .or_else(|| trimmed.strip_prefix("+++\r\n"))
    else {
        return (None, trimmed);
    };

    for (idx, line) in rest.lines().scan(0usize, |pos, l| {
        let start = *pos;
        *pos += l.len()
            + if rest[start + l.len()..].starts_with("\r\n") {
                2
            } else {
                usize::from(rest[start + l.len()..].starts_with('\n'))
            };
        Some((start, l))
    }) {
        if line.trim() == "+++" {
            let fm = &rest[..idx];
            let after = &rest[idx + line.len()..];
            let body = after
                .strip_prefix("\r\n")
                .or_else(|| after.strip_prefix('\n'))
                .unwrap_or(after);
            return (Some(fm), body);
        }
    }
    (None, trimmed)
}

pub(crate) fn parse_skill_markdown(
    raw: &str,
    dir_path: &Path,
    is_trusted: bool,
) -> Option<SkillDefinition> {
    let fallback_name = dir_path.file_name()?.to_str()?.trim().to_string();
    let (fm_str, body) = split_toml_frontmatter(raw);
    let fm: SkillFrontmatter = match fm_str {
        Some(s) => match toml::from_str(s) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse TOML frontmatter in skill '{}': {e}",
                    dir_path.display()
                );
                SkillFrontmatter::default()
            }
        },
        None => SkillFrontmatter::default(),
    };

    let name = if fm.name.trim().is_empty() {
        fallback_name
    } else {
        fm.name.trim().to_string()
    };
    if name.is_empty() {
        return None;
    }

    let prompt = body.trim().to_string();
    let description = if fm.description.trim().is_empty() {
        prompt
            .lines()
            .map(|l| l.trim().trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or("Custom skill")
            .chars()
            .take(120)
            .collect()
    } else {
        fm.description.trim().to_string()
    };

    let hooks = fm
        .hooks
        .into_iter()
        .filter_map(|mut h| {
            h.tool = h.tool.trim().to_string();
            if h.tool.is_empty() {
                return None;
            }
            if h.args.is_null() {
                h.args = default_hook_args();
            }
            Some(h)
        })
        .collect();

    Some(SkillDefinition {
        name,
        description,
        command: fm.command.trim().trim_start_matches('/').to_string(),
        tools: fm.tools,
        hooks,
        prompt,
        dir_path: Some(dir_path.to_path_buf()),
        is_trusted,
    })
}

pub(crate) fn scan_skills_dir(dir: &Path, is_trusted: bool) -> Vec<SkillDefinition> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();

    let mut skills = Vec::new();
    for path in entries {
        if path.is_dir() {
            let md_path = path.join("SKILL.md");
            match fs::read_to_string(&md_path) {
                Ok(raw) => {
                    if let Some(skill) = parse_skill_markdown(&raw, &path, is_trusted) {
                        skills.push(skill);
                    }
                }
                Err(e) if md_path.exists() => {
                    tracing::warn!("Failed to read skill file '{}': {e}", md_path.display());
                }
                Err(_) => {}
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            match fs::read_to_string(&path) {
                Ok(raw) => {
                    let stem_dir = path.with_extension("");
                    if let Some(skill) = parse_skill_markdown(&raw, &stem_dir, is_trusted) {
                        skills.push(skill);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read skill file '{}': {e}", path.display());
                }
            }
        }
    }
    skills
}

pub fn discover_skills(
    skills_dir: &str,
    workspace_skills_dir: Option<&Path>,
) -> Vec<SkillDefinition> {
    let mut merged = Vec::new();
    let upsert = |list: &mut Vec<SkillDefinition>, incoming: SkillDefinition| {
        if let Some(existing) = list
            .iter_mut()
            .find(|s| s.name.eq_ignore_ascii_case(&incoming.name))
        {
            if existing.is_trusted && !incoming.is_trusted {
                return;
            }
            *existing = incoming;
        } else {
            list.push(incoming);
        }
    };

    let global_dir = expand_tilde(skills_dir);
    for skill in scan_skills_dir(&global_dir, true) {
        upsert(&mut merged, skill);
    }

    if let Some(ws_dir) = workspace_skills_dir {
        for skill in scan_skills_dir(ws_dir, true) {
            upsert(&mut merged, skill);
        }
    }

    merged
}

pub fn find_skill<'a>(config: &'a Config, name_or_cmd: &str) -> Option<&'a SkillDefinition> {
    let q = name_or_cmd.trim().trim_start_matches('@');
    if q.is_empty() {
        return None;
    }
    config.skills.iter().find(|s| {
        s.name.eq_ignore_ascii_case(q)
            || (!s.command.is_empty() && s.command.eq_ignore_ascii_case(q))
    })
}

pub fn parse_at_command(raw_prompt: &str) -> Option<(&str, &str)> {
    let trimmed = raw_prompt.trim();
    let rest = trimmed.strip_prefix('@')?;
    let (cmd_token, args_str) = match rest.find(char::is_whitespace) {
        Some(idx) => (&rest[..idx], rest[idx..].trim()),
        None => (rest, ""),
    };
    if cmd_token.is_empty() {
        return None;
    }
    Some((cmd_token, args_str))
}

pub fn format_hook_summary(hook: &SkillHook) -> String {
    let call = ToolCall {
        function: ToolFunction {
            name: hook.tool.clone(),
            arguments: hook.args.clone(),
        },
        thought_signature: None,
    };
    crate::tools::format_call_summary(&call)
}

pub fn hook_execution_key(skill_name: &str, idx: usize, hook: &SkillHook) -> String {
    format!(
        "{}::{idx}::{}::{}",
        skill_name.to_ascii_lowercase(),
        hook.tool,
        hook.args
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
            (m.role == "hook" || (m.role == "local" && m.include_in_context))
                && m.tool_name.as_deref() == Some(summary)
        })
}

pub fn preview_due_hooks(
    config: &Config,
    active_skill: Option<&str>,
    executed_hooks: &HashSet<String>,
    messages: &[ChatMessage],
) -> Vec<String> {
    let Some(skill) = active_skill.and_then(|name| find_skill(config, name)) else {
        return Vec::new();
    };
    skill
        .hooks
        .iter()
        .enumerate()
        .filter_map(|(idx, hook)| {
            let summary = format_hook_summary(hook);
            let key = hook_execution_key(&skill.name, idx, hook);
            let due = match hook.trigger {
                HookTrigger::OnActivate => {
                    !is_hook_already_in_session(&key, &summary, executed_hooks, messages)
                }
                HookTrigger::EveryTurn => true,
            };
            due.then_some(summary)
        })
        .collect()
}

pub fn collect_due_hooks(
    config: &Config,
    active_skill: Option<&str>,
    executed_hooks: &mut HashSet<String>,
    messages: &[ChatMessage],
) -> Vec<(String, ToolCall)> {
    let Some(skill) = active_skill.and_then(|name| find_skill(config, name)) else {
        return Vec::new();
    };
    let mut due_hooks = Vec::new();
    for (idx, hook) in skill.hooks.iter().enumerate() {
        let summary = format_hook_summary(hook);
        let key = hook_execution_key(&skill.name, idx, hook);
        let due = match hook.trigger {
            HookTrigger::OnActivate => {
                let already = is_hook_already_in_session(&key, &summary, executed_hooks, messages);
                executed_hooks.insert(key);
                !already
            }
            HookTrigger::EveryTurn => true,
        };
        if due {
            due_hooks.push((
                summary,
                ToolCall {
                    function: ToolFunction {
                        name: hook.tool.clone(),
                        arguments: hook.args.clone(),
                    },
                    thought_signature: None,
                },
            ));
        }
    }
    due_hooks
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
        if name.is_empty() || name == "*" {
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
) -> std::result::Result<String, String> {
    let clean_name = skill_name.trim().trim_start_matches('@');
    let Some(skill) = find_skill(config, clean_name) else {
        return Err(format!("Skill '@{clean_name}' not found."));
    };
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

pub fn list_skill_reference_paths(skill: &SkillDefinition) -> Vec<String> {
    let Some(dir) = skill.dir_path.as_deref().filter(|d| d.is_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    fn walk(cur: &Path, out: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(cur) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
                out.push(format_tilde_path(&p));
            }
        }
    }
    walk(dir, &mut out);
    out.sort();
    out
}

pub fn compose_system_prompt(
    base_system_prompt: &str,
    active_skill: Option<&str>,
    skill_args: Option<&str>,
    config: &Config,
) -> String {
    let mut sections = Vec::new();
    if !base_system_prompt.trim().is_empty() {
        sections.push(base_system_prompt.trim().to_string());
    }

    if let Some(mem_block) = config.effective_memory().format_for_prompt() {
        sections.push(mem_block);
    }

    if let Some(skill_name) = active_skill
        && let Some(skill) = find_skill(config, skill_name)
    {
        let refs = list_skill_reference_paths(skill);
        let args = skill_args.unwrap_or("").trim();
        let prompt_body = skill.prompt.replace("{{args}}", args).trim().to_string();
        let mut block = String::new();
        if !refs.is_empty() {
            let _ = writeln!(
                block,
                "Reference files (read via `read_file`): {}",
                refs.join(", ")
            );
        }
        if !prompt_body.is_empty() {
            let _ = writeln!(block, "{prompt_body}");
        } else if refs.is_empty() && args.is_empty() {
            let _ = writeln!(block, "Active skill: {}", skill.description);
        }
        if !args.is_empty() && !skill.prompt.contains("{{args}}") {
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
