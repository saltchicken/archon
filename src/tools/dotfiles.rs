use super::{
    BlockingTool, ToolContext, atomic_write_file, build_globset,
    edit_file::{EditHunk, apply_hunks_to_str},
    ensure_parent_dir, has_git_dir_component,
    read_file::read_file_contents,
    render_directory_listing,
    search::{SearchExecutionParams, search_candidate_files},
};
use crate::core::config::expand_tilde;
use anyhow::{Context, Result, bail};
use globset::GlobSet;
use ignore::WalkBuilder;
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    fmt::Write as _,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

pub struct Tool;

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
}

#[derive(Deserialize, JsonSchema)]
pub struct DotfilesArgs {
    /// The operation to perform in `~/.config`.
    pub action: DotfilesAction,
    /// Path relative to `~/.config` (e.g. `"hypr/hyprland.conf"`, `"fish/config.fish"`, or `"."` for `list`/`search`/`validate`).
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
    /// Full file contents when `action` is `create`.
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

fn resolve_existing_config_path(raw: &str, allow_root: bool) -> Result<(PathBuf, PathBuf, String)> {
    let root = config_root()?;
    let rel = normalize_config_subpath(raw, &root)?;
    if rel.as_os_str().is_empty() && !allow_root {
        bail!("'path' inside ~/.config is required for this action");
    }
    let blocked = blocked_globset(&[])?;
    check_not_blocked(&rel, &blocked)?;

    let candidate = if rel.as_os_str().is_empty() {
        root.clone()
    } else {
        root.join(&rel)
    };
    if !candidate.exists() {
        bail!("Target path does not exist: ~/.config/{}", rel.display());
    }
    let canon = dunce::canonicalize(&candidate)?;
    if !canon.starts_with(&root) {
        bail!(
            "Security violation: '~/.config/{}' resolves outside ~/.config ({})",
            rel.display(),
            canon.display()
        );
    }
    let effective_rel = diff_paths(&canon, &root).unwrap_or(rel);
    check_not_blocked(&effective_rel, &blocked)?;
    let display = if effective_rel.as_os_str().is_empty() {
        "~/.config".to_string()
    } else {
        format!("~/.config/{}", effective_rel.display())
    };
    Ok((root, canon, display))
}

fn resolve_new_config_path(raw: &str) -> Result<(PathBuf, PathBuf, String)> {
    let root = config_root()?;
    let rel = normalize_config_subpath(raw, &root)?;
    if rel.as_os_str().is_empty() {
        bail!("'path' is required when creating a file in ~/.config");
    }
    let blocked = blocked_globset(&[])?;
    check_not_blocked(&rel, &blocked)?;

    let target = root.join(&rel);
    if target.exists() {
        bail!(
            "File already exists: ~/.config/{}. Use action: \"edit\" instead.",
            rel.display()
        );
    }
    if let Some(existing) = target.ancestors().find(|p| p.exists()) {
        let canon_anc = dunce::canonicalize(existing)?;
        if !canon_anc.starts_with(&root) {
            bail!(
                "Security violation: '~/.config/{}' escapes ~/.config",
                rel.display()
            );
        }
    }
    let display = format!("~/.config/{}", rel.display());
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

fn run_validate(raw_path: &str) -> Result<String> {
    let (root, target, display) = resolve_existing_config_path(raw_path, true)?;
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
                && check_not_blocked(&rel, &blocked).is_err()
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

pub fn preview_edit(args: &DotfilesArgs) -> Result<(PathBuf, String, String, String, usize)> {
    if args.edits.is_empty() {
        bail!("'edits' array cannot be empty for dotfiles edit");
    }
    let (_, target_path, display) = resolve_existing_config_path(&args.path, false)?;
    let original = fs::read_to_string(&target_path).context("Error reading config file")?;
    let updated = apply_hunks_to_str(&original, &display, &args.edits)?;
    Ok((target_path, display, original, updated, args.edits.len()))
}

pub fn preview_create(args: &DotfilesArgs) -> Result<(PathBuf, String)> {
    let (_, target_path, display) = resolve_new_config_path(&args.path)?;
    Ok((target_path, display))
}

impl BlockingTool for Tool {
    const NAME: &'static str = "dotfiles";
    const DESCRIPTION: &'static str = "Manage personal user configuration files in `~/.config` (list, read, search, edit, create, validate). Protects sensitive credentials/browser directories and validates Fish/Hyprland configs.";
    type Args = DotfilesArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        match args.action {
            DotfilesAction::List => {
                let (root, target, display) = resolve_existing_config_path(&args.path, true)?;
                let blocked = blocked_globset(&[])?;
                let max_depth = args.max_depth.unwrap_or(2).clamp(1, 5);
                render_directory_listing(
                    &target,
                    &root,
                    &display,
                    max_depth,
                    ctx.max_output_size,
                    |path| {
                        diff_paths(path, &root)
                            .is_some_and(|rel| check_not_blocked(&rel, &blocked).is_ok())
                    },
                )
            }
            DotfilesAction::Read => {
                let (_, target_path, display) = resolve_existing_config_path(&args.path, false)?;
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
                let (root, scan_root, _) = resolve_existing_config_path(&args.path, true)?;
                let blocked = blocked_globset(&args.exclude)?;
                let include_set = (!args.include.is_empty())
                    .then(|| build_globset(&args.include, true))
                    .transpose()?;

                let mut candidate_files = Vec::new();
                if scan_root.is_file() {
                    let rel = diff_paths(&scan_root, &root).unwrap_or_else(|| scan_root.clone());
                    check_not_blocked(&rel, &blocked)?;
                    let display = format!("~/.config/{}", rel.display());
                    candidate_files.push((scan_root, display));
                } else {
                    for entry in WalkBuilder::new(&scan_root)
                        .hidden(false)
                        .git_ignore(false)
                        .build()
                        .flatten()
                    {
                        if candidate_files.len() >= ctx.snapshot.max_files {
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
                        if check_not_blocked(&rel, &blocked).is_err() {
                            continue;
                        }
                        if include_set.as_ref().is_some_and(|inc| !inc.is_match(&rel)) {
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
                        max_file_size: ctx.snapshot.max_size,
                        max_output_size: ctx.max_output_size,
                    },
                )
            }
            DotfilesAction::Edit => {
                let (target_path, display, _, updated, hunk_count) = preview_edit(&args)?;
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
                let (root, target_path, display) = resolve_new_config_path(&args.path)?;
                ensure_parent_dir(&target_path, &root, "created parent directory")?;
                atomic_write_file(&target_path, &args.content)
                    .context("Error writing new config file")?;
                let mut msg = format!("Successfully created {display}");
                if let Some(check) = validate_written_file(&target_path) {
                    let _ = write!(msg, "\n{check}");
                }
                Ok(msg)
            }
            DotfilesAction::Validate => run_validate(&args.path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_validates_config_paths() {
        let root = PathBuf::from("/home/user/.config");
        assert_eq!(
            normalize_config_subpath("hypr/hyprland.conf", &root).unwrap(),
            PathBuf::from("hypr/hyprland.conf")
        );
        assert_eq!(
            normalize_config_subpath("~/.config/fish/config.fish", &root).unwrap(),
            PathBuf::from("fish/config.fish")
        );
        assert_eq!(
            normalize_config_subpath(".config/waybar/config", &root).unwrap(),
            PathBuf::from("waybar/config")
        );
        assert!(normalize_config_subpath("../.ssh/id_rsa", &root).is_err());
        assert!(normalize_config_subpath("/etc/passwd", &root).is_err());
    }

    #[test]
    fn blocks_sensitive_config_paths() {
        let blocked = blocked_globset(&[]).unwrap();
        assert!(check_not_blocked(Path::new("gh/hosts.yml"), &blocked).is_err());
        assert!(check_not_blocked(Path::new("google-chrome/Default/Cookies"), &blocked).is_err());
        assert!(check_not_blocked(Path::new("Bitwarden/data.json"), &blocked).is_err());
        assert!(check_not_blocked(Path::new("rclone/rclone.conf"), &blocked).is_err());
        assert!(check_not_blocked(Path::new("my_app/.env.local"), &blocked).is_err());
        assert!(check_not_blocked(Path::new("hypr/hyprland.conf"), &blocked).is_ok());
        assert!(check_not_blocked(Path::new("fish/config.fish"), &blocked).is_ok());
    }
}
