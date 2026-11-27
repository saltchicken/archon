use archon::core::config::{Config, ConfigSource, ToolProvenance, build_provenance};

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
    cfg.tool_groups
        .insert("mycustom".into(), vec!["sqlite".into(), "@planning".into()]);
    cfg.tool_groups
        .insert("cyclic_a".into(), vec!["@cyclic_b".into(), "git".into()]);
    cfg.tool_groups
        .insert("cyclic_b".into(), vec!["@cyclic_a".into(), "cargo".into()]);

    // Enabled tools expanded through built-in and nested custom groups.
    assert!(archon::tools::is_tool_enabled("read_file", &cfg));
    assert!(archon::tools::is_tool_enabled("cargo", &cfg));
    assert!(archon::tools::is_tool_enabled("sqlite", &cfg));
    assert!(archon::tools::is_tool_enabled("task", &cfg));
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
    assert!(archon::tools::is_tool_enabled("task", &cfg));
    assert!(!archon::tools::is_tool_enabled("read_file", &cfg));
    assert_eq!(
        cfg.tool_provenance("read_file"),
        ToolProvenance::CliDisabled
    );
}
