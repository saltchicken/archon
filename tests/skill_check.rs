use std::fs;
use std::path::Path;

use archon::core::config::{Config, MemoryScope};
use archon::core::skill_check::{
    check_scanned_registry, check_skills_in_dir, check_workspace_skills, scan_raw_dir,
};

#[test]
fn skill_check_isolates_global_overrides_supports_cross_scope_extends_and_checks_optional_hook_args()
 {
    let tmp = tempfile::tempdir().unwrap();
    let global_dir = tmp.path().join("global");
    let ws_dir = tmp.path().join("workspace");

    // Global: valid base + valid skill `shared`
    fs::create_dir_all(global_dir.join("base-rust")).unwrap();
    fs::write(
        global_dir.join("base-rust/SKILL.md"),
        "+++\nname = \"base-rust\"\nabstract = true\ntools = [\"read_file\"]\n+++\nBase.",
    )
    .unwrap();
    fs::create_dir_all(global_dir.join("shared")).unwrap();
    fs::write(
        global_dir.join("shared/SKILL.md"),
        "+++\nname = \"shared\"\nextends = \"base-rust\"\n+++\nGlobal shared.",
    )
    .unwrap();

    // Workspace: broken override of `shared` + valid `ws-child` extending global `base-rust`
    // + skill with optional no-default arg wired to a required tool parameter
    fs::create_dir_all(ws_dir.join("shared")).unwrap();
    fs::write(
        ws_dir.join("shared/SKILL.md"),
        "+++\nname = \"shared\"\nextends = \"missing-parent\"\n+++\nBroken ws override.",
    )
    .unwrap();
    fs::create_dir_all(ws_dir.join("ws-child")).unwrap();
    fs::write(
        ws_dir.join("ws-child/SKILL.md"),
        "+++\nname = \"ws-child\"\nextends = \"base-rust\"\n+++\nChild.",
    )
    .unwrap();
    fs::create_dir_all(ws_dir.join("opt-hook")).unwrap();
    fs::write(
        ws_dir.join("opt-hook/SKILL.md"),
        r#"+++
name = "opt-hook"
tools = ["read_file"]

[[args]]
name = "file"
type = "path"
required = false

[[hooks]]
trigger = "on_activate"
tool = "read_file"
args = { file_path = "{{file}}" }
+++
Read optional file."#,
    )
    .unwrap();

    let cfg = Config::default();

    // Combined check: global `shared` must stay passing while workspace `shared` fails.
    let mut combined = scan_raw_dir(&global_dir, "global", MemoryScope::Global);
    combined.extend(scan_raw_dir(&ws_dir, "workspace", MemoryScope::Workspace));
    let results = check_scanned_registry(combined, &[], &cfg, false, None);

    let global_shared = results
        .iter()
        .find(|r| r.name == "shared" && r.scope_label == "global")
        .unwrap();
    assert!(
        global_shared.is_ok(true),
        "global shared should not inherit workspace override errors: {:?}",
        global_shared.diagnostics
    );

    let ws_shared = results
        .iter()
        .find(|r| r.name == "shared" && r.scope_label == "workspace")
        .unwrap();
    assert!(!ws_shared.is_ok(false));

    // Workspace-only check resolves `ws-child` against global `base-rust`.
    let ws_report = check_workspace_skills(&ws_dir, &global_dir, &cfg, false, None);
    let ws_child = ws_report
        .results
        .iter()
        .find(|r| r.name == "ws-child")
        .unwrap();
    assert!(
        ws_child.is_ok(true),
        "ws-child should extend global base-rust cleanly: {:?}",
        ws_child.diagnostics
    );

    // Optional no-default arg wired to required `read_file.file_path` must be flagged.
    let opt_hook = ws_report
        .results
        .iter()
        .find(|r| r.name == "opt-hook")
        .unwrap();
    assert!(!opt_hook.is_ok(false));
    assert!(
        opt_hook
            .diagnostics
            .iter()
            .any(|d| d.message.contains("when optional argument(s) are omitted")),
        "{:?}",
        opt_hook.diagnostics
    );
}

#[test]
fn skill_check_flags_super_in_hooks_and_root_disabled_inherited_tools_and_duplicate_shadowing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // Duplicate name in same scope: `a_dup` has broken `extends`, `b_dup` is valid.
    fs::create_dir_all(dir.join("a_dup")).unwrap();
    fs::write(
        dir.join("a_dup/SKILL.md"),
        "+++\nname = \"dup\"\nextends = \"missing-parent\"\n+++\nBroken dup.",
    )
    .unwrap();
    fs::create_dir_all(dir.join("b_dup")).unwrap();
    fs::write(
        dir.join("b_dup/SKILL.md"),
        "+++\nname = \"dup\"\n+++\nValid dup.",
    )
    .unwrap();

    // Root skill using `{{super}}` in prompt and hook.
    fs::create_dir_all(dir.join("bad-super")).unwrap();
    fs::write(
        dir.join("bad-super/SKILL.md"),
        r#"+++
name = "bad-super"
tools = ["repo_map"]

[[hooks]]
trigger = "on_activate"
tool = "repo_map"
args = { action = "map", target_path = "{{super}}" }
+++
Use {{super}} without extends."#,
    )
    .unwrap();

    // Parent with a tool disabled in config; child inherits it.
    fs::create_dir_all(dir.join("base-edit")).unwrap();
    fs::write(
        dir.join("base-edit/SKILL.md"),
        "+++\nname = \"base-edit\"\nabstract = true\ntools = [\"edit_file\"]\n+++\nBase.",
    )
    .unwrap();
    fs::create_dir_all(dir.join("child-edit")).unwrap();
    fs::write(
        dir.join("child-edit/SKILL.md"),
        "+++\nname = \"child-edit\"\nextends = \"base-edit\"\ntools = [\"read_file\"]\n+++\nChild.",
    )
    .unwrap();

    let mut cfg = Config::default();
    cfg.app.enabled_tools.retain(|t| t != "edit_file");

    let report = check_skills_in_dir(dir, "test", &cfg, true, None);

    // `a_dup` must retain its own `unknown parent skill 'missing-parent'` error rather than inheriting `b_dup`.
    let a_dup = report
        .results
        .iter()
        .find(|r| r.file_path.ends_with("a_dup/SKILL.md"))
        .unwrap();
    assert!(
        a_dup
            .diagnostics
            .iter()
            .any(|d| d.message.contains("unknown parent skill 'missing-parent'")),
        "{:?}",
        a_dup.diagnostics
    );

    // `bad-super` must flag `{{super}}` in both hook and prompt.
    let bad_super = report
        .results
        .iter()
        .find(|r| r.name == "bad-super")
        .unwrap();
    assert!(
        bad_super
            .diagnostics
            .iter()
            .any(|d| d.message.contains("Hook #1") && d.message.contains("{{super}}")),
        "{:?}",
        bad_super.diagnostics
    );
    assert!(
        bad_super.diagnostics.iter().any(|d| d
            .message
            .contains("Prompt references undeclared template variable `{{super}}`")),
        "{:?}",
        bad_super.diagnostics
    );

    // `child-edit` must warn that inherited `edit_file` is disabled even when filtered to `child-edit`.
    let child_only = check_skills_in_dir(dir, "test", &cfg, true, Some("child-edit"));
    assert_eq!(child_only.results.len(), 1);
    assert!(
        child_only.results[0].diagnostics.iter().any(|d| d
            .message
            .contains("Inherited tool 'edit_file' is not enabled")),
        "{:?}",
        child_only.results[0].diagnostics
    );
}

#[test]
fn skill_check_validates_auto_approve_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    fs::create_dir_all(dir.join("base-aa")).unwrap();
    fs::write(
        dir.join("base-aa/SKILL.md"),
        "+++\nname = \"base-aa\"\nabstract = true\ntools = [\"cargo\", \"edit_file\"]\nauto_approve = [\"*\"]\n+++\nBase.",
    )
    .unwrap();

    // Valid child using `!*` and `cargo:check`.
    fs::create_dir_all(dir.join("valid-child")).unwrap();
    fs::write(
        dir.join("valid-child/SKILL.md"),
        "+++\nname = \"valid-child\"\nextends = \"base-aa\"\nauto_approve = [\"!*\", \"cargo:check\", \"edit_file\"]\n+++\nValid.",
    )
    .unwrap();

    // Invalid rules: unknown tool, unsupported subaction, unknown subaction, whitespace around `:`.
    fs::create_dir_all(dir.join("bad-rules")).unwrap();
    fs::write(
        dir.join("bad-rules/SKILL.md"),
        "+++\nname = \"bad-rules\"\ntools = [\"read_file\", \"cargo\"]\nauto_approve = [\"no_such_tool\", \"read_file:sub\", \"cargo:bogus\", \"cargo : check\"]\n+++\nBad.",
    )
    .unwrap();

    // Warnings: negation without `extends`, and rule targeting tool excluded from skill's `tools`.
    fs::create_dir_all(dir.join("warn-rules")).unwrap();
    fs::write(
        dir.join("warn-rules/SKILL.md"),
        "+++\nname = \"warn-rules\"\ntools = [\"read_file\"]\nauto_approve = [\"!edit_file\", \"edit_file\"]\n+++\nWarn.",
    )
    .unwrap();

    let cfg = Config::default();
    let report = check_skills_in_dir(dir, "test", &cfg, false, None);

    let valid_child = report
        .results
        .iter()
        .find(|r| r.name == "valid-child")
        .unwrap();
    assert!(
        valid_child.is_ok(true),
        "valid-child should pass strict skill check: {:?}",
        valid_child.diagnostics
    );

    let bad_rules = report
        .results
        .iter()
        .find(|r| r.name == "bad-rules")
        .unwrap();
    assert!(!bad_rules.is_ok(false));
    let bad_msgs: Vec<&str> = bad_rules
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        bad_msgs
            .iter()
            .any(|m| m.contains("Unknown tool 'no_such_tool'")),
        "{bad_msgs:?}"
    );
    assert!(
        bad_msgs
            .iter()
            .any(|m| m.contains("does not support subactions")),
        "{bad_msgs:?}"
    );
    assert!(
        bad_msgs
            .iter()
            .any(|m| m.contains("Unknown subaction 'bogus'")),
        "{bad_msgs:?}"
    );
    assert!(
        bad_msgs
            .iter()
            .any(|m| m.contains("Unexpected whitespace around ':'")),
        "{bad_msgs:?}"
    );

    let warn_rules = report
        .results
        .iter()
        .find(|r| r.name == "warn-rules")
        .unwrap();
    assert!(warn_rules.is_ok(false));
    assert!(!warn_rules.is_ok(true));
    let warn_msgs: Vec<&str> = warn_rules
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        warn_msgs
            .iter()
            .any(|m| m.contains("has no effect on a skill without `extends`")),
        "{warn_msgs:?}"
    );
    assert!(
        warn_msgs
            .iter()
            .any(|m| m.contains("not enabled by this skill's `tools`")),
        "{warn_msgs:?}"
    );
}

#[test]
fn repo_skills_directory_passes_skill_check() {
    let repo_skills = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
    if repo_skills.is_dir() {
        let cfg = Config::default();
        let report = check_skills_in_dir(&repo_skills, "repo", &cfg, false, None);
        assert!(
            report.is_ok(true),
            "Repo skills check failed:\n{}",
            report.format_report(true)
        );
    }
}

#[test]
fn skill_check_and_resolve_support_tool_groups() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    fs::create_dir_all(dir.join("base-all")).unwrap();
    fs::write(
        dir.join("base-all/SKILL.md"),
        "+++\nname = \"base-all\"\nabstract = true\ntools = [\"*\"]\n+++\nBase.",
    )
    .unwrap();
    fs::create_dir_all(dir.join("git-helper")).unwrap();
    fs::write(
        dir.join("git-helper/SKILL.md"),
        "+++\nname = \"git-helper\"\ntools = [\"@git\", \"read_file\"]\n+++\nGit helper.",
    )
    .unwrap();
    fs::create_dir_all(dir.join("offline-agent")).unwrap();
    fs::write(
        dir.join("offline-agent/SKILL.md"),
        "+++\nname = \"offline-agent\"\nextends = \"base-all\"\ntools = [\"!@web\"]\n+++\nOffline.",
    )
    .unwrap();

    let cfg = Config::default();
    let report = check_skills_in_dir(dir, "test", &cfg, true, None);
    assert!(
        report.is_ok(true),
        "Expected group-based skills to pass check:\n{}",
        report.format_report(true)
    );

    let skills = archon::core::skills::discover_skills(&dir.to_string_lossy(), None);
    let git_helper = skills.iter().find(|s| s.name == "git-helper").unwrap();
    assert!(git_helper.allows_tool("git"));
    assert!(git_helper.allows_tool("github_repo"));
    assert!(git_helper.allows_tool("read_file"));
    assert!(!git_helper.allows_tool("cargo"));

    let offline = skills.iter().find(|s| s.name == "offline-agent").unwrap();
    assert!(!offline.allows_tool("fetch_url"));
    assert!(!offline.allows_tool("curl"));
    assert!(!offline.allows_tool("browser"));
    assert!(offline.allows_tool("git"));
}

#[test]
fn skill_check_and_resolve_support_custom_tool_groups_and_catch_bad_members() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    fs::create_dir_all(dir.join("custom-ok")).unwrap();
    fs::write(
        dir.join("custom-ok/SKILL.md"),
        "+++\nname = \"custom-ok\"\ntools = [\"@myteam\"]\n+++\nUse custom team tools.",
    )
    .unwrap();

    fs::create_dir_all(dir.join("custom-bad")).unwrap();
    fs::write(
        dir.join("custom-bad/SKILL.md"),
        "+++\nname = \"custom-bad\"\ntools = [\"@broken\"]\n+++\nBroken group.",
    )
    .unwrap();

    let mut cfg = Config::default();
    cfg.tool_groups
        .insert("myteam".into(), vec!["git".into(), "cargo".into()]);
    cfg.tool_groups
        .insert("broken".into(), vec!["git".into(), "@no_such_sub".into()]);

    let report_ok = check_skills_in_dir(dir, "test", &cfg, true, Some("custom-ok"));
    assert!(
        report_ok.is_ok(true),
        "Expected custom tool group skill to pass:\n{}",
        report_ok.format_report(true)
    );

    let report_bad = check_skills_in_dir(dir, "test", &cfg, true, Some("custom-bad"));
    assert!(
        !report_bad.is_ok(false),
        "Expected broken nested custom group to fail skill check"
    );

    let skills = archon::core::skills::discover_skills_with_groups(
        &dir.to_string_lossy(),
        None,
        &cfg.tool_groups,
    );
    let custom_ok = skills.iter().find(|s| s.name == "custom-ok").unwrap();
    assert!(custom_ok.allows_tool("git"));
    assert!(custom_ok.allows_tool("cargo"));
    assert!(!custom_ok.allows_tool("browser"));
}
