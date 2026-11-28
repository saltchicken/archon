//! Meta-tool for activating deferred tools on demand during a session.

use super::groups::{format_valid_tools_and_groups, resolve_tool_group};
use crate::config::Config;
use crate::tools::{ALL_TOOL_NAMES, BlockingTool, SessionToolState, ToolContext};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadToolsArgs {
    /// Tool names (e.g. `["git", "cargo"]`) and/or `@group` names (e.g. `["@mygroup"]`) to activate so their full schemas are included on subsequent turns.
    #[serde(deserialize_with = "super::utils::serde_helpers::deserialize_string_list")]
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadToolsResolution {
    pub newly_loaded: Vec<String>,
    pub already_active: Vec<String>,
    pub skipped_disabled: Vec<String>,
    pub evicted: Vec<String>,
}

fn push_unique(vec: &mut Vec<String>, item: String) {
    if !vec.iter().any(|existing| existing == &item) {
        vec.push(item);
    }
}

/// Validates and resolves a `load_tools` request against the active config,
/// session tool overrides, and active skill.
pub fn resolve_load_tools_request(
    names: &[String],
    config: &Config,
    session_tools: &SessionToolState,
    active_skill: Option<&str>,
) -> Result<LoadToolsResolution> {
    let cleaned: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        bail!("Provide at least one tool name or `@group` name in `names`.");
    }

    let currently_active =
        crate::tools::active_regular_tools_for_turn(config, session_tools, active_skill);

    let mut unknown = Vec::new();
    let mut disabled = Vec::new();
    let mut newly_loaded = Vec::new();
    let mut already_active = Vec::new();
    let mut skipped_disabled = Vec::new();

    for entry in cleaned {
        if let Some(group_key) = entry.strip_prefix('@') {
            let Some(members) = resolve_tool_group(group_key, &config.tool_groups) else {
                push_unique(&mut unknown, entry.to_string());
                continue;
            };
            let mut group_enabled_any = false;
            let mut group_disabled = Vec::new();
            for member in members {
                if !ALL_TOOL_NAMES.contains(&member.as_str()) {
                    push_unique(&mut unknown, member);
                    continue;
                }
                if !crate::tools::is_tool_enabled_for_turn(
                    &member,
                    config,
                    session_tools,
                    active_skill,
                ) {
                    push_unique(&mut group_disabled, member);
                    continue;
                }
                group_enabled_any = true;
                if currently_active.iter().any(|a| a == &member) {
                    push_unique(&mut already_active, member);
                } else {
                    push_unique(&mut newly_loaded, member);
                }
            }
            if group_enabled_any {
                for d in group_disabled {
                    push_unique(&mut skipped_disabled, d);
                }
            } else if !group_disabled.is_empty() {
                push_unique(
                    &mut disabled,
                    format!(
                        "{entry} (all members disabled: {})",
                        group_disabled.join(", ")
                    ),
                );
            }
        } else {
            let Some(&canonical) = ALL_TOOL_NAMES
                .iter()
                .find(|&&name| name.eq_ignore_ascii_case(entry))
            else {
                push_unique(&mut unknown, entry.to_string());
                continue;
            };
            if !crate::tools::is_tool_enabled_for_turn(
                canonical,
                config,
                session_tools,
                active_skill,
            ) {
                push_unique(&mut disabled, canonical.to_string());
                continue;
            }
            if currently_active.iter().any(|a| a == canonical) {
                push_unique(&mut already_active, canonical.to_string());
            } else {
                push_unique(&mut newly_loaded, canonical.to_string());
            }
        }
    }

    if !unknown.is_empty() || !disabled.is_empty() {
        let mut parts = Vec::new();
        if !unknown.is_empty() {
            parts.push(format!(
                "Unknown tool/group(s): {}. {}",
                unknown.join(", "),
                format_valid_tools_and_groups(&config.tool_groups)
            ));
        }
        if !disabled.is_empty() {
            parts.push(format!(
                "Disabled tool(s) cannot be loaded: {} (not enabled for this turn/session).",
                disabled.join(", ")
            ));
        }
        bail!("{}", parts.join(" "));
    }

    // Reject single requests that exceed the available dynamic slot budget so we never
    // immediately evict newly requested tools in the same call.
    let core_count =
        crate::tools::core_regular_tools_for_turn(config, session_tools, active_skill).len();
    if config.app.max_loaded_tools > 0 {
        let dynamic_cap = config
            .app
            .max_loaded_tools
            .saturating_sub(core_count)
            .max(1);
        if newly_loaded.len() > dynamic_cap {
            bail!(
                "Requested {} tool(s) ({}), which exceeds the dynamic tool slot budget ({} slot(s) available with max_loaded_tools={} and {} core tool(s)). Load fewer tools at once or raise `app.max_loaded_tools`.",
                newly_loaded.len(),
                newly_loaded.join(", "),
                dynamic_cap,
                config.app.max_loaded_tools,
                core_count
            );
        }
    }

    // Preview FIFO evictions without mutating the caller's state.
    // Skip eviction preview when nothing new is loaded to match
    // `CoreEngine::apply_load_tools_side_effect`, which is a no-op on empty batches.
    let evicted = if newly_loaded.is_empty() {
        Vec::new()
    } else {
        let mut preview_state = session_tools.clone();
        preview_state.apply_loaded(&newly_loaded, core_count, config.app.max_loaded_tools)
    };

    Ok(LoadToolsResolution {
        newly_loaded,
        already_active,
        skipped_disabled,
        evicted,
    })
}

impl BlockingTool for Tool {
    const NAME: &'static str = "load_tools";
    const DESCRIPTION: &'static str = "Activate one or more deferred tools (or `@group` names) so their full JSON schemas become available on the next turn.";
    const PARAM_HINT: &'static str = "<tool|@group> [tool|@group ...]";
    const SHORT_SUMMARY: &'static str = "Activate deferred tools or @groups on demand so their full schemas are loaded on the next turn.";
    type Args = LoadToolsArgs;

    fn apply_positional_arg(token: &str, map: &mut serde_json::Map<String, serde_json::Value>) {
        let entry = map
            .entry("names".to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let serde_json::Value::Array(arr) = entry {
            arr.push(serde_json::Value::String(token.to_string()));
        }
    }

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let res = resolve_load_tools_request(
            &args.names,
            &ctx.config,
            &ctx.session_tools,
            ctx.active_skill.as_deref(),
        )?;

        let mut parts = Vec::new();
        if !res.newly_loaded.is_empty() {
            parts.push(format!(
                "Activated tool schema(s) for next turn: {}.",
                res.newly_loaded.join(", ")
            ));
        }
        if !res.already_active.is_empty() {
            parts.push(format!(
                "Already active: {}.",
                res.already_active.join(", ")
            ));
        }
        if !res.skipped_disabled.is_empty() {
            parts.push(format!(
                "Skipped disabled group member(s): {}.",
                res.skipped_disabled.join(", ")
            ));
        }
        if !res.evicted.is_empty() {
            parts.push(format!(
                "Evicted oldest loaded tool(s) to stay within max_loaded_tools={}: {}.",
                ctx.config.app.max_loaded_tools,
                res.evicted.join(", ")
            ));
        }

        if parts.is_empty() {
            Ok("No tools needed to be loaded.".to_string())
        } else {
            Ok(parts.join(" "))
        }
    }
}
