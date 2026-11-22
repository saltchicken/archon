use archon::core::config::Config;
use archon::tools::BlockingTool;
use archon::tools::ToolContext;
use archon::tools::fs;
use archon::tools::workspace_snapshot;

#[test]
fn hidden_files_and_folders_blocked_by_default_and_configurable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(tmp.path()).unwrap();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
    std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(root.join(".gitignore"), "/target\n").unwrap();
    std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(root.join(".venv/pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::write(root.join(".github/workflows/ci.yml"), "name: CI\n").unwrap();

    let mut ctx = ToolContext {
        root: root.clone(),
        max_output_size: 65536,
        active_skill: None,
        config: Config::default(),
    };

    // 1. Default (`allow_hidden = false`): all hidden files and folders are blocked
    assert!(ctx.ensure_not_excluded(&root.join("src/main.rs")).is_ok());
    assert!(ctx.ensure_not_excluded(&root.join(".gitignore")).is_err());
    assert!(ctx.ensure_not_excluded(&root.join(".github")).is_err());
    assert!(
        ctx.ensure_not_excluded(&root.join(".github/workflows/ci.yml"))
            .is_err()
    );
    assert!(ctx.ensure_not_excluded(&root.join(".venv")).is_err());
    assert!(
        ctx.ensure_not_excluded(&root.join(".venv/pyvenv.cfg"))
            .is_err()
    );
    assert!(
        ctx.ensure_not_excluded(&root.join(".git/hooks/pre-commit"))
            .is_err()
    );

    let walked = ctx.walk_workspace_files(&root, &root, &[], &[]).unwrap();
    let rel_paths: Vec<&str> = walked.iter().map(|(_, r)| r.as_str()).collect();
    assert_eq!(rel_paths, vec!["src/main.rs"]);

    // 2. Opt-in via `allow_hidden = true`: non-excluded dotfiles are allowed,
    //    while `.git`, `.venv`, and `.env*` remain protected by DEFAULT_EXCLUDE_PATTERNS.
    ctx.config.app.allow_hidden = true;
    assert!(ctx.ensure_not_excluded(&root.join(".gitignore")).is_ok());
    assert!(
        ctx.ensure_not_excluded(&root.join(".github/workflows/ci.yml"))
            .is_ok()
    );
    assert!(ctx.ensure_not_excluded(&root.join(".env")).is_err());
    assert!(
        ctx.ensure_not_excluded(&root.join(".venv/pyvenv.cfg"))
            .is_err()
    );
    assert!(
        ctx.ensure_not_excluded(&root.join(".git/hooks/pre-commit"))
            .is_err()
    );

    let walked_hidden = ctx.walk_workspace_files(&root, &root, &[], &[]).unwrap();
    let rel_hidden: Vec<&str> = walked_hidden.iter().map(|(_, r)| r.as_str()).collect();
    assert_eq!(
        rel_hidden,
        vec![".github/workflows/ci.yml", ".gitignore", "src/main.rs"]
    );

    // 3. Selective whitelist via `!` negation in `workspace_snapshot.exclude`
    //    while keeping `allow_hidden = false`.
    ctx.config.app.allow_hidden = false;
    ctx.config
        .workspace_snapshot
        .exclude
        .push("!.gitignore".into());
    assert!(ctx.ensure_not_excluded(&root.join(".gitignore")).is_ok());
    assert!(
        ctx.ensure_not_excluded(&root.join(".github/workflows/ci.yml"))
            .is_err()
    );

    // 4. Nested hidden file negation allows ancestor directory traversal in
    //    resolve_existing_under_root, resolve_scan_roots, and workspace_snapshot.
    ctx.config
        .workspace_snapshot
        .exclude
        .push("!.github/workflows/ci.yml".into());
    assert!(
        ctx.ensure_not_excluded(&root.join(".github/workflows/ci.yml"))
            .is_ok()
    );
    assert!(
        ctx.resolve_existing_under_root(".github", "escapes")
            .is_ok()
    );
    assert!(
        ctx.resolve_existing_under_root(".github/workflows", "escapes")
            .is_ok()
    );
    assert!(ctx.resolve_scan_roots(".github", "escapes").is_ok());

    let walked_neg = ctx.walk_workspace_files(&root, &root, &[], &[]).unwrap();
    let rel_neg: Vec<&str> = walked_neg.iter().map(|(_, r)| r.as_str()).collect();
    assert_eq!(
        rel_neg,
        vec![".github/workflows/ci.yml", ".gitignore", "src/main.rs"]
    );

    // 5. `workspace_snapshot` respects bare directory exclusions (`secret_dir`)
    //    and blocks all descendant files inside it.
    std::fs::create_dir_all(root.join("secret_dir/sub")).unwrap();
    std::fs::write(root.join("secret_dir/sub/token.txt"), "top-secret\n").unwrap();
    ctx.config
        .workspace_snapshot
        .exclude
        .push("secret_dir".into());
    let snap = workspace_snapshot::Tool::execute(
        workspace_snapshot::WorkspaceSnapshotArgs { tree_only: None },
        &ctx,
    )
    .unwrap();
    assert!(!snap.contains("secret_dir"));
    assert!(!snap.contains("token.txt"));
    assert!(!snap.contains("top-secret"));

    // 6. `fs` copy, rename, and remove reject directories containing hidden or
    //    excluded children upfront without leaving partial copies behind.
    std::fs::create_dir_all(root.join("pkg")).unwrap();
    std::fs::write(root.join("pkg/lib.rs"), "pub fn f() {}\n").unwrap();
    std::fs::write(root.join("pkg/.hidden_secret"), "nope\n").unwrap();

    let copy_res = fs::Tool::execute(
        serde_json::from_value(serde_json::json!({
            "action": "copy",
            "path": "pkg",
            "destination": "pkg_copy"
        }))
        .unwrap(),
        &ctx,
    );
    assert!(copy_res.is_err());
    assert!(!root.join("pkg_copy").exists());

    let rename_res = fs::Tool::execute(
        serde_json::from_value(serde_json::json!({
            "action": "rename",
            "path": "pkg",
            "destination": "pkg_moved"
        }))
        .unwrap(),
        &ctx,
    );
    assert!(rename_res.is_err());
    assert!(root.join("pkg").exists());
    assert!(!root.join("pkg_moved").exists());

    let remove_res = fs::Tool::execute(
        serde_json::from_value(serde_json::json!({
            "action": "remove",
            "path": "pkg",
            "recursive": true
        }))
        .unwrap(),
        &ctx,
    );
    assert!(remove_res.is_err());
    assert!(root.join("pkg").exists());
}
