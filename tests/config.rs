use archon::config::{Config, ConfigSource, ToolProvenance, build_provenance};

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
