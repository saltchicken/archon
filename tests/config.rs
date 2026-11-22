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
