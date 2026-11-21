//! Skill system: parsing, discovery, inheritance, typed arguments,
//! template rendering, and hook management.

mod args;
mod hooks;
mod parse;
mod resolve;
mod template;
mod types;

pub use args::{
    activate_skill_for_turn, apply_at_skill_to_turn, format_arg_value, validate_arg_json_value,
    validate_turn_skill,
};
pub use hooks::{
    collect_after_turn_hooks, collect_due_hooks, format_hook_summary, interpolate_hook_args,
    preview_due_hooks,
};
pub use parse::{
    DetailedSkillParse, discover_skills, find_skill, parse_at_command,
    parse_skill_markdown_detailed, scan_skills_dir, split_toml_frontmatter,
};
pub use resolve::{check_skill_activation, missing_skill_tools, resolve_skills};
pub use template::{TemplateContext, inspect_template, render_template};
pub use types::{
    HookTrigger, KNOWN_FRONTMATTER_KEYS, RESERVED_ARG_NAMES, SkillArgDef, SkillArgType,
    SkillDefinition, SkillHook,
};

use crate::core::config::Config;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Composes the full system prompt for a turn, incorporating the base prompt,
/// memory blocks, and the active skill's rendered prompt.
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
        } else if args.is_empty() && unreferenced_inputs.is_empty() {
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
    use crate::core::config::MemoryScope;
    use crate::core::session::ChatMessage;
    use serde_json::json;
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;

    use super::args::resolve_skill_inputs;
    use super::parse::parse_skill_markdown;
    use super::resolve::replace_super_placeholder;

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
    fn skill_composition_extends_merges_tools_hooks_args_and_prompts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        fs::create_dir_all(dir.join("coder")).unwrap();
        fs::create_dir_all(dir.join("readonly-rust")).unwrap();

        fs::write(
            dir.join("base-rust/SKILL.md"),
            r#"+++
name = "base-rust"
abstract = true
tools = ["repo_map", "search", "read_file", "workspace_snapshot", "cargo"]

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
