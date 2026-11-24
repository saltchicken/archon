use super::{
    AsyncTool, ManagedChild, ToolContext,
    utils::{
        fs::{read_capped, read_file_contents, read_utf8_capped, render_directory_listing},
        path::{build_globset, has_git_dir_component, resolve_existing_under},
        search::{SearchExecutionParams, search_candidate_files},
    },
};
use crate::core::config::format_tilde_path;
use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, sync::Mutex, time::timeout};

const CLONE_TIMEOUT_SECS: u64 = 60;
const MAX_GIT_OUTPUT_BYTES: u64 = 64 * 1024;
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
    /// Search file contents inside a cached GitHub repo using a regex or literal pattern.
    Search,
}

#[derive(Deserialize, JsonSchema)]
pub struct GithubRepoArgs {
    /// Operation to perform: `fetch` (default), `list`, `read`, or `search`.
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
    /// Relative file or directory path within the cached repo (required for `read`, optional for `list` and `search`).
    #[serde(default)]
    pub path: String,
    /// Maximum directory traversal depth when `action` is `list` (default: 2, max: 5).
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// Optional 1-based starting line number when `action` is `read`.
    #[serde(default)]
    pub start_line: Option<usize>,
    /// Optional 1-based ending line number (inclusive) when `action` is `read`.
    #[serde(default)]
    pub end_line: Option<usize>,
    /// Regex or literal search pattern when `action` is `search`.
    #[serde(default)]
    pub pattern: String,
    /// Optional glob patterns to filter included files when `action` is `search` (e.g. `["*.rs", "src/**/*.ts"]`).
    #[serde(default)]
    pub include: Vec<String>,
    /// Optional glob patterns to exclude when `action` is `search`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// If true during `search`, treats `pattern` as a literal string instead of a regular expression.
    #[serde(default)]
    pub fixed_strings: bool,
    /// If true during `search`, returns only matching file paths and counts.
    #[serde(default)]
    pub files_with_matches: bool,
    /// Optional case-sensitivity override for `search`.
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    /// Surrounding context lines (0 to 5) for `search`.
    #[serde(default)]
    pub context_lines: Option<usize>,
    /// Maximum number of matches to return for `search` (default: 100, max: 500).
    #[serde(default)]
    pub max_results: Option<usize>,
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
    #[cfg(unix)]
    cmd.process_group(0);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let mut child = ManagedChild::new(cmd.spawn().context("Failed to execute git")?);
    let stdout = child
        .child
        .stdout
        .take()
        .context("Failed to capture git stdout")?;
    let stderr = child
        .child
        .stderr
        .take()
        .context("Failed to capture git stderr")?;

    let (out_bytes, err_bytes, status) = timeout(Duration::from_secs(CLONE_TIMEOUT_SECS), async {
        let (r1, r2) = tokio::join!(
            read_capped(stdout, MAX_GIT_OUTPUT_BYTES),
            read_capped(stderr, MAX_GIT_OUTPUT_BYTES),
        );
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((r1?, r2?, status))
    })
    .await
    .map_err(|_| anyhow::anyhow!("git timed out after {CLONE_TIMEOUT_SECS}s"))?
    .context("Failed to read git output")?;

    if !status.success() {
        let stderr = String::from_utf8_lossy(&err_bytes).trim().to_string();
        bail!("git failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&out_bytes).trim().to_string())
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

fn unique_staging_path(root: &Path, owner: &str, name: &str) -> PathBuf {
    let pid = std::process::id();
    for attempt in 0..64u32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = root.join(format!(".{owner}__{name}.clone-{pid}-{nanos}-{attempt}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    root.join(format!(".{owner}__{name}.clone-{pid}"))
}

async fn fetch_repo(
    owner: &str,
    name: &str,
    git_ref: &str,
    refresh: bool,
) -> Result<(PathBuf, bool)> {
    let _lock = FETCH_LOCK.lock().await;
    let root = cache_root_dir()?;
    let url = format!("https://github.com/{owner}/{name}.git");
    fetch_repo_from_url(&root, owner, name, &url, git_ref, refresh).await
}

async fn fetch_repo_from_url(
    root: &Path,
    owner: &str,
    name: &str,
    url: &str,
    git_ref: &str,
    refresh: bool,
) -> Result<(PathBuf, bool)> {
    let ref_trimmed = validate_git_ref(git_ref)?;
    fs::create_dir_all(root)
        .with_context(|| format!("Failed to create cache directory '{}'", root.display()))?;

    let dest = root.join(format!("{owner}__{name}"));

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
                "-c",
                "core.symlinks=false",
                "reset",
                "--hard",
                "FETCH_HEAD",
            ],
            Some(&dest),
        )
        .await?;
        return Ok((dunce::canonicalize(&dest).unwrap_or(dest), true));
    }

    // Clone into a sibling staging directory and atomically rename into `dest`
    // so a failed/cancelled clone never leaves a poisoned cache entry behind.
    let staging = unique_staging_path(root, owner, name);
    let staging_str = staging.to_string_lossy().into_owned();
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
    clone_args.push("--");
    clone_args.push(url);
    clone_args.push(&staging_str);

    if let Err(err) = run_git(&clone_args, None).await {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }

    if dest.is_dir() {
        if let Err(err) = fs::remove_dir_all(&dest) {
            let _ = fs::remove_dir_all(&staging);
            return Err(err).with_context(|| {
                format!(
                    "Failed to remove stale cache directory '{}'",
                    dest.display()
                )
            });
        }
    } else if dest.exists()
        && let Err(err) = fs::remove_file(&dest)
    {
        let _ = fs::remove_dir_all(&staging);
        return Err(err)
            .with_context(|| format!("Failed to remove stale cache path '{}'", dest.display()));
    }

    if let Err(err) = fs::rename(&staging, &dest) {
        let _ = fs::remove_dir_all(&staging);
        return Err(err).with_context(|| {
            format!("Failed to move cloned repository into '{}'", dest.display())
        });
    }

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

    let readme_candidates = [
        "README.md",
        "readme.md",
        "Readme.md",
        "README.rst",
        "README.txt",
        "README",
    ];
    for rel in readme_candidates {
        let path = repo_dir.join(rel);
        if path.is_file()
            && let Some(content) = read_utf8_capped(&path, ORIENTATION_FILE_CAP)
        {
            let _ = writeln!(out, "## `{rel}`\n\n{content}\n");
            break;
        }
    }

    let key_files = [
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
        "Tip: Use `repo_map` with `github_repo: \"owner/repo\"` (`action: \"map\"`, `\"definition\"`, or `\"references\"`) to inspect AST signatures and implementations, or `github_repo` (`action: \"read\"`, `\"list\"`, or `\"search\"`) to inspect specific files and search text.",
    );
    out
}

fn execute_list(
    repo_dir: &Path,
    label: &str,
    sub_path: &str,
    max_depth: Option<usize>,
    max_output_size: usize,
) -> Result<String> {
    let target = resolve_existing_under(repo_dir, sub_path, "path escapes cached repo root")?;
    if has_git_dir_component(&target) {
        bail!("Security violation: access to '.git' is not allowed");
    }

    let trimmed = sub_path.trim();
    let display = if trimmed.is_empty() || trimmed == "." {
        label.to_string()
    } else {
        format!("{label}/{trimmed}")
    };
    let max_depth = max_depth.unwrap_or(2).clamp(1, 5);
    render_directory_listing(
        &target,
        repo_dir,
        &display,
        max_depth,
        max_output_size,
        |path| !has_git_dir_component(path),
    )
}

fn execute_read(
    repo_dir: &Path,
    label: &str,
    sub_path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_output_size: usize,
) -> Result<String> {
    let trimmed = sub_path.trim();
    if trimmed.is_empty() {
        bail!("'path' is required when `action` is `read`");
    }
    let target = resolve_existing_under(repo_dir, trimmed, "path escapes cached repo root")?;
    if has_git_dir_component(&target) {
        bail!("Security violation: access to '.git' is not allowed");
    }
    let display = format!("{label}/{trimmed}");
    read_file_contents(&target, &display, start_line, end_line, max_output_size)
}

struct GithubSearchOptions {
    path: String,
    pattern: String,
    include: Vec<String>,
    exclude: Vec<String>,
    fixed_strings: bool,
    files_with_matches: bool,
    case_sensitive: Option<bool>,
    context_lines: Option<usize>,
    max_results: Option<usize>,
    max_files: usize,
    max_file_size: u64,
    max_output_size: usize,
}

fn execute_search(repo_dir: &Path, label: &str, opts: &GithubSearchOptions) -> Result<String> {
    if opts.pattern.is_empty() {
        bail!("'pattern' is required when `action` is `search`");
    }
    let scan_root = resolve_existing_under(repo_dir, &opts.path, "path escapes cached repo root")?;
    if has_git_dir_component(&scan_root) {
        bail!("Security violation: access to '.git' is not allowed");
    }

    let include_set = (!opts.include.is_empty())
        .then(|| build_globset(&opts.include, true))
        .transpose()?;
    let exclude_set = (!opts.exclude.is_empty())
        .then(|| build_globset(&opts.exclude, true))
        .transpose()?;

    let mut candidate_files = Vec::new();
    if scan_root.is_file() {
        let rel = diff_paths(&scan_root, repo_dir).unwrap_or_else(|| scan_root.clone());
        let excluded = exclude_set.as_ref().is_some_and(|ex| ex.is_match(&rel));
        let included = include_set.as_ref().is_none_or(|inc| inc.is_match(&rel));
        if !excluded && included {
            let display = format!("{label}/{}", rel.to_string_lossy());
            candidate_files.push((scan_root, display));
        }
    } else {
        for entry in WalkBuilder::new(&scan_root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .filter_entry(|e| e.file_name() != ".git")
            .build()
            .flatten()
        {
            if candidate_files.len() >= opts.max_files {
                break;
            }
            let path = entry.path();
            if !path.is_file() || has_git_dir_component(path) {
                continue;
            }
            let Ok(canon) = dunce::canonicalize(path) else {
                continue;
            };
            if !canon.starts_with(repo_dir) {
                continue;
            }
            let Some(rel) = diff_paths(&canon, repo_dir) else {
                continue;
            };
            if exclude_set.as_ref().is_some_and(|ex| ex.is_match(&rel))
                || include_set.as_ref().is_some_and(|inc| !inc.is_match(&rel))
            {
                continue;
            }
            candidate_files.push((canon, format!("{label}/{}", rel.to_string_lossy())));
        }
        candidate_files.sort_by(|a, b| a.1.cmp(&b.1));
    }

    search_candidate_files(
        candidate_files,
        &SearchExecutionParams {
            pattern: &opts.pattern,
            fixed_strings: opts.fixed_strings,
            files_with_matches: opts.files_with_matches,
            case_sensitive: opts.case_sensitive,
            context_lines: opts.context_lines,
            max_results: opts.max_results,
            max_file_size: opts.max_file_size,
            max_output_size: opts.max_output_size,
        },
    )
}

impl AsyncTool for Tool {
    const NAME: &'static str = "github_repo";
    const DESCRIPTION: &'static str = "Fetch, browse, read, and search external GitHub repositories in a local cache (`~/.cache/archon/repos/`) to study their architecture, integrate them into a project, or prepare contributions. Supports `fetch` (shallow-clone + orientation bundle), `list` (directory listing), `read` (file/line-range reading), and `search` (regex/literal content search). After fetching, pair with `repo_map` (`github_repo: \"owner/repo\"`) for AST maps, symbol definitions, and references.";
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
                let max_depth = args.max_depth;
                let max_output_size = ctx.max_output_size;
                tokio::task::spawn_blocking(move || {
                    execute_list(&repo_dir, &label, &path, max_depth, max_output_size)
                })
                .await?
            }
            GithubRepoAction::Read => {
                let (repo_dir, _) = resolve_cached_repo_dir(&args.repo)?;
                let max_output_size = ctx.max_output_size;
                let path = args.path;
                let start = args.start_line;
                let end = args.end_line;
                tokio::task::spawn_blocking(move || {
                    execute_read(&repo_dir, &label, &path, start, end, max_output_size)
                })
                .await?
            }
            GithubRepoAction::Search => {
                let (repo_dir, _) = resolve_cached_repo_dir(&args.repo)?;
                let opts = GithubSearchOptions {
                    path: args.path,
                    pattern: args.pattern,
                    include: args.include,
                    exclude: args.exclude,
                    fixed_strings: args.fixed_strings,
                    files_with_matches: args.files_with_matches,
                    case_sensitive: args.case_sensitive,
                    context_lines: args.context_lines,
                    max_results: args.max_results,
                    max_files: ctx.config.workspace_snapshot.max_files,
                    max_file_size: ctx.config.workspace_snapshot.max_size,
                    max_output_size: ctx.max_output_size,
                };
                tokio::task::spawn_blocking(move || execute_search(&repo_dir, &label, &opts))
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

    fn make_sample_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(
            root.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join("ignored_dir")).unwrap();
        fs::write(root.join(".gitignore"), "ignored_dir/\n").unwrap();
        fs::write(root.join("README.md"), "# Demo Repo\nHello world\n").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(root.join("empty.txt"), "").unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    println!(\"needle_one\");\n    println!(\"line_three\");\n}\n",
        )
        .unwrap();
        fs::write(root.join("src/nested/mod.rs"), "pub fn needle_two() {}\n").unwrap();
        fs::write(
            root.join("ignored_dir/noise.rs"),
            "pub fn needle_ignored() {}\n",
        )
        .unwrap();
        (tmp, root)
    }

    #[test]
    fn execute_list_renders_tree_and_blocks_git_and_traversal() {
        let (_tmp, root) = make_sample_repo();

        let out = execute_list(&root, "owner/demo", "", Some(2), 65536).unwrap();
        assert!(out.contains("Directory listing for 'owner/demo' (max_depth: 2):"));
        assert!(out.contains("src/"));
        assert!(out.contains("main.rs"));
        assert!(out.contains("README.md"));
        assert!(out.contains(".gitignore"));
        assert!(!out.contains(".git/"));

        let sub = execute_list(&root, "owner/demo", "src", Some(1), 65536).unwrap();
        assert!(sub.contains("Directory listing for 'owner/demo/src' (max_depth: 1):"));
        assert!(sub.contains("nested/"));
        assert!(!sub.contains("mod.rs"));

        assert!(execute_list(&root, "owner/demo", ".git", None, 65536).is_err());
        assert!(execute_list(&root, "owner/demo", "../", None, 65536).is_err());
    }

    #[test]
    fn execute_read_handles_full_range_empty_file_and_blocks_git() {
        let (_tmp, root) = make_sample_repo();

        let full = execute_read(&root, "owner/demo", "src/main.rs", None, None, 65536).unwrap();
        assert!(full.contains("fn main()"));
        assert!(full.contains("needle_one"));

        let range =
            execute_read(&root, "owner/demo", "src/main.rs", Some(2), Some(3), 65536).unwrap();
        assert!(range.contains("   2:     println!(\"needle_one\");"));
        assert!(range.contains("   3:     println!(\"line_three\");"));
        assert!(range.contains("[Showing lines 2..3 of 4]"));

        // Empty files must succeed rather than erroring as binary/unreadable.
        let empty_raw = execute_read(&root, "owner/demo", "empty.txt", None, None, 65536).unwrap();
        assert_eq!(empty_raw, "");
        let empty_range =
            execute_read(&root, "owner/demo", "empty.txt", Some(1), Some(5), 65536).unwrap();
        assert_eq!(empty_range, "(Empty file)");

        // Validation & security errors
        assert!(execute_read(&root, "owner/demo", "", None, None, 65536).is_err());
        assert!(execute_read(&root, "owner/demo", "src", None, None, 65536).is_err());
        assert!(execute_read(&root, "owner/demo", ".git/config", None, None, 65536).is_err());
        assert!(execute_read(&root, "owner/demo", "../etc/passwd", None, None, 65536).is_err());
    }

    #[test]
    fn execute_search_respects_gitignore_globs_and_blocks_git() {
        let (_tmp, root) = make_sample_repo();

        let base_opts = |pattern: &str| GithubSearchOptions {
            path: String::new(),
            pattern: pattern.into(),
            include: Vec::new(),
            exclude: Vec::new(),
            fixed_strings: false,
            files_with_matches: false,
            case_sensitive: None,
            context_lines: None,
            max_results: None,
            max_files: 1000,
            max_file_size: 1024 * 1024,
            max_output_size: 65536,
        };

        let out = execute_search(&root, "owner/demo", &base_opts("needle_\\w+")).unwrap();
        assert!(out.contains("owner/demo/src/main.rs"));
        assert!(out.contains("owner/demo/src/nested/mod.rs"));
        // Ignored by `.gitignore` and `.git` excluded
        assert!(!out.contains("needle_ignored"));
        assert!(!out.contains(".git/config"));

        // Include filter & files_with_matches
        let mut filtered = base_opts("needle_");
        filtered.fixed_strings = true;
        filtered.files_with_matches = true;
        filtered.include = vec!["**/mod.rs".into()];
        let out_filtered = execute_search(&root, "owner/demo", &filtered).unwrap();
        assert!(out_filtered.contains("owner/demo/src/nested/mod.rs"));
        assert!(!out_filtered.contains("src/main.rs"));

        // Single-file search
        let mut single = base_opts("line_three");
        single.path = "src/main.rs".into();
        let out_single = execute_search(&root, "owner/demo", &single).unwrap();
        assert!(out_single.contains("owner/demo/src/main.rs"));
        assert!(out_single.contains("line_three"));

        // Reject empty pattern and `.git`
        assert!(execute_search(&root, "owner/demo", &base_opts("")).is_err());
        let mut git_opts = base_opts("core");
        git_opts.path = ".git".into();
        assert!(execute_search(&root, "owner/demo", &git_opts).is_err());
    }

    #[test]
    fn manual_tool_invocation_maps_github_repo_positional_args() {
        let (call, _) =
            crate::tools::manual::parse_manual_tool_call("github_repo owner/repo").unwrap();
        assert_eq!(call.function.name, "github_repo");
        assert_eq!(call.function.arguments["action"], "fetch");
        assert_eq!(call.function.arguments["repo"], "owner/repo");

        let (call, _) =
            crate::tools::manual::parse_manual_tool_call("github_repo:fetch owner/repo v1.2.0")
                .unwrap();
        assert_eq!(call.function.arguments["action"], "fetch");
        assert_eq!(call.function.arguments["repo"], "owner/repo");
        assert_eq!(call.function.arguments["git_ref"], "v1.2.0");

        let (call, _) =
            crate::tools::manual::parse_manual_tool_call("github_repo:read owner/repo Cargo.toml")
                .unwrap();
        assert_eq!(call.function.arguments["action"], "read");
        assert_eq!(call.function.arguments["repo"], "owner/repo");
        assert_eq!(call.function.arguments["path"], "Cargo.toml");

        let (call, _) = crate::tools::manual::parse_manual_tool_call(
            "github_repo:search owner/repo \"fn main\" src",
        )
        .unwrap();
        assert_eq!(call.function.arguments["action"], "search");
        assert_eq!(call.function.arguments["repo"], "owner/repo");
        assert_eq!(call.function.arguments["pattern"], "fn main");
        assert_eq!(call.function.arguments["path"], "src");
    }

    #[tokio::test]
    async fn fetch_repo_clones_atomically_and_cleans_up_on_failure() {
        let origin_tmp = tempfile::tempdir().unwrap();
        let origin = origin_tmp.path();

        // Initialize a real local origin repo with one commit.
        run_git(&["init", "-b", "main"], Some(origin))
            .await
            .unwrap();
        fs::write(origin.join("README.md"), "# Origin\n").unwrap();
        run_git(&["add", "."], Some(origin)).await.unwrap();
        run_git(
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "init",
            ],
            Some(origin),
        )
        .await
        .unwrap();

        let cache_tmp = tempfile::tempdir().unwrap();
        let cache_root = cache_tmp.path();
        let origin_url = format!("file://{}", origin.display());

        // 1. Failed clone (non-existent branch) must not leave staging or dest behind.
        let err = fetch_repo_from_url(
            cache_root,
            "octo",
            "demo",
            &origin_url,
            "no-such-branch",
            false,
        )
        .await;
        assert!(err.is_err());
        assert_eq!(fs::read_dir(cache_root).unwrap().count(), 0);

        // 2. Stale/corrupt non-git directory at dest is replaced cleanly by a successful clone.
        let dest = cache_root.join("octo__demo");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("leftover.txt"), "stale").unwrap();

        let (cloned, freshly_fetched) =
            fetch_repo_from_url(cache_root, "octo", "demo", &origin_url, "", false)
                .await
                .unwrap();
        assert!(freshly_fetched);
        assert!(cloned.join(".git").is_dir());
        assert!(cloned.join("README.md").is_file());
        assert!(!cloned.join("leftover.txt").exists());
        // No staging leftovers in cache_root
        assert_eq!(fs::read_dir(cache_root).unwrap().count(), 1);

        // 3. Subsequent call without refresh hits cache without re-fetching.
        let (_, freshly_fetched_again) =
            fetch_repo_from_url(cache_root, "octo", "demo", &origin_url, "", false)
                .await
                .unwrap();
        assert!(!freshly_fetched_again);
    }
}
