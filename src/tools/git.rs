use crate::tools::{
    auto_schema, ensure_path_or_ancestor_within_workspace, parse_args, resolve_existing_path,
    run_workspace_command, validate_relative_path, workspace_root,
};
use anyhow::{bail, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum GitAction {
    /// Show working tree status (`git status --short --branch`)
    Status,
    /// Show changes between commits, commit and working tree, etc.
    Diff,
    /// Show commit logs (`git log --oneline --decorate -n <limit>`)
    Log,
    /// Show a specific commit's metadata and patch (`git show <target>`)
    Show,
    /// Show what revision and author last modified each line of a file (`git blame <path>`)
    Blame,
    /// Stage files (`git add -- <path>`). Pass "." in `path` to stage all changes.
    Add,
    /// Commit staged changes (`git commit -m <message>`)
    Commit,
}

#[derive(Deserialize, JsonSchema)]
struct GitArgs {
    /// The git operation to perform.
    action: GitAction,
    /// Optional git ref, commit SHA, branch, range (e.g. "HEAD~2..HEAD", "main"), or "--staged" (for diff).
    #[serde(default)]
    target: String,
    /// Optional relative file or directory path within the workspace to scope the operation. Required for `blame` and `add` (use "." to stage all).
    #[serde(default)]
    path: String,
    /// Maximum number of log entries to return for `log` (default: 15, max: 100).
    #[serde(default)]
    limit: Option<usize>,
    /// Commit message required when `action` is `commit`.
    #[serde(default)]
    message: String,
}

pub fn schema() -> Value {
    auto_schema::<GitArgs>(
        "git",
        "Executes git operations (status, diff, log, show, blame, add, commit) within the workspace.",
    )
}

fn validate_git_target<'a>(raw: &'a str, allow_staged: bool) -> Result<Option<&'a str>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if allow_staged && (trimmed == "--staged" || trimmed == "--cached") {
        return Ok(Some(trimmed));
    }
    if trimmed.starts_with('-') {
        bail!("Security violation: git target cannot start with '-': '{trimmed}'");
    }
    if trimmed.contains(['\0', '\n', '\r']) {
        bail!("Invalid characters in git target");
    }
    Ok(Some(trimmed))
}

fn validate_scoped_path(raw: &str, must_exist: bool) -> Result<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        if must_exist {
            bail!("'path' is required for this git action");
        }
        return Ok(None);
    }

    if must_exist {
        let (cwd, canon) = resolve_existing_path(trimmed)?;
        let rel = pathdiff::diff_paths(&canon, &cwd).unwrap_or(canon);
        Ok(Some(rel.to_string_lossy().into_owned()))
    } else {
        let rel = validate_relative_path(trimmed)?;
        let cwd = workspace_root()?;
        ensure_path_or_ancestor_within_workspace(&cwd.join(rel), &cwd, trimmed)?;
        Ok(Some(rel.to_string_lossy().into_owned()))
    }
}

pub async fn execute(json_args: &Value, max_output_size: usize) -> Result<String> {
    let args: GitArgs = parse_args(json_args, "git")?;
    let mut cmd_args: Vec<String> = vec!["--no-pager".into()];

    let empty_msg = match args.action {
        GitAction::Status => {
            cmd_args.extend(["status".into(), "--short".into(), "--branch".into()]);
            if let Some(p) = validate_scoped_path(&args.path, false)? {
                cmd_args.push("--".into());
                cmd_args.push(p);
            }
            "Working tree clean."
        }
        GitAction::Diff => {
            cmd_args.extend(["diff".into(), "--no-ext-diff".into()]);
            if let Some(t) = validate_git_target(&args.target, true)? {
                cmd_args.push(t.into());
            }
            if let Some(p) = validate_scoped_path(&args.path, false)? {
                cmd_args.push("--".into());
                cmd_args.push(p);
            }
            "No differences found."
        }
        GitAction::Log => {
            let n = args.limit.unwrap_or(15).clamp(1, 100);
            cmd_args.extend([
                "log".into(),
                "--oneline".into(),
                "--decorate".into(),
                format!("-n{n}"),
            ]);
            if let Some(t) = validate_git_target(&args.target, false)? {
                cmd_args.push(t.into());
            }
            if let Some(p) = validate_scoped_path(&args.path, false)? {
                cmd_args.push("--".into());
                cmd_args.push(p);
            }
            "No commits found."
        }
        GitAction::Show => {
            cmd_args.extend(["show".into(), "--no-ext-diff".into()]);
            let target = validate_git_target(&args.target, false)?.unwrap_or("HEAD");
            cmd_args.push(target.into());
            if let Some(p) = validate_scoped_path(&args.path, false)? {
                cmd_args.push("--".into());
                cmd_args.push(p);
            }
            "No commit output."
        }
        GitAction::Blame => {
            let path = validate_scoped_path(&args.path, true)?
                .expect("must_exist guarantees Some");
            cmd_args.push("blame".into());
            if let Some(t) = validate_git_target(&args.target, false)? {
                cmd_args.push(t.into());
            }
            cmd_args.push("--".into());
            cmd_args.push(path);
            "No blame output."
        }
        GitAction::Add => {
            let path = validate_scoped_path(&args.path, false)?
                .ok_or_else(|| anyhow::anyhow!("'path' is required for git add (use '.' for all)"))?;
            cmd_args.extend(["add".into(), "--".into(), path]);
            "Files staged successfully."
        }
        GitAction::Commit => {
            let msg = args.message.trim();
            if msg.is_empty() {
                bail!("'message' cannot be empty for git commit");
            }
            cmd_args.extend(["commit".into(), "-m".into(), msg.to_string()]);
            "Commit created."
        }
    };

    run_workspace_command("git", &cmd_args, empty_msg, max_output_size).await
}
