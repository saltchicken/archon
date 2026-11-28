use archon::config::{
    Config, ConfigSource, ToolProvenance, WorkspaceEnsureOutcome, build_provenance,
    ensure_workspace_config_with_io,
};
use std::io::Cursor;

#[test]
fn provenance_tracks_global_and_workspace_overrides() {
    let global_toml: toml::Table = toml::from_str(
        r#"
        [app]
        default_model = "qwen3.8:27b"
        enabled_tools = ["cargo", "git"]
        auto_approve = ["cargo:check"]

        [workspace_snapshot]
        git_root = true
        include = ["**/*"]
        exclude = ["target/**"]

        [dotfiles]
        allowed_dirs = ["fish", "kitty"]
        "#,
    )
    .unwrap();

    let ws_toml: toml::Table = toml::from_str(
        r#"
        workspace = "/tmp/demo"

        [app]
        enabled_tools = ["cargo", "read_file"]

        [workspace_snapshot]
        git_root = true
        include = ["**/*.rs"]
        file = ["README.md"]
        "#,
    )
    .unwrap();

    let prov = build_provenance(Some(&global_toml), Some(&ws_toml));
    assert!(prov.has_global_config);
    assert!(prov.has_workspace_config);
    assert_eq!(prov.default_model, ConfigSource::Global);
    assert_eq!(prov.enabled_tools, ConfigSource::WorkspaceOverridesGlobal);
    assert!(prov.enabled_tools.overwrote_global());
    assert_eq!(prov.auto_approve, ConfigSource::Global);
    assert_eq!(prov.dotfiles, ConfigSource::Global);
    assert_eq!(
        prov.workspace_snapshot,
        ConfigSource::WorkspaceOverridesGlobal
    );
    assert_eq!(
        prov.overwritten_keys,
        vec![
            "app.enabled_tools".to_string(),
            "workspace_snapshot.include".to_string(),
        ]
    );

    let mut cfg = Config {
        provenance: prov,
        ..Config::default()
    };
    cfg.app.enabled_tools = vec!["cargo".into(), "read_file".into()];

    assert_eq!(
        cfg.tool_provenance("cargo"),
        ToolProvenance::GlobalAndWorkspaceEnabled
    );
    assert_eq!(
        cfg.tool_provenance("read_file"),
        ToolProvenance::WorkspaceAdded
    );
    assert_eq!(cfg.tool_provenance("git"), ToolProvenance::WorkspaceRemoved);
    assert_eq!(
        cfg.tool_provenance("dotfiles"),
        ToolProvenance::GlobalAndWorkspaceDisabled
    );

    cfg.restrict_enabled_tools(&["cargo".to_string()]);
    assert_eq!(cfg.provenance.enabled_tools, ConfigSource::Cli);
    assert_eq!(
        cfg.tool_provenance("read_file"),
        ToolProvenance::CliDisabled
    );
}

#[test]
fn tool_groups_expand_in_enabled_core_provenance_and_cli() {
    let global_toml: toml::Table = toml::from_str(
        r#"
        [app]
        enabled_tools = ["@code", "@git"]
        core_tools = ["@code"]
        "#,
    )
    .unwrap();

    let ws_toml: toml::Table = toml::from_str(
        r#"
        workspace = "/tmp/demo"

        [app]
        enabled_tools = ["@code", "@rust", "@mycustom"]

        [tool_groups]
        mycustom = ["sqlite", "@planning"]
        cyclic_a = ["@cyclic_b", "git"]
        cyclic_b = ["@cyclic_a", "cargo"]
        "#,
    )
    .unwrap();

    let prov = build_provenance(Some(&global_toml), Some(&ws_toml));
    let mut cfg = Config {
        provenance: prov,
        ..Config::default()
    };
    cfg.app.enabled_tools = vec!["@code".into(), "@rust".into(), "@mycustom".into()];
    cfg.app.core_tools = vec!["@code".into()];
    // Explicitly define the groups referenced above (no compiled-in defaults).
    cfg.tool_groups.insert(
        "code".into(),
        vec![
            "read_file".into(),
            "edit_file".into(),
            "create_file".into(),
            "search".into(),
            "repo_map".into(),
            "fs".into(),
            "workspace_snapshot".into(),
        ],
    );
    cfg.tool_groups
        .insert("git".into(), vec!["git".into(), "github_repo".into()]);
    cfg.tool_groups.insert("rust".into(), vec!["cargo".into()]);
    cfg.tool_groups
        .insert("planning".into(), vec!["plan".into(), "todo".into()]);
    cfg.tool_groups
        .insert("mycustom".into(), vec!["sqlite".into(), "@planning".into()]);
    cfg.tool_groups
        .insert("cyclic_a".into(), vec!["@cyclic_b".into(), "git".into()]);
    cfg.tool_groups
        .insert("cyclic_b".into(), vec!["@cyclic_a".into(), "cargo".into()]);

    // Enabled tools expanded through top-level and nested custom groups.
    assert!(archon::tools::is_tool_enabled("read_file", &cfg));
    assert!(archon::tools::is_tool_enabled("cargo", &cfg));
    assert!(archon::tools::is_tool_enabled("sqlite", &cfg));
    assert!(archon::tools::is_tool_enabled("plan", &cfg));
    assert!(archon::tools::is_tool_enabled("todo", &cfg));
    assert!(!archon::tools::is_tool_enabled("git", &cfg));

    // Core tools expanded through `@code`.
    assert!(archon::tools::is_core_tool("read_file", &cfg));
    assert!(archon::tools::is_core_tool("workspace_snapshot", &cfg));
    assert!(!archon::tools::is_core_tool("cargo", &cfg));

    // Cyclic custom groups terminate safely without infinite recursion.
    let cyclic = archon::tools::resolve_tool_group("@cyclic_a", &cfg.tool_groups).unwrap();
    assert!(cyclic.contains(&"git".to_string()));
    assert!(cyclic.contains(&"cargo".to_string()));

    // Provenance accurately resolves `@group` across global and workspace configs.
    assert_eq!(
        cfg.tool_provenance("read_file"),
        ToolProvenance::GlobalAndWorkspaceEnabled
    );
    assert_eq!(cfg.tool_provenance("cargo"), ToolProvenance::WorkspaceAdded);
    assert_eq!(
        cfg.tool_provenance("sqlite"),
        ToolProvenance::WorkspaceAdded
    );
    assert_eq!(cfg.tool_provenance("git"), ToolProvenance::WorkspaceRemoved);
    assert_eq!(
        cfg.tool_provenance("browser"),
        ToolProvenance::GlobalAndWorkspaceDisabled
    );

    // CLI `--allowed-tools` validation & restriction with `@group`.
    assert!(cfg.validate_allowed_tools(&["@mycustom".into()]).is_ok());
    assert!(
        cfg.validate_allowed_tools(&["@nonexistent".into()])
            .is_err()
    );

    cfg.restrict_enabled_tools(&["@mycustom".into()]);
    assert!(archon::tools::is_tool_enabled("sqlite", &cfg));
    assert!(archon::tools::is_tool_enabled("plan", &cfg));
    assert!(!archon::tools::is_tool_enabled("read_file", &cfg));
    assert_eq!(
        cfg.tool_provenance("read_file"),
        ToolProvenance::CliDisabled
    );
}

#[test]
fn config_load_applies_workspaces_toml_overrides_and_allows_unconfigured_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let base_cfg_path = tmp.path().join("config.toml");
    let workspaces_path = tmp.path().join("workspaces.toml");
    let history_dir = tmp.path().join("history");

    let proj_dir = tmp.path().join("my-proj");
    let other_dir = tmp.path().join("unconfigured-proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::create_dir_all(&other_dir).unwrap();

    let canon_proj = dunce::canonicalize(&proj_dir).unwrap();
    let canon_other = dunce::canonicalize(&other_dir).unwrap();

    std::fs::write(
        &base_cfg_path,
        format!(
            r#"
            [app]
            history_dir = "{}"
            allow_hidden = false
            enabled_tools = ["read_file", "git"]

            [workspace_snapshot]
            file = ["global.md"]

            [[schedules]]
            name = "global-job"
            on_calendar = "daily"
            prompt = "global"
            "#,
            history_dir.display()
        ),
    )
    .unwrap();

    std::fs::write(
        &workspaces_path,
        format!(
            r#"
            [my-proj]
            workspace = "{}"

            [my-proj.app]
            allow_hidden = true
            enabled_tools = ["read_file", "cargo"]

            [my-proj.workspace_snapshot]
            include = ["**/*.rs"]
            file = ["local.md"]

            [[my-proj.schedules]]
            name = "proj-job"
            on_calendar = "hourly"
            prompt = "local"
            "#,
            canon_proj.display()
        ),
    )
    .unwrap();

    // 1. Configured directory receives overrides, provenance, and workspace schedules.
    let (cfg, err) = Config::load(&base_cfg_path, &workspaces_path, &proj_dir);
    assert!(err.is_none(), "unexpected config err: {err:?}");
    assert!(cfg.provenance.has_global_config);
    assert!(cfg.provenance.has_workspace_config);
    assert_eq!(
        cfg.workspace.as_deref(),
        Some(canon_proj.to_string_lossy().as_ref())
    );
    assert_eq!(cfg.workspace_name.as_deref(), Some("my-proj"));
    assert!(cfg.app.allow_hidden);
    assert_eq!(cfg.app.enabled_tools, vec!["read_file", "cargo"]);
    assert_eq!(cfg.workspace_snapshot.include, vec!["**/*.rs"]);
    assert_eq!(cfg.workspace_snapshot.file, vec!["local.md"]);
    assert_eq!(cfg.workspace_snapshot.global_files, vec!["global.md"]);
    assert_eq!(cfg.schedules.len(), 1);
    assert_eq!(cfg.schedules[0].name, "proj-job");
    assert_eq!(cfg.global_schedules.len(), 1);
    assert_eq!(cfg.global_schedules[0].name, "global-job");
    assert_eq!(
        cfg.provenance.overwritten_keys,
        vec![
            "app.allow_hidden".to_string(),
            "app.enabled_tools".to_string(),
        ]
    );

    // 2. Unconfigured directory runs cleanly with global config without creating any workspace dir.
    let (other_cfg, other_err) = Config::load(&base_cfg_path, &workspaces_path, &other_dir);
    assert!(other_err.is_none(), "unexpected config err: {other_err:?}");
    assert!(other_cfg.provenance.has_global_config);
    assert!(!other_cfg.provenance.has_workspace_config);
    assert_eq!(
        other_cfg.workspace.as_deref(),
        Some(canon_other.to_string_lossy().as_ref())
    );
    assert_eq!(other_cfg.workspace_name, None);
    assert!(!other_cfg.app.allow_hidden);
    assert_eq!(other_cfg.app.enabled_tools, vec!["read_file", "git"]);
    assert!(other_cfg.schedules.is_empty());
    assert_eq!(other_cfg.global_schedules.len(), 1);
    assert!(!tmp.path().join("workspaces").exists());
}

#[test]
fn ensure_workspace_config_defaults_to_no_and_exits_without_adding_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let base_cfg_path = tmp.path().join("config.toml");
    let workspaces_path = tmp.path().join("workspaces.toml");
    let proj_dir = tmp.path().join("new-proj");
    std::fs::create_dir_all(&proj_dir).unwrap();

    // Empty Enter (default = No)
    let mut out = Vec::new();
    let mut reader = Cursor::new(b"\n".as_slice());
    let outcome =
        ensure_workspace_config_with_io(&workspaces_path, &proj_dir, Some(&mut reader), &mut out)
            .unwrap();
    assert_eq!(outcome, WorkspaceEnsureOutcome::Declined);
    let printed = String::from_utf8(out).unwrap();
    assert!(printed.contains("[y/N]:"), "{printed}");
    let (cfg, _) = Config::load(&base_cfg_path, &workspaces_path, &proj_dir);
    assert!(!cfg.provenance.has_workspace_config);

    // Explicit "n"
    let mut out2 = Vec::new();
    let mut reader2 = Cursor::new(b"n\n".as_slice());
    let outcome2 =
        ensure_workspace_config_with_io(&workspaces_path, &proj_dir, Some(&mut reader2), &mut out2)
            .unwrap();
    assert_eq!(outcome2, WorkspaceEnsureOutcome::Declined);
    let (cfg2, _) = Config::load(&base_cfg_path, &workspaces_path, &proj_dir);
    assert!(!cfg2.provenance.has_workspace_config);
}

#[test]
fn ensure_workspace_config_adds_entry_on_yes_and_skips_prompt_when_already_configured() {
    let tmp = tempfile::tempdir().unwrap();
    let base_cfg_path = tmp.path().join("config.toml");
    let workspaces_path = tmp.path().join("workspaces.toml");
    let proj_dir = tmp.path().join("my.proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    let canon_proj = dunce::canonicalize(&proj_dir).unwrap();

    let mut out = Vec::new();
    let mut reader = Cursor::new(b"y\n".as_slice());
    let outcome =
        ensure_workspace_config_with_io(&workspaces_path, &proj_dir, Some(&mut reader), &mut out)
            .unwrap();
    assert_eq!(outcome, WorkspaceEnsureOutcome::Configured);

    let (cfg, err) = Config::load(&base_cfg_path, &workspaces_path, &proj_dir);
    assert!(err.is_none(), "unexpected config err: {err:?}");
    assert!(cfg.provenance.has_workspace_config);
    assert_eq!(
        cfg.workspace.as_deref(),
        Some(canon_proj.to_string_lossy().as_ref())
    );
    assert_eq!(cfg.workspace_name.as_deref(), Some("my.proj"));

    // Subsequent call (even non-interactive with reader = None) succeeds silently.
    let mut out2 = Vec::new();
    let outcome2 = ensure_workspace_config_with_io::<Cursor<&[u8]>, _>(
        &workspaces_path,
        &proj_dir,
        None,
        &mut out2,
    )
    .unwrap();
    assert_eq!(outcome2, WorkspaceEnsureOutcome::Configured);
    assert!(out2.is_empty());
}

#[test]
fn ensure_workspace_config_handles_collisions_reserved_names_and_non_interactive_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let base_cfg_path = tmp.path().join("config.toml");
    let workspaces_path = tmp.path().join("workspaces.toml");

    let work_api = tmp.path().join("work").join("api");
    let personal_api = tmp.path().join("personal").join("api");
    let reserved_dir = tmp.path().join("workspaces");
    let unconfigured = tmp.path().join("unconfigured");
    std::fs::create_dir_all(&work_api).unwrap();
    std::fs::create_dir_all(&personal_api).unwrap();
    std::fs::create_dir_all(&reserved_dir).unwrap();
    std::fs::create_dir_all(&unconfigured).unwrap();

    // Non-interactive call on an unconfigured directory must refuse without modifying workspaces.toml.
    let mut out_no_tty = Vec::new();
    let outcome_no_tty = ensure_workspace_config_with_io::<Cursor<&[u8]>, _>(
        &workspaces_path,
        &unconfigured,
        None,
        &mut out_no_tty,
    )
    .unwrap();
    assert_eq!(
        outcome_no_tty,
        WorkspaceEnsureOutcome::NoInteractiveTerminal
    );
    let (cfg_unconf, _) = Config::load(&base_cfg_path, &workspaces_path, &unconfigured);
    assert!(!cfg_unconf.provenance.has_workspace_config);

    // Add work/api -> [api]
    let mut out1 = Vec::new();
    let mut r1 = Cursor::new(b"yes\n".as_slice());
    assert_eq!(
        ensure_workspace_config_with_io(&workspaces_path, &work_api, Some(&mut r1), &mut out1)
            .unwrap(),
        WorkspaceEnsureOutcome::Configured
    );

    // Add personal/api -> must not collide with [api]
    let mut out2 = Vec::new();
    let mut r2 = Cursor::new(b"y\n".as_slice());
    assert_eq!(
        ensure_workspace_config_with_io(&workspaces_path, &personal_api, Some(&mut r2), &mut out2)
            .unwrap(),
        WorkspaceEnsureOutcome::Configured
    );

    // Add directory literally named `workspaces` -> must not collide with reserved key `workspaces`
    let mut out3 = Vec::new();
    let mut r3 = Cursor::new(b"y\n".as_slice());
    assert_eq!(
        ensure_workspace_config_with_io(&workspaces_path, &reserved_dir, Some(&mut r3), &mut out3)
            .unwrap(),
        WorkspaceEnsureOutcome::Configured
    );

    let (c1, e1) = Config::load(&base_cfg_path, &workspaces_path, &work_api);
    let (c2, e2) = Config::load(&base_cfg_path, &workspaces_path, &personal_api);
    let (c3, e3) = Config::load(&base_cfg_path, &workspaces_path, &reserved_dir);
    assert!(e1.is_none(), "{e1:?}");
    assert!(e2.is_none(), "{e2:?}");
    assert!(e3.is_none(), "{e3:?}");
    assert_eq!(c1.workspace_name.as_deref(), Some("api"));
    assert_eq!(c2.workspace_name.as_deref(), Some("personal-api"));
    assert_ne!(c3.workspace_name.as_deref(), Some("workspaces"));
}

#[test]
fn ensure_workspace_config_errors_without_mutating_invalid_toml() {
    let tmp = tempfile::tempdir().unwrap();
    let workspaces_path = tmp.path().join("workspaces.toml");
    let proj_dir = tmp.path().join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();

    let broken = "[broken\nworkspace = \"/tmp\"\n";
    std::fs::write(&workspaces_path, broken).unwrap();

    let mut out = Vec::new();
    let mut r = Cursor::new(b"y\n".as_slice());
    let err = ensure_workspace_config_with_io(&workspaces_path, &proj_dir, Some(&mut r), &mut out)
        .unwrap_err();
    assert!(err.to_string().contains("parse error"), "{err}");
    assert_eq!(std::fs::read_to_string(&workspaces_path).unwrap(), broken);
}
