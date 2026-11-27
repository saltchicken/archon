//! Parsing, scanning, and discovery of `SKILL.md` files.

use super::types::{KNOWN_FRONTMATTER_KEYS, SkillArgDef, SkillDefinition, SkillHook};
use crate::core::config::{Config, MemoryScope, expand_tilde};
use serde::{Deserialize, Deserializer};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing;

fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    match Option::<OneOrMany>::deserialize(deserializer)? {
        None => Ok(Vec::new()),
        Some(OneOrMany::One(s)) => {
            let trimmed = s.trim();
            Ok(if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            })
        }
        Some(OneOrMany::Many(v)) => Ok(v
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SkillFrontmatter {
    name: String,
    description: String,
    command: String,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    extends: Vec<String>,
    #[serde(default, rename = "abstract")]
    is_abstract: bool,
    tools: Vec<String>,
    auto_approve: Vec<String>,
    hooks: Vec<SkillHook>,
    args: Vec<SkillArgDef>,
}

/// Detailed parse outcome for `archon skill check` and strict validation.
#[derive(Debug)]
pub struct DetailedSkillParse {
    pub definition: Option<SkillDefinition>,
    pub unclosed_frontmatter: bool,
    pub frontmatter_error: Option<String>,
    pub unknown_frontmatter_keys: Vec<String>,
    pub raw_tools: Vec<String>,
    pub raw_auto_approve: Vec<String>,
    pub raw_hooks: Vec<SkillHook>,
}

pub fn split_toml_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let trimmed = raw.trim_start_matches('\u{feff}').trim_start();
    let Some(rest) = trimmed
        .strip_prefix("+++\n")
        .or_else(|| trimmed.strip_prefix("+++\r\n"))
    else {
        return (None, trimmed);
    };

    let mut offset = 0usize;
    for chunk in rest.split_inclusive('\n') {
        if chunk.trim() == "+++" {
            let body = &rest[offset + chunk.len()..];
            return (Some(&rest[..offset]), body);
        }
        offset += chunk.len();
    }
    (None, trimmed)
}

pub fn parse_skill_markdown_detailed(raw: &str, dir_path: &Path) -> DetailedSkillParse {
    let trimmed = raw.trim_start_matches('\u{feff}').trim_start();
    let starts_with_fence =
        trimmed.starts_with("+++\n") || trimmed.starts_with("+++\r\n") || trimmed.trim() == "+++";
    let (fm_str, body) = split_toml_frontmatter(raw);
    let unclosed_frontmatter = starts_with_fence && fm_str.is_none();

    let mut frontmatter_error = None;
    let mut unknown_frontmatter_keys = Vec::new();

    let fm: SkillFrontmatter = match fm_str {
        Some(s) => {
            if let Ok(table) = toml::from_str::<toml::Table>(s) {
                for key in table.keys() {
                    if !KNOWN_FRONTMATTER_KEYS.contains(&key.as_str()) {
                        unknown_frontmatter_keys.push(key.clone());
                    }
                }
                unknown_frontmatter_keys.sort();
            }
            match toml::from_str(s) {
                Ok(parsed) => parsed,
                Err(e) => {
                    frontmatter_error = Some(e.to_string());
                    SkillFrontmatter::default()
                }
            }
        }
        None => SkillFrontmatter::default(),
    };

    let fallback_name = dir_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let name = if fm.name.trim().is_empty() {
        fallback_name
    } else {
        fm.name.trim().to_string()
    };

    if name.is_empty() {
        return DetailedSkillParse {
            definition: None,
            unclosed_frontmatter,
            frontmatter_error,
            unknown_frontmatter_keys,
            raw_tools: Vec::new(),
            raw_auto_approve: Vec::new(),
            raw_hooks: Vec::new(),
        };
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

    let raw_tools = fm.tools.clone();
    let raw_auto_approve = fm.auto_approve.clone();
    let raw_hooks = fm.hooks.clone();

    let hooks = fm
        .hooks
        .into_iter()
        .filter_map(|mut h| {
            h.tool = h.tool.trim().to_string();
            if h.tool.is_empty() {
                return None;
            }
            if h.args.is_null() {
                h.args = serde_json::Value::Object(serde_json::Map::new());
            }
            Some(h)
        })
        .collect();

    let args = fm
        .args
        .into_iter()
        .map(|mut a| {
            a.name = a.name.trim().to_string();
            a.description = a.description.trim().to_string();
            a.choices = a
                .choices
                .into_iter()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();
            a
        })
        .collect();

    let extends = fm
        .extends
        .into_iter()
        .map(|e| e.trim().trim_start_matches('@').to_string())
        .filter(|e| !e.is_empty())
        .collect();

    DetailedSkillParse {
        definition: Some(SkillDefinition {
            name,
            description,
            command: fm.command.trim().trim_start_matches('/').to_string(),
            extends,
            is_abstract: fm.is_abstract,
            tools: fm.tools,
            excluded_tools: Vec::new(),
            tools_restricted: false,
            auto_approve: fm.auto_approve,
            excluded_auto_approve: Vec::new(),
            hooks,
            args,
            prompt,
            dir_path: Some(dir_path.to_path_buf()),
            inherited_dirs: Vec::new(),
            scope: MemoryScope::Global,
            overrides_global: false,
            resolution_errors: Vec::new(),
        }),
        unclosed_frontmatter,
        frontmatter_error,
        unknown_frontmatter_keys,
        raw_tools,
        raw_auto_approve,
        raw_hooks,
    }
}

pub struct ScannedRawSkill {
    pub(crate) scope_label: String,
    pub(crate) scope: MemoryScope,
    pub(crate) file_path: PathBuf,
    pub(crate) skill_dir: PathBuf,
    pub(crate) read_error: Option<String>,
    pub(crate) parsed: Option<DetailedSkillParse>,
}

pub fn scan_raw_dir(dir: &Path, scope_label: &str, scope: MemoryScope) -> Vec<ScannedRawSkill> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();

    let mut out = Vec::new();
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
                let parsed = parse_skill_markdown_detailed(&raw, &skill_dir);
                out.push(ScannedRawSkill {
                    scope_label: scope_label.to_string(),
                    scope,
                    file_path: md_path,
                    skill_dir,
                    read_error: None,
                    parsed: Some(parsed),
                });
            }
            Err(e) => {
                out.push(ScannedRawSkill {
                    scope_label: scope_label.to_string(),
                    scope,
                    file_path: md_path,
                    skill_dir,
                    read_error: Some(e.to_string()),
                    parsed: None,
                });
            }
        }
    }
    out
}

/// Insert or replace a skill by name (case-insensitive).
///
/// When replacing, sets `overrides_global` on the incoming definition if it is
/// a workspace-scoped entry overriding a global-scoped entry.
pub fn upsert_skill_by_name(list: &mut Vec<SkillDefinition>, mut incoming: SkillDefinition) {
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
}

pub fn parse_skill_markdown(raw: &str, dir_path: &Path) -> Option<SkillDefinition> {
    let detailed = parse_skill_markdown_detailed(raw, dir_path);
    if let Some(ref err) = detailed.frontmatter_error {
        tracing::warn!(
            "Failed to parse TOML frontmatter in skill '{}': {err}",
            dir_path.display()
        );
    }
    detailed.definition
}

pub fn scan_skills_dir(dir: &Path) -> Vec<SkillDefinition> {
    scan_raw_dir(dir, "", MemoryScope::Global)
        .into_iter()
        .filter_map(|item| {
            if let Some(err) = item.read_error {
                tracing::warn!(
                    "Failed to read skill file '{}': {err}",
                    item.file_path.display()
                );
                return None;
            }
            let detailed = item.parsed?;
            if let Some(ref err) = detailed.frontmatter_error {
                tracing::warn!(
                    "Failed to parse TOML frontmatter in skill '{}': {err}",
                    item.skill_dir.display()
                );
            }
            detailed.definition
        })
        .collect()
}

pub fn discover_skills_with_groups(
    skills_dir: &str,
    workspace_skills_dir: Option<&Path>,
    custom_groups: &std::collections::BTreeMap<String, Vec<String>>,
) -> Vec<SkillDefinition> {
    let mut merged = Vec::new();

    let global_dir = expand_tilde(skills_dir);
    for mut skill in scan_skills_dir(&global_dir) {
        skill.scope = MemoryScope::Global;
        upsert_skill_by_name(&mut merged, skill);
    }

    if let Some(ws_dir) = workspace_skills_dir {
        for mut skill in scan_skills_dir(ws_dir) {
            skill.scope = MemoryScope::Workspace;
            upsert_skill_by_name(&mut merged, skill);
        }
    }

    super::resolve::resolve_skills_with_groups(merged, custom_groups)
}

pub fn discover_skills(
    skills_dir: &str,
    workspace_skills_dir: Option<&Path>,
) -> Vec<SkillDefinition> {
    discover_skills_with_groups(
        skills_dir,
        workspace_skills_dir,
        &std::collections::BTreeMap::new(),
    )
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
