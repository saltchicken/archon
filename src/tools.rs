mod cargo;
mod create_file;
mod dotfiles;
mod edit_file;
mod fetch_url;
mod fs;
mod git;
mod memory;
mod notify;
mod read_file;
mod repo_map;
mod search;
mod skill;
mod system;
pub(crate) mod utils;
mod workspace_snapshot;

use crate::core::config::{Config, expand_tilde};
use anyhow::{Context, Result, bail};
use globset::GlobSet;
use ignore::{DirEntry, WalkBuilder};
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use similar::TextDiff;
use std::{
    ffi::OsStr,
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::timeout};
use tracing::{debug, error, info};
use utils::{
    fs::read_capped,
    path::{
        build_ancestor_dir_globset, build_globset, ensure_within_root, has_git_dir_component,
        has_hidden_component, partition_negated_patterns, resolve_existing_under,
        validate_relative_path, workspace_root,
    },
};

const COMMAND_TIMEOUT_SECS: u64 = 30;

#[cfg(unix)]
fn kill_process_group(pgid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    if let Ok(pgid) = i32::try_from(pgid)
        && pgid > 0
    {
        // SAFETY: Sending SIGKILL to a negative process group ID (-pgid) is a
        // standard POSIX syscall with no memory-safety preconditions.
        unsafe {
            let _ = kill(-pgid, SIGKILL);
        }
    }
}

struct ManagedChild {
    child: tokio::process::Child,
    #[cfg(unix)]
    pgid: Option<u32>,
}

impl ManagedChild {
    fn new(child: tokio::process::Child) -> Self {
        #[cfg(unix)]
        let pgid = child.id();
        Self {
            child,
            #[cfg(unix)]
            pgid,
        }
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await;
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            kill_process_group(pgid);
        }
        status
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            kill_process_group(pgid);
        }
    }
}

const DEFAULT_EXCLUDE_PATTERNS: &[&str] = &[
    "**/.git/**",
    "**/*.venv/**",
    "**/node_modules/**",
    "**/target/**",
    "**/.env*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/secrets.json",
];

pub(super) trait BlockingTool {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String>;
}

pub(super) trait AsyncTool {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

    async fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String>;
}

#[derive(Clone)]
pub(super) struct ToolContext {
    pub root: PathBuf,
    pub max_output_size: usize,
    pub active_skill: Option<String>,
    pub config: Config,
}

impl ToolContext {
    fn new(config: &Config, active_skill: Option<&str>, max_output_size: usize) -> Result<Self> {
        Ok(Self {
            root: workspace_root()?,
            max_output_size,
            active_skill: active_skill.map(String::from),
            config: config.clone(),
        })
    }

    pub fn ensure_within_workspace(&self, path: &Path, label: &str) -> Result<PathBuf> {
        ensure_within_root(path, &self.root, label)
    }

    pub fn ensure_path_or_ancestor_within_workspace(
        &self,
        target: &Path,
        label: &str,
    ) -> Result<PathBuf> {
        if let Some(existing) = target
            .ancestors()
            .find(|p| p.exists() || p.symlink_metadata().is_ok())
        {
            let canon_anc = self.ensure_within_workspace(existing, label)?;
            if let Ok(rem) = target.strip_prefix(existing) {
                return Ok(canon_anc.join(rem));
            }
            return Ok(canon_anc);
        }
        Ok(target.to_path_buf())
    }

    pub fn validate_scoped_relative_path<'a>(&self, raw: &'a str) -> Result<&'a Path> {
        let rel = validate_relative_path(raw)?;
        let target = self.root.join(rel);
        let resolved = self.ensure_path_or_ancestor_within_workspace(&target, raw.trim())?;
        self.ensure_not_excluded(&target)?;
        self.ensure_not_excluded(&resolved)?;
        Ok(rel)
    }

    pub fn resolve_existing_under_root(&self, raw: &str, escape_label: &str) -> Result<PathBuf> {
        let trimmed = raw.trim();
        let neg_ancestors = self.build_neg_exclude_ancestor_globset()?;
        if !trimmed.is_empty() && trimmed != "." {
            let rel = validate_relative_path(trimmed)?;
            let target = self.root.join(rel);
            if !(target.is_dir() && neg_ancestors.is_match(rel)) {
                self.ensure_not_excluded(&target)?;
            }
        }
        let canon = resolve_existing_under(&self.root, raw, escape_label)?;
        let canon_rel = diff_paths(&canon, &self.root).unwrap_or_else(|| canon.clone());
        if !(canon.is_dir() && neg_ancestors.is_match(&canon_rel)) {
            self.ensure_not_excluded(&canon)?;
        }
        Ok(canon)
    }

    pub fn check_rel_path_allowed(
        &self,
        rel: &Path,
        exclude_set: &GlobSet,
        neg_exclude_set: &GlobSet,
    ) -> Result<()> {
        if rel.as_os_str().is_empty() || rel == Path::new(".") {
            return Ok(());
        }
        if has_git_dir_component(rel) {
            bail!(
                "Security violation: '{}' matches an excluded pattern",
                rel.display()
            );
        }
        let is_negated = neg_exclude_set.is_match(rel);
        if !self.config.app.allow_hidden && has_hidden_component(rel) && !is_negated {
            bail!(
                "Security violation: '{}' is a hidden file or directory (set `app.allow_hidden = true` in config.toml to allow access)",
                rel.display()
            );
        }
        if exclude_set.is_match(rel) && !is_negated {
            bail!(
                "Security violation: '{}' matches an excluded pattern",
                rel.display()
            );
        }
        let mut prefix = PathBuf::new();
        for comp in rel.components() {
            prefix.push(comp);
            if exclude_set.is_match(&prefix) && !is_negated {
                bail!(
                    "Security violation: '{}' matches an excluded pattern",
                    rel.display()
                );
            }
        }
        Ok(())
    }

    pub fn ensure_not_excluded_under(&self, path: &Path, base_root: &Path) -> Result<()> {
        let rel = diff_paths(path, base_root).unwrap_or_else(|| path.to_path_buf());
        let (exclude_set, neg_exclude_set) = self.build_exclude_globsets(&[], true)?;
        self.check_rel_path_allowed(&rel, &exclude_set, &neg_exclude_set)
    }

    pub fn ensure_not_excluded(&self, canon_path: &Path) -> Result<()> {
        self.ensure_not_excluded_under(canon_path, &self.root)
    }

    pub fn resolve_existing_path(&self, raw: &str) -> Result<PathBuf> {
        let trimmed = raw.trim();
        let target = self.root.join(validate_relative_path(trimmed)?);
        if !target.exists() {
            bail!("Target path does not exist: '{trimmed}'");
        }
        self.ensure_not_excluded(&target)?;
        let canon = self.ensure_within_workspace(&target, trimmed)?;
        self.ensure_not_excluded(&canon)?;
        Ok(canon)
    }

    pub fn resolve_readable_path(&self, raw: &str) -> Result<PathBuf> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("Security violation: path cannot be empty");
        }

        let expanded = expand_tilde(trimmed);
        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            self.root.join(validate_relative_path(trimmed)?)
        };

        if !candidate.exists() {
            bail!("Target path does not exist: '{trimmed}'");
        }
        let canon = dunce::canonicalize(&candidate)?;

        let global_skills_root = expand_tilde(&self.config.app.skills_dir);
        let canon_global = dunce::canonicalize(&global_skills_root).unwrap_or(global_skills_root);
        let canon_ws = self
            .config
            .workspace_skills_dir
            .as_ref()
            .map(|d| dunce::canonicalize(d).unwrap_or_else(|_| d.clone()));

        if canon.starts_with(&canon_global)
            || canon_ws.as_ref().is_some_and(|ws| canon.starts_with(ws))
        {
            let allowed = self
                .active_skill
                .as_deref()
                .and_then(|name| crate::core::skills::find_skill(&self.config, name))
                .and_then(|s| s.dir_path.as_deref())
                .filter(|d| d.is_dir())
                .is_some_and(|dir| {
                    let abs_dir = if dir.is_absolute() {
                        dir.to_path_buf()
                    } else {
                        self.root.join(dir)
                    };
                    let canon_dir = dunce::canonicalize(&abs_dir).unwrap_or(abs_dir);
                    canon.starts_with(&canon_dir)
                });

            if !allowed {
                bail!(
                    "Security violation: cannot read skill file '{trimmed}' because its skill is not active."
                );
            }
            return Ok(canon);
        }

        if !candidate.starts_with(&canon_global)
            && !canon_ws
                .as_ref()
                .is_some_and(|ws| candidate.starts_with(ws))
        {
            self.ensure_not_excluded(&candidate)?;
        }
        let canon = self.ensure_within_workspace(&canon, trimmed)?;
        self.ensure_not_excluded(&canon)?;
        Ok(canon)
    }

    pub fn resolve_new_path(&self, raw: &str) -> Result<PathBuf> {
        let trimmed = raw.trim();
        let target = self.root.join(validate_relative_path(trimmed)?);
        if target.exists() || target.symlink_metadata().is_ok() {
            bail!("File already exists: {trimmed}. Use edit_file instead.");
        }
        let resolved = self.ensure_path_or_ancestor_within_workspace(&target, trimmed)?;
        self.ensure_not_excluded(&target)?;
        self.ensure_not_excluded(&resolved)?;
        Ok(resolved)
    }

    pub fn resolve_scan_roots(
        &self,
        target_path: &str,
        escape_label: &str,
    ) -> Result<(PathBuf, PathBuf)> {
        let base_root = if self.config.workspace_snapshot.git_root {
            self.root
                .ancestors()
                .find(|a| a.join(".git").exists())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.clone())
        } else {
            self.root.clone()
        };
        let base_root = dunce::canonicalize(&base_root).unwrap_or(base_root);
        let trimmed = target_path.trim();
        let neg_ancestors = self.build_neg_exclude_ancestor_globset()?;
        if !trimmed.is_empty() && trimmed != "." {
            let rel = validate_relative_path(trimmed)?;
            let target = base_root.join(rel);
            if !(target.is_dir() && neg_ancestors.is_match(rel)) {
                self.ensure_not_excluded_under(&target, &base_root)?;
            }
        }
        let scan_root = resolve_existing_under(&base_root, target_path, escape_label)?;
        let scan_rel = diff_paths(&scan_root, &base_root).unwrap_or_else(|| scan_root.clone());
        if !(scan_root.is_dir() && neg_ancestors.is_match(&scan_rel)) {
            self.ensure_not_excluded_under(&scan_root, &base_root)?;
        }
        Ok((base_root, scan_root))
    }

    pub fn build_walker(&self, scan_root: &Path) -> impl Iterator<Item = DirEntry> {
        let use_git_ignore = self.config.workspace_snapshot.git_root;
        let allow_hidden = self.config.app.allow_hidden;
        let has_neg_exclude = self
            .config
            .workspace_snapshot
            .exclude
            .iter()
            .any(|p| p.trim().starts_with('!'));
        let has_hidden_tree = self
            .config
            .workspace_snapshot
            .include_in_tree
            .iter()
            .any(|p| {
                let s = p
                    .trim()
                    .trim_start_matches('!')
                    .trim()
                    .trim_start_matches('/');
                s.starts_with('.') || s.contains("/.")
            });
        let prune_hidden = !allow_hidden && !has_neg_exclude && !has_hidden_tree;

        WalkBuilder::new(scan_root)
            .hidden(false)
            .git_ignore(use_git_ignore)
            .git_global(use_git_ignore)
            .git_exclude(use_git_ignore)
            .filter_entry(move |entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name();
                if name == ".git" {
                    return false;
                }
                if prune_hidden && name.as_encoded_bytes().first() == Some(&b'.') {
                    return false;
                }
                true
            })
            .build()
            .flatten()
    }

    pub fn build_exclude_globsets(
        &self,
        call_exclude: &[String],
        allow_recursive_shorthand: bool,
    ) -> Result<(GlobSet, GlobSet)> {
        let (neg_cfg, pos_cfg) = partition_negated_patterns(
            self.config
                .workspace_snapshot
                .exclude
                .iter()
                .map(String::as_str),
        );
        let (_, pos_call) = partition_negated_patterns(call_exclude.iter().map(String::as_str));
        let pos_patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
            .iter()
            .map(|&s| String::from(s))
            .chain(pos_cfg)
            .chain(pos_call)
            .collect();
        Ok((
            build_globset(&pos_patterns, allow_recursive_shorthand)?,
            build_globset(&neg_cfg, allow_recursive_shorthand)?,
        ))
    }

    pub fn build_neg_exclude_ancestor_globset(&self) -> Result<GlobSet> {
        let (neg_cfg, _) = partition_negated_patterns(
            self.config
                .workspace_snapshot
                .exclude
                .iter()
                .map(String::as_str),
        );
        build_ancestor_dir_globset(&neg_cfg)
    }

    pub fn walk_workspace_files(
        &self,
        base_root: &Path,
        scan_root: &Path,
        include_patterns: &[String],
        call_exclude: &[String],
    ) -> Result<Vec<(PathBuf, String)>> {
        let include_set = (!include_patterns.is_empty())
            .then(|| build_globset(include_patterns, true))
            .transpose()?;
        let (exclude_set, neg_exclude_set) = self.build_exclude_globsets(call_exclude, true)?;
        let neg_ancestors = self.build_neg_exclude_ancestor_globset()?;

        if scan_root != base_root {
            let rel = diff_paths(scan_root, base_root).unwrap_or_else(|| scan_root.to_path_buf());
            if !(scan_root.is_dir() && neg_ancestors.is_match(&rel)) {
                self.check_rel_path_allowed(&rel, &exclude_set, &neg_exclude_set)?;
            }
        }
        if scan_root.is_file() {
            let rel = diff_paths(scan_root, base_root).unwrap_or_else(|| scan_root.to_path_buf());
            return Ok(vec![(
                scan_root.to_path_buf(),
                rel.to_string_lossy().into_owned(),
            )]);
        }

        let mut files = Vec::new();
        for entry in self.build_walker(scan_root) {
            if files.len() >= self.config.workspace_snapshot.max_files {
                break;
            }
            let path = entry.path();
            if !path.is_file() || has_git_dir_component(path) {
                continue;
            }
            let Some(relative) = diff_paths(path, base_root) else {
                continue;
            };
            if self
                .check_rel_path_allowed(&relative, &exclude_set, &neg_exclude_set)
                .is_err()
                || include_set
                    .as_ref()
                    .is_some_and(|inc| !inc.is_match(&relative))
            {
                continue;
            }
            files.push((path.to_path_buf(), relative.to_string_lossy().into_owned()));
        }

        files.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(files)
    }

    pub async fn run_command<I, S>(&self, program: &str, args: I, empty_msg: &str) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_command_with_timeout(program, args, empty_msg, COMMAND_TIMEOUT_SECS)
            .await
    }

    pub async fn run_command_with_timeout<I, S>(
        &self,
        program: &str,
        args: I,
        empty_msg: &str,
        timeout_secs: u64,
    ) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(program);
        cmd.current_dir(&self.root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = ManagedChild::new(
            cmd.spawn()
                .with_context(|| format!("Error executing {program}"))?,
        );

        let cap = (self.max_output_size as u64).saturating_add(1);
        let stdout = child
            .child
            .stdout
            .take()
            .context("Failed to capture stdout")?;
        let stderr = child
            .child
            .stderr
            .take()
            .context("Failed to capture stderr")?;

        let (out_bytes, err_bytes, status) = timeout(Duration::from_secs(timeout_secs), async {
            let (r1, r2) = tokio::join!(read_capped(stdout, cap), read_capped(stderr, cap));
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((r1?, r2?, status))
        })
        .await
        .map_err(|_| anyhow::anyhow!("Command '{program}' timed out after {timeout_secs} seconds"))?
        .with_context(|| format!("Error reading output from {program}"))?;

        let mut combined = String::from_utf8_lossy(&out_bytes).into_owned();
        if !err_bytes.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            // Only label stderr on failure: a non-zero exit means the
            // stderr text is the actual error and must not be mistaken for
            // the command's output. Successful commands (e.g. git push
            // printing "Everything up-to-date") write informational
            // messages to stderr, so merge them without a label.
            if !status.success() {
                combined.push_str("[STDERR]\n");
            }
            combined.push_str(&String::from_utf8_lossy(&err_bytes));
        }
        if !status.success() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            match status.code() {
                Some(code) => combined.push_str(&format!("[Exit code: {code}]")),
                None => combined.push_str("[Process terminated by signal]"),
            }
        }

        Ok(if combined.trim().is_empty() {
            empty_msg.to_string()
        } else {
            combined
        })
    }
}

fn parse_args<T: DeserializeOwned>(json_args: &Value, tool_name: &str) -> Result<T> {
    serde_json::from_value(json_args.clone())
        .with_context(|| format!("Invalid arguments provided to {tool_name}"))
}

fn auto_schema<T: JsonSchema>(name: &str, description: &str) -> Value {
    let mut parameters = serde_json::to_value(schemars::schema_for!(T)).unwrap();
    if let Some(obj) = parameters.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    json!({
        "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters }
    })
}

async fn run_blocking<T: BlockingTool>(json_args: &Value, ctx: ToolContext) -> Result<String> {
    let args: T::Args = parse_args(json_args, T::NAME)?;
    tokio::task::spawn_blocking(move || T::execute(args, &ctx))
        .await
        .map_err(|e| anyhow::anyhow!("Tool task panicked: {e}"))?
}

async fn run_async<T: AsyncTool>(json_args: &Value, ctx: &ToolContext) -> Result<String> {
    let args: T::Args = parse_args(json_args, T::NAME)?;
    T::execute(args, ctx).await
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: ToolFunction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: Value,
}

pub fn call_signature(call: &ToolCall) -> String {
    let name = call.function.name.as_str();
    let args = &call.function.arguments;

    let sub = match name {
        "git" | "cargo" | "fs" | "repo_map" | "dotfiles" | "system" | "skill" | "memory" => {
            let raw = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if name == "skill" && matches!(raw, "edit" | "update") {
                "modify".to_string()
            } else if name == "memory" && raw == "get" {
                "read".to_string()
            } else if name == "memory" && matches!(raw, "delete" | "rm") {
                "remove".to_string()
            } else if name == "dotfiles" && matches!(raw, "revert" | "checkout") {
                "restore".to_string()
            } else {
                raw.to_string()
            }
        }
        _ => String::new(),
    };

    if sub.is_empty() {
        name.to_string()
    } else {
        format!("{name}:{sub}")
    }
}

pub fn format_call_summary(call: &ToolCall) -> String {
    let sig = call_signature(call);
    let Some(obj) = call
        .function
        .arguments
        .as_object()
        .filter(|o| !o.is_empty())
    else {
        return sig;
    };
    let has_sub = sig.contains(':');
    let kvs: Vec<String> = obj
        .iter()
        .filter(|(k, _)| !(has_sub && k.as_str() == "action"))
        .map(|(k, v)| match v.as_str() {
            Some(s) => format!("{k}={s}"),
            None => format!("{k}={v}"),
        })
        .collect();
    if kvs.is_empty() {
        sig
    } else {
        format!("{sig} ({})", kvs.join(", "))
    }
}

pub fn parse_hash_tool_prefix(raw_prompt: &str) -> Option<(bool, &str)> {
    let trimmed = raw_prompt.trim();
    if trimmed.starts_with("###") {
        return None;
    }
    let (include_in_context, rest) = if let Some(r) = trimmed.strip_prefix("##") {
        (true, r)
    } else {
        (false, trimmed.strip_prefix('#')?)
    };
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some((include_in_context, rest))
}

pub fn tool_subactions(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "cargo" => &[
            "check",
            "clippy",
            "fmt_check",
            "fmt",
            "doc",
            "metadata",
            "info",
            "tree",
        ],
        "git" => &[
            "status",
            "diff",
            "log",
            "show",
            "blame",
            "branch",
            "add",
            "unstage",
            "commit",
            "stash",
            "stash_pop",
            "switch",
            "merge",
            "branch_delete",
        ],
        "fs" => &["list", "rename", "copy", "remove"],
        "repo_map" => &["map", "definition", "references"],
        "memory" => &[
            "list", "read", "add", "remove", "set", "clear", "enable", "disable",
        ],
        "skill" => &["list", "read", "create", "modify"],
        "dotfiles" => &[
            "list", "read", "search", "edit", "create", "validate", "status", "diff", "restore",
            "sync",
        ],
        "system" => &[
            "overview",
            "journal",
            "services",
            "processes",
            "fish",
            "hyprland",
            "btrfs",
            "packages",
        ],
        _ => &[],
    }
}

pub fn tool_param_hint(tool_name: &str) -> &'static str {
    match tool_name {
        "workspace_snapshot" => "[tree_only=true|false]",
        "read_file" => "<file_path> [start_line=N] [end_line=N]",
        "search" => "<pattern> [path=...] [files_with_matches=true]",
        "repo_map" => "[map|definition|references] [symbol=...] [max_tokens=N]",
        "cargo" => "[check|clippy|fmt_check|fmt|doc|metadata|info|tree] [package=...]",
        "git" => "[status|diff|log|show|blame|branch|add|...] [target=...] [path=...]",
        "fs" => "[list|rename|copy|remove] [path=...] [destination=...]",
        "fetch_url" => "<url> [raw=true|false]",
        "memory" => {
            "[list|read|add|remove|clear|enable|disable] [scope=global|workspace] [item=...]"
        }
        "notify" => "<summary> [body=...] [urgency=low|normal|critical] [timeout_ms=N]",
        "skill" => "[list|read|create|modify] [name=...]",
        "dotfiles" => "[list|read|search|edit|create|validate|status|diff|restore|sync] [path=...]",
        "system" => {
            "[overview|journal|services|processes|fish|hyprland|btrfs|packages] [query=...]"
        }
        "create_file" => "file_path=... content=...",
        "edit_file" => "{\"file_path\": \"...\", \"edits\": [...]}",
        _ => "[key=value | JSON]",
    }
}

pub fn matching_tool_triggers(prefix_mark: &str, query: &str) -> Vec<String> {
    let lower = query.to_ascii_lowercase();
    if let Some((tool_part, sub_part)) = lower.split_once(':') {
        let Some(&canonical) = ALL_TOOL_NAMES.iter().find(|&&t| t == tool_part) else {
            return Vec::new();
        };
        return tool_subactions(canonical)
            .iter()
            .filter(|&&act| act.starts_with(sub_part))
            .map(|&act| format!("{prefix_mark}{canonical}:{act}"))
            .collect();
    }

    ALL_TOOL_NAMES
        .iter()
        .filter(|&&name| name.starts_with(&lower))
        .map(|&name| format!("{prefix_mark}{name}"))
        .collect()
}

fn parse_manual_arg_value(raw: &str) -> Value {
    match raw {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => {
            if let Ok(n) = raw.parse::<i64>() {
                Value::Number(n.into())
            } else if let Ok(n) = raw.parse::<u64>() {
                Value::Number(n.into())
            } else if let Ok(f) = raw.parse::<f64>()
                && let Some(num) = serde_json::Number::from_f64(f)
            {
                Value::Number(num)
            } else if (raw.starts_with('[') && raw.ends_with(']'))
                || (raw.starts_with('{') && raw.ends_with('}'))
            {
                serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
            } else {
                Value::String(raw.to_string())
            }
        }
    }
}

fn apply_positional_manual_arg(tool_name: &str, token: &str, map: &mut Map<String, Value>) {
    if tool_name == "read_file" && map.contains_key("file_path") {
        if let Ok(n) = token.parse::<usize>()
            && let Some(&k) = ["start_line", "end_line"]
                .iter()
                .find(|&&k| !map.contains_key(k))
        {
            map.insert(k.into(), Value::Number(n.into()));
        }
        return;
    }
    if tool_name == "workspace_snapshot" {
        if matches!(token, "tree" | "tree_only") && !map.contains_key("tree_only") {
            map.insert("tree_only".into(), Value::Bool(true));
        }
        return;
    }

    let keys: &[&str] = match tool_name {
        "repo_map" if matches!(token, "map" | "definition" | "references") => {
            &["action", "symbol", "target_path"]
        }
        "repo_map" => &["symbol", "target_path"],
        "cargo" => &["action", "package"],
        "git" if !map.contains_key("action") => &["action"],
        "git" => match map.get("action").and_then(Value::as_str).unwrap_or("") {
            "blame" | "add" | "unstage" => &["path"],
            "commit" => &["message", "target", "path"],
            _ => &["target", "path"],
        },
        "fs" => &["action", "path", "destination"],
        "dotfiles" if map.get("action").and_then(Value::as_str) == Some("sync") => &["content"],
        "dotfiles" => &["action", "path", "pattern"],
        "skill" => &["action", "name", "file"],
        "memory" if !map.contains_key("action") => &["action"],
        "memory" if matches!(token, "global" | "workspace") => &["scope", "item"],
        "memory" => &["item"],
        "notify" if matches!(token, "low" | "normal" | "critical") => {
            &["urgency", "summary", "body"]
        }
        "notify" => &["summary", "body"],
        "system" => &["action", "query", "target"],
        "read_file" => &["file_path"],
        "search" => &["pattern", "path"],
        "fetch_url" => &["url"],
        "create_file" => &["file_path", "content"],
        _ => &[],
    };

    if let Some(&k) = keys.iter().find(|&&k| !map.contains_key(k)) {
        map.insert(k.into(), Value::String(token.to_string()));
    }
}

pub fn parse_manual_tool_call(raw_after_hash: &str) -> Result<(ToolCall, String)> {
    let trimmed = raw_after_hash.trim();
    if trimmed.is_empty() {
        bail!("Manual tool invocation cannot be empty");
    }

    let (tool_token, args_str) = match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim()),
        None => (trimmed, ""),
    };

    let (raw_name, colon_action) = match tool_token.split_once(':') {
        Some((n, a)) => (n.trim(), Some(a.trim()).filter(|s| !s.is_empty())),
        None => (tool_token.trim(), None),
    };

    let Some(&tool_name) = ALL_TOOL_NAMES
        .iter()
        .find(|&&name| name.eq_ignore_ascii_case(raw_name))
    else {
        bail!(
            "Unknown tool '{}'. Available tools: {}",
            raw_name,
            ALL_TOOL_NAMES.join(", ")
        );
    };

    let mut map = if args_str.starts_with('{') {
        serde_json::from_str::<Map<String, Value>>(args_str)
            .with_context(|| format!("Invalid JSON arguments for #{tool_name}"))?
    } else {
        let mut m = Map::new();
        if let Some(act) = colon_action {
            m.insert("action".into(), Value::String(act.to_string()));
        }
        if !args_str.is_empty() {
            let tokens = shlex::split(args_str)
                .ok_or_else(|| anyhow::anyhow!("Unmatched quotes in arguments for #{tool_name}"))?;
            for token in tokens {
                if let Some((k, v)) = token.split_once('=')
                    && !k.is_empty()
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    m.insert(k.to_string(), parse_manual_arg_value(v));
                } else {
                    apply_positional_manual_arg(tool_name, &token, &mut m);
                }
            }
        }
        m
    };

    if let Some(act) = colon_action {
        map.entry("action".to_string())
            .or_insert_with(|| Value::String(act.to_string()));
    }

    if !map.contains_key("action") {
        let default_action = match tool_name {
            "git" => Some("status"),
            "cargo" => Some("check"),
            "fs" | "memory" | "skill" | "dotfiles" => Some("list"),
            "system" => Some("overview"),
            _ => None,
        };
        if let Some(act) = default_action {
            map.insert("action".into(), Value::String(act.into()));
        }
    }

    let call = ToolCall {
        function: ToolFunction {
            name: tool_name.to_string(),
            arguments: Value::Object(map),
        },
        thought_signature: None,
    };
    let summary = format_call_summary(&call);
    Ok((call, summary))
}

pub fn is_auto_approved<'a, I>(call: &ToolCall, rules: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    let name = call.function.name.as_str();
    let sig = call_signature(call);

    rules
        .into_iter()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .any(|r| {
            r == "*"
                || r == name
                || r == sig
                || r.strip_suffix(":*")
                    .is_some_and(|p| name == p || sig.starts_with(&format!("{p}:")))
        })
}

fn format_kv(k: &str, v: &Value) -> String {
    match v.as_str() {
        Some(s) => format!("  {k}: {s}\n"),
        None => format!("  {k}: {v}\n"),
    }
}

fn append_filtered_kvs(out: &mut String, obj: &Map<String, Value>, skip: &[&str]) {
    for (k, v) in obj.iter().filter(|(k, _)| !skip.contains(&k.as_str())) {
        out.push_str(&format_kv(k, v));
    }
}

fn append_warning(out: &mut String, err: &anyhow::Error) {
    let _ = write!(out, "\n--- WARNING ---\n{err}! Execution will fail.\n");
}

fn append_unified_diff(out: &mut String, file_path: &str, old: &str, new: &str) {
    let diff = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{file_path}"), &format!("b/{file_path}"))
        .to_string();

    out.push_str("\n--- UNIFIED DIFF ---\n");
    out.push_str(if diff.trim().is_empty() {
        "(No differences)\n"
    } else {
        &diff
    });
}

fn append_diff_result(out: &mut String, res: Result<(String, String, String, Option<usize>)>) {
    match res {
        Ok((display, original, updated, hunk_count)) => {
            if let Some(n) = hunk_count.filter(|&n| n > 0) {
                let _ = writeln!(out, "  hunks: {n}");
            }
            append_unified_diff(out, &display, &original, &updated);
        }
        Err(e) => append_warning(out, &e),
    }
}

fn format_edit_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
    active_skill: Option<&str>,
) {
    let file_path = obj.get("file_path").and_then(Value::as_str).unwrap_or("");
    let _ = writeln!(out, "  file_path: {file_path}");
    append_filtered_kvs(out, obj, &["file_path", "edits"]);

    let preview =
        parse_args::<edit_file::EditFileArgs>(args, edit_file::Tool::NAME).and_then(|parsed| {
            edit_file::preview_edits(&parsed, &ToolContext::new(config, active_skill, 0)?)
        });

    match preview {
        Ok((_, original, updated, hunk_count)) => {
            let _ = writeln!(out, "  hunks: {hunk_count}");
            append_unified_diff(out, file_path, &original, &updated);
        }
        Err(e) => {
            append_warning(out, &e);
            let (raw_search, raw_replace) =
                parse_args::<edit_file::EditFileArgs>(args, edit_file::Tool::NAME)
                    .map(|parsed| edit_file::fallback_hunk_preview(&parsed))
                    .unwrap_or_default();
            append_unified_diff(out, file_path, &raw_search, &raw_replace);
        }
    }
}

fn format_fs_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
    active_skill: Option<&str>,
) {
    append_filtered_kvs(out, obj, &[]);
    let preview = parse_args::<fs::FsArgs>(args, fs::Tool::NAME).and_then(|parsed| {
        fs::preview_action(&parsed, &ToolContext::new(config, active_skill, 0)?)
    });
    match preview {
        Ok((summary, maybe_diff)) => {
            let _ = write!(out, "\n--- OPERATION ---\n{summary}\n");
            if let Some((file_path, deleted_content)) = maybe_diff {
                append_unified_diff(out, &file_path, &deleted_content, "");
            }
        }
        Err(e) => append_warning(out, &e),
    }
}

fn format_dotfiles_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
) {
    append_filtered_kvs(out, obj, &["edits", "content"]);

    if let Ok(parsed) = parse_args::<dotfiles::DotfilesArgs>(args, dotfiles::Tool::NAME) {
        match parsed.action {
            dotfiles::DotfilesAction::Edit => append_diff_result(
                out,
                dotfiles::preview_edit(&parsed, config)
                    .map(|(_, d, o, u, n)| (d, o, u, Some(n.max(1)))),
            ),
            dotfiles::DotfilesAction::Create => append_diff_result(
                out,
                dotfiles::preview_create(&parsed, config)
                    .map(|(_, d)| (d, String::new(), parsed.content, None)),
            ),
            dotfiles::DotfilesAction::Restore => match dotfiles::preview_restore(&parsed, config) {
                Ok((file_count, diff)) => {
                    let _ = writeln!(out, "  files: {file_count}");
                    out.push_str("\n--- UNIFIED DIFF ---\n");
                    out.push_str(if diff.trim().is_empty() {
                        "(No differences)\n"
                    } else {
                        &diff
                    });
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                Err(e) => append_warning(out, &e),
            },
            _ => {}
        }
    }
}

fn format_skill_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
    active_skill: Option<&str>,
) {
    append_filtered_kvs(out, obj, &["edits", "content", "prompt"]);

    if let Ok(parsed) = parse_args::<skill::SkillArgs>(args, skill::Tool::NAME)
        && let Ok(ctx) = ToolContext::new(config, active_skill, 0)
    {
        match parsed.action {
            skill::SkillAction::Modify => append_diff_result(
                out,
                skill::preview_modify(&parsed, &ctx).map(|(_, d, o, u, n)| (d, o, u, Some(n))),
            ),
            skill::SkillAction::Create => append_diff_result(
                out,
                skill::preview_create(&parsed, &ctx).map(|(_, d, r)| (d, String::new(), r, None)),
            ),
            _ => {}
        }
    }
}

fn format_memory_approval(
    out: &mut String,
    args: &Value,
    obj: &Map<String, Value>,
    config: &Config,
    active_skill: Option<&str>,
) {
    append_filtered_kvs(out, obj, &[]);

    if let Ok(parsed) = parse_args::<memory::MemoryArgs>(args, memory::Tool::NAME)
        && let Ok(ctx) = ToolContext::new(config, active_skill, 0)
        && !matches!(
            parsed.action,
            memory::MemoryAction::List | memory::MemoryAction::Read
        )
    {
        append_diff_result(
            out,
            memory::preview_mutation(&parsed, &ctx).map(|(_, d, o, u, _)| (d, o, u, None)),
        );
    }
}

pub fn format_approval(call: &ToolCall, config: &Config, active_skill: Option<&str>) -> String {
    let name = &call.function.name;
    let args = &call.function.arguments;
    let sig = call_signature(call);

    let Some(obj) = args.as_object() else {
        return format!(
            "Tool: {name} [{sig}]\n{}",
            serde_json::to_string_pretty(args).unwrap_or_default()
        );
    };

    let mut out = format!("Tool: {name} (Rule: {sig})\n");
    match name.as_str() {
        "edit_file" => format_edit_approval(&mut out, args, obj, config, active_skill),
        "fs" => format_fs_approval(&mut out, args, obj, config, active_skill),
        "dotfiles" => format_dotfiles_approval(&mut out, args, obj, config),
        "skill" => format_skill_approval(&mut out, args, obj, config, active_skill),
        "memory" => format_memory_approval(&mut out, args, obj, config, active_skill),
        _ => {
            let mut multiline_blocks = Vec::new();
            for (k, v) in obj {
                match v.as_str() {
                    Some(s) if s.contains('\n') || matches!(k.as_str(), "content" | "message") => {
                        multiline_blocks.push((k, s));
                    }
                    _ => out.push_str(&format_kv(k, v)),
                }
            }
            multiline_blocks.sort_by(|a, b| b.0.cmp(a.0));
            for (k, s) in multiline_blocks {
                out.push_str(&format!("\n--- {} ---\n{s}\n", k.to_uppercase()));
            }
        }
    }

    out.trim_end().to_string()
}

pub fn is_tool_enabled(name: &str, config: &Config) -> bool {
    config
        .app
        .enabled_tools
        .iter()
        .map(|s| s.trim())
        .any(|t| t == "*" || t == name)
}

pub fn is_tool_enabled_for_turn(name: &str, config: &Config, active_skill: Option<&str>) -> bool {
    if !is_tool_enabled(name, config) {
        return false;
    }
    let Some(skill) = active_skill.and_then(|s| crate::core::skills::find_skill(config, s)) else {
        return true;
    };
    if skill.tools.is_empty() {
        return true;
    }
    skill
        .tools
        .iter()
        .map(|t| t.trim())
        .any(|t| t == "*" || t == name)
}

macro_rules! register_tools {
    ($( $kind:ident : $mod:ident ),* $(,)?) => {
        pub const ALL_TOOL_NAMES: &[&str] = &[ $( $mod::Tool::NAME ),* ];

        pub fn schemas_for_turn(config: &Config, active_skill: Option<&str>) -> Vec<Value> {
            [ $( ( $mod::Tool::NAME, register_tools!(@schema $kind, $mod), ) ),* ]
                .into_iter()
                .filter_map(|(name, schema)| {
                    is_tool_enabled_for_turn(name, config, active_skill).then_some(schema)
                })
                .collect()
        }

        async fn dispatch_tool(
            name: &str,
            args: &Value,
            config: &Config,
            active_skill: Option<&str>,
            max_output_size: usize,
            filter: ToolFilter,
        ) -> Result<String> {
            let allowed = match filter {
                ToolFilter::Turn => is_tool_enabled_for_turn(name, config, active_skill),
                ToolFilter::Enabled => is_tool_enabled(name, config),
                ToolFilter::Manual => ALL_TOOL_NAMES.contains(&name),
            };
            if !allowed {
                bail!("Unknown tool: '{name}'");
            }
            let ctx = ToolContext::new(config, active_skill, max_output_size)?;
            match name {
                $( $mod::Tool::NAME => register_tools!(@run $kind, $mod::Tool, args, ctx), )*
                _ => bail!("Unknown tool: '{name}'"),
            }
        }
    };
    (@schema blocking, $mod:ident) => {
        auto_schema::<<$mod::Tool as BlockingTool>::Args>($mod::Tool::NAME, $mod::Tool::DESCRIPTION)
    };
    (@schema async, $mod:ident) => {
        auto_schema::<<$mod::Tool as AsyncTool>::Args>($mod::Tool::NAME, $mod::Tool::DESCRIPTION)
    };
    (@run blocking, $tool:ty, $args:ident, $ctx:ident) => {
        run_blocking::<$tool>($args, $ctx).await
    };
    (@run async, $tool:ty, $args:ident, $ctx:ident) => {
        run_async::<$tool>($args, &$ctx).await
    };
}

register_tools! {
    async: cargo,
    blocking: create_file,
    blocking: dotfiles,
    blocking: edit_file,
    async: fetch_url,
    blocking: fs,
    async: git,
    blocking: memory,
    async: notify,
    blocking: read_file,
    blocking: repo_map,
    blocking: search,
    blocking: skill,
    async: system,
    blocking: workspace_snapshot,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionOutput {
    pub truncated_output: String,
    pub full_output: String,
    pub raw_bytes: usize,
    pub truncated: bool,
}

impl ToolExecutionOutput {
    fn error(msg: String) -> Self {
        let raw_bytes = msg.len();
        Self {
            truncated_output: msg.clone(),
            full_output: msg,
            raw_bytes,
            truncated: false,
        }
    }
}

fn truncate_at_limit(mut out: String, limit: usize) -> (String, bool) {
    let truncated = out.len() > limit;
    if truncated {
        let truncate_at = out.floor_char_boundary(limit);
        let dropped = out.len() - truncate_at;
        out.truncate(truncate_at);
        let _ = write!(
            out,
            "\n\n[TRUNCATED: Output exceeded the {limit} byte limit — {dropped} bytes were dropped. Content is incomplete.]"
        );
    }
    (out, truncated)
}

fn package_tool_output(raw: String, max_size: usize, full_cap: usize) -> ToolExecutionOutput {
    let raw_bytes = raw.len();
    let (full_output, _) = truncate_at_limit(raw.clone(), full_cap);
    let (truncated_output, truncated) = truncate_at_limit(raw, max_size);
    ToolExecutionOutput {
        truncated_output,
        full_output,
        raw_bytes,
        truncated,
    }
}

pub async fn run_manual_command(
    program: &str,
    args: &[String],
    config: &Config,
    max_size: usize,
    full_cap: usize,
) -> ToolExecutionOutput {
    let capture_limit = full_cap.max(max_size);
    let ctx = match ToolContext::new(config, None, capture_limit) {
        Ok(c) => c,
        Err(e) => {
            return ToolExecutionOutput::error(format!("Error resolving workspace root: {e}"));
        }
    };

    match ctx
        .run_command(program, args, "(Command completed with no output)")
        .await
    {
        Ok(out) => package_tool_output(out, max_size, full_cap),
        Err(e) => ToolExecutionOutput::error(format!("Error: {e}")),
    }
}

#[derive(Clone, Copy)]
enum ToolFilter {
    Turn,
    Enabled,
    Manual,
}

async fn execute_with_filter(
    call: &ToolCall,
    config: &Config,
    active_skill: Option<&str>,
    max_size: usize,
    full_cap: usize,
    filter: ToolFilter,
) -> ToolExecutionOutput {
    let name = &call.function.name;
    let args = &call.function.arguments;
    let capture_limit = full_cap.max(max_size);
    info!("Executing tool: {name}");
    debug!("Tool arguments: {args}");

    match dispatch_tool(name, args, config, active_skill, capture_limit, filter).await {
        Ok(out) => {
            let packaged = package_tool_output(out, max_size, full_cap);
            info!(
                "Tool '{name}' executed successfully. Raw bytes: {}, Truncated: {}",
                packaged.raw_bytes, packaged.truncated
            );
            debug!(
                "Tool output snippet: {}",
                packaged
                    .truncated_output
                    .replace('\n', " ")
                    .chars()
                    .take(200)
                    .collect::<String>()
            );
            packaged
        }
        Err(e) => {
            error!("Tool '{name}' failed: {e}");
            ToolExecutionOutput::error(format!("Error: {e}"))
        }
    }
}

pub async fn execute(
    call: &ToolCall,
    config: &Config,
    active_skill: Option<&str>,
    max_size: usize,
    full_cap: usize,
) -> ToolExecutionOutput {
    execute_with_filter(
        call,
        config,
        active_skill,
        max_size,
        full_cap,
        ToolFilter::Turn,
    )
    .await
}

pub async fn execute_hook(
    call: &ToolCall,
    config: &Config,
    active_skill: Option<&str>,
    max_size: usize,
    full_cap: usize,
) -> ToolExecutionOutput {
    execute_with_filter(
        call,
        config,
        active_skill,
        max_size,
        full_cap,
        ToolFilter::Enabled,
    )
    .await
}

pub async fn execute_manual(
    call: &ToolCall,
    config: &Config,
    active_skill: Option<&str>,
    max_size: usize,
    full_cap: usize,
) -> ToolExecutionOutput {
    execute_with_filter(
        call,
        config,
        active_skill,
        max_size,
        full_cap,
        ToolFilter::Manual,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
