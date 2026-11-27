mod approval;
pub(crate) mod browser;
mod cargo;
mod create_file;
mod curl;
mod dotfiles;
mod edit_file;
mod fetch_url;
pub mod fs;
mod git;
mod github_repo;
mod manual;
mod memory;
mod notify;
mod process;
pub(crate) mod progress;
mod read_file;
mod repo_map;
mod schedule;
mod search;
mod session;
mod skill;
mod sqlite;
mod subagent;
mod system;
pub mod task;
pub mod todo;
pub(crate) mod utils;
pub mod workspace_snapshot;

pub use approval::format_approval;
pub use manual::parse_manual_tool_call;

use crate::core::config::{Config, expand_tilde};
use anyhow::{Context, Result, bail};
use globset::GlobSet;
use ignore::{DirEntry, WalkBuilder};
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

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
        build_ancestor_dir_globset, build_globset, ensure_ancestor_within_root, ensure_within_root,
        has_git_dir_component, has_hidden_component, partition_negated_patterns,
        resolve_existing_under, validate_relative_path, workspace_root,
    },
};

#[cfg(unix)]
use crate::core::processes::kill_process_group;

const COMMAND_TIMEOUT_SECS: u64 = 30;

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

pub(super) async fn run_captured_command(
    mut cmd: Command,
    program: &str,
    cap: u64,
    timeout_secs: u64,
) -> Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus)> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = ManagedChild::new(
        cmd.spawn()
            .with_context(|| format!("Error executing {program}"))?,
    );
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

    timeout(Duration::from_secs(timeout_secs), async {
        let (r1, r2) = tokio::join!(read_capped(stdout, cap), read_capped(stderr, cap));
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((r1?, r2?, status))
    })
    .await
    .map_err(|_| anyhow::anyhow!("Command '{program}' timed out after {timeout_secs} seconds"))?
    .with_context(|| format!("Error reading output from {program}"))
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

pub trait BlockingTool {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String>;
}

pub trait AsyncTool {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

    fn execute(
        args: Self::Args,
        ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
}

#[derive(Clone)]
pub struct ToolContext {
    pub root: PathBuf,
    pub max_output_size: usize,
    pub active_skill: Option<String>,
    pub session_id: Option<String>,
    pub config: Config,
}

impl ToolContext {
    fn new(
        config: &Config,
        active_skill: Option<&str>,
        session_id: Option<&str>,
        max_output_size: usize,
    ) -> Result<Self> {
        Ok(Self {
            root: workspace_root()?,
            max_output_size,
            active_skill: active_skill.map(String::from),
            session_id: session_id.map(String::from),
            config: config.clone(),
        })
    }

    pub fn ensure_within_workspace(&self, path: &Path, label: &str) -> Result<PathBuf> {
        ensure_within_root(path, &self.root, label)
    }

    /// Canonicalized, existing directories the user has explicitly allowed
    /// `read_file`/`search` to read, in addition to the workspace root.
    pub fn extra_read_roots(&self) -> Vec<PathBuf> {
        self.config
            .app
            .read_roots
            .iter()
            .map(|s| expand_tilde(s))
            .filter_map(|p| dunce::canonicalize(p).ok())
            .collect()
    }

    /// `app.read_roots` entries that currently fail to resolve to an existing
    /// path (a typo, or a lazy directory like `~/.cargo/registry/src` before
    /// the first dependency fetch), as a comma-joined list. Surfaced in
    /// rejection messages so a misconfigured entry is not silently ignored.
    fn read_roots_hint(&self) -> Option<String> {
        let unresolved: Vec<&str> = self
            .config
            .app
            .read_roots
            .iter()
            .filter(|raw| dunce::canonicalize(expand_tilde(raw)).is_err())
            .map(String::as_str)
            .collect();
        (!unresolved.is_empty()).then(|| unresolved.join(", "))
    }

    pub fn ensure_path_or_ancestor_within_workspace(
        &self,
        target: &Path,
        label: &str,
    ) -> Result<PathBuf> {
        ensure_ancestor_within_root(target, &self.root, label)
    }

    pub fn validate_scoped_relative_path<'a>(&self, raw: &'a str) -> Result<&'a Path> {
        let rel = validate_relative_path(raw)?;
        let target = self.root.join(rel);
        let resolved = self.ensure_path_or_ancestor_within_workspace(&target, raw.trim())?;
        self.ensure_not_excluded(&target)?;
        self.ensure_not_excluded(&resolved)?;
        Ok(rel)
    }

    fn resolve_under_base(
        &self,
        base_root: &Path,
        raw: &str,
        escape_label: &str,
    ) -> Result<PathBuf> {
        let trimmed = raw.trim();
        let neg_ancestors = self.build_neg_exclude_ancestor_globset()?;
        if !trimmed.is_empty() && trimmed != "." {
            let rel = validate_relative_path(trimmed)?;
            let target = base_root.join(rel);
            if !(target.is_dir() && neg_ancestors.is_match(rel)) {
                self.ensure_not_excluded_under(&target, base_root)?;
            }
        }
        let canon = resolve_existing_under(base_root, raw, escape_label)?;
        let canon_rel = diff_paths(&canon, base_root).unwrap_or_else(|| canon.clone());
        if !(canon.is_dir() && neg_ancestors.is_match(&canon_rel)) {
            self.ensure_not_excluded_under(&canon, base_root)?;
        }
        Ok(canon)
    }

    pub fn resolve_existing_under_root(&self, raw: &str, escape_label: &str) -> Result<PathBuf> {
        self.resolve_under_base(&self.root, raw, escape_label)
    }

    pub fn check_rel_path_allowed(
        &self,
        rel: &Path,
        exclude_set: &GlobSet,
        neg_exclude_set: &GlobSet,
    ) -> Result<()> {
        self.check_rel_path_allowed_impl(rel, exclude_set, neg_exclude_set, false)
    }

    /// Variant of [`Self::check_rel_path_allowed`] for paths under explicitly
    /// configured `read_roots`.  Skips the hidden-file check because
    /// `read_roots` is an explicit permission grant — dot-named files and
    /// directories under the root are readable without `app.allow_hidden`.
    /// The `.git` and exclude-pattern checks still apply.
    pub fn check_rel_path_allowed_read_root(
        &self,
        rel: &Path,
        exclude_set: &GlobSet,
        neg_exclude_set: &GlobSet,
    ) -> Result<()> {
        self.check_rel_path_allowed_impl(rel, exclude_set, neg_exclude_set, true)
    }

    fn check_rel_path_allowed_impl(
        &self,
        rel: &Path,
        exclude_set: &GlobSet,
        neg_exclude_set: &GlobSet,
        skip_hidden: bool,
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
        if neg_exclude_set.is_match(rel) {
            return Ok(());
        }
        if !skip_hidden && !self.config.app.allow_hidden && has_hidden_component(rel) {
            bail!(
                "Security violation: '{}' is a hidden file or directory (set `app.allow_hidden = true` in config.toml to allow access)",
                rel.display()
            );
        }
        if rel
            .ancestors()
            .any(|p| !p.as_os_str().is_empty() && exclude_set.is_match(p))
        {
            bail!(
                "Security violation: '{}' matches an excluded pattern",
                rel.display()
            );
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
        let is_absolute = expanded.is_absolute();
        let candidate = if is_absolute {
            expanded
        } else {
            self.root.join(validate_relative_path(trimmed)?)
        };

        if !candidate.exists() {
            bail!("Target path does not exist: '{trimmed}'");
        }
        let canon = dunce::canonicalize(&candidate)?;

        if let Some(root) = self
            .extra_read_roots()
            .iter()
            .find(|r| canon.starts_with(r))
        {
            // Enforce `.git` and default-exclude checks on the path relative
            // to the read root.  `read_roots` is an explicit permission grant,
            // so hidden files/dirs under the root are readable without
            // `app.allow_hidden = true`.
            let rel = diff_paths(&canon, root).unwrap_or_else(|| canon.clone());
            let (exclude_set, neg_exclude_set) = self.build_exclude_globsets(&[], true)?;
            self.check_rel_path_allowed_read_root(&rel, &exclude_set, &neg_exclude_set)?;
            return Ok(canon);
        }

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
                .is_some_and(|s| {
                    s.all_dirs().filter(|d| d.is_dir()).any(|dir| {
                        let abs_dir = if dir.is_absolute() {
                            dir.to_path_buf()
                        } else {
                            self.root.join(dir)
                        };
                        let canon_dir = dunce::canonicalize(&abs_dir).unwrap_or(abs_dir);
                        canon.starts_with(&canon_dir)
                    })
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
        let canon = match self.ensure_within_workspace(&canon, trimmed) {
            Ok(canon) => canon,
            Err(err) => {
                // An absolute path was rejected while a configured read root
                // failed to resolve: name the misconfigured entry instead of
                // letting it fail silently.
                if is_absolute && let Some(hint) = self.read_roots_hint() {
                    bail!("{err} (unresolved `app.read_roots` entries: {hint})");
                }
                return Err(err);
            }
        };
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
        // Allow searching an explicitly-configured extra root (e.g.
        // ~/.cargo/registry/src) by passing its absolute path as `path`.
        let expanded = expand_tilde(target_path.trim());
        if expanded.is_absolute()
            && let Ok(canon) = dunce::canonicalize(&expanded)
            && let Some(root) = self
                .extra_read_roots()
                .iter()
                .find(|r| canon.starts_with(r))
        {
            // Anchor the display `base_root` at the configured read root rather
            // than at `canon` itself. When `canon` is a single file, returning
            // `(canon, canon)` would make `diff_paths(scan_root, base_root)`
            // empty and `walk_filtered_files` would emit a blank filename; keying
            // off the read root keeps the relative path (and the workspace-style
            // exclude/hidden checks) well-defined for both file and dir targets.
            return Ok((root.clone(), canon));
        }

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
        let scan_root = match self.resolve_under_base(&base_root, target_path, escape_label) {
            Ok(scan_root) => scan_root,
            Err(err) => {
                // See the matching note in `resolve_readable_path`: name the
                // misconfigured entry instead of failing opaquely.
                if expanded.is_absolute()
                    && let Some(hint) = self.read_roots_hint()
                {
                    bail!("{err} (unresolved `app.read_roots` entries: {hint})");
                }
                return Err(err);
            }
        };
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

    pub fn walk_filtered_files<I, F>(
        &self,
        base_root: &Path,
        scan_root: &Path,
        include_patterns: &[String],
        call_exclude: &[String],
        walker: I,
        mut format_rel: F,
    ) -> Result<Vec<(PathBuf, String)>>
    where
        I: IntoIterator<Item = DirEntry>,
        F: FnMut(&Path) -> String,
    {
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
            return Ok(vec![(scan_root.to_path_buf(), format_rel(&rel))]);
        }

        let mut files = Vec::new();
        for entry in walker {
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
            files.push((path.to_path_buf(), format_rel(&relative)));
        }

        files.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(files)
    }

    pub fn walk_workspace_files(
        &self,
        base_root: &Path,
        scan_root: &Path,
        include_patterns: &[String],
        call_exclude: &[String],
    ) -> Result<Vec<(PathBuf, String)>> {
        self.walk_filtered_files(
            base_root,
            scan_root,
            include_patterns,
            call_exclude,
            self.build_walker(scan_root),
            |rel| rel.to_string_lossy().into_owned(),
        )
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
        cmd.current_dir(&self.root).args(args);
        let cap = (self.max_output_size as u64).saturating_add(1);
        let (out_bytes, err_bytes, status) =
            run_captured_command(cmd, program, cap, timeout_secs).await?;

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
    let sub = if !tool_subactions(name).is_empty() {
        let raw = call
            .function
            .arguments
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        match (name, raw) {
            ("skill", "edit" | "update") => "modify",
            ("memory", "get") => "read",
            ("memory", "delete" | "rm") => "remove",
            ("dotfiles", "revert" | "checkout") => "restore",
            (_, r) => r,
        }
    } else {
        ""
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
            "test",
            "fmt_check",
            "fmt",
            "doc",
            "metadata",
            "info",
            "tree",
            "build",
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
        "github_repo" => &["fetch", "list", "read", "search"],
        "memory" => &[
            "list", "read", "add", "remove", "set", "clear", "enable", "disable",
        ],
        "session" => &["list", "search", "read", "stats", "rename", "pin"],
        "skill" => &["list", "read", "create", "modify", "check"],
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
        "process" => &["list", "output", "poll", "kill", "clean"],
        "browser" => &[
            "goto",
            "read",
            "click",
            "type",
            "paste",
            "eval",
            "extract",
            "find",
            "screenshot",
            "snapshot",
            "status",
            "close",
        ],
        "schedule" => &["list", "logs", "run", "sync", "clean"],
        "sqlite" => &["tables", "schema", "query"],
        "task" => &["list", "add", "update", "remove", "clear"],
        "todo" => &["list", "add", "update", "remove", "clear"],
        _ => &[],
    }
}

pub fn tool_param_hint(tool_name: &str) -> &'static str {
    match tool_name {
        "workspace_snapshot" => "[tree_only=true|false]",
        "read_file" => "<file_path> [start_line=N] [end_line=N]",
        "search" => "<pattern> [path=...] [files_with_matches=true]",
        "repo_map" => "[map|definition|references] [symbol=...] [max_tokens=N]",
        "github_repo" => {
            "[fetch|list|read|search] repo=<owner/repo> [path=...] [pattern=...] [git_ref=...]"
        }
        "cargo" => {
            "[check|clippy|test|fmt_check|fmt|doc|metadata|info|tree] [package=...] [bin=...]"
        }
        "git" => "[status|diff|log|show|blame|branch|add|...] [target=...] [path=...]",
        "fs" => "[list|rename|copy|remove] [path=...] [destination=...]",
        "fetch_url" => "<url> [raw=true|false]",
        "curl" => {
            "<url> [method=post] [headers=...] [body=... | json=...] [head=true] [show_headers=true]"
        }
        "memory" => {
            "[list|read|add|remove|clear|enable|disable] [scope=global|workspace] [item=...]"
        }
        "notify" => "<summary> [body=...] [urgency=low|normal|critical] [timeout_ms=N]",
        "session" => {
            "[list|search|read|stats|rename|pin] [id=...] [query=...] [start_message=N] [end_message=N]"
        }
        "skill" => "[list|read|create|modify|check] [name=...]",
        "dotfiles" => "[list|read|search|edit|create|validate|status|diff|restore|sync] [path=...]",
        "system" => {
            "[overview|journal|services|processes|fish|hyprland|btrfs|packages] [query=...]"
        }
        "process" => "[list|output|poll|kill|clean] [id=...] [tail_lines=N | wait_ms=N]",
        "browser" => {
            "[goto|read|click|type|paste|eval|extract|find|screenshot|snapshot|status|close] [url=...] [selector=...] [text=...] [source=clipboard|workspace_snapshot|workspace_snapshot_tree] [script=...]"
        }
        "schedule" => "[list|logs|run|sync|clean] [name=...] [lines=N] [global=true|false]",
        "sqlite" => "[tables|schema|query] path=<db_path> [table=...] [sql=...] [limit=N]",
        "task" => {
            "[list|add|update|remove|clear] [id=N] [title=...] [status=pending|in_progress|blocked]"
        }
        "todo" => {
            "[list|add|update|remove|clear] [scope=workspace|global] [id=N] [title=...] [status=pending|in_progress|blocked] [priority=low|medium|high]"
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
    skill.allows_tool(name)
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

        pub fn tool_schema_by_name(name: &str) -> Option<Value> {
            match name {
                $( $mod::Tool::NAME => Some(register_tools!(@schema $kind, $mod)), )*
                _ => None,
            }
        }

        pub fn schemas_for_tool_names(tool_names: &[String]) -> Vec<Value> {
            tool_names
                .iter()
                .filter_map(|name| tool_schema_by_name(name))
                .collect()
        }

        pub fn deserialize_tool_args(name: &str, args: &Value) -> Result<()> {
            match name {
                $( $mod::Tool::NAME => {
                    let _: register_tools!(@args_ty $kind, $mod) = parse_args(args, $mod::Tool::NAME)?;
                    Ok(())
                } )*
                _ => bail!("Unknown tool: '{name}'"),
            }
        }

        async fn dispatch_tool(
            name: &str,
            args: &Value,
            config: &Config,
            active_skill: Option<&str>,
            session_id: Option<&str>,
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
            let ctx = ToolContext::new(config, active_skill, session_id, max_output_size)?;
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
    (@args_ty blocking, $mod:ident) => {
        <$mod::Tool as BlockingTool>::Args
    };
    (@args_ty async, $mod:ident) => {
        <$mod::Tool as AsyncTool>::Args
    };
    (@run blocking, $tool:ty, $args:ident, $ctx:ident) => {
        run_blocking::<$tool>($args, $ctx).await
    };
    (@run async, $tool:ty, $args:ident, $ctx:ident) => {
        run_async::<$tool>($args, &$ctx).await
    };
}

register_tools! {
    async: browser,
    async: cargo,
    blocking: create_file,
    blocking: dotfiles,
    blocking: edit_file,
    async: fetch_url,
    async: curl,
    blocking: fs,
    async: git,
    async: github_repo,
    blocking: memory,
    async: notify,
    async: process,
    blocking: read_file,
    blocking: repo_map,
    blocking: schedule,
    blocking: search,
    blocking: session,
    blocking: skill,
    blocking: sqlite,
    async: subagent,
    async: system,
    blocking: task,
    blocking: todo,
    blocking: workspace_snapshot,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionOutput {
    pub truncated_output: String,
    pub full_output: String,
    pub raw_bytes: usize,
    pub truncated: bool,
    pub is_error: bool,
}

impl ToolExecutionOutput {
    pub fn error(msg: String) -> Self {
        let raw_bytes = msg.len();
        Self {
            truncated_output: msg.clone(),
            full_output: msg,
            raw_bytes,
            truncated: false,
            is_error: true,
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
        is_error: false,
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
    let ctx = match ToolContext::new(config, None, None, capture_limit) {
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
    session_id: Option<&str>,
    max_size: usize,
    full_cap: usize,
    filter: ToolFilter,
) -> ToolExecutionOutput {
    let name = &call.function.name;
    let args = &call.function.arguments;
    let capture_limit = full_cap.max(max_size);
    info!("Executing tool: {name}");
    debug!("Tool arguments: {args}");

    match dispatch_tool(
        name,
        args,
        config,
        active_skill,
        session_id,
        capture_limit,
        filter,
    )
    .await
    {
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
    session_id: Option<&str>,
    max_size: usize,
    full_cap: usize,
) -> ToolExecutionOutput {
    execute_with_filter(
        call,
        config,
        active_skill,
        session_id,
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
    session_id: Option<&str>,
    max_size: usize,
    full_cap: usize,
) -> ToolExecutionOutput {
    execute_with_filter(
        call,
        config,
        active_skill,
        session_id,
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
    session_id: Option<&str>,
    max_size: usize,
    full_cap: usize,
) -> ToolExecutionOutput {
    execute_with_filter(
        call,
        config,
        active_skill,
        session_id,
        max_size,
        full_cap,
        ToolFilter::Manual,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{ToolContext, diff_paths};
    use crate::core::config::Config;
    use std::path::Path;

    #[test]
    fn read_root_file_search_anchors_base_root_to_read_root() {
        // Keep the `TempDir` guard alive so the directory is not deleted.
        let root_dir = tempfile::tempdir().unwrap();
        let read_root = dunce::canonicalize(root_dir.path()).unwrap();
        let file = read_root.join("dep.rs");
        std::fs::write(&file, "fn hello() {}\n").unwrap();

        let mut config = Config::default();
        config.app.read_roots = vec![read_root.to_string_lossy().into_owned()];
        let ctx = ToolContext::new(&config, None, None, 1024).unwrap();

        // A single file under a read root must resolve with a non-empty relative
        // path: `base_root` anchors at the read root, not the file itself.
        let (base_root, scan_root) = ctx
            .resolve_scan_roots(&file.to_string_lossy(), "label")
            .unwrap();
        assert_eq!(scan_root, dunce::canonicalize(&file).unwrap());
        assert_eq!(
            diff_paths(&scan_root, &base_root).as_deref(),
            Some(Path::new("dep.rs"))
        );

        // A directory target still resolves (base_root == scan_root == the root).
        let (base_root, scan_root) = ctx
            .resolve_scan_roots(&read_root.to_string_lossy(), "label")
            .unwrap();
        assert_eq!(base_root, scan_root);
        assert_eq!(base_root, read_root);
    }

    #[test]
    fn read_file_via_read_root_succeeds() {
        let root_dir = tempfile::tempdir().unwrap();
        let read_root = dunce::canonicalize(root_dir.path()).unwrap();
        let f = read_root.join("lib.rs");
        std::fs::write(&f, "pub fn x() {}\n").unwrap();

        let mut config = Config::default();
        config.app.read_roots = vec![read_root.to_string_lossy().into_owned()];
        let ctx = ToolContext::new(&config, None, None, 1024).unwrap();

        let resolved = ctx.resolve_readable_path(f.to_str().unwrap()).unwrap();
        assert_eq!(resolved, f);
    }

    #[test]
    fn read_file_hidden_file_in_read_root_allowed() {
        let root_dir = tempfile::tempdir().unwrap();
        let read_root = dunce::canonicalize(root_dir.path()).unwrap();
        // Create a hidden subdirectory with a file inside it.
        let hidden_dir = read_root.join(".secret");
        std::fs::create_dir(&hidden_dir).unwrap();
        let secret = hidden_dir.join("credentials.txt");
        std::fs::write(&secret, "token=abc\n").unwrap();

        let mut config = Config::default();
        config.app.read_roots = vec![read_root.to_string_lossy().into_owned()];
        config.app.allow_hidden = false;
        let ctx = ToolContext::new(&config, None, None, 1024).unwrap();

        // `read_roots` is an explicit permission grant, so hidden files under
        // the root are readable even without `app.allow_hidden`.
        let resolved = ctx.resolve_readable_path(secret.to_str().unwrap()).unwrap();
        assert_eq!(resolved, secret);
    }

    #[test]
    fn read_file_hidden_file_in_read_root_allowed_when_configured() {
        let root_dir = tempfile::tempdir().unwrap();
        let read_root = dunce::canonicalize(root_dir.path()).unwrap();
        let hidden_dir = read_root.join(".secret");
        std::fs::create_dir(&hidden_dir).unwrap();
        let secret = hidden_dir.join("credentials.txt");
        std::fs::write(&secret, "token=abc\n").unwrap();

        let mut config = Config::default();
        config.app.read_roots = vec![read_root.to_string_lossy().into_owned()];
        config.app.allow_hidden = true;
        let ctx = ToolContext::new(&config, None, None, 1024).unwrap();

        let resolved = ctx.resolve_readable_path(secret.to_str().unwrap()).unwrap();
        assert_eq!(resolved, secret);
    }

    #[test]
    fn file_outside_all_read_roots_is_rejected() {
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = dunce::canonicalize(outside_dir.path())
            .unwrap()
            .join("dep.rs");
        std::fs::write(&outside_file, "x\n").unwrap();

        let other_dir = tempfile::tempdir().unwrap();
        let other_root = dunce::canonicalize(other_dir.path()).unwrap();
        let mut config = Config::default();
        config.app.read_roots = vec![other_root.to_string_lossy().into_owned()];
        let ctx = ToolContext::new(&config, None, None, 1024).unwrap();

        let err = ctx
            .resolve_scan_roots(&outside_file.to_string_lossy(), "label")
            .unwrap_err();
        assert!(
            err.to_string().contains("relative") || err.to_string().contains("Security violation"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_file_secret_pattern_in_read_root_blocked() {
        let root_dir = tempfile::tempdir().unwrap();
        let read_root = dunce::canonicalize(root_dir.path()).unwrap();
        // `secrets.json` matches DEFAULT_EXCLUDE_PATTERNS: it must stay blocked
        // under a read root exactly as it does inside the workspace.
        let secret = read_root.join("secrets.json");
        std::fs::write(&secret, "{}\n").unwrap();

        let mut config = Config::default();
        config.app.read_roots = vec![read_root.to_string_lossy().into_owned()];
        config.app.allow_hidden = false;
        let ctx = ToolContext::new(&config, None, None, 1024).unwrap();

        let err = ctx
            .resolve_readable_path(secret.to_str().unwrap())
            .unwrap_err();
        assert!(
            err.to_string().contains("excluded pattern"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unresolved_read_root_hint_surfaces_in_rejection() {
        // Non-hidden prefix: tempdir's default `.tmp*` name would trip the
        // hidden-file policy before reaching the workspace-escape check.
        let base = tempfile::Builder::new()
            .prefix("archon_root_")
            .tempdir()
            .unwrap();
        let broken = base.path().join("missing-root");
        let mut config = Config::default();
        config.app.read_roots = vec![broken.to_string_lossy().into_owned()];
        let ctx = ToolContext::new(&config, None, None, 1024).unwrap();

        // `read_file`: an absolute path outside the workspace is rejected, and
        // the misconfigured entry is named in the message.
        let msg = ctx
            .resolve_readable_path(base.path().to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(msg.contains("escapes workspace"), "unexpected error: {msg}");
        assert!(msg.contains("`app.read_roots`"), "unexpected error: {msg}");
        assert!(msg.contains("missing-root"), "unexpected error: {msg}");

        // `search`: the same path must get the same actionable hint instead of
        // just "path must be relative".
        let msg = ctx
            .resolve_scan_roots(base.path().to_str().unwrap(), "label")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("relative"), "unexpected error: {msg}");
        assert!(msg.contains("missing-root"), "unexpected error: {msg}");
    }
}
