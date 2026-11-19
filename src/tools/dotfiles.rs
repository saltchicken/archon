use super::{
    BlockingTool, ToolContext,
    utils::{
        edit::{EditHunk, apply_hunks_to_str},
        fs::{atomic_write_file, read_file_contents, render_directory_listing},
        path::{
            build_globset, ensure_ancestor_within_root, ensure_parent_dir, ensure_within_root,
            has_git_dir_component,
        },
        search::{SearchExecutionParams, search_candidate_files},
    },
};
use crate::core::{
    config::{Config, expand_tilde},
    provider::Provider,
    session::truncate_str,
};
use anyhow::{Context, Result, bail};
use globset::GlobSet;
use ignore::WalkBuilder;
use pathdiff::diff_paths;
use reqwest::Client;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    fmt::Write as _,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

pub struct Tool;

const BARE_REPO_PATH: &str = "~/.local/share/archon/dotfiles.git";

const DOTFILES_BLOCKED_PATTERNS: &[&str] = &[
    "**/.git/**",
    "**/.env*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/secrets*",
    "**/*secret*",
    "**/*token*",
    "**/*credential*",
    "**/cookies*",
    "**/Login Data*",
    "gh/hosts.yml",
    "gh/**",
    "hub",
    "hub/**",
    "rclone/**",
    "gcloud/**",
    "aws/**",
    "kube/**",
    "op/**",
    "sops/**",
    "age/**",
    "gnupg/**",
    "Bitwarden/**",
    "1Password/**",
    "keepassxc/**",
    "google-chrome/**",
    "chromium/**",
    "BraveSoftware/**",
    "vivaldi/**",
    "opera/**",
    "microsoft-edge/**",
    "zen/**",
    "librewolf/**",
    "mozilla/**",
    "pulse/cookie",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DotfilesAction {
    /// List files and directories inside `~/.config` (or a subdirectory at `path`).
    List,
    /// Read a configuration file inside `~/.config` with optional 1-based `[start_line, end_line]`.
    Read,
    /// Search configuration files inside `~/.config` using regex or literal strings.
    Search,
    /// Edit an existing file inside `~/.config` using disjoint search/replace `edits` hunks.
    Edit,
    /// Create a new configuration file inside `~/.config` with `content`.
    Create,
    /// Validate configuration syntax (`fish -n` for Fish scripts and `hyprctl configerrors` for Hyprland).
    Validate,
    /// Show bare git status and diff summary for tracked `~/.config` files in `dotfiles.allowed_dirs`.
    Status,
    /// Show unified git diff of uncommitted changes in tracked `~/.config` files (optionally scoped to `path`).
    Diff,
    /// Discard uncommitted changes and restore tracked `~/.config` files to the last committed `HEAD` state (scoped to `path`, or all tracked files in `dotfiles.allowed_dirs` when `path` is empty).
    #[serde(alias = "revert", alias = "checkout")]
    Restore,
    /// Initialize/clone the bare git repo (`~/.local/share/archon/dotfiles.git`), stage files in `dotfiles.allowed_dirs`, commit (using `content` as the optional commit message), and push to `dotfiles.repo_url`.
    Sync,
}

#[derive(Deserialize, JsonSchema)]
pub struct DotfilesArgs {
    /// The operation to perform in `~/.config`.
    pub action: DotfilesAction,
    /// Path relative to `~/.config` (e.g. `"hypr/hyprland.conf"`, `"fish/config.fish"`, or `"."` for `list`/`search`/`validate`/`diff`/`restore`).
    #[serde(default)]
    pub path: String,
    /// Optional 1-based starting line number (inclusive) when `action` is `read`.
    #[serde(default)]
    pub start_line: Option<usize>,
    /// Optional 1-based ending line number (inclusive) when `action` is `read`.
    #[serde(default)]
    pub end_line: Option<usize>,
    /// Disjoint search/replace hunks when `action` is `edit`.
    #[serde(default)]
    pub edits: Vec<EditHunk>,
    /// Full file contents when `action` is `create`, or optional commit message when `action` is `sync`.
    #[serde(default)]
    pub content: String,
    /// Regex or literal search pattern when `action` is `search`.
    #[serde(default)]
    pub pattern: String,
    /// Optional glob patterns to filter included files when `action` is `search` (e.g. `["*.conf", "fish/**/*.fish"]`).
    #[serde(default)]
    pub include: Vec<String>,
    /// Optional additional glob patterns to exclude when `action` is `search`.
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
    /// Maximum directory traversal depth when `action` is `list` (default: 2, max: 5).
    #[serde(default)]
    pub max_depth: Option<usize>,
}

fn config_root() -> Result<PathBuf> {
    let dir = dirs::config_dir().unwrap_or_else(|| expand_tilde("~/.config"));
    if !dir.exists() {
        bail!("Config directory does not exist: {}", dir.display());
    }
    Ok(dunce::canonicalize(&dir).unwrap_or(dir))
}

fn blocked_globset(extra_exclude: &[String]) -> Result<GlobSet> {
    let patterns: Vec<String> = DOTFILES_BLOCKED_PATTERNS
        .iter()
        .map(|&s| String::from(s))
        .chain(extra_exclude.iter().cloned())
        .collect();
    build_globset(&patterns, true)
}

fn normalize_config_subpath(raw: &str, root: &Path) -> Result<PathBuf> {
    let mut trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == "~/.config" || trimmed == ".config" {
        return Ok(PathBuf::new());
    }

    if let Some(rest) = trimmed.strip_prefix("~/.config/") {
        trimmed = rest;
    } else if let Some(rest) = trimmed.strip_prefix(".config/") {
        trimmed = rest;
    } else if let Some(root_str) = root.to_str()
        && let Some(rest) = trimmed.strip_prefix(root_str)
    {
        trimmed = rest.trim_start_matches('/');
    }
    trimmed = trimmed.trim_start_matches("./");

    if trimmed.is_empty() || trimmed == "." {
        return Ok(PathBuf::new());
    }

    let rel = Path::new(trimmed);
    if rel.is_absolute() {
        bail!("Security violation: path must be inside ~/.config (got '{raw}')");
    }
    if rel
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("Security violation: path traversal ('..') is blocked in '{raw}'");
    }
    Ok(rel.to_path_buf())
}

fn check_not_blocked(rel: &Path, blocked: &GlobSet) -> Result<()> {
    if rel.as_os_str().is_empty() {
        return Ok(());
    }
    if has_git_dir_component(rel) || blocked.is_match(rel) {
        bail!(
            "Security violation: access to '~/.config/{}' is restricted by the dotfiles security policy",
            rel.display()
        );
    }
    // Also check each prefix directory so e.g. `google-chrome` is caught even when matched as a directory
    let mut prefix = PathBuf::new();
    for comp in rel.components() {
        prefix.push(comp);
        let dir_probe = prefix.join("_probe");
        if blocked.is_match(&prefix) || blocked.is_match(&dir_probe) {
            bail!(
                "Security violation: access to '~/.config/{}' is restricted by the dotfiles security policy",
                rel.display()
            );
        }
    }
    Ok(())
}

fn check_allowed_dir(rel: &Path, config: &Config) -> Result<()> {
    let allowed = &config.dotfiles.allowed_dirs;
    if allowed.is_empty() || allowed.iter().any(|s| s == "*") {
        return Ok(());
    }

    let is_allowed = allowed.iter().any(|dir| {
        let allowed_path = Path::new(dir);
        rel.starts_with(allowed_path) || allowed_path.starts_with(rel)
    });

    if !is_allowed {
        bail!(
            "Security violation: access to '~/.config/{}' is restricted by the `dotfiles.allowed_dirs` policy",
            rel.display()
        );
    }
    Ok(())
}

fn check_allowed_file(rel: &Path, blocked: &GlobSet, config: &Config) -> Result<()> {
    check_not_blocked(rel, blocked)?;
    check_allowed_dir(rel, config)
}

fn resolve_existing_config_path(
    raw: &str,
    allow_root: bool,
    config: &Config,
) -> Result<(PathBuf, PathBuf, String)> {
    let root = config_root()?;
    let rel = normalize_config_subpath(raw, &root)?;
    if rel.as_os_str().is_empty() && !allow_root {
        bail!("'path' inside ~/.config is required for this action");
    }
    let blocked = blocked_globset(&[])?;
    check_allowed_file(&rel, &blocked, config)?;

    let candidate = if rel.as_os_str().is_empty() {
        root.clone()
    } else {
        root.join(&rel)
    };
    if !candidate.exists() {
        bail!("Target path does not exist: ~/.config/{}", rel.display());
    }
    let canon = ensure_within_root(&candidate, &root, &format!("~/.config/{}", rel.display()))?;
    let effective_rel = diff_paths(&canon, &root).unwrap_or(rel);
    check_allowed_file(&effective_rel, &blocked, config)?;
    let display = if effective_rel.as_os_str().is_empty() {
        "~/.config".to_string()
    } else {
        format!("~/.config/{}", effective_rel.display())
    };
    Ok((root, canon, display))
}

fn resolve_new_config_path(raw: &str, config: &Config) -> Result<(PathBuf, PathBuf, String)> {
    let root = config_root()?;
    let rel = normalize_config_subpath(raw, &root)?;
    if rel.as_os_str().is_empty() {
        bail!("'path' is required when creating a file in ~/.config");
    }
    let blocked = blocked_globset(&[])?;
    check_allowed_file(&rel, &blocked, config)?;

    let target = root.join(&rel);
    let display = format!("~/.config/{}", rel.display());
    if target.exists() || target.symlink_metadata().is_ok() {
        bail!("File already exists: {display}. Use action: \"edit\" instead.");
    }
    ensure_ancestor_within_root(&target, &root, &display)?;
    Ok((root, target, display))
}

fn validate_written_file(path: &Path) -> Option<String> {
    if path.extension().and_then(|e| e.to_str()) == Some("fish") {
        let out = Command::new("fish").arg("-n").arg(path).output().ok()?;
        return Some(if out.status.success() {
            "[Syntax check: `fish -n` passed]".into()
        } else {
            format!(
                "[WARNING: `fish -n` reported syntax errors:\n{}]",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        });
    }
    if path.components().any(|c| c.as_os_str() == "hypr")
        && path.extension().and_then(|e| e.to_str()) == Some("conf")
    {
        let out = Command::new("hyprctl")
            .arg("configerrors")
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let trimmed = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return Some(if trimmed.is_empty() {
            "[Hyprland check: no config errors]".into()
        } else {
            format!("[Hyprland `hyprctl configerrors`: {trimmed}]")
        });
    }
    None
}

fn run_validate(raw_path: &str, config: &Config) -> Result<String> {
    let (root, target, display) = resolve_existing_config_path(raw_path, true, config)?;
    let blocked = blocked_globset(&[])?;
    let mut report = Vec::new();

    if target.is_file() {
        if let Some(msg) = validate_written_file(&target) {
            report.push(format!("{display}: {msg}"));
        } else {
            report.push(format!(
                "{display}: No specialized syntax validator for this file type."
            ));
        }
        return Ok(report.join("\n"));
    }

    // Check fish files under target (or target/fish)
    let fish_dir = if target == root {
        root.join("fish")
    } else {
        target.clone()
    };
    if fish_dir.exists() {
        let mut fish_checked = 0usize;
        let mut fish_errors = Vec::new();
        for entry in WalkBuilder::new(&fish_dir)
            .hidden(false)
            .git_ignore(false)
            .build()
            .flatten()
        {
            let p = entry.path();
            if !p.is_file() || p.extension().and_then(|e| e.to_str()) != Some("fish") {
                continue;
            }
            if let Some(rel) = diff_paths(p, &root)
                && check_allowed_file(&rel, &blocked, config).is_err()
            {
                continue;
            }
            if let Ok(out) = Command::new("fish").arg("-n").arg(p).output() {
                fish_checked += 1;
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    fish_errors.push(format!("  {}: {err}", p.display()));
                }
            }
        }
        if fish_checked > 0 {
            if fish_errors.is_empty() {
                report.push(format!(
                    "Fish syntax (`fish -n`): All {fish_checked} `.fish` file(s) passed."
                ));
            } else {
                report.push(format!(
                    "Fish syntax (`fish -n`): {} error(s) across {fish_checked} file(s):\n{}",
                    fish_errors.len(),
                    fish_errors.join("\n")
                ));
            }
        }
    }

    // Check Hyprland config errors if hyprctl is active
    if (target == root || target.starts_with(root.join("hypr")))
        && let Ok(out) = Command::new("hyprctl").arg("configerrors").output()
        && out.status.success()
    {
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if stdout.is_empty() {
            report.push("Hyprland (`hyprctl configerrors`): No configuration errors.".into());
        } else {
            report.push(format!("Hyprland (`hyprctl configerrors`):\n{stdout}"));
        }
    }

    if report.is_empty() {
        Ok(format!("No validators matched under {display}."))
    } else {
        Ok(report.join("\n\n"))
    }
}

fn bare_repo_dir() -> PathBuf {
    expand_tilde(BARE_REPO_PATH)
}

fn resolve_repo_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains("://")
        || trimmed.contains('@')
        || trimmed.starts_with('/')
        || trimmed.starts_with('~')
    {
        return Some(trimmed.to_string());
    }
    let slug = trimmed.trim_end_matches(".git").trim_matches('/');
    if slug.contains('/') {
        Some(format!("https://github.com/{slug}.git"))
    } else {
        Some(trimmed.to_string())
    }
}

fn bare_git_cmd(git_dir: &Path, work_tree: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(work_tree)
        .arg("--no-pager")
        .arg(format!("--git-dir={}", git_dir.display()))
        .arg(format!("--work-tree={}", work_tree.display()))
        .args(["-c", "status.showUntrackedFiles=no"]);
    cmd
}

fn configured_allowed_paths(root: &Path, config: &Config) -> Result<Vec<PathBuf>> {
    let allowed = &config.dotfiles.allowed_dirs;
    if allowed.is_empty() || allowed.iter().any(|s| s == "*") {
        bail!(
            "Set explicit directories or files in `dotfiles.allowed_dirs` in config.toml \
             (e.g. `allowed_dirs = [\"fish\", \"hypr\", \"archon\"]`) to specify which ~/.config files to track."
        );
    }
    let blocked = blocked_globset(&[])?;
    let mut paths = Vec::new();
    for entry in allowed {
        let rel = normalize_config_subpath(entry, root)?;
        if rel.as_os_str().is_empty() {
            bail!("`dotfiles.allowed_dirs` entries must be specific paths inside ~/.config");
        }
        check_not_blocked(&rel, &blocked)?;
        paths.push(rel);
    }
    Ok(paths)
}

fn collect_stageable_files(root: &Path, allowed_paths: &[PathBuf]) -> Result<Vec<String>> {
    let blocked = blocked_globset(&[])?;
    let mut files = Vec::new();

    for rel_target in allowed_paths {
        let full = root.join(rel_target);
        if !full.exists() {
            continue;
        }
        if full.is_file() {
            check_not_blocked(rel_target, &blocked)?;
            files.push(rel_target.to_string_lossy().into_owned());
            continue;
        }
        for entry in WalkBuilder::new(&full)
            .hidden(false)
            .git_ignore(false)
            .build()
            .flatten()
        {
            let p = entry.path();
            if !p.is_file() || has_git_dir_component(p) {
                continue;
            }
            let Ok(canon) = dunce::canonicalize(p) else {
                continue;
            };
            if !canon.starts_with(root) {
                continue;
            }
            let Some(rel) = diff_paths(&canon, root) else {
                continue;
            };
            if check_not_blocked(&rel, &blocked).is_ok() {
                files.push(rel.to_string_lossy().into_owned());
            }
        }
    }

    files.sort();
    files.dedup();
    Ok(files)
}

fn run_git_status(config: &Config) -> Result<String> {
    let root = config_root()?;
    let git_dir = bare_repo_dir();
    if !git_dir.join("HEAD").is_file() {
        return Ok(format!(
            "Bare git repository is not initialized at {BARE_REPO_PATH}.\n\
             Run `dotfiles` with `action: \"sync\"` to initialize or clone from `{}`.",
            config.dotfiles.repo_url
        ));
    }

    let allowed_paths = configured_allowed_paths(&root, config).ok();
    let existing_targets: Vec<String> = allowed_paths
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|p| root.join(p).exists())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    let mut status_cmd = bare_git_cmd(&git_dir, &root);
    status_cmd.args(["status", "--short", "--branch"]);
    if let Some(ref allowed) = allowed_paths {
        let stageable = collect_stageable_files(&root, allowed)?;
        if !stageable.is_empty() {
            status_cmd.arg("-uall").arg("--").args(&stageable);
        } else if !existing_targets.is_empty() {
            status_cmd.arg("--").args(&existing_targets);
        }
    }

    let status_out = status_cmd
        .output()
        .context("Failed to execute `git status` on bare repo")?;
    if !status_out.status.success() {
        bail!(
            "Bare git status failed: {}",
            String::from_utf8_lossy(&status_out.stderr).trim()
        );
    }

    let mut diff_cmd = bare_git_cmd(&git_dir, &root);
    diff_cmd.args(["diff", "--stat"]);
    if !existing_targets.is_empty() {
        diff_cmd.arg("--").args(&existing_targets);
    }
    let diff_out = diff_cmd.output().ok();

    let mut out = String::from_utf8_lossy(&status_out.stdout)
        .trim_end()
        .to_string();
    if let Some(d) = diff_out.filter(|o| o.status.success()) {
        let stat = String::from_utf8_lossy(&d.stdout).trim().to_string();
        if !stat.is_empty() {
            let _ = write!(out, "\n\n{stat}");
        }
    }

    if out.trim().is_empty() {
        Ok("Dotfiles working tree clean.".into())
    } else {
        Ok(out)
    }
}

fn has_bare_head(git_dir: &Path, work_tree: &Path) -> bool {
    bare_git_cmd(git_dir, work_tree)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn resolve_git_targets(
    raw_path: &str,
    root: &Path,
    config: &Config,
) -> Result<(Option<String>, Vec<String>)> {
    let rel = normalize_config_subpath(raw_path, root)?;
    let blocked = blocked_globset(&[])?;

    if !rel.as_os_str().is_empty() {
        check_allowed_file(&rel, &blocked, config)?;
        let candidate = root.join(&rel);
        let label = format!("~/.config/{}", rel.display());
        let effective_rel = if candidate.exists() {
            let canon = ensure_within_root(&candidate, root, &label)?;
            let eff = diff_paths(&canon, root).unwrap_or(rel);
            check_allowed_file(&eff, &blocked, config)?;
            eff
        } else {
            ensure_ancestor_within_root(&candidate, root, &label)?;
            rel
        };

        let rel_str = effective_rel.to_string_lossy().into_owned();
        return Ok((Some(format!("~/.config/{rel_str}")), vec![rel_str]));
    }

    let targets = configured_allowed_paths(root, config)
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    Ok((None, targets))
}

fn run_git_diff(raw_path: &str, config: &Config) -> Result<String> {
    let root = config_root()?;
    let git_dir = bare_repo_dir();
    if !git_dir.join("HEAD").is_file() {
        return Ok(format!(
            "Bare git repository is not initialized at {BARE_REPO_PATH}.\n\
             Run `dotfiles` with `action: \"sync\"` to initialize or clone from `{}`.",
            config.dotfiles.repo_url
        ));
    }

    let (scoped_display, targets) = resolve_git_targets(raw_path, &root, config)?;
    let mut diff_cmd = bare_git_cmd(&git_dir, &root);
    diff_cmd.arg("diff");
    if has_bare_head(&git_dir, &root) {
        diff_cmd.arg("HEAD");
    }
    diff_cmd.arg("--no-ext-diff");
    if !targets.is_empty() {
        diff_cmd.arg("--").args(&targets);
    }

    let diff_out = diff_cmd
        .output()
        .context("Failed to execute `git diff` on bare repo")?;
    if !diff_out.status.success() {
        bail!(
            "Bare git diff failed: {}",
            String::from_utf8_lossy(&diff_out.stderr).trim()
        );
    }

    let out = String::from_utf8_lossy(&diff_out.stdout)
        .trim_end()
        .to_string();
    if out.trim().is_empty() {
        Ok(match scoped_display {
            Some(display) => format!("No uncommitted changes for {display}."),
            None => "No uncommitted changes in tracked dotfiles.".into(),
        })
    } else {
        Ok(out)
    }
}

fn collect_restorable_files(
    raw_path: &str,
    root: &Path,
    git_dir: &Path,
    config: &Config,
) -> Result<(Option<String>, Vec<String>)> {
    if !git_dir.join("HEAD").is_file() || !has_bare_head(git_dir, root) {
        bail!(
            "Bare git repository at {BARE_REPO_PATH} has no commits yet. \
             Run `dotfiles` with `action: \"sync\"` to create a stable baseline first."
        );
    }

    let (scoped_display, targets) = resolve_git_targets(raw_path, root, config)?;

    if let Some(ref display) = scoped_display {
        let mut ls_cmd = bare_git_cmd(git_dir, root);
        ls_cmd.args(["ls-tree", "HEAD", "--"]).args(&targets);
        let ls_out = ls_cmd
            .output()
            .context("Failed to query tracked files in bare repo")?;
        if !ls_out.status.success() || String::from_utf8_lossy(&ls_out.stdout).trim().is_empty() {
            bail!("No tracked files in HEAD match '{display}'");
        }
    }

    let mut diff_cmd = bare_git_cmd(git_dir, root);
    diff_cmd.args(["diff", "--name-only", "HEAD"]);
    if !targets.is_empty() {
        diff_cmd.arg("--").args(&targets);
    }
    let diff_out = diff_cmd
        .output()
        .context("Failed to list modified files in bare repo")?;
    if !diff_out.status.success() {
        bail!(
            "Failed to inspect modified dotfiles: {}",
            String::from_utf8_lossy(&diff_out.stderr).trim()
        );
    }

    let blocked = blocked_globset(&[])?;
    let mut files: Vec<String> = String::from_utf8_lossy(&diff_out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|rel_str| check_allowed_file(Path::new(rel_str), &blocked, config).is_ok())
        .map(String::from)
        .collect();
    files.sort();
    files.dedup();

    Ok((scoped_display, files))
}

fn run_git_restore(raw_path: &str, config: &Config) -> Result<String> {
    let root = config_root()?;
    let git_dir = bare_repo_dir();
    let (scoped_display, changed_files) =
        collect_restorable_files(raw_path, &root, &git_dir, config)?;

    if changed_files.is_empty() {
        return Ok(match scoped_display {
            Some(display) => {
                format!("No uncommitted changes to restore for {display} (already matches HEAD).")
            }
            None => {
                "No uncommitted changes to restore; tracked dotfiles already match HEAD.".into()
            }
        });
    }

    let co_out = bare_git_cmd(&git_dir, &root)
        .args(["checkout", "HEAD", "--"])
        .args(&changed_files)
        .output()
        .context("Failed to run `git checkout HEAD` on bare repo")?;
    if !co_out.status.success() {
        bail!(
            "Failed to restore dotfiles from HEAD: {}",
            String::from_utf8_lossy(&co_out.stderr).trim()
        );
    }

    let mut msg = format!(
        "Restored {} file(s) to HEAD:\n{}",
        changed_files.len(),
        changed_files
            .iter()
            .map(|f| format!("  ~/.config/{f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let mut checks = Vec::new();
    for f in &changed_files {
        let full_path = root.join(f);
        if full_path.is_file()
            && let Some(check) = validate_written_file(&full_path)
            && !checks.contains(&check)
        {
            checks.push(check);
        }
    }
    for check in checks {
        let _ = write!(msg, "\n{check}");
    }

    Ok(msg)
}

fn ensure_bare_repo(git_dir: &Path, work_tree: &Path, config: &Config) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let repo_url = resolve_repo_url(&config.dotfiles.repo_url);

    if !git_dir.join("HEAD").is_file() {
        if let Some(parent) = git_dir.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }

        let mut cloned = false;
        if let Some(ref url) = repo_url
            && Command::new("git")
                .args(["clone", "--bare", url])
                .arg(git_dir)
                .output()
                .is_ok_and(|o| o.status.success())
        {
            notes.push(format!(
                "Cloned bare repo from {url} into {BARE_REPO_PATH}."
            ));
            cloned = true;
        }

        if !cloned {
            let init_out = Command::new("git")
                .args(["init", "--bare"])
                .arg(git_dir)
                .output()
                .context("Failed to run `git init --bare`")?;
            if !init_out.status.success() {
                bail!(
                    "git init --bare failed: {}",
                    String::from_utf8_lossy(&init_out.stderr).trim()
                );
            }
            notes.push(format!(
                "Initialized bare git repository at {BARE_REPO_PATH}."
            ));
        }

        let _ = bare_git_cmd(git_dir, work_tree)
            .args(["config", "status.showUntrackedFiles", "no"])
            .output();

        if cloned && has_bare_head(git_dir, work_tree) {
            let _ = bare_git_cmd(git_dir, work_tree)
                .args(["read-tree", "HEAD"])
                .output();
        }
    }

    if let Some(ref url) = repo_url {
        let has_origin = bare_git_cmd(git_dir, work_tree)
            .args(["remote", "get-url", "origin"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !has_origin {
            let _ = bare_git_cmd(git_dir, work_tree)
                .args(["remote", "add", "origin", url])
                .output();
        }
    }

    Ok(notes)
}

fn generate_commit_message(git_dir: &Path, work_tree: &Path, config: &Config) -> Option<String> {
    let stat_out = bare_git_cmd(git_dir, work_tree)
        .args(["diff", "--cached", "--stat"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let stat = String::from_utf8_lossy(&stat_out.stdout).trim().to_string();
    if stat.is_empty() {
        return None;
    }

    let diff_out = bare_git_cmd(git_dir, work_tree)
        .args(["diff", "--cached", "--no-ext-diff"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let diff = truncate_str(String::from_utf8_lossy(&diff_out.stdout).trim(), 8_000);

    let handle = tokio::runtime::Handle::try_current().ok()?;
    let model = config.startup_model();
    let provider = Provider::from_model(&model);
    let num_ctx = config.context_for_model(&model);
    let cfg = config.clone();

    let system_prompt = "You write concise, conventional git commit messages (e.g. 'feat(hypr): ...', 'chore(fish): ...') for ~/.config dotfiles. Output ONLY the raw commit message (a short summary line under 72 characters, optionally followed by brief bullet points if multiple configs changed). Do not wrap in quotes or markdown code blocks.";
    let prompt = format!(
        "Generate a commit message for these staged ~/.config changes:\n\n<stat>\n{stat}\n</stat>\n\n<diff>\n{diff}\n</diff>"
    );

    handle.block_on(async move {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .ok()?;
        let (response, _, _) = provider
            .complete_oneshot(client, model, cfg, num_ctx, system_prompt, prompt)
            .await
            .ok()?;

        let cleaned = response
            .trim()
            .trim_start_matches("```text")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
            .trim_matches('"')
            .trim();
        (!cleaned.is_empty()).then(|| cleaned.to_string())
    })
}

fn run_git_sync(commit_msg: &str, config: &Config) -> Result<String> {
    let root = config_root()?;
    let allowed_paths = configured_allowed_paths(&root, config)?;
    let git_dir = bare_repo_dir();

    let mut report = ensure_bare_repo(&git_dir, &root, config)?;

    // Update/stage deletions for already-tracked paths within allowed_dirs
    if has_bare_head(&git_dir, &root) {
        let rel_strings: Vec<String> = allowed_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let _ = bare_git_cmd(&git_dir, &root)
            .args(["add", "-u", "--"])
            .args(&rel_strings)
            .output();
    }

    // Stage all non-blocked files inside allowed_dirs
    let stageable = collect_stageable_files(&root, &allowed_paths)?;
    if !stageable.is_empty() {
        let add_out = bare_git_cmd(&git_dir, &root)
            .args(["add", "--"])
            .args(&stageable)
            .output()
            .context("Failed to stage dotfiles")?;
        if !add_out.status.success() {
            bail!(
                "Failed to stage dotfiles: {}",
                String::from_utf8_lossy(&add_out.stderr).trim()
            );
        }
    }

    let has_staged = bare_git_cmd(&git_dir, &root)
        .args(["diff", "--cached", "--quiet"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false);

    if has_staged {
        let generated_msg;
        let msg = if !commit_msg.trim().is_empty() {
            commit_msg.trim()
        } else if let Some(llm_msg) = generate_commit_message(&git_dir, &root, config) {
            generated_msg = llm_msg;
            &generated_msg
        } else {
            "Sync dotfiles"
        };
        let commit_out = bare_git_cmd(&git_dir, &root)
            .args(["commit", "-m", msg])
            .output()
            .context("Failed to commit dotfiles")?;
        if !commit_out.status.success() {
            bail!(
                "git commit failed: {}",
                String::from_utf8_lossy(&commit_out.stderr).trim()
            );
        }
        report.push(
            String::from_utf8_lossy(&commit_out.stdout)
                .trim()
                .to_string(),
        );
    } else {
        report.push("No local changes to commit in tracked `dotfiles.allowed_dirs`.".into());
    }

    if resolve_repo_url(&config.dotfiles.repo_url).is_some() && has_bare_head(&git_dir, &root) {
        let push_out = bare_git_cmd(&git_dir, &root)
            .args(["push", "-u", "origin", "HEAD"])
            .output()
            .context("Failed to run `git push`")?;
        if push_out.status.success() {
            let stderr = String::from_utf8_lossy(&push_out.stderr).trim().to_string();
            report.push(if stderr.is_empty() {
                "Pushed to origin.".into()
            } else {
                format!("Pushed to origin ({stderr}).")
            });
        } else {
            let err = String::from_utf8_lossy(&push_out.stderr).trim().to_string();
            report.push(format!("Push to origin skipped/failed: {err}"));
        }
    }

    Ok(report.join("\n"))
}

pub fn preview_edit(
    args: &DotfilesArgs,
    config: &Config,
) -> Result<(PathBuf, String, String, String, usize)> {
    if args.edits.is_empty() {
        bail!("'edits' array cannot be empty for dotfiles edit");
    }
    let (_, target_path, display) = resolve_existing_config_path(&args.path, false, config)?;
    let original = fs::read_to_string(&target_path).context("Error reading config file")?;
    let updated = apply_hunks_to_str(&original, &display, &args.edits)?;
    Ok((target_path, display, original, updated, args.edits.len()))
}

pub fn preview_create(args: &DotfilesArgs, config: &Config) -> Result<(PathBuf, String)> {
    let (_, target_path, display) = resolve_new_config_path(&args.path, config)?;
    Ok((target_path, display))
}

pub fn preview_restore(args: &DotfilesArgs, config: &Config) -> Result<(usize, String)> {
    let root = config_root()?;
    let git_dir = bare_repo_dir();
    let (scoped_display, changed_files) =
        collect_restorable_files(&args.path, &root, &git_dir, config)?;

    if changed_files.is_empty() {
        bail!(
            "{}",
            match scoped_display {
                Some(display) => format!("No uncommitted changes to restore in {display}"),
                None => "No uncommitted changes to restore in tracked dotfiles".into(),
            }
        );
    }

    let diff_out = bare_git_cmd(&git_dir, &root)
        .args(["diff", "-R", "HEAD", "--no-ext-diff", "--"])
        .args(&changed_files)
        .output()
        .context("Failed to preview restore diff")?;
    let diff = String::from_utf8_lossy(&diff_out.stdout)
        .trim_end()
        .to_string();
    Ok((changed_files.len(), diff))
}

impl BlockingTool for Tool {
    const NAME: &'static str = "dotfiles";
    const DESCRIPTION: &'static str = "Manage personal user configuration files in `~/.config` (list, read, search, edit, create, validate, status, diff, restore, sync). Tracks `dotfiles.allowed_dirs` in a bare git repo (`~/.local/share/archon/dotfiles.git`).";
    type Args = DotfilesArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        match args.action {
            DotfilesAction::List => {
                let (root, target, display) =
                    resolve_existing_config_path(&args.path, true, &ctx.config)?;
                let blocked = blocked_globset(&[])?;
                let max_depth = args.max_depth.unwrap_or(2).clamp(1, 5);
                render_directory_listing(
                    &target,
                    &root,
                    &display,
                    max_depth,
                    ctx.max_output_size,
                    |path| {
                        diff_paths(path, &root).is_some_and(|rel| {
                            check_allowed_file(&rel, &blocked, &ctx.config).is_ok()
                        })
                    },
                )
            }
            DotfilesAction::Read => {
                let (_, target_path, display) =
                    resolve_existing_config_path(&args.path, false, &ctx.config)?;
                read_file_contents(
                    &target_path,
                    &display,
                    args.start_line,
                    args.end_line,
                    ctx.max_output_size,
                )
            }
            DotfilesAction::Search => {
                if args.pattern.is_empty() {
                    bail!("'pattern' is required when `action` is `search`");
                }
                let (root, scan_root, _) =
                    resolve_existing_config_path(&args.path, true, &ctx.config)?;
                let blocked = blocked_globset(&args.exclude)?;
                let include_set = (!args.include.is_empty())
                    .then(|| build_globset(&args.include, true))
                    .transpose()?;

                let mut candidate_files = Vec::new();
                if scan_root.is_file() {
                    let rel = diff_paths(&scan_root, &root).unwrap_or_else(|| scan_root.clone());
                    check_allowed_file(&rel, &blocked, &ctx.config)?;
                    let display = format!("~/.config/{}", rel.display());
                    candidate_files.push((scan_root, display));
                } else {
                    for entry in WalkBuilder::new(&scan_root)
                        .hidden(false)
                        .git_ignore(false)
                        .build()
                        .flatten()
                    {
                        if candidate_files.len() >= ctx.config.workspace_snapshot.max_files {
                            break;
                        }
                        let path = entry.path();
                        if !path.is_file() || has_git_dir_component(path) {
                            continue;
                        }
                        let Ok(canon) = dunce::canonicalize(path) else {
                            continue;
                        };
                        if !canon.starts_with(&root) {
                            continue;
                        }
                        let Some(rel) = diff_paths(&canon, &root) else {
                            continue;
                        };
                        if check_allowed_file(&rel, &blocked, &ctx.config).is_err()
                            || include_set.as_ref().is_some_and(|inc| !inc.is_match(&rel))
                        {
                            continue;
                        }
                        candidate_files
                            .push((canon, format!("~/.config/{}", rel.to_string_lossy())));
                    }
                    candidate_files.sort_by(|a, b| a.1.cmp(&b.1));
                }

                search_candidate_files(
                    candidate_files,
                    &SearchExecutionParams {
                        pattern: &args.pattern,
                        fixed_strings: args.fixed_strings,
                        files_with_matches: args.files_with_matches,
                        case_sensitive: args.case_sensitive,
                        context_lines: args.context_lines,
                        max_results: args.max_results,
                        max_file_size: ctx.config.workspace_snapshot.max_size,
                        max_output_size: ctx.max_output_size,
                    },
                )
            }
            DotfilesAction::Edit => {
                let (target_path, display, _, updated, hunk_count) =
                    preview_edit(&args, &ctx.config)?;
                atomic_write_file(&target_path, updated).context("Error writing config file")?;
                let mut msg = if hunk_count == 1 {
                    format!("Successfully edited {display}")
                } else {
                    format!("Successfully applied {hunk_count} edits to {display}")
                };
                if let Some(check) = validate_written_file(&target_path) {
                    let _ = write!(msg, "\n{check}");
                }
                Ok(msg)
            }
            DotfilesAction::Create => {
                let (root, target_path, display) =
                    resolve_new_config_path(&args.path, &ctx.config)?;
                ensure_parent_dir(&target_path, &root, "created parent directory")?;
                atomic_write_file(&target_path, &args.content)
                    .context("Error writing new config file")?;
                let mut msg = format!("Successfully created {display}");
                if let Some(check) = validate_written_file(&target_path) {
                    let _ = write!(msg, "\n{check}");
                }
                Ok(msg)
            }
            DotfilesAction::Validate => run_validate(&args.path, &ctx.config),
            DotfilesAction::Status => run_git_status(&ctx.config),
            DotfilesAction::Diff => run_git_diff(&args.path, &ctx.config),
            DotfilesAction::Restore => run_git_restore(&args.path, &ctx.config),
            DotfilesAction::Sync => run_git_sync(&args.content, &ctx.config),
        }
    }
}
