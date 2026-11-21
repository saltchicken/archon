use crate::{
    core::{
        config::{Config, MemoryScope, expand_tilde, format_tilde_path},
        session::ChatMessage,
    },
    tools::{ToolCall, ToolFunction},
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

pub const RESERVED_ARG_NAMES: &[&str] = &["args", "raw_args", "last_response", "super"];

pub const KNOWN_FRONTMATTER_KEYS: &[&str] = &[
    "name",
    "description",
    "command",
    "extends",
    "abstract",
    "tools",
    "hooks",
    "args",
    "references",
];

fn default_hook_args() -> Value {
    Value::Object(serde_json::Map::new())
}

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
    /// Tool arguments object (e.g. `{"tree_only": true}`). String values may include `{{last_response}}`, `{{args}}`, or declared `{{arg_name}}` placeholders.
    #[serde(default = "default_hook_args")]
    pub args: Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillArgType {
    /// Arbitrary string value (default).
    #[default]
    #[serde(alias = "str", alias = "text")]
    String,
    /// Relative or tilde-expanded file/directory path.
    #[serde(alias = "file", alias = "dir")]
    Path,
    /// 64-bit signed integer.
    #[serde(alias = "integer", alias = "i64")]
    Int,
    /// Floating-point number.
    #[serde(alias = "number", alias = "f64")]
    Float,
    /// Boolean (`true`/`false`, `yes`/`no`, `1`/`0`).
    #[serde(alias = "boolean")]
    Bool,
    /// One of the allowed values declared in `choices`.
    #[serde(alias = "choice")]
    Enum,
}

impl SkillArgType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Path => "path",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::Enum => "enum",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct SkillArgDef {
    /// Argument identifier (used in `key=value` and `{{name}}` template placeholders).
    pub name: String,
    /// Expected input type: `string` (default), `path`, `int`, `float`, `bool`, or `enum`.
    #[serde(default, rename = "type")]
    pub arg_type: SkillArgType,
    /// Whether the user must provide this argument when invoking the skill (default: `false`).
    #[serde(default)]
    pub required: bool,
    /// Optional default value used when the argument is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Allowed values when `type = "enum"` (or to constrain string/path values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// Short human-readable description shown in usage hints and error messages.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl SkillArgDef {
    pub fn usage_token(&self) -> String {
        let rhs = if !self.choices.is_empty() {
            self.choices.join("|")
        } else {
            format!("<{}>", self.arg_type.as_str())
        };
        if self.required {
            format!("{}={rhs}", self.name)
        } else {
            format!("[{}={rhs}]", self.name)
        }
    }

    pub fn sample_value(&self) -> Value {
        if let Some(ref def) = self.default
            && let Ok(valid) = validate_arg_json_value(self, def)
        {
            return valid;
        }
        match self.arg_type {
            SkillArgType::String => self
                .choices
                .first()
                .map(|c| Value::String(c.clone()))
                .unwrap_or_else(|| Value::String("sample".into())),
            SkillArgType::Path => self
                .choices
                .first()
                .map(|c| Value::String(c.clone()))
                .unwrap_or_else(|| Value::String("src".into())),
            SkillArgType::Int => Value::Number(1.into()),
            SkillArgType::Float => serde_json::Number::from_f64(1.0)
                .map(Value::Number)
                .unwrap_or(Value::Number(1.into())),
            SkillArgType::Bool => Value::Bool(true),
            SkillArgType::Enum => self
                .choices
                .first()
                .map(|c| Value::String(c.clone()))
                .unwrap_or_else(|| Value::String("sample".into())),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub command: String,
    pub extends: Vec<String>,
    pub is_abstract: bool,
    pub tools: Vec<String>,
    pub excluded_tools: Vec<String>,
    pub tools_restricted: bool,
    pub hooks: Vec<SkillHook>,
    pub args: Vec<SkillArgDef>,
    pub references: Vec<String>,
    pub prompt: String,
    pub dir_path: Option<PathBuf>,
    pub inherited_dirs: Vec<PathBuf>,
    pub scope: MemoryScope,
    pub overrides_global: bool,
    pub resolution_errors: Vec<String>,
}

impl SkillDefinition {
    pub fn trigger(&self) -> &str {
        if self.command.is_empty() {
            &self.name
        } else {
            &self.command
        }
    }

    pub fn allows_tool(&self, name: &str) -> bool {
        if self
            .excluded_tools
            .iter()
            .any(|t| t.eq_ignore_ascii_case(name))
        {
            return false;
        }
        if self.tools.is_empty() {
            return !self.tools_restricted;
        }
        self.tools
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .any(|t| t == "*" || t == name)
    }

    pub fn all_dirs(&self) -> impl Iterator<Item = &Path> {
        self.dir_path
            .as_deref()
            .into_iter()
            .chain(self.inherited_dirs.iter().map(PathBuf::as_path))
    }

    pub fn args_usage_hint(&self) -> String {
        self.args
            .iter()
            .map(SkillArgDef::usage_token)
            .collect::<Vec<_>>()
            .join(" ")
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
    hooks: Vec<SkillHook>,
    args: Vec<SkillArgDef>,
    references: Vec<String>,
}

/// Detailed parse outcome for `archon skill check` and strict validation.
#[derive(Debug)]
pub struct DetailedSkillParse {
    pub definition: Option<SkillDefinition>,
    pub unclosed_frontmatter: bool,
    pub frontmatter_error: Option<String>,
    pub unknown_frontmatter_keys: Vec<String>,
    pub raw_tools: Vec<String>,
    pub raw_hooks: Vec<SkillHook>,
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
                h.args = default_hook_args();
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

    let references = fm
        .references
        .into_iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
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
            hooks,
            args,
            references,
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
        raw_hooks,
    }
}

pub(crate) fn parse_skill_markdown(raw: &str, dir_path: &Path) -> Option<SkillDefinition> {
    let detailed = parse_skill_markdown_detailed(raw, dir_path);
    if let Some(ref err) = detailed.frontmatter_error {
        tracing::warn!(
            "Failed to parse TOML frontmatter in skill '{}': {err}",
            dir_path.display()
        );
    }
    detailed.definition
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
    let mut merged_hooks: Vec<SkillHook> = Vec::new();
    let mut merged_args: Vec<SkillArgDef> = Vec::new();
    let mut merged_refs: Vec<String> = Vec::new();
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
        for r in &parent.references {
            push_unique_string(&mut merged_refs, r);
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

    // Apply child's references.
    for r in &child.references {
        push_unique_string(&mut merged_refs, r);
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
        references: merged_refs,
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

fn replace_super_placeholder(prompt: &str, base: &str) -> Option<String> {
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

    resolve_skills(merged)
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

// ============================================================================
// Typed Argument Coercion, Parsing & Validation
// ============================================================================

pub fn format_arg_value(val: &Value) -> String {
    match val {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn match_choice(def: &SkillArgDef, candidate: &str) -> Result<String, String> {
    if def.choices.is_empty() {
        return Err(format!(
            "Argument '{}' has type 'enum' but declares no `choices`.",
            def.name
        ));
    }
    if let Some(exact) = def.choices.iter().find(|c| c.as_str() == candidate) {
        return Ok(exact.clone());
    }
    if let Some(ci) = def
        .choices
        .iter()
        .find(|c| c.eq_ignore_ascii_case(candidate))
    {
        return Ok(ci.clone());
    }
    Err(format!(
        "Invalid value '{candidate}' for argument '{}'. Expected one of: {}",
        def.name,
        def.choices.join(", ")
    ))
}

pub fn coerce_arg_value(def: &SkillArgDef, raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    match def.arg_type {
        SkillArgType::String => {
            if !def.choices.is_empty() {
                return Ok(Value::String(match_choice(def, trimmed)?));
            }
            Ok(Value::String(raw.to_string()))
        }
        SkillArgType::Path => {
            if trimmed.is_empty() {
                return Err(format!("Argument '{}' expects a non-empty path.", def.name));
            }
            if !def.choices.is_empty() {
                return Ok(Value::String(match_choice(def, trimmed)?));
            }
            Ok(Value::String(trimmed.to_string()))
        }
        SkillArgType::Int => trimmed
            .parse::<i64>()
            .map(|n| Value::Number(n.into()))
            .map_err(|_| {
                format!(
                    "Invalid value '{trimmed}' for argument '{}': expected an integer.",
                    def.name
                )
            }),
        SkillArgType::Float => {
            let f = trimmed.parse::<f64>().map_err(|_| {
                format!(
                    "Invalid value '{trimmed}' for argument '{}': expected a number.",
                    def.name
                )
            })?;
            if !f.is_finite() {
                return Err(format!(
                    "Invalid value '{trimmed}' for argument '{}': expected a finite number.",
                    def.name
                ));
            }
            serde_json::Number::from_f64(f)
                .map(Value::Number)
                .ok_or_else(|| {
                    format!(
                        "Invalid value '{trimmed}' for argument '{}': expected a finite number.",
                        def.name
                    )
                })
        }
        SkillArgType::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Ok(Value::Bool(true)),
            "false" | "no" | "0" | "off" => Ok(Value::Bool(false)),
            _ => Err(format!(
                "Invalid value '{trimmed}' for argument '{}': expected a boolean (true/false).",
                def.name
            )),
        },
        SkillArgType::Enum => Ok(Value::String(match_choice(def, trimmed)?)),
    }
}

pub fn validate_arg_json_value(def: &SkillArgDef, val: &Value) -> Result<Value, String> {
    match (def.arg_type, val) {
        (SkillArgType::String, Value::String(s)) => coerce_arg_value(def, s),
        (SkillArgType::Path, Value::String(s)) => coerce_arg_value(def, s),
        (SkillArgType::Enum, Value::String(s)) => coerce_arg_value(def, s),
        (SkillArgType::Int, Value::Number(n)) if n.is_i64() => Ok(val.clone()),
        (SkillArgType::Int, Value::Number(n))
            if n.as_u64().is_some_and(|u| u <= i64::MAX as u64) =>
        {
            Ok(Value::Number((n.as_u64().unwrap() as i64).into()))
        }
        (SkillArgType::Float, Value::Number(n)) if n.as_f64().is_some_and(f64::is_finite) => {
            Ok(val.clone())
        }
        (SkillArgType::Bool, Value::Bool(_)) => Ok(val.clone()),
        _ => Err(format!(
            "Default value {val} for argument '{}' does not match declared type '{}'.",
            def.name,
            def.arg_type.as_str()
        )),
    }
}

#[derive(Debug)]
struct ArgTokenSpan {
    start: usize,
    end: usize,
    key: Option<String>,
    value: String,
}

fn scan_quoted_segment(s: &str, start: usize) -> Option<(usize, String)> {
    let mut chars = s[start..].char_indices();
    let (_, quote) = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    let mut out = String::new();
    let mut escaped = false;
    for (rel_idx, ch) in chars {
        if escaped {
            match ch {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                other => out.push(other),
            }
            escaped = false;
            continue;
        }
        if quote == '"' && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some((start + rel_idx + ch.len_utf8(), out));
        }
        out.push(ch);
    }
    None
}

fn scan_arg_tokens(raw: &str) -> Vec<ArgTokenSpan> {
    let mut tokens = Vec::new();
    let len = raw.len();
    let mut pos = 0usize;

    while pos < len {
        while pos < len {
            let ch = raw[pos..].chars().next().unwrap();
            if ch.is_whitespace() {
                pos += ch.len_utf8();
            } else {
                break;
            }
        }
        if pos >= len {
            break;
        }

        let token_start = pos;
        let first_ch = raw[pos..].chars().next().unwrap();

        if (first_ch == '"' || first_ch == '\'')
            && let Some((end, unquoted)) = scan_quoted_segment(raw, pos)
        {
            tokens.push(ArgTokenSpan {
                start: token_start,
                end,
                key: None,
                value: unquoted,
            });
            pos = end;
            continue;
        }

        let mut scan = pos;
        let mut eq_pos = None;
        while scan < len {
            let ch = raw[scan..].chars().next().unwrap();
            if ch.is_whitespace() {
                break;
            }
            if ch == '=' && scan > pos {
                let candidate_key = &raw[pos..scan];
                if candidate_key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                {
                    eq_pos = Some(scan);
                }
                break;
            }
            scan += ch.len_utf8();
        }

        if let Some(eq) = eq_pos {
            let key = raw[pos..eq].to_string();
            let val_start = eq + 1;
            if val_start < len {
                let vch = raw[val_start..].chars().next().unwrap();
                if (vch == '"' || vch == '\'')
                    && let Some((end, unquoted)) = scan_quoted_segment(raw, val_start)
                {
                    tokens.push(ArgTokenSpan {
                        start: token_start,
                        end,
                        key: Some(key),
                        value: unquoted,
                    });
                    pos = end;
                    continue;
                }
            }
            let mut end = val_start;
            while end < len {
                let ch = raw[end..].chars().next().unwrap();
                if ch.is_whitespace() {
                    break;
                }
                end += ch.len_utf8();
            }
            tokens.push(ArgTokenSpan {
                start: token_start,
                end,
                key: Some(key),
                value: raw[val_start..end].to_string(),
            });
            pos = end;
            continue;
        }

        let mut end = pos;
        while end < len {
            let ch = raw[end..].chars().next().unwrap();
            if ch.is_whitespace() {
                break;
            }
            end += ch.len_utf8();
        }
        tokens.push(ArgTokenSpan {
            start: token_start,
            end,
            key: None,
            value: raw[token_start..end].to_string(),
        });
        pos = end;
    }

    tokens
}

fn excise_spans(raw: &str, consumed: &[(usize, usize)]) -> String {
    if consumed.is_empty() {
        return raw.trim().to_string();
    }
    let mut spans = consumed.to_vec();
    spans.sort_by_key(|(s, _)| *s);

    let mut out = String::with_capacity(raw.len());
    let mut cursor = 0usize;
    for (start, end) in spans {
        if start > cursor {
            out.push_str(&raw[cursor..start]);
        }
        cursor = end;
        if out.ends_with(' ')
            && let Some(rest) = raw.get(cursor..)
            && rest.starts_with(' ')
        {
            cursor += 1;
        }
    }
    if cursor < raw.len() {
        out.push_str(&raw[cursor..]);
    }
    out.trim().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkillInvocation {
    pub inputs: BTreeMap<String, Value>,
    pub remaining_prompt: String,
    pub raw_args: String,
}

pub fn resolve_skill_inputs(
    skill: &SkillDefinition,
    raw_args: &str,
    explicit_overrides: &[(String, String)],
) -> Result<ResolvedSkillInvocation, String> {
    let raw_trimmed = raw_args.trim().to_string();

    if skill.args.is_empty() {
        if let Some((k, _)) = explicit_overrides.first() {
            return Err(format!(
                "Skill '@{}' does not declare any typed arguments (unexpected argument '{k}').",
                skill.name
            ));
        }
        return Ok(ResolvedSkillInvocation {
            inputs: BTreeMap::new(),
            remaining_prompt: raw_trimmed.clone(),
            raw_args: raw_trimmed,
        });
    }

    let declared: HashMap<String, &SkillArgDef> = skill
        .args
        .iter()
        .map(|a| (a.name.to_ascii_lowercase(), a))
        .collect();

    let mut inputs: BTreeMap<String, Value> = BTreeMap::new();

    for (k, v) in explicit_overrides {
        let Some(&def) = declared.get(&k.trim().to_ascii_lowercase()) else {
            let valid: Vec<&str> = skill.args.iter().map(|a| a.name.as_str()).collect();
            return Err(format!(
                "Unknown argument '{k}' for skill '@{}'. Declared arguments: {}",
                skill.name,
                valid.join(", ")
            ));
        };
        let coerced = coerce_arg_value(def, v)?;
        inputs.insert(def.name.clone(), coerced);
    }

    let tokens = scan_arg_tokens(raw_args);
    let mut consumed_indices = HashSet::new();
    for (idx, tok) in tokens.iter().enumerate() {
        if tok.key.is_none() && tok.value == "--" {
            consumed_indices.insert(idx);
            break;
        }
        if let Some(ref key) = tok.key
            && let Some(&def) = declared.get(&key.to_ascii_lowercase())
        {
            if inputs.contains_key(&def.name) {
                break;
            }
            let coerced = coerce_arg_value(def, &tok.value)?;
            inputs.insert(def.name.clone(), coerced);
            consumed_indices.insert(idx);
            continue;
        }
        if let Some(def) = skill
            .args
            .iter()
            .find(|d| d.required && !inputs.contains_key(&d.name))
        {
            let positional_raw = if tok.key.is_some() {
                &raw_args[tok.start..tok.end]
            } else {
                &tok.value
            };
            let coerced = coerce_arg_value(def, positional_raw)?;
            inputs.insert(def.name.clone(), coerced);
            consumed_indices.insert(idx);
        } else {
            break;
        }
    }

    let mut missing = Vec::new();
    for def in &skill.args {
        if inputs.contains_key(&def.name) {
            continue;
        }
        if def.required {
            let desc = if def.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", def.description)
            };
            missing.push(format!("'{}' ({}){desc}", def.name, def.arg_type.as_str()));
        } else if let Some(ref default_val) = def.default {
            let valid = validate_arg_json_value(def, default_val)?;
            inputs.insert(def.name.clone(), valid);
        }
    }

    if !missing.is_empty() {
        return Err(format!(
            "Skill '@{}' is missing required argument(s): {}. Usage: @{} {}",
            skill.name,
            missing.join("; "),
            skill.trigger(),
            skill.args_usage_hint()
        ));
    }

    let consumed_spans: Vec<(usize, usize)> = consumed_indices
        .into_iter()
        .map(|i| (tokens[i].start, tokens[i].end))
        .collect();
    let remaining_prompt = excise_spans(raw_args, &consumed_spans);

    Ok(ResolvedSkillInvocation {
        inputs,
        remaining_prompt,
        raw_args: raw_trimmed,
    })
}

// ============================================================================
// Templating Engine (`{{var}}`, `{{#if var}}...{{else}}...{{/if}}`, `{{#unless var}}...{{/unless}}`)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateToken<'a> {
    Literal(&'a str),
    Var(&'a str),
    If(&'a str),
    Unless(&'a str),
    Else,
    EndIf,
    EndUnless,
}

fn is_valid_template_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn tokenize_template(src: &str) -> Vec<TemplateToken<'_>> {
    let mut out = Vec::new();
    let mut pos = 0usize;

    while let Some(rel_open) = src[pos..].find("{{") {
        let open = pos + rel_open;
        let Some(rel_close) = src[open + 2..].find("}}") else {
            break;
        };
        let close = open + 2 + rel_close;
        let inner = src[open + 2..close].trim();

        let parsed = if inner == "else" {
            Some(TemplateToken::Else)
        } else if inner == "/if" {
            Some(TemplateToken::EndIf)
        } else if inner == "/unless" {
            Some(TemplateToken::EndUnless)
        } else if let Some(var) = inner
            .strip_prefix("#if")
            .and_then(|r| r.strip_prefix(char::is_whitespace))
            .map(str::trim)
            && is_valid_template_ident(var)
        {
            Some(TemplateToken::If(var))
        } else if let Some(var) = inner
            .strip_prefix("#unless")
            .and_then(|r| r.strip_prefix(char::is_whitespace))
            .map(str::trim)
            && is_valid_template_ident(var)
        {
            Some(TemplateToken::Unless(var))
        } else if is_valid_template_ident(inner) {
            Some(TemplateToken::Var(inner))
        } else {
            None
        };

        if let Some(tok) = parsed {
            if open > pos {
                out.push(TemplateToken::Literal(&src[pos..open]));
            }
            out.push(tok);
            pos = close + 2;
        } else {
            out.push(TemplateToken::Literal(&src[pos..open + 1]));
            pos = open + 1;
        }
    }

    if pos < src.len() {
        out.push(TemplateToken::Literal(&src[pos..]));
    }
    out
}

pub struct TemplateContext<'a> {
    pub skill_name: &'a str,
    pub args: &'a str,
    pub raw_args: &'a str,
    pub last_response: Option<&'a str>,
    pub inputs: &'a BTreeMap<String, Value>,
}

impl<'a> TemplateContext<'a> {
    /// Resolves `var` against the typed skill inputs, falling back to a
    /// case-insensitive key match.
    fn input_value(&self, var: &str) -> Option<&Value> {
        self.inputs.get(var).or_else(|| {
            self.inputs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(var))
                .map(|(_, v)| v)
        })
    }

    fn lookup_str(&self, var: &str) -> Option<String> {
        match var {
            "args" => Some(self.args.to_string()),
            "raw_args" => Some(self.raw_args.to_string()),
            "skill.name" => Some(self.skill_name.to_string()),
            "last_response" => self.last_response.map(str::to_string),
            _ => self.input_value(var).map(format_arg_value),
        }
    }

    fn is_truthy(&self, var: &str) -> bool {
        match var {
            "args" => !self.args.trim().is_empty(),
            "raw_args" => !self.raw_args.trim().is_empty(),
            "skill.name" => !self.skill_name.trim().is_empty(),
            "last_response" => self.last_response.is_some_and(|s| !s.trim().is_empty()),
            _ => match self.input_value(var) {
                Some(Value::Bool(b)) => *b,
                Some(Value::String(s)) => !s.trim().is_empty(),
                Some(Value::Null) | None => false,
                Some(_) => true,
            },
        }
    }
}

/// Which block a rendered branch belongs to; determines the `else`/terminator
/// boundary that ends the branch body.
#[derive(Clone, Copy)]
enum Block {
    /// Top level: no boundary; stray `else`/`/if`/`/unless` tokens are skipped.
    Top,
    /// Inside a `{{#if ...}}` block (bounded by `else` / `{{/if}}`).
    If,
    /// Inside a `{{#unless ...}}` block (bounded by `else` / `{{/unless}}`).
    Unless,
}

impl Block {
    fn in_block(self) -> bool {
        matches!(self, Self::If | Self::Unless)
    }

    fn is_terminator(self, tok: &TemplateToken) -> bool {
        matches!(
            (self, tok),
            (Self::If, TemplateToken::EndIf) | (Self::Unless, TemplateToken::EndUnless)
        )
    }
}

/// Renders a `{{#if ...}}` / `{{#unless ...}}` block (primary branch, optional
/// `else` branch, then the closing terminator) starting at its opening token.
fn render_block(
    tokens: &[TemplateToken<'_>],
    idx: &mut usize,
    ctx: &TemplateContext<'_>,
    emit: bool,
    cond: bool,
    block: Block,
    out: &mut String,
) {
    // `idx` points at the `#if`/`#unless` token; advance past it.
    *idx += 1;
    // Render the primary branch up to the `else` or the block terminator.
    render_tokens(tokens, idx, ctx, emit && cond, block, out);
    // Render the `else` branch when one is present.
    if matches!(tokens.get(*idx), Some(TemplateToken::Else)) {
        *idx += 1;
        render_tokens(tokens, idx, ctx, emit && !cond, block, out);
    }
    // Consume the block terminator.
    if tokens.get(*idx).is_some_and(|t| block.is_terminator(t)) {
        *idx += 1;
    }
}

fn render_tokens(
    tokens: &[TemplateToken<'_>],
    idx: &mut usize,
    ctx: &TemplateContext<'_>,
    emit: bool,
    block: Block,
    out: &mut String,
) {
    while *idx < tokens.len() {
        match tokens[*idx] {
            TemplateToken::Literal(s) => {
                if emit {
                    out.push_str(s);
                }
                *idx += 1;
            }
            TemplateToken::Var(var) => {
                if emit && let Some(val) = ctx.lookup_str(var) {
                    out.push_str(&val);
                }
                *idx += 1;
            }
            TemplateToken::If(var) => {
                render_block(tokens, idx, ctx, emit, ctx.is_truthy(var), Block::If, out)
            }
            TemplateToken::Unless(var) => render_block(
                tokens,
                idx,
                ctx,
                emit,
                !ctx.is_truthy(var),
                Block::Unless,
                out,
            ),
            TemplateToken::Else => {
                if block.in_block() {
                    return;
                }
                *idx += 1;
            }
            TemplateToken::EndIf | TemplateToken::EndUnless => {
                if block.is_terminator(&tokens[*idx]) {
                    return;
                }
                *idx += 1;
            }
        }
    }
}

pub fn render_template(src: &str, ctx: &TemplateContext<'_>) -> String {
    if !src.contains("{{") {
        return src.to_string();
    }
    let tokens = tokenize_template(src);
    let mut out = String::with_capacity(src.len());
    let mut idx = 0usize;
    render_tokens(&tokens, &mut idx, ctx, true, Block::Top, &mut out);
    out
}

/// Validates template block nesting (`#if`/`#unless`/`else`/`/if`/`/unless`) and returns
/// `(referenced_variables, syntax_errors)`.
pub fn inspect_template(src: &str) -> (Vec<String>, Vec<String>) {
    let mut vars = Vec::new();
    let mut errors = Vec::new();
    if !src.contains("{{") {
        return (vars, errors);
    }

    let tokens = tokenize_template(src);
    let mut stack: Vec<(bool, bool, &str)> = Vec::new();

    for tok in tokens {
        match tok {
            TemplateToken::Literal(_) => {}
            TemplateToken::Var(v) => push_unique_string(&mut vars, v),
            TemplateToken::If(v) => {
                push_unique_string(&mut vars, v);
                stack.push((true, false, v));
            }
            TemplateToken::Unless(v) => {
                push_unique_string(&mut vars, v);
                stack.push((false, false, v));
            }
            TemplateToken::Else => match stack.last_mut() {
                Some((_, seen_else, _)) if !*seen_else => *seen_else = true,
                Some((_, _, v)) => {
                    errors.push(format!("Duplicate `{{{{else}}}}` inside block for '{v}'"));
                }
                None => {
                    errors.push("Unexpected `{{else}}` outside any `#if`/`#unless` block".into());
                }
            },
            TemplateToken::EndIf => match stack.pop() {
                Some((true, _, _)) => {}
                Some((false, _, v)) => {
                    errors.push(format!(
                        "Mismatched closing tag `{{{{/if}}}}` for `{{{{#unless {v}}}}}`"
                    ));
                }
                None => errors.push("Unexpected `{{/if}}` with no matching `{{#if}}`".into()),
            },
            TemplateToken::EndUnless => match stack.pop() {
                Some((false, _, _)) => {}
                Some((true, _, v)) => {
                    errors.push(format!(
                        "Mismatched closing tag `{{{{/unless}}}}` for `{{{{#if {v}}}}}`"
                    ));
                }
                None => {
                    errors.push("Unexpected `{{/unless}}` with no matching `{{#unless}}`".into());
                }
            },
        }
    }

    for (is_if, _, v) in stack.into_iter().rev() {
        let open = if is_if { "#if" } else { "#unless" };
        let close = if is_if { "/if" } else { "/unless" };
        errors.push(format!(
            "Unclosed template block `{{{{{open} {v}}}}}` (missing `{{{{{close}}}}}`)"
        ));
    }

    (vars, errors)
}

// ============================================================================
// Activation, Hooks & Prompt Composition
// ============================================================================

pub fn activate_skill_for_turn(
    skill_name: &str,
    raw_args: &str,
    explicit_overrides: &[(String, String)],
    turn: &mut crate::core::TurnConfig,
    config: &Config,
) -> Result<String, String> {
    let canonical_name = check_skill_activation(skill_name, turn.enable_tools, config)?;
    let skill = find_skill(config, &canonical_name)
        .ok_or_else(|| format!("Skill '@{canonical_name}' not found."))?;
    let invocation = resolve_skill_inputs(skill, raw_args, explicit_overrides)?;

    turn.active_skill = Some(canonical_name);
    turn.skill_args =
        (!invocation.remaining_prompt.is_empty()).then(|| invocation.remaining_prompt.clone());
    turn.raw_skill_args = (!invocation.raw_args.is_empty()).then_some(invocation.raw_args);
    turn.skill_inputs = invocation.inputs;

    Ok(if invocation.remaining_prompt.is_empty() {
        format!("@{}", skill.trigger())
    } else {
        invocation.remaining_prompt
    })
}

pub fn apply_at_skill_to_turn(
    raw_prompt: &str,
    turn: &mut crate::core::TurnConfig,
    config: &Config,
) -> Result<String, String> {
    if let Some((cmd_token, args_str)) = parse_at_command(raw_prompt) {
        let prompt = activate_skill_for_turn(cmd_token, args_str, &[], turn, config)?;
        Ok(if args_str.is_empty() {
            raw_prompt.trim().to_string()
        } else {
            prompt
        })
    } else {
        Ok(raw_prompt.to_string())
    }
}

/// Validates (and normalizes defaults for) a `TurnConfig` that already carries `active_skill`.
pub fn validate_turn_skill(
    turn: &mut crate::core::TurnConfig,
    config: &Config,
) -> Result<(), String> {
    let Some(skill_name) = turn.active_skill.clone() else {
        return Ok(());
    };
    let canonical_name = check_skill_activation(&skill_name, turn.enable_tools, config)?;
    let skill = find_skill(config, &canonical_name)
        .ok_or_else(|| format!("Skill '@{canonical_name}' not found."))?;
    turn.active_skill = Some(canonical_name);

    if skill.args.is_empty() {
        if let Some(k) = turn.skill_inputs.keys().next() {
            return Err(format!(
                "Skill '@{}' does not declare any typed arguments (unexpected argument '{k}').",
                skill.name
            ));
        }
        return Ok(());
    }

    if turn.skill_inputs.is_empty() && turn.raw_skill_args.is_none() {
        let raw_args = turn.skill_args.as_deref().unwrap_or("");
        let resolved = resolve_skill_inputs(skill, raw_args, &[])?;
        turn.skill_inputs = resolved.inputs;
        turn.raw_skill_args = (!resolved.raw_args.is_empty()).then_some(resolved.raw_args);
        turn.skill_args =
            (!resolved.remaining_prompt.is_empty()).then_some(resolved.remaining_prompt);
    } else {
        let overrides: Vec<(String, String)> = turn
            .skill_inputs
            .iter()
            .map(|(k, v)| (k.clone(), format_arg_value(v)))
            .collect();
        let resolved = resolve_skill_inputs(skill, "", &overrides)?;
        turn.skill_inputs = resolved.inputs;
    }

    Ok(())
}

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
    (!inner.contains("{{") && !inner.contains("}}") && is_valid_template_ident(inner))
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
            Some(Value::String(render_template(s, ctx)))
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
    let skill = active_skill.and_then(|name| find_skill(config, name))?;
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
    let Some(skill) = find_skill(config, clean_name) else {
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

pub fn list_skill_reference_paths(skill: &SkillDefinition) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(
        cur: &Path,
        canon_root: &Path,
        depth: usize,
        visited: &mut HashSet<PathBuf>,
        out: &mut Vec<String>,
    ) {
        const MAX_DEPTH: usize = 16;
        if depth > MAX_DEPTH {
            return;
        }
        let Ok(canon_cur) = dunce::canonicalize(cur) else {
            return;
        };
        if !canon_cur.starts_with(canon_root) || !visited.insert(canon_cur) {
            return;
        }
        let Ok(rd) = fs::read_dir(cur) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, canon_root, depth + 1, visited, out);
            } else if p.file_name().and_then(|n| n.to_str()) != Some("SKILL.md")
                && let Ok(canon_p) = dunce::canonicalize(&p)
                && canon_p.starts_with(canon_root)
            {
                let formatted = format_tilde_path(&p);
                if !out.contains(&formatted) {
                    out.push(formatted);
                }
            }
        }
    }
    for dir in skill.all_dirs().filter(|d| d.is_dir()) {
        let canon_root = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let mut visited = HashSet::new();
        walk(dir, &canon_root, 0, &mut visited, &mut out);
    }
    out.sort();
    out
}

pub fn compose_system_prompt(
    base_system_prompt: &str,
    active_skill: Option<&str>,
    skill_args: Option<&str>,
    raw_skill_args: Option<&str>,
    skill_inputs: &BTreeMap<String, Value>,
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
        if !refs.is_empty() {
            let _ = writeln!(
                block,
                "Reference files (read via `read_file`): {}",
                refs.join(", ")
            );
        }

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
        } else if refs.is_empty() && args.is_empty() && unreferenced_inputs.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn skill_composition_extends_merges_tools_hooks_args_refs_and_prompts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        fs::create_dir_all(dir.join("base-rust/references")).unwrap();
        fs::create_dir_all(dir.join("coder")).unwrap();
        fs::create_dir_all(dir.join("readonly-rust")).unwrap();

        fs::write(dir.join("base-rust/references/rust-rules.md"), "# Rules").unwrap();
        fs::write(
            dir.join("base-rust/SKILL.md"),
            r#"+++
name = "base-rust"
abstract = true
tools = ["repo_map", "search", "read_file", "workspace_snapshot", "cargo"]
references = ["references/rust-rules.md"]

[[hooks]]
trigger = "on_activate"
tool = "workspace_snapshot"
args = { tree_only = false }

[[hooks]]
trigger = "on_activate"
tool = "repo_map"
args = { action = "map", max_tokens = 8000 }

[[args]]
name = "target"
type = "path"
default = "src"
+++
Always follow idiomatic Rust conventions."#,
        )
        .unwrap();

        fs::write(
            dir.join("coder/SKILL.md"),
            r#"+++
name = "coder"
command = "coder"
extends = "base-rust"
tools = ["edit_file", "create_file", "cargo"]
+++
Focus on clean changes."#,
        )
        .unwrap();

        fs::write(
            dir.join("readonly-rust/SKILL.md"),
            r#"+++
name = "readonly-rust"
extends = "base-rust"
tools = ["!cargo", "git"]
+++
Review only.

{{super}}"#,
        )
        .unwrap();

        let skills = discover_skills(&dir.to_string_lossy(), None);
        let base = skills.iter().find(|s| s.name == "base-rust").unwrap();
        assert!(base.is_abstract);

        let coder = skills.iter().find(|s| s.name == "coder").unwrap();
        assert!(!coder.is_abstract);
        assert!(coder.resolution_errors.is_empty());
        assert_eq!(
            coder.tools,
            vec![
                "repo_map",
                "search",
                "read_file",
                "workspace_snapshot",
                "cargo",
                "edit_file",
                "create_file"
            ]
        );
        assert_eq!(coder.hooks.len(), 2);
        assert_eq!(coder.args.len(), 1);
        assert_eq!(coder.args[0].name, "target");
        assert_eq!(
            coder.prompt,
            "Always follow idiomatic Rust conventions.\n\nFocus on clean changes."
        );
        assert_eq!(list_skill_reference_paths(coder).len(), 1);

        let ro = skills.iter().find(|s| s.name == "readonly-rust").unwrap();
        assert!(!ro.tools.iter().any(|t| t == "cargo"));
        assert!(ro.tools.iter().any(|t| t == "git"));
        assert_eq!(
            ro.prompt,
            "Review only.\n\nAlways follow idiomatic Rust conventions."
        );

        let cfg = Config {
            skills,
            ..Config::default()
        };
        assert!(check_skill_activation("base-rust", true, &cfg).is_err());
        assert!(check_skill_activation("coder", true, &cfg).is_ok());
    }

    #[test]
    fn skill_composition_detects_cycles_and_missing_parents() {
        let a = parse_skill_markdown(
            "+++\nname = \"a\"\nextends = \"b\"\n+++\nA",
            Path::new("/tmp/a"),
        )
        .unwrap();
        let b = parse_skill_markdown(
            "+++\nname = \"b\"\nextends = \"a\"\n+++\nB",
            Path::new("/tmp/b"),
        )
        .unwrap();
        let c = parse_skill_markdown(
            "+++\nname = \"c\"\nextends = \"nonexistent\"\n+++\nC",
            Path::new("/tmp/c"),
        )
        .unwrap();

        let resolved = resolve_skills(vec![a, b, c]);
        let ra = resolved.iter().find(|s| s.name == "a").unwrap();
        assert!(
            ra.resolution_errors
                .iter()
                .any(|e| e.contains("Circular skill inheritance"))
        );

        let rc = resolved.iter().find(|s| s.name == "c").unwrap();
        assert!(
            rc.resolution_errors
                .iter()
                .any(|e| e.contains("unknown parent skill 'nonexistent'"))
        );
    }

    #[test]
    fn typed_args_parsing_validation_and_templating() {
        let raw = r#"+++
name = "audit"
command = "audit"
tools = ["repo_map", "read_file"]

[[args]]
name = "target"
type = "path"
required = true
description = "Target file or directory"

[[args]]
name = "severity"
type = "enum"
choices = ["low", "medium", "high"]
default = "medium"

[[args]]
name = "max_tokens"
type = "int"
default = 4000

[[args]]
name = "strict"
type = "bool"
default = false

[[hooks]]
trigger = "on_activate"
tool = "repo_map"
args = { action = "map", target_path = "{{target}}", max_tokens = "{{max_tokens}}" }
+++
Inspect `{{target}}` (min severity: {{severity}}).
{{#if strict}}Strict mode enabled.{{else}}Standard mode.{{/if}}
{{#if args}}Extra notes: {{args}}{{/if}}"#;

        let skill = parse_skill_markdown(raw, Path::new("/tmp/audit")).unwrap();

        let err = resolve_skill_inputs(&skill, "", &[]).unwrap_err();
        assert!(err.contains("missing required argument"));
        assert!(err.contains("target"));

        let err = resolve_skill_inputs(&skill, "src/main.rs severity=bogus", &[]).unwrap_err();
        assert!(err.contains("Expected one of: low, medium, high"));

        let err = resolve_skill_inputs(&skill, "src/main.rs max_tokens=abc", &[]).unwrap_err();
        assert!(err.contains("expected an integer"));

        let inv = resolve_skill_inputs(
            &skill,
            "src/core strict=true severity=HIGH Don't miss panics\nCheck line 2",
            &[],
        )
        .unwrap();
        assert_eq!(inv.inputs["target"], json!("src/core"));
        assert_eq!(inv.inputs["severity"], json!("high"));
        assert_eq!(inv.inputs["max_tokens"], json!(4000));
        assert_eq!(inv.inputs["strict"], json!(true));
        assert_eq!(inv.remaining_prompt, "Don't miss panics\nCheck line 2");

        let cfg = Config {
            skills: vec![skill],
            ..Config::default()
        };
        let sys = compose_system_prompt(
            "",
            Some("audit"),
            Some(&inv.remaining_prompt),
            Some(&inv.raw_args),
            &inv.inputs,
            &cfg,
        );
        assert!(sys.contains("Inspect `src/core` (min severity: high)."));
        assert!(sys.contains("Strict mode enabled."));
        assert!(!sys.contains("Standard mode."));
        assert!(sys.contains("Extra notes: Don't miss panics\nCheck line 2"));
        assert!(!sys.contains("User arguments / instructions:"));

        let mut executed = HashSet::new();
        let hooks = collect_due_hooks(
            &cfg,
            Some("audit"),
            Some(&inv.remaining_prompt),
            Some(&inv.raw_args),
            &inv.inputs,
            &mut executed,
            &[],
        );
        assert_eq!(hooks.len(), 1);
        assert_eq!(
            hooks[0].1.function.arguments["target_path"],
            json!("src/core")
        );
        assert_eq!(hooks[0].1.function.arguments["max_tokens"], json!(4000));
        assert!(
            hooks[0].0.contains("src/core"),
            "hook summary should show resolved typed arg: {}",
            hooks[0].0
        );

        // Changing a typed arg in the same session must re-run `on_activate` even when turn 1's hook is in `messages`.
        let prev_hook_msg = ChatMessage::new_hook(hooks[0].0.clone(), "ok", false);
        let inv2 = resolve_skill_inputs(&cfg.skills[0], "src/ui", &[]).unwrap();
        let hooks2 = collect_due_hooks(
            &cfg,
            Some("audit"),
            Some(&inv2.remaining_prompt),
            Some(&inv2.raw_args),
            &inv2.inputs,
            &mut executed,
            &[prev_hook_msg],
        );
        assert_eq!(hooks2.len(), 1);
        assert_eq!(
            hooks2[0].1.function.arguments["target_path"],
            json!("src/ui")
        );
    }

    #[test]
    fn typed_args_do_not_hijack_prose_and_respect_double_dash_and_raw_args() {
        let raw = r#"+++
name = "review"
command = "review"
tools = ["read_file"]

[[args]]
name = "target"
type = "string"
required = false

[[args]]
name = "focus"
type = "enum"
choices = ["all", "security", "performance"]
default = "all"

[[args]]
name = "max_tokens"
type = "int"
required = false

[[hooks]]
trigger = "on_activate"
tool = "repo_map"
args = { action = "map", target_path = "{{target}}", max_tokens = "{{max_tokens}}" }
+++
Raw={{raw_args}} | Args={{args}} | Focus={{focus}}"#;

        let skill = parse_skill_markdown(raw, Path::new("/tmp/review")).unwrap();

        // Prose containing `focus=invalid` must not error or mutate the prompt.
        let inv =
            resolve_skill_inputs(&skill, "Check why focus=invalid panics in cli.rs", &[]).unwrap();
        assert_eq!(inv.inputs["focus"], json!("all"));
        assert!(!inv.inputs.contains_key("target"));
        assert_eq!(
            inv.remaining_prompt,
            "Check why focus=invalid panics in cli.rs"
        );

        // Leading arg is parsed, while later `focus=security` in prose is preserved verbatim.
        let inv2 = resolve_skill_inputs(
            &skill,
            "focus=performance Compare focus=all vs focus=security",
            &[],
        )
        .unwrap();
        assert_eq!(inv2.inputs["focus"], json!("performance"));
        assert_eq!(inv2.remaining_prompt, "Compare focus=all vs focus=security");
        assert_eq!(
            inv2.raw_args,
            "focus=performance Compare focus=all vs focus=security"
        );

        // Explicit `--arg` override prevents re-parsing/erroring on `focus=broken` in `-p`.
        let inv3 = resolve_skill_inputs(
            &skill,
            "focus=broken is the failing input",
            &[("focus".into(), "security".into())],
        )
        .unwrap();
        assert_eq!(inv3.inputs["focus"], json!("security"));
        assert_eq!(inv3.remaining_prompt, "focus=broken is the failing input");

        // `--` stops argument parsing explicitly.
        let inv4 = resolve_skill_inputs(&skill, "-- focus=security", &[]).unwrap();
        assert_eq!(inv4.inputs["focus"], json!("all"));
        assert_eq!(inv4.remaining_prompt, "focus=security");

        // Absent optional hook args (`target`, `max_tokens`) are omitted rather than turned into `""`.
        let cfg = Config {
            skills: vec![skill],
            ..Config::default()
        };
        let mut executed = HashSet::new();
        let hooks = collect_due_hooks(
            &cfg,
            Some("review"),
            Some(&inv2.remaining_prompt),
            Some(&inv2.raw_args),
            &inv2.inputs,
            &mut executed,
            &[],
        );
        assert_eq!(hooks.len(), 1);
        let hook_obj = hooks[0].1.function.arguments.as_object().unwrap();
        assert_eq!(hook_obj.get("action"), Some(&json!("map")));
        assert!(!hook_obj.contains_key("target_path"));
        assert!(!hook_obj.contains_key("max_tokens"));

        // `{{raw_args}}` retains un-excised raw arguments while `{{args}}` has remaining prompt.
        let sys = compose_system_prompt(
            "",
            Some("review"),
            Some(&inv2.remaining_prompt),
            Some(&inv2.raw_args),
            &inv2.inputs,
            &cfg,
        );
        assert!(sys.contains(
            "Raw=focus=performance Compare focus=all vs focus=security | Args=Compare focus=all vs focus=security | Focus=performance"
        ));
    }

    #[test]
    fn required_arg_is_enforced_even_if_default_is_set_and_star_negation_and_super_spaces() {
        let base = parse_skill_markdown(
            "+++\nname = \"base-all\"\nabstract = true\ntools = [\"*\"]\n+++\nBase prompt.",
            Path::new("/tmp/base-all"),
        )
        .unwrap();
        let child = parse_skill_markdown(
            r#"+++
name = "safe-child"
extends = "base-all"
tools = ["!edit_file"]

[[args]]
name = "item"
type = "string"
required = true
default = "fallback"
+++
Child intro.
{{ super }}"#,
            Path::new("/tmp/safe-child"),
        )
        .unwrap();

        let resolved = resolve_skills(vec![base, child]);
        let sc = resolved.iter().find(|s| s.name == "safe-child").unwrap();
        assert!(!sc.allows_tool("edit_file"));
        assert!(sc.allows_tool("read_file"));
        assert_eq!(sc.prompt, "Child intro.\nBase prompt.");

        // Disabling an unrelated tool in config must not block activating a `*` + `!tool` child.
        let mut cfg = Config {
            skills: resolved,
            ..Config::default()
        };
        cfg.app.enabled_tools.retain(|t| t != "dotfiles");
        assert!(check_skill_activation("safe-child", true, &cfg).is_ok());

        let sc = cfg.skills.iter().find(|s| s.name == "safe-child").unwrap();
        let err = resolve_skill_inputs(sc, "", &[]).unwrap_err();
        assert!(err.contains("missing required argument"));
    }

    #[test]
    fn negating_all_inherited_tools_denies_all_tools() {
        let base = parse_skill_markdown(
            "+++\nname = \"base-cargo\"\nabstract = true\ntools = [\"cargo\"]\n+++\nBase.",
            Path::new("/tmp/base-cargo"),
        )
        .unwrap();
        let child = parse_skill_markdown(
            "+++\nname = \"no-tools\"\nextends = \"base-cargo\"\ntools = [\"!cargo\"]\n+++\nChild.",
            Path::new("/tmp/no-tools"),
        )
        .unwrap();

        let resolved = resolve_skills(vec![base, child]);
        let nt = resolved.iter().find(|s| s.name == "no-tools").unwrap();
        assert!(nt.tools.is_empty());
        assert!(nt.tools_restricted);
        assert!(!nt.allows_tool("cargo"));
        assert!(!nt.allows_tool("read_file"));
    }

    #[test]
    fn positional_required_arg_preserves_undeclared_key_equals_value() {
        let skill = parse_skill_markdown(
            r#"+++
name = "filter"
[[args]]
name = "query"
type = "string"
required = true
+++
Filter by {{query}}."#,
            Path::new("/tmp/filter"),
        )
        .unwrap();

        let inv = resolve_skill_inputs(&skill, "RUST_LOG=debug check startup", &[]).unwrap();
        assert_eq!(inv.inputs["query"], json!("RUST_LOG=debug"));
        assert_eq!(inv.remaining_prompt, "check startup");
    }

    #[test]
    fn template_engine_preserves_non_template_braces() {
        let inputs = BTreeMap::new();
        let ctx = TemplateContext {
            skill_name: "demo",
            args: "hi",
            raw_args: "hi",
            last_response: None,
            inputs: &inputs,
        };
        let src = r#"Rust format!("{{}}") and JSON {{"a": 1}} and {{#if_not_directive}} and {{args}} and {{{args}}}."#;
        let rendered = render_template(src, &ctx);
        assert_eq!(
            rendered,
            r#"Rust format!("{{}}") and JSON {{"a": 1}} and {{#if_not_directive}} and hi and {hi}."#
        );
        let (vars, errs) = inspect_template(src);
        assert_eq!(vars, vec!["args"]);
        assert!(errs.is_empty());

        // `{{{super}}}` should also resolve the inner `{{super}}` while keeping surrounding braces.
        assert_eq!(
            replace_super_placeholder("Wrap {{{super}}}", "BASE"),
            Some("Wrap {BASE}".to_string())
        );
    }

    #[test]
    fn template_conditionals_render_branches_and_nesting() {
        let inputs: BTreeMap<String, Value> = [("on".into(), json!(true))].into_iter().collect();
        let ctx = TemplateContext {
            skill_name: "t",
            args: "x",
            raw_args: "x",
            last_response: None,
            inputs: &inputs,
        };
        // `#if` truthy: primary branch, `else` skipped.
        assert_eq!(
            render_template("{{#if on}}yes{{else}}no{{/if}}", &ctx),
            "yes"
        );
        // `#unless` over a truthy input: `else` branch wins.
        assert_eq!(
            render_template("{{#unless on}}a{{else}}b{{/unless}}", &ctx),
            "b"
        );
        // Absent input is falsy: `#if` falls through to `else`.
        assert_eq!(render_template("{{#if off}}a{{else}}b{{/if}}", &ctx), "b");
        // Nested same-direction block: both primary branches emit, outer `else` skipped.
        assert_eq!(
            render_template("{{#if on}}A{{#if on}}B{{/if}}C{{else}}D{{/if}}", &ctx),
            "ABC"
        );
        // Nested block inside an `else` branch still resolves its own condition.
        assert_eq!(
            render_template(
                "{{#if off}}a{{else}}E{{#if on}}F{{else}}G{{/if}}{{/if}}",
                &ctx
            ),
            "EF"
        );
        // Stray cross-terminator tokens are skipped, not treated as boundaries.
        assert_eq!(
            render_template("{{#if on}}1{{/unless}}2{{/if}}", &ctx),
            "12"
        );
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
        let empty = BTreeMap::new();

        let mut executed = HashSet::new();
        let pre_turn_1 =
            collect_due_hooks(&cfg, Some("runner"), None, None, &empty, &mut executed, &[]);
        assert_eq!(pre_turn_1.len(), 2);
        assert_eq!(pre_turn_1[0].1.function.name, "cargo");
        assert_eq!(pre_turn_1[1].1.function.name, "cargo");

        let pre_turn_2 =
            collect_due_hooks(&cfg, Some("runner"), None, None, &empty, &mut executed, &[]);
        assert_eq!(pre_turn_2.len(), 1);
        assert_eq!(pre_turn_2[0].0, "cargo:check");

        let after_turn = collect_after_turn_hooks(
            &cfg,
            Some("runner"),
            None,
            None,
            &empty,
            "All checks passed",
        );
        assert_eq!(after_turn.len(), 1);
        assert_eq!(after_turn[0].1.function.name, "notify");
        assert_eq!(after_turn[0].0, "notify (summary=Turn finished)");

        // An `after_turn` hook message (which has `include_in_context = false`)
        // should not suppress a subsequent `on_activate` hook with the same summary.
        let mut after_msg = ChatMessage::new_hook("cargo:metadata", "ok", false);
        after_msg.include_in_context = false;
        let mut fresh_executed = HashSet::new();
        let due_with_after_msg = collect_due_hooks(
            &cfg,
            Some("runner"),
            None,
            None,
            &empty,
            &mut fresh_executed,
            &[after_msg],
        );
        assert_eq!(due_with_after_msg.len(), 2);
        assert_eq!(due_with_after_msg[0].0, "cargo:metadata");
    }

    #[test]
    fn hooks_interpolate_last_response_in_string_args() {
        let raw = r#"+++
name = "notifier"
command = "notifier"
tools = ["notify", "create_file"]

[[hooks]]
trigger = "every_turn"
tool = "create_file"
args = { file_path = "prev.txt", content = "Prev: {{last_response}}" }

[[hooks]]
trigger = "after_turn"
tool = "notify"
args = { summary = "Turn finished", body = "{{last_response}}", nested = ["a", "{{last_response}}"] }
+++
Notify with last response."#;

        let skill = parse_skill_markdown(raw, Path::new("/tmp/notifier")).unwrap();
        let cfg = Config {
            skills: vec![skill],
            ..Config::default()
        };
        let empty = BTreeMap::new();

        // `after_turn` interpolates the just-completed response while preserving the clean template summary.
        let after = collect_after_turn_hooks(
            &cfg,
            Some("notifier"),
            None,
            None,
            &empty,
            "  Build succeeded.\n",
        );
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].0,
            "notify (body={{last_response}}, nested=[\"a\",\"{{last_response}}\"], summary=Turn finished)"
        );
        assert_eq!(after[0].1.function.arguments["summary"], "Turn finished");
        assert_eq!(after[0].1.function.arguments["body"], "Build succeeded.");
        assert_eq!(
            after[0].1.function.arguments["nested"],
            serde_json::json!(["a", "Build succeeded."])
        );

        // `every_turn` pulls the most recent non-empty assistant message (or "" if none).
        let mut executed = HashSet::new();
        let pre_empty = collect_due_hooks(
            &cfg,
            Some("notifier"),
            None,
            None,
            &empty,
            &mut executed,
            &[],
        );
        assert_eq!(pre_empty.len(), 1);
        assert_eq!(pre_empty[0].1.function.arguments["content"], "Prev: ");

        let msgs = vec![
            ChatMessage::new("user", "hi"),
            ChatMessage::new("assistant", "First reply"),
            ChatMessage::new("assistant", "   "),
            ChatMessage::new("user", "next"),
        ];
        let pre_with_prev = collect_due_hooks(
            &cfg,
            Some("notifier"),
            None,
            None,
            &empty,
            &mut executed,
            &msgs,
        );
        assert_eq!(
            pre_with_prev[0].1.function.arguments["content"],
            "Prev: First reply"
        );
    }
}
