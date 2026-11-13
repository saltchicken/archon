use super::{AsyncTool, ToolContext};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;

pub struct Tool;

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
    /// List local and remote branches with tracking and latest commit info (`git branch -a -vv`)
    Branch,
    /// Stage files (`git add -- <path>`). Pass "." in `path` to stage all changes.
    Add,
    /// Unstage staged files (`git restore --staged -- <path>`). Pass "." in `path` to unstage all changes.
    Unstage,
    /// Commit staged changes (`git commit -m <message>`)
    Commit,
    /// Stash working directory changes including untracked files (`git stash push -u`). Optional `message` and `path`.
    Stash,
    /// Apply and remove a stashed state (`git stash pop [target]`). Optional `target` like `stash@{0}`.
    StashPop,
}

#[derive(Deserialize, JsonSchema)]
pub struct GitArgs {
    /// The git operation to perform.
    action: GitAction,
    /// Optional git ref, commit SHA, branch, range (e.g. "HEAD~2..HEAD", "main"), "--staged" (for diff), or stash ref (e.g. "stash@{0}" for `stash_pop`).
    #[serde(default)]
    target: String,
    /// Optional relative file or directory path within the workspace to scope the operation. Required for `blame`, `add`, and `unstage` (use "." for all).
    #[serde(default)]
    path: String,
    /// Maximum number of log entries to return for `log` (default: 15, max: 100).
    #[serde(default)]
    limit: Option<usize>,
    /// Commit message (required when `action` is `commit`, optional description when `action` is `stash`).
    #[serde(default)]
    message: String,
}

fn validate_git_target(raw: &str, allow_staged: bool) -> Result<Option<&str>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if allow_staged && matches!(trimmed, "--staged" | "--cached") {
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

fn validate_scoped_path(raw: &str, must_exist: bool, ctx: &ToolContext) -> Result<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        if must_exist {
            bail!("'path' is required for this git action");
        }
        return Ok(None);
    }

    if must_exist {
        let canon = ctx.resolve_existing_path(trimmed)?;
        let rel = pathdiff::diff_paths(&canon, &ctx.root).unwrap_or(canon);
        Ok(Some(rel.to_string_lossy().into_owned()))
    } else {
        let rel = ctx.validate_scoped_relative_path(trimmed)?;
        Ok(Some(rel.to_string_lossy().into_owned()))
    }
}

fn require_scoped_path(raw: &str, must_exist: bool, ctx: &ToolContext) -> Result<String> {
    validate_scoped_path(raw, must_exist, ctx)?
        .ok_or_else(|| anyhow::anyhow!("'path' is required for this git action (use '.' for all)"))
}

fn push_optional_path(cmd_args: &mut Vec<String>, raw_path: &str, ctx: &ToolContext) -> Result<()> {
    if let Some(p) = validate_scoped_path(raw_path, false, ctx)? {
        cmd_args.extend(["--".into(), p]);
    }
    Ok(())
}

impl AsyncTool for Tool {
    const NAME: &'static str = "git";
    const DESCRIPTION: &'static str = "Executes git operations (status, diff, log, show, blame, branch, add, unstage, commit, stash, stash_pop) within the workspace.";
    type Args = GitArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let mut cmd_args: Vec<String> = vec!["--no-pager".into()];

        let empty_msg = match args.action {
            GitAction::Status => {
                cmd_args.extend(["status".into(), "--short".into(), "--branch".into()]);
                push_optional_path(&mut cmd_args, &args.path, ctx)?;
                "Working tree clean."
            }
            GitAction::Diff => {
                cmd_args.extend(["diff".into(), "--no-ext-diff".into()]);
                if let Some(t) = validate_git_target(&args.target, true)? {
                    cmd_args.push(t.into());
                }
                push_optional_path(&mut cmd_args, &args.path, ctx)?;
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
                push_optional_path(&mut cmd_args, &args.path, ctx)?;
                "No commits found."
            }
            GitAction::Show => {
                let target = validate_git_target(&args.target, false)?.unwrap_or("HEAD");
                cmd_args.extend(["show".into(), "--no-ext-diff".into(), target.into()]);
                push_optional_path(&mut cmd_args, &args.path, ctx)?;
                "No commit output."
            }
            GitAction::Blame => {
                let path = require_scoped_path(&args.path, true, ctx)?;
                cmd_args.push("blame".into());
                if let Some(t) = validate_git_target(&args.target, false)? {
                    cmd_args.push(t.into());
                }
                cmd_args.extend(["--".into(), path]);
                "No blame output."
            }
            GitAction::Branch => {
                cmd_args.extend(["branch".into(), "-a".into(), "-vv".into()]);
                "No branches found."
            }
            GitAction::Add => {
                let path = require_scoped_path(&args.path, false, ctx)?;
                cmd_args.extend(["add".into(), "--".into(), path]);
                "Files staged successfully."
            }
            GitAction::Unstage => {
                let path = require_scoped_path(&args.path, false, ctx)?;
                cmd_args.extend(["restore".into(), "--staged".into(), "--".into(), path]);
                "Files unstaged successfully."
            }
            GitAction::Commit => {
                let msg = args.message.trim();
                if msg.is_empty() {
                    bail!("'message' cannot be empty for git commit");
                }
                cmd_args.extend(["commit".into(), "-m".into(), msg.to_string()]);
                "Commit created."
            }
            GitAction::Stash => {
                cmd_args.extend(["stash".into(), "push".into(), "-u".into()]);
                let msg = args.message.trim();
                if !msg.is_empty() {
                    cmd_args.extend(["-m".into(), msg.to_string()]);
                }
                push_optional_path(&mut cmd_args, &args.path, ctx)?;
                "Changes stashed."
            }
            GitAction::StashPop => {
                cmd_args.extend(["stash".into(), "pop".into()]);
                if let Some(t) = validate_git_target(&args.target, false)? {
                    cmd_args.push(t.into());
                }
                "Stash popped successfully."
            }
        };

        ctx.run_command("git", &cmd_args, empty_msg).await
    }
}
