use super::{
    AsyncTool, ToolContext,
    utils::{
        fs::read_utf8_capped,
        path::{has_git_dir_component, resolve_existing_under},
    },
};
use crate::core::config::format_tilde_path;
use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::LazyLock,
    time::Duration,
};
use tokio::{process::Command, sync::Mutex, time::timeout};

const CLONE_TIMEOUT_SECS: u64 = 60;
const ORIENTATION_FILE_CAP: u64 = 32 * 1024;
const MAX_TREE_ENTRIES: usize = 120;

/// Serializes concurrent `fetch_repo` calls to prevent TOCTOU races
/// when two fetches target the same repo directory.
static FETCH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub struct Tool;

#[derive(Default, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GithubRepoAction {
    /// Shallow-clone (or update) a GitHub repo into `~/.cache/archon/repos/` and return an orientation bundle (README, manifest, CONTRIBUTING, and top-level tree).
    #[default]
    Fetch,
    /// List files and directories inside a cached GitHub repo (optionally scoped to `path`).
    List,
    /// Read a file (or 1-based line range) from a cached GitHub repo.
    Read,
}

#[derive(Deserialize, JsonSchema)]
pub struct GithubRepoArgs {
    /// Operation to perform: `fetch` (default), `list`, or `read`.
    #[serde(default)]
    pub action: GithubRepoAction,
    /// GitHub repository in `owner/repo` format or full `https://github.com/owner/repo` URL.
    pub repo: String,
    /// Optional branch or tag name when `action` is `fetch`.
    #[serde(default)]
    pub git_ref: String,
    /// If true when `action` is `fetch`, force a `git fetch` + reset even if already cached.
    #[serde(default)]
    pub refresh: bool,
    /// Relative file or directory path within the cached repo (required for `read`, optional for `list`).
    #[serde(default)]
    pub path: String,
    /// Optional 1-based starting line number when `action` is `read`.
    #[serde(default)]
    pub start_line: Option<usize>,
    /// Optional 1-based ending line number (inclusive) when `action` is `read`.
    #[serde(default)]
    pub end_line: Option<usize>,
}

/// Validates and normalizes `owner/repo` or `https://github.com/owner/repo(.git)`.
pub fn parse_repo_slug(raw: &str) -> Result<(String, String)> {
    let trimmed = raw.trim().trim_end_matches('/').trim_end_matches(".git");

    let slug = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("github.com/"))
        .unwrap_or(trimmed);

    let mut parts = slug.split('/');
    let owner = parts.next().unwrap_or("").trim();
    let name = parts.next().unwrap_or("").trim();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        bail!(
            "Invalid GitHub repo '{raw}'. Expected 'owner/repo' or 'https://github.com/owner/repo'."
        );
    }

    let valid_part = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && !s.contains("..")
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !valid_part(owner) || !valid_part(name) {
        bail!("Invalid characters in GitHub repo '{owner}/{name}'.");
    }

    Ok((owner.to_string(), name.to_string()))
}

fn cache_root_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))
        .context("Cannot determine user cache directory")?;
    Ok(base.join("archon").join("repos"))
}

pub fn cached_repo_path(owner: &str, name: &str) -> Result<PathBuf> {
    Ok(cache_root_dir()?.join(format!("{owner}__{name}")))
}

/// Shared resolver used by both `github_repo` and `repo_map` (`github_repo` param).
pub fn resolve_cached_repo_dir(raw_repo: &str) -> Result<(PathBuf, String)> {
    let (owner, name) = parse_repo_slug(raw_repo)?;
    let dir = cached_repo_path(&owner, &name)?;
    if !dir.is_dir() {
        bail!(
            "GitHub repo '{owner}/{name}' is not cached yet. Run `github_repo` with `action: \"fetch\"` and `repo: \"{owner}/{name}\"` first."
        );
    }
    let canon = dunce::canonicalize(&dir).unwrap_or(dir);
    Ok((canon, format!("{owner}/{name}")))
}

async fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let output = timeout(Duration::from_secs(CLONE_TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("git timed out after {CLONE_TIMEOUT_SECS}s"))?
        .context("Failed to execute git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("git failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn validate_git_ref(raw: &str) -> Result<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok("");
    }
    if trimmed.starts_with('-')
        || trimmed.contains("..")
        || trimmed.contains("@{")
        || trimmed.ends_with('/')
        || trimmed.ends_with(".lock")
        || trimmed.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '+')
        })
    {
        bail!("Invalid git_ref '{trimmed}'.");
    }
    Ok(trimmed)
}

async fn fetch_repo(
    owner: &str,
    name: &str,
    git_ref: &str,
    refresh: bool,
) -> Result<(PathBuf, bool)> {
    let _lock = FETCH_LOCK.lock().await;
    let ref_trimmed = validate_git_ref(git_ref)?;
    let root = cache_root_dir()?;
    fs::create_dir_all(&root)
        .with_context(|| format!("Failed to create cache directory '{}'", root.display()))?;

    let dest = root.join(format!("{owner}__{name}"));
    let url = format!("https://github.com/{owner}/{name}.git");

    if dest.join(".git").is_dir() {
        if !refresh && ref_trimmed.is_empty() {
            return Ok((dunce::canonicalize(&dest).unwrap_or(dest), false));
        }
        let mut fetch_args = vec![
            "-c",
            "core.hooksPath=/dev/null",
            "fetch",
            "--depth",
            "1",
            "--",
            "origin",
        ];
        if !ref_trimmed.is_empty() {
            fetch_args.push(ref_trimmed);
        }
        run_git(&fetch_args, Some(&dest)).await?;
        run_git(
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "reset",
                "--hard",
                "FETCH_HEAD",
            ],
            Some(&dest),
        )
        .await?;
        return Ok((dunce::canonicalize(&dest).unwrap_or(dest), true));
    }

    let dest_str = dest.to_string_lossy().into_owned();
    let mut clone_args = vec![
        "clone",
        "--depth",
        "1",
        "--single-branch",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.symlinks=false",
    ];
    if !ref_trimmed.is_empty() {
        clone_args.push("--branch");
        clone_args.push(ref_trimmed);
    }
    clone_args.push(&url);
    clone_args.push(&dest_str);

    run_git(&clone_args, None).await?;
    Ok((dunce::canonicalize(&dest).unwrap_or(dest), true))
}

fn build_orientation_bundle(repo_dir: &Path, label: &str, freshly_fetched: bool) -> String {
    let status = if freshly_fetched {
        "cloned/updated"
    } else {
        "cached (pass `refresh: true` to update)"
    };
    let mut out = format!(
        "# GitHub Repo: `{label}` ({status})\nCache path: `{}`\n\n",
        format_tilde_path(repo_dir)
    );

    out.push_str("## Directory Overview (depth <= 2)\n\n```text\n");
    let mut entries: Vec<String> = WalkBuilder::new(repo_dir)
        .max_depth(Some(2))
        .hidden(false)
        .git_ignore(true)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
        .filter(|e| e.depth() > 0)
        .filter_map(|e| {
            let rel = e.path().strip_prefix(repo_dir).ok()?;
            let mut s = rel.to_string_lossy().into_owned();
            if e.path().is_dir() {
                s.push('/');
            }
            Some(s)
        })
        .collect();
    entries.sort();
    let total = entries.len();
    for entry in entries.iter().take(MAX_TREE_ENTRIES) {
        let _ = writeln!(out, "{entry}");
    }
    if total > MAX_TREE_ENTRIES {
        let _ = writeln!(out, "... ({} more entries)", total - MAX_TREE_ENTRIES);
    }
    out.push_str("```\n\n");

    let key_files = [
        "README.md",
        "readme.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "CONTRIBUTING.md",
    ];
    for rel in key_files {
        let path = repo_dir.join(rel);
        if path.is_file()
            && let Some(content) = read_utf8_capped(&path, ORIENTATION_FILE_CAP)
        {
            let _ = writeln!(out, "## `{rel}`\n\n{content}\n");
        }
    }

    out.push_str(
        "Tip: Use `repo_map` with `github_repo: \"owner/repo\"` (`action: \"map\"`, `\"definition\"`, or `\"references\"`) to inspect AST signatures and implementations, or `github_repo` (`action: \"read\"`) to read specific files.",
    );
    out
}

fn execute_list(repo_dir: &Path, label: &str, sub_path: &str) -> Result<String> {
    let target = resolve_existing_under(repo_dir, sub_path, "path escapes cached repo root")?;
    if has_git_dir_component(&target) {
        bail!("Security violation: access to '.git' is not allowed");
    }
    if !target.is_dir() {
        bail!("'{}' is not a directory", sub_path.trim());
    }

    let mut items = Vec::new();
    for entry in fs::read_dir(&target)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        items.push(if is_dir { format!("{name}/") } else { name });
    }
    items.sort();

    let scope = if sub_path.trim().is_empty() {
        label.to_string()
    } else {
        format!("{label}/{}", sub_path.trim())
    };
    Ok(format!(
        "=== {scope} ({} entries) ===\n{}",
        items.len(),
        items.join("\n")
    ))
}

fn execute_read(
    repo_dir: &Path,
    label: &str,
    sub_path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_file_size: u64,
) -> Result<String> {
    if sub_path.trim().is_empty() {
        bail!("'path' is required when `action` is `read`");
    }
    let target = resolve_existing_under(repo_dir, sub_path, "path escapes cached repo root")?;
    if has_git_dir_component(&target) {
        bail!("Security violation: access to '.git' is not allowed");
    }
    if !target.is_file() {
        bail!("'{}' is not a file", sub_path.trim());
    }

    let content = read_utf8_capped(&target, max_file_size).ok_or_else(|| {
        anyhow::anyhow!(
            "File '{}' is binary, unreadable, or exceeds size limit",
            sub_path.trim()
        )
    })?;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Ok(format!("=== {label}/{} ===\n(empty file)", sub_path.trim()));
    }
    let start = start_line.unwrap_or(1).max(1);
    let end = end_line.unwrap_or(total).min(total);
    if start > end {
        bail!("Invalid line range {start}-{end} (file has {total} lines)");
    }

    let mut out = format!(
        "=== {label}/{} (lines {start}-{end} of {total}) ===\n",
        sub_path.trim()
    );
    for (idx, line) in lines.iter().enumerate().take(end).skip(start - 1) {
        let _ = writeln!(out, "{:>4}: {line}", idx + 1);
    }
    Ok(out.trim_end().to_string())
}

impl AsyncTool for Tool {
    const NAME: &'static str = "github_repo";
    const DESCRIPTION: &'static str = "Fetch, browse, and read external GitHub repositories in a local cache (`~/.cache/archon/repos/`) to study their architecture, integrate them into a project, or prepare contributions. Supports `fetch` (shallow-clone + orientation bundle), `list` (directory listing), and `read` (file/line-range reading). After fetching, pair with `repo_map` (`github_repo: \"owner/repo\"`) for AST maps, symbol definitions, and references.";
    type Args = GithubRepoArgs;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let (owner, name) = parse_repo_slug(&args.repo)?;
        let label = format!("{owner}/{name}");

        match args.action {
            GithubRepoAction::Fetch => {
                let (repo_dir, freshly_fetched) =
                    fetch_repo(&owner, &name, &args.git_ref, args.refresh).await?;
                Ok(tokio::task::spawn_blocking(move || {
                    build_orientation_bundle(&repo_dir, &label, freshly_fetched)
                })
                .await?)
            }
            GithubRepoAction::List => {
                let (repo_dir, _) = resolve_cached_repo_dir(&args.repo)?;
                let path = args.path;
                tokio::task::spawn_blocking(move || execute_list(&repo_dir, &label, &path)).await?
            }
            GithubRepoAction::Read => {
                let (repo_dir, _) = resolve_cached_repo_dir(&args.repo)?;
                let max_size = ctx.config.workspace_snapshot.max_size;
                let path = args.path;
                let start = args.start_line;
                let end = args.end_line;
                tokio::task::spawn_blocking(move || {
                    execute_read(&repo_dir, &label, &path, start, end, max_size)
                })
                .await?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_repo_slug_accepts_valid_inputs() {
        assert_eq!(
            parse_repo_slug("owner/repo").unwrap(),
            ("owner".into(), "repo".into())
        );
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo").unwrap(),
            ("owner".into(), "repo".into())
        );
        assert_eq!(
            parse_repo_slug("http://github.com/owner/repo").unwrap(),
            ("owner".into(), "repo".into())
        );
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo.git").unwrap(),
            ("owner".into(), "repo".into())
        );
        assert_eq!(
            parse_repo_slug("github.com/owner/repo").unwrap(),
            ("owner".into(), "repo".into())
        );
        // Trailing slash
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo/").unwrap(),
            ("owner".into(), "repo".into())
        );
    }

    #[test]
    fn parse_repo_slug_rejects_invalid_inputs() {
        // Three path segments
        assert!(parse_repo_slug("owner/repo/sub").is_err());
        // Missing repo name
        assert!(parse_repo_slug("owner").is_err());
        // Missing owner
        assert!(parse_repo_slug("/repo").is_err());
        // Path traversal
        assert!(parse_repo_slug("../evil").is_err());
        // Dot-prefixed parts
        assert!(parse_repo_slug(".hidden/repo").is_err());
        // Double-dot
        assert!(parse_repo_slug("a..b/repo").is_err());
        // Non-ASCII
        assert!(parse_repo_slug("own\u{20ac}r/repo").is_err());
        // Spaces
        assert!(parse_repo_slug("own er/repo").is_err());
        // Empty
        assert!(parse_repo_slug("").is_err());
    }

    #[test]
    fn validate_git_ref_accepts_valid_refs() {
        assert_eq!(validate_git_ref("main").unwrap(), "main");
        assert_eq!(validate_git_ref("v1.0.0").unwrap(), "v1.0.0");
        assert_eq!(
            validate_git_ref("refs/heads/main").unwrap(),
            "refs/heads/main"
        );
        assert_eq!(
            validate_git_ref("refs/tags/v2.0").unwrap(),
            "refs/tags/v2.0"
        );
        assert_eq!(validate_git_ref("").unwrap(), "");
        // Underscore and dash are fine
        assert_eq!(
            validate_git_ref("feature/my-branch").unwrap(),
            "feature/my-branch"
        );
    }

    #[test]
    fn validate_git_ref_rejects_dangerous_inputs() {
        // Leading dash (option injection)
        assert!(validate_git_ref("-o").is_err());
        // Range spec
        assert!(validate_git_ref("HEAD..main").is_err());
        // Reflog
        assert!(validate_git_ref("HEAD@{1}").is_err());
        // Whitespace
        assert!(validate_git_ref("main branch").is_err());
        // Control characters
        assert!(validate_git_ref("main\u{0}").is_err());
        // Path traversal
        assert!(validate_git_ref("../main").is_err());
        // Glob
        assert!(validate_git_ref("ma*n").is_err());
        // Force refspec prefix
        assert!(validate_git_ref("+refs/heads/main").is_err());
        // Refspec with colon
        assert!(validate_git_ref("a:b").is_err());
        // Tilde
        assert!(validate_git_ref("HEAD~").is_err());
        // Caret
        assert!(validate_git_ref("v1.0^{}").is_err());
        // Backslash
        assert!(validate_git_ref("a\\b").is_err());
        // Bracket
        assert!(validate_git_ref("refs[0]").is_err());
        // Trailing slash
        assert!(validate_git_ref("main/").is_err());
        // Lock suffix
        assert!(validate_git_ref("HEAD.lock").is_err());
        // Question mark
        assert!(validate_git_ref("ma?n").is_err());
    }

    #[test]
    fn validate_git_ref_trims_whitespace() {
        assert_eq!(validate_git_ref("  main  ").unwrap(), "main");
    }
}
