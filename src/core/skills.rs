use crate::core::config::{Config, expand_tilde};
use serde::Deserialize;
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub command: String,
    pub tools: Vec<String>,
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
}

fn split_toml_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let trimmed = raw.trim_start_matches('\u{feff}');
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

fn parse_skill_markdown(raw: &str, dir_path: &Path, is_trusted: bool) -> Option<SkillDefinition> {
    let fallback_name = dir_path.file_name()?.to_str()?.trim().to_string();
    let (fm_str, body) = split_toml_frontmatter(raw);
    let fm: SkillFrontmatter = fm_str
        .and_then(|s| toml::from_str(s).ok())
        .unwrap_or_default();

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

    Some(SkillDefinition {
        name,
        description,
        command: fm.command.trim().trim_start_matches('/').to_string(),
        tools: fm.tools,
        prompt,
        dir_path: Some(dir_path.to_path_buf()),
        is_trusted,
    })
}

fn scan_skills_dir(dir: &Path, is_trusted: bool) -> Vec<SkillDefinition> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();

    let mut skills = Vec::new();
    for path in entries {
        if path.is_dir() {
            let md_path = path.join("SKILL.md");
            if let Ok(raw) = fs::read_to_string(&md_path)
                && let Some(skill) = parse_skill_markdown(&raw, &path, is_trusted)
            {
                skills.push(skill);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md")
            && let Ok(raw) = fs::read_to_string(&path)
        {
            let stem_dir = path.with_extension("");
            if let Some(skill) = parse_skill_markdown(&raw, &stem_dir, is_trusted) {
                skills.push(skill);
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

pub fn expand_at_command(raw_prompt: &str, config: &Config) -> Option<(String, String)> {
    let trimmed = raw_prompt.trim();
    let rest = trimmed.strip_prefix('@')?;
    let (cmd_token, args_str) = match rest.find(char::is_whitespace) {
        Some(idx) => (&rest[..idx], rest[idx..].trim()),
        None => (rest, ""),
    };
    if cmd_token.is_empty() {
        return None;
    }

    let skill = config.skills.iter().find(|s| {
        (!s.command.is_empty() && s.command.eq_ignore_ascii_case(cmd_token))
            || s.name.eq_ignore_ascii_case(cmd_token)
    })?;

    let user_prompt = if args_str.is_empty() {
        trimmed.to_string()
    } else {
        args_str.to_string()
    };

    Some((skill.name.clone(), user_prompt))
}

fn format_skill_path_for_read(path: &Path) -> String {
    if path.is_relative() {
        return path.to_string_lossy().into_owned();
    }
    if let Some(home) = dirs::home_dir()
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        return format!("~/{}", stripped.to_string_lossy());
    }
    path.to_string_lossy().into_owned()
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
                out.push(format_skill_path_for_read(&p));
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
    config: &Config,
) -> String {
    let mut sections = Vec::new();
    if !base_system_prompt.trim().is_empty() {
        sections.push(base_system_prompt.trim().to_string());
    }

    if let Some(skill_name) = active_skill
        && let Some(skill) = find_skill(config, skill_name)
    {
        let refs = list_skill_reference_paths(skill);
        let prompt_body = skill.prompt.replace("{{args}}", "").trim().to_string();
        if !refs.is_empty() || !prompt_body.is_empty() {
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
            }
            sections.push(format!(
                "<skill name=\"{}\">\n{}\n</skill>",
                skill.name,
                block.trim_end()
            ));
        }
    }

    sections.join("\n\n")
}
