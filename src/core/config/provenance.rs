//! Config provenance tracking: which source (default, global, workspace,
//! env, CLI) set each configuration value, and which keys were overridden.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConfigSource {
    #[default]
    Default,
    Global,
    Workspace,
    /// Present in both Global and Workspace config files with different values (Workspace overwrote Global).
    WorkspaceOverridesGlobal,
    Env,
    Cli,
}

impl ConfigSource {
    pub fn short_tag(self) -> &'static str {
        match self {
            Self::Default => "Def",
            Self::Global => "G",
            Self::Workspace => "WS",
            Self::WorkspaceOverridesGlobal => "G→WS",
            Self::Env => "Env",
            Self::Cli => "CLI",
        }
    }

    pub fn overwrote_global(self) -> bool {
        matches!(self, Self::WorkspaceOverridesGlobal)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProvenance {
    /// Enabled by compiled defaults (no config file set `enabled_tools`).
    DefaultEnabled,
    /// Enabled by Global config (`~/.config/archon/config.toml`).
    GlobalEnabled,
    /// Enabled by Workspace config (when Global did not set `enabled_tools`).
    WorkspaceEnabled,
    /// Enabled in both Global and Workspace config.
    GlobalAndWorkspaceEnabled,
    /// Disabled in Global config, but re-enabled by Workspace config override.
    WorkspaceAdded,
    /// Disabled by Global config (and Workspace did not override `enabled_tools`).
    GlobalDisabled,
    /// Enabled in Global (or Default), but disabled/dropped by Workspace `enabled_tools` override.
    WorkspaceRemoved,
    /// Disabled in both Global and Workspace `enabled_tools`.
    GlobalAndWorkspaceDisabled,
    /// Disabled by CLI `--allowed-tools` flag.
    CliDisabled,
}

impl ToolProvenance {
    pub fn short_tag(self) -> &'static str {
        match self {
            Self::DefaultEnabled => "[Def]",
            Self::GlobalEnabled => "[G]",
            Self::WorkspaceEnabled => "[WS]",
            Self::GlobalAndWorkspaceEnabled => "[G+WS]",
            Self::WorkspaceAdded => "[+WS]",
            Self::GlobalDisabled => "[-G]",
            Self::WorkspaceRemoved => "[-WS]",
            Self::GlobalAndWorkspaceDisabled => "[-G/WS]",
            Self::CliDisabled => "[-CLI]",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigProvenance {
    pub has_global_config: bool,
    pub has_workspace_config: bool,
    pub global_enabled_tools: Option<Vec<String>>,
    pub workspace_enabled_tools: Option<Vec<String>>,
    pub cli_allowed_tools: Option<Vec<String>>,
    pub default_model: ConfigSource,
    pub enabled_tools: ConfigSource,
    pub core_tools: ConfigSource,
    pub tool_groups: ConfigSource,
    pub auto_approve: ConfigSource,
    pub models: ConfigSource,
    pub system_presets: ConfigSource,
    pub ui: ConfigSource,
    pub dotfiles: ConfigSource,
    pub workspace_snapshot: ConfigSource,
    pub allow_hidden: ConfigSource,
    pub auto_compact: ConfigSource,
    /// Keys (`section.key` or top-level) set in both Global and Workspace `config.toml`
    /// with different values (Workspace overwrote Global).
    pub overwritten_keys: Vec<String>,
}

fn extract_toml_string_array(
    table: Option<&toml::Table>,
    section: &str,
    key: &str,
) -> Option<Vec<String>> {
    let arr = table?.get(section)?.as_table()?.get(key)?.as_array()?;
    Some(
        arr.iter()
            .filter_map(toml::Value::as_str)
            .map(|s| s.trim().to_string())
            .collect(),
    )
}

fn find_table_key<'a>(
    table: Option<&'a toml::Table>,
    section: &str,
    keys: &[&str],
) -> Option<&'a toml::Value> {
    let sub = table?.get(section)?.as_table()?;
    keys.iter().find_map(|k| sub.get(*k))
}

fn source_for_table_keys(
    global: Option<&toml::Table>,
    ws: Option<&toml::Table>,
    section: &str,
    keys: &[&str],
) -> ConfigSource {
    match (
        find_table_key(global, section, keys),
        find_table_key(ws, section, keys),
    ) {
        (Some(gv), Some(wv)) if gv != wv => ConfigSource::WorkspaceOverridesGlobal,
        (_, Some(_)) => ConfigSource::Workspace,
        (Some(_), None) => ConfigSource::Global,
        (None, None) => ConfigSource::Default,
    }
}

fn source_for_table_key(
    global: Option<&toml::Table>,
    ws: Option<&toml::Table>,
    section: &str,
    key: &str,
) -> ConfigSource {
    source_for_table_keys(global, ws, section, &[key])
}

fn combine_table_key_sources(
    global: Option<&toml::Table>,
    ws: Option<&toml::Table>,
    section: &str,
    keys: &[&str],
) -> ConfigSource {
    let mut best = ConfigSource::Default;
    for key in keys {
        match source_for_table_key(global, ws, section, key) {
            ConfigSource::WorkspaceOverridesGlobal => {
                return ConfigSource::WorkspaceOverridesGlobal;
            }
            ConfigSource::Workspace => best = ConfigSource::Workspace,
            ConfigSource::Global if best == ConfigSource::Default => best = ConfigSource::Global,
            _ => {}
        }
    }
    best
}

fn source_for_section(
    global: Option<&toml::Table>,
    ws: Option<&toml::Table>,
    section: &str,
) -> ConfigSource {
    let g_val = global.and_then(|t| t.get(section));
    let ws_val = ws.and_then(|t| t.get(section));
    match (g_val, ws_val) {
        (Some(toml::Value::Table(gt)), Some(toml::Value::Table(wt))) => {
            let has_diff_override = wt.iter().any(|(k, wv)| {
                !(section == "workspace_snapshot" && k == "file")
                    && gt.get(k).is_some_and(|gv| gv != wv)
            });
            if has_diff_override {
                ConfigSource::WorkspaceOverridesGlobal
            } else if !wt.is_empty() {
                ConfigSource::Workspace
            } else {
                ConfigSource::Global
            }
        }
        (Some(gv), Some(wv)) if gv != wv => ConfigSource::WorkspaceOverridesGlobal,
        (_, Some(_)) => ConfigSource::Workspace,
        (Some(_), None) => ConfigSource::Global,
        (None, None) => ConfigSource::Default,
    }
}

fn collect_overwritten_keys(global: Option<&toml::Table>, ws: Option<&toml::Table>) -> Vec<String> {
    let (Some(g_table), Some(w_table)) = (global, ws) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, w_val) in w_table {
        if matches!(k.as_str(), "workspace" | "schedules") {
            continue;
        }
        let Some(g_val) = g_table.get(k) else {
            continue;
        };
        match (g_val, w_val) {
            (toml::Value::Table(g_sub), toml::Value::Table(w_sub)) => {
                for (sub_k, w_sub_val) in w_sub {
                    if k == "workspace_snapshot" && sub_k == "file" {
                        continue;
                    }
                    let g_sub_val = g_sub.get(sub_k).or_else(|| {
                        if k == "app" && matches!(sub_k.as_str(), "allow_hidden" | "include_hidden")
                        {
                            g_sub
                                .get("allow_hidden")
                                .or_else(|| g_sub.get("include_hidden"))
                        } else {
                            None
                        }
                    });
                    if let Some(g_sub_val) = g_sub_val
                        && g_sub_val != w_sub_val
                    {
                        let norm_k = if k == "app" && sub_k == "include_hidden" {
                            "allow_hidden"
                        } else {
                            sub_k.as_str()
                        };
                        out.push(format!("{k}.{norm_k}"));
                    }
                }
            }
            _ => {
                if g_val != w_val {
                    out.push(k.clone());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn build_provenance(
    global_table: Option<&toml::Table>,
    ws_table: Option<&toml::Table>,
) -> ConfigProvenance {
    ConfigProvenance {
        has_global_config: global_table.is_some(),
        has_workspace_config: ws_table.is_some(),
        global_enabled_tools: extract_toml_string_array(global_table, "app", "enabled_tools"),
        workspace_enabled_tools: extract_toml_string_array(ws_table, "app", "enabled_tools"),
        cli_allowed_tools: None,
        default_model: source_for_table_key(global_table, ws_table, "app", "default_model"),
        enabled_tools: source_for_table_key(global_table, ws_table, "app", "enabled_tools"),
        core_tools: source_for_table_key(global_table, ws_table, "app", "core_tools"),
        tool_groups: source_for_section(global_table, ws_table, "tool_groups"),
        auto_approve: source_for_table_key(global_table, ws_table, "app", "auto_approve"),
        models: source_for_section(global_table, ws_table, "models"),
        system_presets: source_for_section(global_table, ws_table, "system_presets"),
        ui: source_for_section(global_table, ws_table, "ui"),
        dotfiles: source_for_section(global_table, ws_table, "dotfiles"),
        workspace_snapshot: source_for_section(global_table, ws_table, "workspace_snapshot"),
        allow_hidden: source_for_table_keys(
            global_table,
            ws_table,
            "app",
            &["allow_hidden", "include_hidden"],
        ),
        auto_compact: combine_table_key_sources(
            global_table,
            ws_table,
            "app",
            &[
                "auto_compact",
                "auto_compact_threshold",
                "auto_compact_cooldown_turns",
            ],
        ),
        overwritten_keys: collect_overwritten_keys(global_table, ws_table),
    }
}

pub fn tool_list_contains(
    list: &[String],
    tool_name: &str,
    custom_groups: &BTreeMap<String, Vec<String>>,
) -> bool {
    list.iter().map(|s| s.trim()).any(|t| {
        if t == "*" || t == tool_name {
            return true;
        }
        if let Some(group_key) = t.strip_prefix('@') {
            return crate::tools::resolve_tool_group(group_key, custom_groups)
                .is_some_and(|members| members.iter().any(|m| m == tool_name));
        }
        false
    })
}
