use crate::{
    core::{
        config::{Config, MemoryScope, expand_tilde, format_tilde_path},
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
    #[serde(alias = "always", alias = "turn", alias = "before_turn")]
    EveryTurn,
    /// Run after every user turn while this skill is active.
    #[serde(alias = "after", alias = "post_turn", alias = "on_complete")]
    AfterTurn,
}

impl HookTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnActivate => "on_activate",
            Self::EveryTurn => "every_turn",
            Self::AfterTurn => "after_turn",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct SkillHook {
    /// When to execute the hook: `on_activate` (default, once per session), `every_turn` (before every turn), or `after_turn` (after every turn).
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
    pub scope: MemoryScope,
    pub overrides_global: bool,
}

impl SkillDefinition {
    pub fn trigger(&self) -> &str {
        if self.command.is_empty() {
            &self.name
        } else {
            &self.command
        }
    }
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

pub(crate) fn parse_skill_markdown(raw: &str, dir_path: &Path) -> Option<SkillDefinition> {
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
        scope: MemoryScope::Global,
        overrides_global: false,
    })
}

pub(crate) fn scan_skills_dir(dir: &Path) -> Vec<SkillDefinition> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();

    let mut skills = Vec::new();
    for path in entries {
        let (md_path, skill_dir) = if path.is_dir() {
            let md = path.join("SKILL.md");
            if !md.exists() {
                continue;
            }
            (md, path)
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let stem = path.with_extension("");
            (path, stem)
        } else {
            continue;
        };

        match fs::read_to_string(&md_path) {
            Ok(raw) => {
                if let Some(skill) = parse_skill_markdown(&raw, &skill_dir) {
                    skills.push(skill);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to read skill file '{}': {e}", md_path.display());
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
    let upsert = |list: &mut Vec<SkillDefinition>, mut incoming: SkillDefinition| {
        if let Some(existing) = list
            .iter_mut()
            .find(|s| s.name.eq_ignore_ascii_case(&incoming.name))
        {
            incoming.overrides_global =
                existing.scope == MemoryScope::Global && incoming.scope == MemoryScope::Workspace;
            *existing = incoming;
        } else {
            list.push(incoming);
        }
    };

    let global_dir = expand_tilde(skills_dir);
    for mut skill in scan_skills_dir(&global_dir) {
        skill.scope = MemoryScope::Global;
        upsert(&mut merged, skill);
    }

    if let Some(ws_dir) = workspace_skills_dir {
        for mut skill in scan_skills_dir(ws_dir) {
            skill.scope = MemoryScope::Workspace;
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
    (!cmd_token.is_empty()).then_some((cmd_token, args_str))
}

pub fn apply_at_skill_to_turn(
    raw_prompt: &str,
    turn: &mut crate::core::TurnConfig,
    config: &Config,
) -> std::result::Result<String, String> {
    if let Some((cmd_token, args_str)) = parse_at_command(raw_prompt) {
        let name = check_skill_activation(cmd_token, turn.enable_tools, config)?;
        turn.active_skill = Some(name);
        turn.skill_args = (!args_str.is_empty()).then(|| args_str.to_string());
        Ok(if args_str.is_empty() {
            raw_prompt.trim().to_string()
        } else {
            args_str.to_string()
        })
    } else {
        Ok(raw_prompt.to_string())
    }
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
            m.include_in_context
                && matches!(m.role.as_str(), "hook" | "local")
                && m.tool_name.as_deref() == Some(summary)
        })
}

pub fn preview_due_hooks(
    config: &Config,
    active_skill: Option<&str>,
    executed_hooks: &HashSet<String>,
    messages: &[ChatMessage],
) -> Vec<String> {
    let mut temp = executed_hooks.clone();
    collect_due_hooks(config, active_skill, &mut temp, messages)
        .into_iter()
        .map(|(summary, _)| summary)
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
            HookTrigger::AfterTurn => false,
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

pub fn collect_after_turn_hooks(
    config: &Config,
    active_skill: Option<&str>,
) -> Vec<(String, ToolCall)> {
    let Some(skill) = active_skill.and_then(|name| find_skill(config, name)) else {
        return Vec::new();
    };
    skill
        .hooks
        .iter()
        .filter(|hook| hook.trigger == HookTrigger::AfterTurn)
        .map(|hook| {
            (
                format_hook_summary(hook),
                ToolCall {
                    function: ToolFunction {
                        name: hook.tool.clone(),
                        arguments: hook.args.clone(),
                    },
                    thought_signature: None,
                },
            )
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_skills_tracks_scope_and_global_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let global_dir = tmp.path().join("global_skills");
        let ws_dir = tmp.path().join("ws_skills");

        fs::create_dir_all(global_dir.join("coder")).unwrap();
        fs::create_dir_all(global_dir.join("commit")).unwrap();
        fs::create_dir_all(ws_dir.join("coder")).unwrap();
        fs::create_dir_all(ws_dir.join("local-only")).unwrap();

        fs::write(
            global_dir.join("coder/SKILL.md"),
            "+++\nname = \"coder\"\ncommand = \"coder\"\n+++\nGlobal coder",
        )
        .unwrap();
        fs::write(
            global_dir.join("commit/SKILL.md"),
            "+++\nname = \"commit\"\ncommand = \"commit\"\n+++\nGlobal commit",
        )
        .unwrap();
        fs::write(
            ws_dir.join("coder/SKILL.md"),
            "+++\nname = \"coder\"\ncommand = \"coder\"\n+++\nWorkspace coder",
        )
        .unwrap();
        fs::write(
            ws_dir.join("local-only/SKILL.md"),
            "+++\nname = \"local-only\"\n+++\nWorkspace local",
        )
        .unwrap();

        let skills = discover_skills(&global_dir.to_string_lossy(), Some(&ws_dir));
        assert_eq!(skills.len(), 3);

        let coder = skills.iter().find(|s| s.name == "coder").unwrap();
        assert_eq!(coder.scope, MemoryScope::Workspace);
        assert!(coder.overrides_global);
        assert_eq!(coder.prompt, "Workspace coder");

        let commit = skills.iter().find(|s| s.name == "commit").unwrap();
        assert_eq!(commit.scope, MemoryScope::Global);
        assert!(!commit.overrides_global);

        let local = skills.iter().find(|s| s.name == "local-only").unwrap();
        assert_eq!(local.scope, MemoryScope::Workspace);
        assert!(!local.overrides_global);
    }

    #[test]
    fn after_turn_hooks_parsed_and_collected_separately_from_pre_turn_hooks() {
        let raw = r#"+++
name = "runner"
command = "runner"
tools = ["cargo", "notify"]

[[hooks]]
trigger = "on_activate"
tool = "cargo"
args = { action = "metadata" }

[[hooks]]
trigger = "every_turn"
tool = "cargo"
args = { action = "check" }

[[hooks]]
trigger = "after_turn"
tool = "notify"
args = { summary = "Turn finished" }
+++
Run checks and notify."#;

        let skill = parse_skill_markdown(raw, Path::new("/tmp/runner")).unwrap();
        assert_eq!(skill.hooks.len(), 3);
        assert_eq!(skill.hooks[0].trigger, HookTrigger::OnActivate);
        assert_eq!(skill.hooks[1].trigger, HookTrigger::EveryTurn);
        assert_eq!(skill.hooks[2].trigger, HookTrigger::AfterTurn);
        assert_eq!(skill.hooks[2].trigger.as_str(), "after_turn");

        let cfg = Config {
            skills: vec![skill],
            ..Config::default()
        };

        let mut executed = HashSet::new();
        let pre_turn_1 = collect_due_hooks(&cfg, Some("runner"), &mut executed, &[]);
        assert_eq!(pre_turn_1.len(), 2);
        assert_eq!(pre_turn_1[0].1.function.name, "cargo");
        assert_eq!(pre_turn_1[1].1.function.name, "cargo");

        let pre_turn_2 = collect_due_hooks(&cfg, Some("runner"), &mut executed, &[]);
        assert_eq!(pre_turn_2.len(), 1);
        assert_eq!(pre_turn_2[0].0, "cargo:check");

        let after_turn = collect_after_turn_hooks(&cfg, Some("runner"));
        assert_eq!(after_turn.len(), 1);
        assert_eq!(after_turn[0].1.function.name, "notify");
        assert_eq!(after_turn[0].0, "notify (summary=Turn finished)");

        // An `after_turn` hook message (which has `include_in_context = false`)
        // should not suppress a subsequent `on_activate` hook with the same summary.
        let mut after_msg = ChatMessage::new_hook("cargo:metadata", "ok", false);
        after_msg.include_in_context = false;
        let mut fresh_executed = HashSet::new();
        let due_with_after_msg =
            collect_due_hooks(&cfg, Some("runner"), &mut fresh_executed, &[after_msg]);
        assert_eq!(due_with_after_msg.len(), 2);
        assert_eq!(due_with_after_msg[0].0, "cargo:metadata");
    }
}
