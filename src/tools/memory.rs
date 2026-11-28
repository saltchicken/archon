use super::{BlockingTool, SessionToolState, ToolContext, approval, parse_args};
use crate::core::config::{
    Config, Memory, MemoryScope, clean_memory_item, format_tilde_path, global_memory_path,
    load_scoped_memory_file, save_memory_file, workspace_config_dir_for, workspace_memory_path,
};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;

pub struct Tool;

#[derive(Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAction {
    /// List all persistent memory items and enabled status.
    List,
    /// Read persistent memory items (alias for `list`).
    #[serde(alias = "get")]
    Read,
    /// Add a new persistent fact or note to `MEMORY.md`.
    Add,
    /// Remove a persistent fact by 1-based index or substring match.
    #[serde(alias = "delete", alias = "rm")]
    Remove,
    /// Replace all persistent memory items in the target scope.
    Set,
    /// Clear all persistent memory items in the target scope.
    Clear,
    /// Enable persistent memory injection into the system prompt.
    Enable,
    /// Disable persistent memory injection into the system prompt.
    Disable,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemoryArgs {
    /// The memory operation to perform.
    pub action: MemoryAction,
    /// Target scope (`global` or `workspace`). Defaults to `global`.
    #[serde(default)]
    pub scope: MemoryScope,
    /// Fact text to add (for `add`), or query/1-based index to remove (for `remove`), or newline-separated facts (for `set`).
    #[serde(default, alias = "fact", alias = "text", alias = "content")]
    pub item: String,
    /// Optional list of facts for `add` or `set`.
    #[serde(default, alias = "facts", alias = "entries")]
    pub items: Vec<String>,
}

fn resolve_scope_path(scope: MemoryScope, ctx: &ToolContext) -> (PathBuf, bool) {
    match scope {
        MemoryScope::Global => (global_memory_path(), true),
        MemoryScope::Workspace => {
            let ws_dir = ctx
                .config
                .workspace_skills_dir
                .as_deref()
                .and_then(|p| p.parent())
                .map(PathBuf::from)
                .unwrap_or_else(|| workspace_config_dir_for(&ctx.root));
            (workspace_memory_path(&ws_dir), false)
        }
    }
}

fn collect_input_items(args: &MemoryArgs) -> Vec<String> {
    let mut raw = Vec::new();
    if !args.item.trim().is_empty() {
        raw.push(args.item.clone());
    }
    raw.extend(args.items.iter().cloned());
    Memory::new(raw, true).items
}

fn preview_mutation(
    args: &MemoryArgs,
    ctx: &ToolContext,
) -> Result<(PathBuf, String, String, String, Memory)> {
    let (mem_path, is_global) = resolve_scope_path(args.scope, ctx);
    let display = format_tilde_path(&mem_path);
    let (original, mut mem) = load_scoped_memory_file(&mem_path, is_global)?;

    match args.action {
        MemoryAction::List | MemoryAction::Read => {
            bail!("Read-only action does not mutate memory");
        }
        MemoryAction::Add => {
            let incoming = collect_input_items(args);
            if incoming.is_empty() {
                bail!("'item' or 'items' is required for memory:add");
            }
            let mut combined = mem.items;
            combined.extend(incoming);
            mem = Memory::new(combined, mem.enabled);
        }
        MemoryAction::Remove => {
            let target = clean_memory_item(&args.item);
            if target.is_empty() {
                bail!("'item' (1-based index or text match) is required for memory:remove");
            }
            if let Ok(idx) = target.parse::<usize>() {
                if idx == 0 || idx > mem.items.len() {
                    bail!("Index {idx} is out of range (1..={})", mem.items.len());
                }
                mem.items.remove(idx - 1);
            } else {
                let q = target.to_ascii_lowercase();
                let before = mem.items.len();
                mem.items
                    .retain(|existing| !existing.to_ascii_lowercase().contains(&q));
                if mem.items.len() == before {
                    bail!("No memory item matched '{target}'");
                }
            }
        }
        MemoryAction::Set => {
            let incoming = collect_input_items(args);
            mem = Memory::new(incoming, mem.enabled);
        }
        MemoryAction::Clear => {
            mem.items.clear();
        }
        MemoryAction::Enable => {
            mem.enabled = true;
        }
        MemoryAction::Disable => {
            mem.enabled = false;
        }
    }

    let updated = mem.to_markdown_file();
    Ok((mem_path, display, original, updated, mem))
}

impl BlockingTool for Tool {
    const NAME: &'static str = "memory";
    const DESCRIPTION: &'static str = "Reads or updates persistent user & workspace memory (`MEMORY.md`) that is automatically injected into the system prompt across sessions.";
    const SUBACTIONS: &'static [&'static str] = &[
        "list", "read", "add", "remove", "set", "clear", "enable", "disable",
    ];
    const DEFAULT_ACTION: Option<&'static str> = Some("list");
    const PARAM_HINT: &'static str =
        "<list|read|add|remove|set|clear|enable|disable> [global|workspace] [item]";
    const SHORT_SUMMARY: &'static str =
        "Read or update persistent global and workspace memory (MEMORY.md).";
    type Args = MemoryArgs;

    fn positional_keys(
        token: &str,
        map: &serde_json::Map<String, serde_json::Value>,
    ) -> &'static [&'static str] {
        if !map.contains_key("action") {
            &["action"]
        } else if matches!(token, "global" | "workspace") {
            &["scope", "item"]
        } else {
            &["item"]
        }
    }

    fn format_approval(
        out: &mut String,
        args: &serde_json::Value,
        obj: &serde_json::Map<String, serde_json::Value>,
        config: &Config,
        active_skill: Option<&str>,
    ) {
        approval::append_filtered_kvs(out, obj, &[]);

        if let Ok(parsed) = parse_args::<MemoryArgs>(args, Self::NAME)
            && let Ok(ctx) =
                ToolContext::new(config, active_skill, None, &SessionToolState::default(), 0)
            && !matches!(parsed.action, MemoryAction::List | MemoryAction::Read)
        {
            approval::append_diff_result(
                out,
                preview_mutation(&parsed, &ctx).map(|(_, d, o, u, _)| (d, o, u, None)),
            );
        }
    }

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        match args.action {
            MemoryAction::List | MemoryAction::Read => {
                let (global_path, _) = resolve_scope_path(MemoryScope::Global, ctx);
                let (ws_path, _) = resolve_scope_path(MemoryScope::Workspace, ctx);
                let (_, global_mem) = load_scoped_memory_file(&global_path, true)?;
                let (_, ws_mem) = load_scoped_memory_file(&ws_path, false)?;

                let status_str = |enabled: bool| if enabled { "ENABLED" } else { "DISABLED" };
                let effective_enabled =
                    global_mem.enabled || (ws_mem.enabled && !ws_mem.items.is_empty());

                let mut out = format!("Memory Status: {}\n", status_str(effective_enabled));
                for (label, path, mem) in [
                    ("Global", &global_path, &global_mem),
                    ("Workspace", &ws_path, &ws_mem),
                ] {
                    out.push_str(&format!(
                        "\n{label} ({}) [{}]:\n",
                        format_tilde_path(path),
                        status_str(mem.enabled)
                    ));
                    if mem.items.is_empty() {
                        out.push_str("  (empty)\n");
                    } else {
                        for (i, item) in mem.items.iter().enumerate() {
                            out.push_str(&format!("  {}. {item}\n", i + 1));
                        }
                    }
                }

                Ok(out.trim_end().to_string())
            }
            _ => {
                let (mem_path, display, _, _, mem) = preview_mutation(&args, ctx)?;
                save_memory_file(&mem_path, &mem)?;
                Ok(format!(
                    "Updated {display} (enabled: {}, items: {}).",
                    mem.enabled,
                    mem.items.len()
                ))
            }
        }
    }
}
