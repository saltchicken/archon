use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::json;

use archon::core::config::{Config, MemoryScope};
use archon::core::session::ChatMessage;
use archon::core::skills::{
    HookTrigger, TemplateContext, check_skill_activation, collect_after_turn_hooks,
    collect_due_hooks, compose_system_prompt, discover_skills, inspect_template,
    parse_skill_markdown, render_template, replace_super_placeholder, resolve_skill_inputs,
    resolve_skills,
};

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
        None,
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
        None,
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
    let inputs: BTreeMap<String, serde_json::Value> =
        [("on".into(), json!(true))].into_iter().collect();
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

use archon::tools::{ToolCall, ToolFunction};

#[test]
fn skill_auto_approve_inheritance_negation_and_matching() {
    let make_call = |name: &str, action: Option<&str>| ToolCall {
        function: ToolFunction {
            name: name.to_string(),
            arguments: match action {
                Some(a) => json!({ "action": a }),
                None => json!({}),
            },
        },
        thought_signature: None,
    };

    // Root skill trims, deduplicates, and strips stray `!` negations.
    let root = parse_skill_markdown(
        r#"+++
name = "root"
tools = ["edit_file", "cargo", "read_file"]
auto_approve = [" edit_file ", "edit_file", "!read_file", "cargo:check", "cargo:test"]
+++
Root."#,
        Path::new("/tmp/root"),
    )
    .unwrap();

    // Parent with wildcard `*` and subaction rules.
    let base_wild = parse_skill_markdown(
        r#"+++
name = "base-wild"
abstract = true
auto_approve = ["*"]
+++
Wild."#,
        Path::new("/tmp/base-wild"),
    )
    .unwrap();

    // Child negates a tool family (`!cargo`) and a single tool (`!edit_file`) against `*`.
    let child_neg_wild = parse_skill_markdown(
        r#"+++
name = "child-neg-wild"
extends = "base-wild"
auto_approve = ["!cargo", "!edit_file"]
+++
Child."#,
        Path::new("/tmp/child-neg-wild"),
    )
    .unwrap();

    // Grandchild re-enables a single subaction (`cargo:check`) after `!cargo`.
    let grandchild_sub = parse_skill_markdown(
        r#"+++
name = "grandchild-sub"
extends = "child-neg-wild"
auto_approve = ["cargo:check"]
+++
Grandchild."#,
        Path::new("/tmp/grandchild-sub"),
    )
    .unwrap();

    // Child negating inherited `cargo:check`/`cargo:test` via `!cargo:*`, plus `!*` on wildcard.
    let child_clear_prefix = parse_skill_markdown(
        r#"+++
name = "child-clear-prefix"
extends = "root"
auto_approve = ["!cargo:*"]
+++
Clear prefix."#,
        Path::new("/tmp/child-clear-prefix"),
    )
    .unwrap();

    let child_clear_wild = parse_skill_markdown(
        r#"+++
name = "child-clear-wild"
extends = "base-wild"
auto_approve = ["!*"]
+++
Clear wild."#,
        Path::new("/tmp/child-clear-wild"),
    )
    .unwrap();

    // Child negating one subaction (`!cargo:fmt`) from parent `cargo`.
    let base_cargo = parse_skill_markdown(
        r#"+++
name = "base-cargo"
abstract = true
tools = ["cargo"]
auto_approve = ["cargo"]
+++
Base cargo."#,
        Path::new("/tmp/base-cargo"),
    )
    .unwrap();
    let child_no_fmt = parse_skill_markdown(
        r#"+++
name = "child-no-fmt"
extends = "base-cargo"
auto_approve = ["!cargo:fmt"]
+++
No fmt."#,
        Path::new("/tmp/child-no-fmt"),
    )
    .unwrap();

    let resolved = resolve_skills(vec![
        root,
        base_wild,
        child_neg_wild,
        grandchild_sub,
        child_clear_prefix,
        child_clear_wild,
        base_cargo,
        child_no_fmt,
    ]);
    let find = |n: &str| resolved.iter().find(|s| s.name == n).unwrap();

    let r_root = find("root");
    assert_eq!(
        r_root.auto_approve,
        vec!["edit_file", "cargo:check", "cargo:test"]
    );
    assert!(r_root.auto_approve_allows(&make_call("edit_file", None)));
    assert!(r_root.auto_approve_allows(&make_call("cargo", Some("check"))));
    assert!(!r_root.auto_approve_allows(&make_call("cargo", Some("fmt"))));
    assert!(!r_root.auto_approve_allows(&make_call("read_file", None)));

    let r_child_wild = find("child-neg-wild");
    assert!(r_child_wild.auto_approve_allows(&make_call("read_file", None)));
    assert!(!r_child_wild.auto_approve_allows(&make_call("edit_file", None)));
    assert!(!r_child_wild.auto_approve_allows(&make_call("cargo", Some("check"))));

    let r_grand = find("grandchild-sub");
    assert!(r_grand.auto_approve_allows(&make_call("cargo", Some("check"))));
    assert!(!r_grand.auto_approve_allows(&make_call("cargo", Some("fmt"))));
    assert!(!r_grand.auto_approve_allows(&make_call("edit_file", None)));

    let r_clear_prefix = find("child-clear-prefix");
    assert_eq!(r_clear_prefix.auto_approve, vec!["edit_file"]);
    assert!(!r_clear_prefix.auto_approve_allows(&make_call("cargo", Some("check"))));
    assert!(!r_clear_prefix.auto_approve_allows(&make_call("cargo", Some("test"))));

    let r_clear_wild = find("child-clear-wild");
    assert!(r_clear_wild.auto_approve.is_empty());
    assert!(!r_clear_wild.auto_approve_allows(&make_call("read_file", None)));

    let r_no_fmt = find("child-no-fmt");
    assert!(r_no_fmt.auto_approve_allows(&make_call("cargo", Some("check"))));
    assert!(!r_no_fmt.auto_approve_allows(&make_call("cargo", Some("fmt"))));
}

#[test]
fn skill_auto_approve_multi_parent_explicit_allow_wins_regardless_of_order() {
    let make_call = |name: &str, action: Option<&str>| ToolCall {
        function: ToolFunction {
            name: name.to_string(),
            arguments: match action {
                Some(a) => json!({ "action": a }),
                None => json!({}),
            },
        },
        thought_signature: None,
    };

    let base_wild = parse_skill_markdown(
        "+++\nname = \"base-wild\"\nabstract = true\nauto_approve = [\"*\"]\n+++\nBase.",
        Path::new("/tmp/base-wild"),
    )
    .unwrap();
    let p_no_cargo = parse_skill_markdown(
        "+++\nname = \"p-no-cargo\"\nextends = \"base-wild\"\nabstract = true\nauto_approve = [\"!cargo\"]\n+++\nNo cargo.",
        Path::new("/tmp/p-no-cargo"),
    )
    .unwrap();
    let p_no_fmt = parse_skill_markdown(
        "+++\nname = \"p-no-fmt\"\nextends = \"base-wild\"\nabstract = true\nauto_approve = [\"!cargo:fmt\"]\n+++\nNo fmt.",
        Path::new("/tmp/p-no-fmt"),
    )
    .unwrap();
    let p_no_edit = parse_skill_markdown(
        "+++\nname = \"p-no-edit\"\nextends = \"base-wild\"\nabstract = true\nauto_approve = [\"!edit_file\"]\n+++\nNo edit.",
        Path::new("/tmp/p-no-edit"),
    )
    .unwrap();
    let p_cargo_star = parse_skill_markdown(
        "+++\nname = \"p-cargo-star\"\nabstract = true\nauto_approve = [\"cargo:*\"]\n+++\nCargo star.",
        Path::new("/tmp/p-cargo-star"),
    )
    .unwrap();
    let p_cargo_bare = parse_skill_markdown(
        "+++\nname = \"p-cargo-bare\"\nabstract = true\nauto_approve = [\"cargo\"]\n+++\nCargo bare.",
        Path::new("/tmp/p-cargo-bare"),
    )
    .unwrap();

    // `cargo:*` vs `!cargo` in both orders.
    let ab_star = parse_skill_markdown(
        "+++\nname = \"ab-star\"\nextends = [\"p-no-cargo\", \"p-cargo-star\"]\n+++\nAB.",
        Path::new("/tmp/ab-star"),
    )
    .unwrap();
    let ba_star = parse_skill_markdown(
        "+++\nname = \"ba-star\"\nextends = [\"p-cargo-star\", \"p-no-cargo\"]\n+++\nBA.",
        Path::new("/tmp/ba-star"),
    )
    .unwrap();

    // `cargo` vs `!cargo:fmt` in both orders.
    let ab_fmt = parse_skill_markdown(
        "+++\nname = \"ab-fmt\"\nextends = [\"p-no-fmt\", \"p-cargo-bare\"]\n+++\nAB.",
        Path::new("/tmp/ab-fmt"),
    )
    .unwrap();
    let ba_fmt = parse_skill_markdown(
        "+++\nname = \"ba-fmt\"\nextends = [\"p-cargo-bare\", \"p-no-fmt\"]\n+++\nBA.",
        Path::new("/tmp/ba-fmt"),
    )
    .unwrap();

    // `["*", "!cargo"]` + `["*", "!cargo:fmt"]`: cargo:check is allowed by p-no-fmt, cargo:fmt blocked by both.
    let ab_narrow = parse_skill_markdown(
        "+++\nname = \"ab-narrow\"\nextends = [\"p-no-cargo\", \"p-no-fmt\"]\n+++\nAB.",
        Path::new("/tmp/ab-narrow"),
    )
    .unwrap();
    let ba_narrow = parse_skill_markdown(
        "+++\nname = \"ba-narrow\"\nextends = [\"p-no-fmt\", \"p-no-cargo\"]\n+++\nBA.",
        Path::new("/tmp/ba-narrow"),
    )
    .unwrap();

    // `["*", "!cargo"]` + `["*", "!edit_file"]`: each parent explicitly allows what the other excluded.
    let ab_disjoint = parse_skill_markdown(
        "+++\nname = \"ab-disjoint\"\nextends = [\"p-no-cargo\", \"p-no-edit\"]\n+++\nAB.",
        Path::new("/tmp/ab-disjoint"),
    )
    .unwrap();
    let ba_disjoint = parse_skill_markdown(
        "+++\nname = \"ba-disjoint\"\nextends = [\"p-no-edit\", \"p-no-cargo\"]\n+++\nBA.",
        Path::new("/tmp/ba-disjoint"),
    )
    .unwrap();

    let resolved = resolve_skills(vec![
        base_wild,
        p_no_cargo,
        p_no_fmt,
        p_no_edit,
        p_cargo_star,
        p_cargo_bare,
        ab_star,
        ba_star,
        ab_fmt,
        ba_fmt,
        ab_narrow,
        ba_narrow,
        ab_disjoint,
        ba_disjoint,
    ]);
    let find = |n: &str| resolved.iter().find(|s| s.name == n).unwrap();

    for name in ["ab-star", "ba-star"] {
        let s = find(name);
        assert!(
            s.auto_approve_allows(&make_call("cargo", Some("check"))),
            "{name}"
        );
        assert!(
            s.auto_approve_allows(&make_call("cargo", Some("fmt"))),
            "{name}"
        );
        assert!(
            s.auto_approve_allows(&make_call("read_file", None)),
            "{name}"
        );
    }
    for name in ["ab-fmt", "ba-fmt"] {
        let s = find(name);
        assert!(
            s.auto_approve_allows(&make_call("cargo", Some("fmt"))),
            "{name}"
        );
        assert!(
            s.auto_approve_allows(&make_call("cargo", Some("check"))),
            "{name}"
        );
    }
    for name in ["ab-narrow", "ba-narrow"] {
        let s = find(name);
        assert!(
            s.auto_approve_allows(&make_call("cargo", Some("check"))),
            "{name}"
        );
        assert!(
            !s.auto_approve_allows(&make_call("cargo", Some("fmt"))),
            "{name}"
        );
    }
    for name in ["ab-disjoint", "ba-disjoint"] {
        let s = find(name);
        assert!(
            s.auto_approve_allows(&make_call("cargo", Some("check"))),
            "{name}"
        );
        assert!(
            s.auto_approve_allows(&make_call("edit_file", None)),
            "{name}"
        );
    }
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
