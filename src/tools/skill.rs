use super::{
    ALL_TOOL_NAMES, BlockingTool, ToolContext,
    utils::{
        edit::{EditHunk, apply_hunks_to_str},
        fs::{atomic_write_file, read_file_contents},
        path::{
            ensure_ancestor_within_root, ensure_parent_dir, ensure_within_root,
            has_git_dir_component, validate_relative_path,
        },
    },
};
use crate::core::{
    config::{expand_tilde, format_tilde_path, workspace_config_dir_for},
    skills::{
        HookTrigger, SkillDefinition, SkillHook, format_hook_summary, list_skill_reference_paths,
        parse_skill_markdown, scan_skills_dir, split_toml_frontmatter,
    },
};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

pub struct Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillAction {
    /// List available skills (across workspace and global skill directories) with their trigger commands, scopes, tools, descriptions, and reference files.
    List,
    /// Read a skill's `SKILL.md` definition (or a reference file inside the skill directory) with optional 1-based `[start_line, end_line]`.
    Read,
    /// Create a new skill (`SKILL.md` with `+++` TOML frontmatter and markdown prompt) or create a reference file inside a skill directory.
    Create,
    /// Modify an existing skill by updating its frontmatter fields (`description`, `command`, `tools`, `hooks`), replacing its `prompt`/`content`, or applying disjoint search/replace `edits` hunks to `SKILL.md` or a reference file.
    #[serde(alias = "edit", alias = "update")]
    Modify,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    /// Default: searches workspace skills first then global skills (for `list`/`read`/`modify`), and creates new skills in the workspace skills directory (`create`).
    #[default]
    Auto,
    /// Workspace skills directory (`~/.config/archon/workspaces/<workspace>/skills`).
    Workspace,
    /// Global user skills directory (`~/.config/archon/skills`).
    Global,
}

#[derive(Deserialize, JsonSchema)]
pub struct SkillArgs {
    /// The skill operation to perform: `list`, `read`, `create`, or `modify`.
    pub action: SkillAction,
    /// Skill name or `@command` trigger (required for `read`, `create`, and `modify`; optional filter for `list`).
    #[serde(default)]
    pub name: String,
    /// Target skill scope: `auto` (default), `workspace`, or `global`.
    #[serde(default)]
    pub scope: SkillScope,
    /// Optional short description of the skill (for `create` or `modify` frontmatter).
    #[serde(default)]
    pub description: Option<String>,
    /// Optional `@command` trigger shorthand without leading `@` (for `create` or `modify` frontmatter; defaults to `name` on `create`).
    #[serde(default)]
    pub command: Option<String>,
    /// Optional list of tool names enabled when this skill is active (for `create` or `modify` frontmatter, e.g. `["cargo", "git"]`).
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// Optional list of tool hooks executed automatically when this skill is used (for `create` or `modify` frontmatter, e.g. `[{"trigger": "on_activate", "tool": "workspace_snapshot", "args": {"tree_only": true}}]`; string values in `args` may reference `{{last_response}}` for the previous assistant response).
    #[serde(default)]
    pub hooks: Option<Vec<SkillHook>>,
    /// Optional markdown prompt body for `SKILL.md` (without `+++` frontmatter) when `action` is `create` or `modify`.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Optional full file content when `action` is `create` or `modify` (can be a complete `SKILL.md` with `+++` frontmatter or content for a reference `file`).
    #[serde(default)]
    pub content: Option<String>,
    /// Disjoint search/replace hunks when `action` is `modify` (applied to `SKILL.md` or `file`).
    #[serde(default)]
    pub edits: Vec<EditHunk>,
    /// Optional relative file path inside the skill's directory (defaults to `"SKILL.md"`; e.g. `"references/playbook.md"` to read, create, or modify a skill reference file).
    #[serde(default)]
    pub file: String,
    /// Optional 1-based starting line number (inclusive) when `action` is `read`.
    #[serde(default)]
    pub start_line: Option<usize>,
    /// Optional 1-based ending line number (inclusive) when `action` is `read`.
    #[serde(default)]
    pub end_line: Option<usize>,
}

impl SkillArgs {
    fn has_frontmatter_overrides(&self) -> bool {
        self.description.is_some()
            || self.command.is_some()
            || self.tools.is_some()
            || self.hooks.is_some()
    }
}

struct ResolvedSkill {
    definition: SkillDefinition,
    scope_label: &'static str,
    skill_dir: PathBuf,
    skill_md_path: PathBuf,
}

fn skill_roots(ctx: &ToolContext) -> (PathBuf, PathBuf) {
    let global_dir = expand_tilde(&ctx.config.app.skills_dir);
    let ws_dir = ctx
        .config
        .workspace_skills_dir
        .clone()
        .unwrap_or_else(|| workspace_config_dir_for(&ctx.root).join("skills"));
    (ws_dir, global_dir)
}

fn create_root_dir(scope: SkillScope, ctx: &ToolContext) -> PathBuf {
    let (ws_dir, global_dir) = skill_roots(ctx);
    match scope {
        SkillScope::Global => global_dir,
        SkillScope::Auto | SkillScope::Workspace => ws_dir,
    }
}

fn clean_skill_query(raw: &str) -> &str {
    raw.trim().trim_start_matches(['@', '/']).trim()
}

fn validate_skill_slug(raw: &str, field: &str) -> Result<String> {
    let cleaned = clean_skill_query(raw);
    if cleaned.is_empty() {
        bail!("'{field}' is required and cannot be empty");
    }
    if cleaned.starts_with(['.', '-']) {
        bail!("Invalid {field} '{cleaned}': cannot start with '.' or '-'");
    }
    if !cleaned
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        bail!("Invalid {field} '{cleaned}': only ASCII letters, digits, '-', and '_' are allowed");
    }
    Ok(cleaned.to_string())
}

fn is_skill_md_target(raw_file: &str) -> bool {
    let trimmed = raw_file.trim();
    trimmed.is_empty() || trimmed.eq_ignore_ascii_case("SKILL.md")
}

fn validate_reference_subpath(raw_file: &str) -> Result<&Path> {
    let rel = validate_relative_path(raw_file)?;
    if has_git_dir_component(rel) {
        bail!("Security violation: skill reference file path cannot contain '.git'");
    }
    Ok(rel)
}

fn resolve_existing_reference(
    resolved: &ResolvedSkill,
    raw_file: &str,
    create_hint: bool,
) -> Result<(PathBuf, String)> {
    let rel = validate_reference_subpath(raw_file)?;
    let candidate = resolved.skill_dir.join(rel);
    if !candidate.exists() {
        let hint = if create_hint {
            ". Use action: \"create\" to create it."
        } else {
            ""
        };
        bail!(
            "Reference file '{}' does not exist in skill '{}'{hint}",
            rel.display(),
            resolved.definition.name
        );
    }
    let canon_skill_dir =
        dunce::canonicalize(&resolved.skill_dir).unwrap_or_else(|_| resolved.skill_dir.clone());
    let canon = ensure_within_root(&candidate, &canon_skill_dir, &rel.display().to_string())?;
    let display = format_tilde_path(&canon);
    Ok((canon, display))
}

fn locate_skill_in_dir(
    root: &Path,
    query: &str,
    scope_label: &'static str,
) -> Option<ResolvedSkill> {
    for skill in scan_skills_dir(root) {
        let dir_path = skill.dir_path.clone()?;
        let dir_name = dir_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let matches = skill.name.eq_ignore_ascii_case(query)
            || (!skill.command.is_empty() && skill.command.eq_ignore_ascii_case(query))
            || dir_name.eq_ignore_ascii_case(query);
        if !matches {
            continue;
        }

        let dir_md = dir_path.join("SKILL.md");
        let flat_md = dir_path.with_extension("md");
        let skill_md_path = if dir_md.is_file() || !flat_md.is_file() {
            dir_md
        } else {
            flat_md
        };

        return Some(ResolvedSkill {
            definition: skill,
            scope_label,
            skill_dir: dir_path,
            skill_md_path,
        });
    }
    None
}

fn resolve_existing_skill(
    raw_name: &str,
    scope: SkillScope,
    ctx: &ToolContext,
) -> Result<ResolvedSkill> {
    let query = clean_skill_query(raw_name);
    if query.is_empty() {
        bail!("'name' is required to identify the skill");
    }

    let (ws_dir, global_dir) = skill_roots(ctx);
    let found = match scope {
        SkillScope::Workspace => locate_skill_in_dir(&ws_dir, query, "workspace"),
        SkillScope::Global => locate_skill_in_dir(&global_dir, query, "global"),
        SkillScope::Auto => locate_skill_in_dir(&ws_dir, query, "workspace")
            .or_else(|| locate_skill_in_dir(&global_dir, query, "global")),
    };

    found.with_context(|| match scope {
        SkillScope::Workspace => format!("Skill '{query}' not found in workspace skills directory"),
        SkillScope::Global => format!("Skill '{query}' not found in global skills directory"),
        SkillScope::Auto => format!("Skill '{query}' not found in workspace or global skills"),
    })
}

fn format_skill_document(
    name: &str,
    description: &str,
    command: &str,
    tools: &[String],
    hooks: &[SkillHook],
    extra: Option<&toml::Table>,
    prompt: &str,
) -> Result<String> {
    let mut table = toml::Table::new();
    table.insert("name".into(), toml::Value::String(name.trim().to_string()));
    table.insert(
        "description".into(),
        toml::Value::String(description.trim().to_string()),
    );
    let clean_cmd = clean_skill_query(command);
    if !clean_cmd.is_empty() {
        table.insert("command".into(), toml::Value::String(clean_cmd.to_string()));
    }
    let clean_tools: Vec<toml::Value> = tools
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| toml::Value::String(t.to_string()))
        .collect();
    table.insert("tools".into(), toml::Value::Array(clean_tools));

    let clean_hooks: Vec<toml::Value> = hooks
        .iter()
        .filter(|h| !h.tool.trim().is_empty())
        .map(|h| {
            let mut ht = toml::Table::new();
            if h.trigger != HookTrigger::OnActivate {
                ht.insert(
                    "trigger".into(),
                    toml::Value::String(h.trigger.as_str().to_string()),
                );
            }
            ht.insert(
                "tool".into(),
                toml::Value::String(h.tool.trim().to_string()),
            );
            if let Some(obj) = h.args.as_object().filter(|o| !o.is_empty())
                && let Ok(toml_args) = toml::Value::try_from(obj.clone())
            {
                ht.insert("args".into(), toml_args);
            }
            toml::Value::Table(ht)
        })
        .collect();
    if !clean_hooks.is_empty() {
        table.insert("hooks".into(), toml::Value::Array(clean_hooks));
    }

    if let Some(ext) = extra {
        for (k, v) in ext {
            if !matches!(
                k.as_str(),
                "name" | "description" | "command" | "tools" | "hooks"
            ) {
                table.insert(k.clone(), v.clone());
            }
        }
    }

    let fm_str = toml::to_string_pretty(&table).context("Failed to serialize skill frontmatter")?;
    let body = prompt.trim();
    if body.is_empty() {
        Ok(format!("+++\n{fm_str}+++\n"))
    } else {
        Ok(format!("+++\n{fm_str}+++\n{body}\n"))
    }
}

fn validate_and_parse_skill_doc(raw: &str, fallback_dir: &Path) -> Result<(toml::Table, String)> {
    let trimmed = raw.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("+++") {
        let (Some(fm_str), body) = split_toml_frontmatter(trimmed) else {
            bail!(
                "Invalid skill markdown: opening '+++' frontmatter fence is missing a closing '+++' line"
            );
        };
        let table: toml::Table =
            toml::from_str(fm_str).context("Invalid TOML in skill '+++' frontmatter")?;
        if parse_skill_markdown(trimmed, fallback_dir).is_none() {
            bail!("Invalid skill definition: skill 'name' could not be resolved");
        }
        Ok((table, body.to_string()))
    } else {
        Ok((toml::Table::new(), trimmed.to_string()))
    }
}

fn table_string(table: &toml::Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(String::from)
}

fn table_string_vec(table: &toml::Table, key: &str) -> Option<Vec<String>> {
    let arr = table.get(key)?.as_array()?;
    Some(
        arr.iter()
            .filter_map(toml::Value::as_str)
            .map(String::from)
            .collect(),
    )
}

fn table_hooks(table: &toml::Table) -> Option<Vec<SkillHook>> {
    let val = table.get("hooks")?.clone();
    val.try_into::<Vec<SkillHook>>().ok()
}

fn check_unknown_tools(tools: &[String]) -> Option<String> {
    let unknown: Vec<&str> = tools
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|&t| !t.is_empty() && t != "*" && !ALL_TOOL_NAMES.contains(&t))
        .collect();
    if unknown.is_empty() {
        None
    } else {
        Some(format!(
            "[Note: unrecognized tool name(s) in `tools`: {}. Valid tools: {}]",
            unknown.join(", "),
            ALL_TOOL_NAMES.join(", ")
        ))
    }
}

fn normalize_doc_newlines(raw: &str) -> String {
    let mut normalized = raw.replace("\r\n", "\n");
    if !normalized.ends_with('\n') {
        normalized.push('\n');
    }
    normalized
}

#[allow(clippy::too_many_arguments)]
fn merge_and_format_skill_doc(
    args: &SkillArgs,
    table: &toml::Table,
    fallback_name: &str,
    fallback_desc: &str,
    fallback_cmd: &str,
    fallback_tools: &[String],
    fallback_hooks: &[SkillHook],
    body: &str,
    validate_cmd: bool,
) -> Result<String> {
    let name = table_string(table, "name").unwrap_or_else(|| fallback_name.to_string());
    let desc = args
        .description
        .clone()
        .or_else(|| table_string(table, "description"))
        .unwrap_or_else(|| fallback_desc.to_string());
    let cmd = args
        .command
        .clone()
        .or_else(|| table_string(table, "command"))
        .unwrap_or_else(|| fallback_cmd.to_string());
    if validate_cmd && !cmd.is_empty() {
        validate_skill_slug(&cmd, "command")?;
    }
    let tools = args
        .tools
        .clone()
        .or_else(|| table_string_vec(table, "tools"))
        .unwrap_or_else(|| fallback_tools.to_vec());
    let hooks = args
        .hooks
        .clone()
        .or_else(|| table_hooks(table))
        .unwrap_or_else(|| fallback_hooks.to_vec());
    format_skill_document(&name, &desc, &cmd, &tools, &hooks, Some(table), body)
}

fn append_unknown_tools_note(msg: &mut String, args: &SkillArgs, doc: &str, path: &Path) {
    if is_skill_md_target(&args.file)
        && let Ok((table, _)) = validate_and_parse_skill_doc(doc, path)
    {
        let mut all_tools = table_string_vec(&table, "tools").unwrap_or_default();
        if let Some(hooks) = table_hooks(&table) {
            all_tools.extend(hooks.into_iter().map(|h| h.tool));
        }
        if let Some(note) = check_unknown_tools(&all_tools) {
            let _ = write!(msg, "\n{note}");
        }
    }
}

pub fn preview_create(args: &SkillArgs, ctx: &ToolContext) -> Result<(PathBuf, String, String)> {
    let slug = validate_skill_slug(&args.name, "name")?;
    let root_dir = create_root_dir(args.scope, ctx);
    let skill_dir = root_dir.join(&slug);

    if !is_skill_md_target(&args.file) {
        let rel = validate_reference_subpath(&args.file)?;
        let target_path = skill_dir.join(rel);
        let display = format_tilde_path(&target_path);
        if target_path.exists() {
            bail!("File already exists: {display}. Use action: \"modify\" instead.");
        }
        if let Some(existing_root) = root_dir.ancestors().find(|p| p.exists())
            && let Ok(canon_root) = dunce::canonicalize(existing_root)
        {
            ensure_ancestor_within_root(&target_path, &canon_root, &display)?;
        }
        let body = args
            .content
            .as_deref()
            .or(args.prompt.as_deref())
            .unwrap_or("");
        if body.trim().is_empty() {
            bail!("'content' (or 'prompt') is required when creating a reference file");
        }
        return Ok((target_path, display, body.to_string()));
    }

    let target_path = skill_dir.join("SKILL.md");
    let flat_path = root_dir.join(format!("{slug}.md"));
    if target_path.exists() || flat_path.exists() {
        let existing = if target_path.exists() {
            &target_path
        } else {
            &flat_path
        };
        bail!(
            "Skill '{slug}' already exists at {}. Use action: \"modify\" instead.",
            format_tilde_path(existing)
        );
    }

    let rendered = if let Some(raw_content) = args.content.as_deref().filter(|c| {
        c.trim_start_matches('\u{feff}')
            .trim_start()
            .starts_with("+++")
    }) {
        let (table, body) = validate_and_parse_skill_doc(raw_content, &skill_dir)?;
        if args.has_frontmatter_overrides() || args.prompt.is_some() {
            let prompt_body = args.prompt.as_deref().unwrap_or(&body);
            merge_and_format_skill_doc(
                args,
                &table,
                &slug,
                "",
                &slug,
                &[],
                &[],
                prompt_body,
                false,
            )?
        } else {
            normalize_doc_newlines(raw_content)
        }
    } else {
        let prompt_body = args
            .prompt
            .as_deref()
            .or(args.content.as_deref())
            .unwrap_or("")
            .trim();
        if prompt_body.is_empty() {
            bail!(
                "Creating a skill requires 'prompt' (the markdown instructions) or full 'content'"
            );
        }
        let desc = args
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                prompt_body
                    .lines()
                    .map(|l| l.trim().trim_start_matches('#').trim())
                    .find(|l| !l.is_empty())
                    .unwrap_or("Custom skill")
            });
        let cmd = args
            .command
            .as_deref()
            .map(clean_skill_query)
            .filter(|s| !s.is_empty())
            .unwrap_or(&slug);
        if !cmd.is_empty() {
            validate_skill_slug(cmd, "command")?;
        }
        let tools = args.tools.as_deref().unwrap_or(&[]);
        let hooks = args.hooks.as_deref().unwrap_or(&[]);
        format_skill_document(&slug, desc, cmd, tools, hooks, None, prompt_body)?
    };

    let display = format_tilde_path(&target_path);
    Ok((target_path, display, rendered))
}

pub fn preview_modify(
    args: &SkillArgs,
    ctx: &ToolContext,
) -> Result<(PathBuf, String, String, String, usize)> {
    let resolved = resolve_existing_skill(&args.name, args.scope, ctx)?;

    if !is_skill_md_target(&args.file) {
        let (canon, display) = resolve_existing_reference(&resolved, &args.file, true)?;
        let original = fs::read_to_string(&canon).context("Error reading skill reference file")?;

        if !args.edits.is_empty() {
            let updated = apply_hunks_to_str(&original, &display, &args.edits)?;
            return Ok((canon, display, original, updated, args.edits.len()));
        }
        if let Some(new_body) = args.content.as_deref().or(args.prompt.as_deref()) {
            return Ok((canon, display, original, new_body.to_string(), 0));
        }
        bail!(
            "Nothing to modify for reference file '{display}': provide 'edits' hunks or 'content'"
        );
    }

    let target_path = resolved.skill_md_path;
    let display = format_tilde_path(&target_path);
    let original = fs::read_to_string(&target_path).context("Error reading SKILL.md")?;

    let mut working = if !args.edits.is_empty() {
        apply_hunks_to_str(&original, &display, &args.edits)?
    } else {
        original.clone()
    };

    let has_field_updates =
        args.has_frontmatter_overrides() || args.prompt.is_some() || args.content.is_some();

    if args.edits.is_empty() && !has_field_updates {
        bail!(
            "Nothing to modify for skill '{}': provide 'edits' hunks or at least one of 'description', 'command', 'tools', 'hooks', 'prompt', or 'content'",
            resolved.definition.name
        );
    }

    let mut body_override = args.prompt.as_deref();
    if let Some(raw_content) = args.content.as_deref() {
        if raw_content
            .trim_start_matches('\u{feff}')
            .trim_start()
            .starts_with("+++")
        {
            working = normalize_doc_newlines(raw_content);
        } else if body_override.is_none() {
            body_override = Some(raw_content);
        }
    }

    if args.has_frontmatter_overrides() || body_override.is_some() {
        let (table, current_body) = validate_and_parse_skill_doc(&working, &resolved.skill_dir)?;
        let def = &resolved.definition;
        let body = body_override.unwrap_or(&current_body);
        working = merge_and_format_skill_doc(
            args,
            &table,
            &def.name,
            &def.description,
            &def.command,
            &def.tools,
            &def.hooks,
            body,
            true,
        )?;
    } else {
        validate_and_parse_skill_doc(&working, &resolved.skill_dir)?;
    }

    Ok((target_path, display, original, working, args.edits.len()))
}

fn execute_list(args: &SkillArgs, ctx: &ToolContext) -> Result<String> {
    let (ws_dir, global_dir) = skill_roots(ctx);
    let filter = clean_skill_query(&args.name);

    let ws_skills = if matches!(args.scope, SkillScope::Auto | SkillScope::Workspace) {
        scan_skills_dir(&ws_dir)
    } else {
        Vec::new()
    };
    let global_skills = if matches!(args.scope, SkillScope::Auto | SkillScope::Global) {
        scan_skills_dir(&global_dir)
    } else {
        Vec::new()
    };

    let mut entries: Vec<(&str, &SkillDefinition, bool)> = Vec::new();
    for s in &ws_skills {
        entries.push(("workspace", s, false));
    }
    for s in &global_skills {
        let shadowed = ws_skills
            .iter()
            .any(|ws| ws.name.eq_ignore_ascii_case(&s.name));
        entries.push(("global", s, shadowed));
    }

    if !filter.is_empty() {
        entries.retain(|(_, s, _)| {
            s.name.eq_ignore_ascii_case(filter)
                || (!s.command.is_empty() && s.command.eq_ignore_ascii_case(filter))
        });
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Skill directories:\n  workspace: {}\n  global:    {}\n",
        format_tilde_path(&ws_dir),
        format_tilde_path(&global_dir),
    );

    if entries.is_empty() {
        if filter.is_empty() {
            let _ = writeln!(out, "No skills found.");
        } else {
            let _ = writeln!(out, "No skills matching '{filter}' found.");
        }
        return Ok(out.trim_end().to_string());
    }

    let _ = writeln!(out, "Declared skills ({}):", entries.len());
    for (scope, skill, shadowed) in entries {
        let trigger = skill.trigger();
        let shadow_note = if shadowed {
            " (overridden by workspace skill)"
        } else {
            ""
        };
        let tools_str = if skill.tools.is_empty() {
            "(none)".to_string()
        } else {
            skill.tools.join(", ")
        };
        let path_str = skill
            .dir_path
            .as_deref()
            .map(|d| {
                let md = d.join("SKILL.md");
                format_tilde_path(if md.is_file() { &md } else { d })
            })
            .unwrap_or_default();
        let refs = list_skill_reference_paths(skill);

        let _ = writeln!(
            out,
            "\n- {} (@{}) [{}{}]",
            skill.name, trigger, scope, shadow_note
        );
        let _ = writeln!(out, "  description: {}", skill.description);
        let _ = writeln!(out, "  tools:       {tools_str}");
        if !skill.hooks.is_empty() {
            let hooks_str: Vec<String> = skill
                .hooks
                .iter()
                .map(|h| format!("{} [{}]", format_hook_summary(h), h.trigger.as_str()))
                .collect();
            let _ = writeln!(out, "  hooks:       {}", hooks_str.join(", "));
        }
        if !path_str.is_empty() {
            let _ = writeln!(out, "  path:        {path_str}");
        }
        if !refs.is_empty() {
            let _ = writeln!(out, "  references:  {}", refs.join(", "));
        }
    }

    Ok(out.trim_end().to_string())
}

fn execute_read(args: &SkillArgs, ctx: &ToolContext) -> Result<String> {
    let resolved = resolve_existing_skill(&args.name, args.scope, ctx)?;

    if !is_skill_md_target(&args.file) {
        let (canon, display) = resolve_existing_reference(&resolved, &args.file, false)?;
        return read_file_contents(
            &canon,
            &display,
            args.start_line,
            args.end_line,
            ctx.max_output_size,
        );
    }

    let display = format_tilde_path(&resolved.skill_md_path);
    let content = read_file_contents(
        &resolved.skill_md_path,
        &display,
        args.start_line,
        args.end_line,
        ctx.max_output_size,
    )?;

    let trigger = resolved.definition.trigger();
    let refs = list_skill_reference_paths(&resolved.definition);
    let mut header = format!(
        "# Skill: {} (@{}) [{}]\n# Path: {}\n",
        resolved.definition.name, trigger, resolved.scope_label, display
    );
    if !refs.is_empty() {
        let _ = writeln!(header, "# References: {}", refs.join(", "));
    }
    header.push('\n');
    header.push_str(&content);
    Ok(header)
}

impl BlockingTool for Tool {
    const NAME: &'static str = "skill";
    const DESCRIPTION: &'static str = "List, read, create, and modify Archon skills (`SKILL.md` definitions and skill reference files) across workspace and global skill directories.";
    type Args = SkillArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        match args.action {
            SkillAction::List => execute_list(&args, ctx),
            SkillAction::Read => execute_read(&args, ctx),
            SkillAction::Create => {
                let (target_path, display, rendered) = preview_create(&args, ctx)?;
                let root_dir = create_root_dir(args.scope, ctx);
                fs::create_dir_all(&root_dir).with_context(|| {
                    format!("Failed to create skills root {}", root_dir.display())
                })?;
                ensure_parent_dir(&target_path, &root_dir, "created skill directory")?;
                atomic_write_file(&target_path, &rendered).context("Error writing skill file")?;

                let mut msg = format!("Successfully created {display}");
                append_unknown_tools_note(&mut msg, &args, &rendered, &target_path);
                Ok(msg)
            }
            SkillAction::Modify => {
                let (target_path, display, _, updated, hunk_count) = preview_modify(&args, ctx)?;
                atomic_write_file(&target_path, &updated).context("Error writing skill file")?;

                let mut msg = if hunk_count > 1 {
                    format!("Successfully applied {hunk_count} edits to {display}")
                } else {
                    format!("Successfully modified {display}")
                };
                append_unknown_tools_note(&mut msg, &args, &updated, &target_path);
                Ok(msg)
            }
        }
    }
}
