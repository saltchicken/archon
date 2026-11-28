use super::{BlockingTool, SessionToolState, ToolContext, executor::approval, parse_args};
use crate::config::{
    Config, Memory, MemoryScope, clean_memory_item, format_tilde_path, global_memory_path,
    load_scoped_memory_file, load_workspace_memory_db, save_scoped_memory,
};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

pub struct Tool;

#[derive(Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAction {
    /// List all persistent memory items and enabled status.
    List,
    /// Read persistent memory items (alias for `list`).
    #[serde(alias = "get")]
    Read,
    /// Add a new persistent fact or note to memory.
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

fn resolve_workspace_key(ctx: &ToolContext) -> String {
    if ctx
        .config
        .workspace
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        ctx.config.workspace_key()
    } else {
        dunce::canonicalize(&ctx.root)
            .unwrap_or_else(|_| ctx.root.clone())
            .to_string_lossy()
            .into_owned()
    }
}

fn scope_display(scope: MemoryScope, ctx: &ToolContext, ws_key: &str) -> String {
    match scope {
        MemoryScope::Global => format_tilde_path(&global_memory_path()),
        MemoryScope::Workspace => format!(
            "{} [workspace: {}]",
            format_tilde_path(&crate::db::archon_db_path(&ctx.config.app.history_dir)),
            format_tilde_path(Path::new(ws_key)),
        ),
    }
}

fn load_scope_for_tool(
    scope: MemoryScope,
    ctx: &ToolContext,
    ws_key: &str,
) -> Result<(String, Memory)> {
    match scope {
        MemoryScope::Global => load_scoped_memory_file(&global_memory_path(), true),
        MemoryScope::Workspace => {
            let mem = load_workspace_memory_db(&ctx.config.app.history_dir, ws_key)?;
            let original = mem.to_markdown_file();
            Ok((original, mem))
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
) -> Result<(String, String, String, String, Memory)> {
    let ws_key = resolve_workspace_key(ctx);
    let display = scope_display(args.scope, ctx, &ws_key);
    let (original, mut mem) = load_scope_for_tool(args.scope, ctx, &ws_key)?;

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
    Ok((ws_key, display, original, updated, mem))
}

impl BlockingTool for Tool {
    const NAME: &'static str = "memory";
    const DESCRIPTION: &'static str = "Reads or updates persistent user & workspace memory (global `MEMORY.md` and workspace memory in `archon.db`) that is automatically injected into the system prompt across sessions.";
    const SUBACTIONS: &'static [&'static str] = &[
        "list", "read", "add", "remove", "set", "clear", "enable", "disable",
    ];
    const DEFAULT_ACTION: Option<&'static str> = Some("list");
    const PARAM_HINT: &'static str =
        "<list|read|add|remove|set|clear|enable|disable> [global|workspace] [item]";
    const SHORT_SUMMARY: &'static str = "Read or update persistent global and workspace memory.";
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
                let ws_key = resolve_workspace_key(ctx);
                let global_display = scope_display(MemoryScope::Global, ctx, &ws_key);
                let ws_display = scope_display(MemoryScope::Workspace, ctx, &ws_key);
                let (_, global_mem) = load_scope_for_tool(MemoryScope::Global, ctx, &ws_key)?;
                let (_, ws_mem) = load_scope_for_tool(MemoryScope::Workspace, ctx, &ws_key)?;

                let status_str = |enabled: bool| if enabled { "ENABLED" } else { "DISABLED" };
                let effective_enabled =
                    global_mem.enabled || (ws_mem.enabled && !ws_mem.items.is_empty());

                let mut out = format!("Memory Status: {}\n", status_str(effective_enabled));
                for (label, location, mem) in [
                    ("Global", &global_display, &global_mem),
                    ("Workspace", &ws_display, &ws_mem),
                ] {
                    out.push_str(&format!(
                        "\n{label} ({location}) [{}]:\n",
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
                let (ws_key, display, _, _, mem) = preview_mutation(&args, ctx)?;
                save_scoped_memory(args.scope, &ctx.config.app.history_dir, &ws_key, &mem)?;
                Ok(format!(
                    "Updated {display} (enabled: {}, items: {}).",
                    mem.enabled,
                    mem.items.len()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(tmp: &tempfile::TempDir) -> ToolContext {
        let mut cfg = Config::default();
        cfg.app.history_dir = tmp.path().to_string_lossy().into_owned();
        cfg.workspace = Some(tmp.path().to_string_lossy().into_owned());
        ToolContext {
            root: tmp.path().to_path_buf(),
            max_output_size: 32_768,
            active_skill: None,
            session_id: None,
            session_tools: SessionToolState::default(),
            config: cfg,
        }
    }

    fn run(
        action: MemoryAction,
        item: &str,
        items: Vec<&str>,
        ctx: &ToolContext,
    ) -> Result<String> {
        Tool::execute(
            MemoryArgs {
                action,
                scope: MemoryScope::Workspace,
                item: item.into(),
                items: items.into_iter().map(String::from).collect(),
            },
            ctx,
        )
    }

    #[test]
    fn workspace_memory_tool_crud_persists_in_archon_db() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&tmp);

        run(
            MemoryAction::Add,
            "Uses Cargo workspace",
            vec!["Runs on Linux"],
            &ctx,
        )
        .unwrap();
        let ws_key = resolve_workspace_key(&ctx);
        let mem = load_workspace_memory_db(&ctx.config.app.history_dir, &ws_key).unwrap();
        assert!(mem.enabled);
        assert_eq!(mem.items, vec!["Uses Cargo workspace", "Runs on Linux"]);

        run(MemoryAction::Remove, "1", vec![], &ctx).unwrap();
        let mem = load_workspace_memory_db(&ctx.config.app.history_dir, &ws_key).unwrap();
        assert_eq!(mem.items, vec!["Runs on Linux"]);

        run(MemoryAction::Disable, "", vec![], &ctx).unwrap();
        let mem = load_workspace_memory_db(&ctx.config.app.history_dir, &ws_key).unwrap();
        assert!(!mem.enabled);
        assert_eq!(mem.items, vec!["Runs on Linux"]);

        run(MemoryAction::Set, "Single fact", vec![], &ctx).unwrap();
        let mem = load_workspace_memory_db(&ctx.config.app.history_dir, &ws_key).unwrap();
        assert!(!mem.enabled);
        assert_eq!(mem.items, vec!["Single fact"]);

        run(MemoryAction::Enable, "", vec![], &ctx).unwrap();
        run(MemoryAction::Clear, "", vec![], &ctx).unwrap();
        let mem = load_workspace_memory_db(&ctx.config.app.history_dir, &ws_key).unwrap();
        assert!(mem.enabled);
        assert!(mem.items.is_empty());
    }
}
