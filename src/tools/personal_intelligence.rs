use super::{BlockingTool, ToolContext, utils::fs::atomic_write_file};
use crate::core::config::{
    PersonalIntelligence, archon_config_root, clean_pi_item, format_tilde_path,
    load_scoped_pi_table, serialize_table_with_pi, workspace_config_dir_for,
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
pub enum PersonalIntelligenceAction {
    /// List all persistent Personal Intelligence items and status (`enabled`).
    List,
    /// Alias of `list`: read persistent Personal Intelligence items and how they are injected into the model prompt.
    #[serde(alias = "get")]
    Read,
    /// Add one or more persistent facts/items to Personal Intelligence (`item` or `items`).
    #[serde(alias = "append")]
    Add,
    /// Remove a Personal Intelligence item by matching text (`item`) or 1-based `index`.
    #[serde(alias = "delete")]
    Remove,
    /// Replace the Personal Intelligence `items` list and/or update `enabled`.
    #[serde(alias = "replace", alias = "update")]
    Set,
    /// Clear all Personal Intelligence items in the target scope.
    Clear,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PersonalIntelligenceScope {
    /// Global user configuration (`~/.config/archon/config.toml`). Default.
    #[default]
    Global,
    /// Workspace-specific configuration (`~/.config/archon/workspaces/<workspace>/config.toml`).
    Workspace,
}

#[derive(Deserialize, JsonSchema)]
pub struct PersonalIntelligenceArgs {
    /// The Personal Intelligence operation to perform: `list`, `read`, `add`, `remove`, `set`, or `clear`.
    pub action: PersonalIntelligenceAction,
    /// Target scope: `global` (`~/.config/archon/config.toml`, default) or `workspace`.
    #[serde(default)]
    pub scope: PersonalIntelligenceScope,
    /// Single fact/item string (or multi-line text) to `add`, `remove`, or `set`.
    #[serde(default)]
    pub item: String,
    /// List of fact/item strings to `add` or `set`.
    #[serde(default)]
    pub items: Vec<String>,
    /// Optional 1-based item index to remove when `action` is `remove`.
    #[serde(default)]
    pub index: Option<usize>,
    /// Optional boolean to enable or disable Personal Intelligence injection (`add`, `set`, `clear`).
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn target_config_path(scope: PersonalIntelligenceScope, ctx: &ToolContext) -> PathBuf {
    match scope {
        PersonalIntelligenceScope::Global => archon_config_root().join("config.toml"),
        PersonalIntelligenceScope::Workspace => ctx
            .config
            .workspace_skills_dir
            .as_deref()
            .and_then(Path::parent)
            .map(|p| p.join("config.toml"))
            .unwrap_or_else(|| workspace_config_dir_for(&ctx.root).join("config.toml")),
    }
}

fn collect_input_items(args: &PersonalIntelligenceArgs) -> Vec<String> {
    std::iter::once(args.item.as_str())
        .chain(args.items.iter().map(String::as_str))
        .flat_map(str::lines)
        .map(clean_pi_item)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

pub fn preview_mutation(
    args: &PersonalIntelligenceArgs,
    ctx: &ToolContext,
) -> Result<(PathBuf, String, String, String, PersonalIntelligence)> {
    let is_global = matches!(args.scope, PersonalIntelligenceScope::Global);
    let cfg_path = target_config_path(args.scope, ctx);
    let display = format_tilde_path(&cfg_path);
    let (original, table, mut pi) = load_scoped_pi_table(&cfg_path, is_global)?;

    match args.action {
        PersonalIntelligenceAction::Add => {
            let incoming = collect_input_items(args);
            if incoming.is_empty() && args.enabled.is_none() {
                bail!("Provide `item` or `items` (or `enabled`) when `action` is `add`");
            }
            let mut merged = pi.items;
            merged.extend(incoming);
            let enabled = args.enabled.unwrap_or(pi.enabled);
            pi = PersonalIntelligence::new(merged, enabled);
        }
        PersonalIntelligenceAction::Remove => {
            if let Some(idx) = args.index {
                if idx == 0 || idx > pi.items.len() {
                    bail!(
                        "Invalid 1-based index {idx}: Personal Intelligence has {} item(s)",
                        pi.items.len()
                    );
                }
                pi.items.remove(idx - 1);
            } else {
                let query = clean_pi_item(&args.item);
                if query.is_empty() {
                    bail!("Provide `item` text or 1-based `index` when `action` is `remove`");
                }
                let q_lower = query.to_ascii_lowercase();
                let before = pi.items.len();
                pi.items
                    .retain(|entry| !entry.to_ascii_lowercase().contains(&q_lower));
                if pi.items.len() == before {
                    bail!("No Personal Intelligence item matched '{query}'");
                }
            }
            if let Some(en) = args.enabled {
                pi.enabled = en;
            }
        }
        PersonalIntelligenceAction::Set => {
            let incoming = collect_input_items(args);
            if incoming.is_empty() && args.enabled.is_none() {
                bail!("Provide `item` / `items` or `enabled` when `action` is `set`");
            }
            let items =
                if incoming.is_empty() && args.item.trim().is_empty() && args.items.is_empty() {
                    pi.items
                } else {
                    incoming
                };
            let enabled = args.enabled.unwrap_or(pi.enabled);
            pi = PersonalIntelligence::new(items, enabled);
        }
        PersonalIntelligenceAction::Clear => {
            pi.items.clear();
            if let Some(en) = args.enabled {
                pi.enabled = en;
            }
        }
        PersonalIntelligenceAction::List | PersonalIntelligenceAction::Read => {
            bail!("`list` and `read` do not mutate Personal Intelligence");
        }
    }

    let updated = serialize_table_with_pi(table, &pi)?;

    Ok((cfg_path, display, original, updated, pi))
}

fn execute_read(ctx: &ToolContext) -> Result<String> {
    let global_path = target_config_path(PersonalIntelligenceScope::Global, ctx);
    let ws_path = target_config_path(PersonalIntelligenceScope::Workspace, ctx);
    let (_, _, global_pi) = load_scoped_pi_table(&global_path, true)?;
    let (_, _, ws_pi) = load_scoped_pi_table(&ws_path, false)?;

    let mut combined = global_pi.items.clone();
    combined.extend(ws_pi.items.clone());
    let effective = PersonalIntelligence::new(combined, ctx.config.personal_intelligence.enabled);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Personal Intelligence Status: {}",
        if effective.enabled {
            "ENABLED"
        } else {
            "DISABLED"
        }
    );
    let _ = writeln!(out, "Global config:    {}", format_tilde_path(&global_path));
    let _ = writeln!(out, "Workspace config: {}\n", format_tilde_path(&ws_path));

    if effective.items.is_empty() {
        let _ = writeln!(out, "No Personal Intelligence items configured.");
    } else {
        let _ = writeln!(out, "Active items ({}):", effective.items.len());
        for (idx, item) in effective.items.iter().enumerate() {
            let scope_tag = if ws_pi.items.iter().any(|w| w.eq_ignore_ascii_case(item))
                && !global_pi.items.iter().any(|g| g.eq_ignore_ascii_case(item))
            {
                " [workspace]"
            } else {
                ""
            };
            let _ = writeln!(out, "  {}. {}{}", idx + 1, item, scope_tag);
        }
    }

    if let Some(block) = effective.format_for_prompt() {
        let _ = writeln!(out, "\nInjected System Prompt Block:\n{block}");
    }

    Ok(out.trim_end().to_string())
}

impl BlockingTool for Tool {
    const NAME: &'static str = "personal_intelligence";
    const DESCRIPTION: &'static str = "View and manage persistent Personal Intelligence facts (`list`, `read`, `add`, `remove`, `set`, `clear`) that are always passed to the model across all sessions and turns.";
    type Args = PersonalIntelligenceArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        match args.action {
            PersonalIntelligenceAction::List | PersonalIntelligenceAction::Read => {
                execute_read(ctx)
            }
            PersonalIntelligenceAction::Add
            | PersonalIntelligenceAction::Remove
            | PersonalIntelligenceAction::Set
            | PersonalIntelligenceAction::Clear => {
                let (cfg_path, display, _, updated, pi) = preview_mutation(&args, ctx)?;
                if let Some(parent) = cfg_path.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("Failed to create directory {}", parent.display())
                    })?;
                }
                atomic_write_file(&cfg_path, &updated)
                    .with_context(|| format!("Error writing {display}"))?;

                Ok(format!(
                    "Updated Personal Intelligence in {display} (enabled: {}, items: {}).",
                    pi.enabled,
                    pi.items.len()
                ))
            }
        }
    }
}
