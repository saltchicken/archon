pub(crate) mod browser;
mod cargo;
mod create_file;
mod curl;
mod dotfiles;
mod edit_file;
pub(crate) mod executor;
mod fetch_url;
pub mod fs;
mod git;
mod github_repo;
mod manual;
mod memory;
mod notify;
pub mod plan;
mod process;
mod read_file;
mod repo_map;
mod schedule;
mod search;
mod session;
mod skill;
mod sqlite;
mod system;
pub mod tasks;
pub mod todo;
pub mod workspace_snapshot;

pub use executor::approval::format_approval;
pub use executor::groups::{
    all_group_names, expand_tool_list, expand_tool_spec_checked, format_valid_tools_and_groups,
    is_known_tool_or_group, resolve_tool_group,
};
use executor::load_tools;
pub use manual::parse_manual_tool_call;

pub const LOAD_TOOLS_NAME: &str = executor::load_tools::Tool::NAME;

use crate::config::{Config, expand_tilde};
use anyhow::{Context, Result, bail};
use globset::GlobSet;
use ignore::{DirEntry, WalkBuilder};
use pathdiff::diff_paths;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use executor::utils::{
    fs::read_capped,
    path::{
        build_ancestor_dir_globset, build_globset, ensure_ancestor_within_root, ensure_within_root,
        has_git_dir_component, has_hidden_component, partition_negated_patterns,
        resolve_existing_under, validate_relative_path, workspace_root,
    },
};
use std::{
    ffi::OsStr,
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::timeout};
use tracing::{debug, error, info};

#[cfg(unix)]
use crate::processes::kill_process_group;

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
    const SUBACTIONS: &'static [&'static str] = &[];
    const DEFAULT_ACTION: Option<&'static str> = None;
    const PARAM_HINT: &'static str = "[key=value | JSON]";
    const SHORT_SUMMARY: &'static str;
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String>;

    fn positional_keys(
        _token: &str,
        _map: &serde_json::Map<String, Value>,
    ) -> &'static [&'static str] {
        &[]
    }

    fn apply_positional_arg(token: &str, map: &mut serde_json::Map<String, Value>) {
        if let Some(&k) = Self::positional_keys(token, map)
            .iter()
            .find(|&&k| !map.contains_key(k))
        {
            map.insert(k.into(), Value::String(token.to_string()));
        }
    }

    fn format_approval(
        out: &mut String,
        _args: &Value,
        obj: &serde_json::Map<String, Value>,
        _config: &Config,
        _active_skill: Option<&str>,
    ) {
        executor::approval::format_generic_approval(out, obj);
    }
}

pub trait AsyncTool {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    const SUBACTIONS: &'static [&'static str] = &[];
    const DEFAULT_ACTION: Option<&'static str> = None;
    const PARAM_HINT: &'static str = "[key=value | JSON]";
    const SHORT_SUMMARY: &'static str;
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

    fn execute(
        args: Self::Args,
        ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<String>> + Send;

    fn positional_keys(
        _token: &str,
        _map: &serde_json::Map<String, Value>,
    ) -> &'static [&'static str] {
        &[]
    }

    fn apply_positional_arg(token: &str, map: &mut serde_json::Map<String, Value>) {
        if let Some(&k) = Self::positional_keys(token, map)
            .iter()
            .find(|&&k| !map.contains_key(k))
        {
            map.insert(k.into(), Value::String(token.to_string()));
        }
    }

    fn format_approval(
        out: &mut String,
        _args: &Value,
        obj: &serde_json::Map<String, Value>,
        _config: &Config,
        _active_skill: Option<&str>,
    ) {
        executor::approval::format_generic_approval(out, obj);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionToolState {
    /// Tools dynamically loaded during this session (via `load_tools` or `/tools +`),
    /// preserved in insertion order for prompt-cache stability and FIFO eviction.
    pub loaded: Vec<String>,
    /// Tools explicitly force-enabled for this session via `/tools +` even if disabled in config.
    pub force_enabled: Vec<String>,
    /// Tools explicitly unloaded/disabled for this session via `/tools -`.
    pub force_disabled: Vec<String>,
}

impl SessionToolState {
    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded.iter().any(|t| t == name)
    }

    pub fn is_force_enabled(&self, name: &str) -> bool {
        self.force_enabled.iter().any(|t| t == name)
    }

    pub fn is_force_disabled(&self, name: &str) -> bool {
        self.force_disabled.iter().any(|t| t == name)
    }

    /// Appends `names` to `loaded` (preserving insertion order) and enforces
    /// `max_loaded_tools` via FIFO eviction of the oldest dynamically loaded
    /// tools. Returns any evicted tool names.
    pub fn apply_loaded(
        &mut self,
        names: &[String],
        core_count: usize,
        max_loaded_tools: usize,
    ) -> Vec<String> {
        for name in names {
            if !self.loaded.iter().any(|t| t == name) {
                self.loaded.push(name.clone());
            }
        }
        let mut evicted = Vec::new();
        if max_loaded_tools > 0 {
            let dynamic_cap = max_loaded_tools.saturating_sub(core_count).max(1);
            while self.loaded.len() > dynamic_cap {
                evicted.push(self.loaded.remove(0));
            }
        }
        evicted
    }

    /// Enables and loads `names` for the current session (via `/tools +`).
    pub fn enable_tools(&mut self, names: &[String], config: &Config) {
        for name in names {
            self.force_disabled.retain(|t| t != name);
            if !is_tool_enabled(name, config) && !self.force_enabled.iter().any(|t| t == name) {
                self.force_enabled.push(name.clone());
            }
            if !is_core_tool(name, config) && !self.loaded.iter().any(|t| t == name) {
                self.loaded.push(name.clone());
            }
        }
    }

    /// Unloads and disables `names` for the current session (via `/tools -`).
    pub fn disable_tools(&mut self, names: &[String]) {
        for name in names {
            self.loaded.retain(|t| t != name);
            self.force_enabled.retain(|t| t != name);
            if !self.force_disabled.iter().any(|t| t == name) {
                self.force_disabled.push(name.clone());
            }
        }
    }

    pub fn reset(&mut self) {
        self.loaded.clear();
        self.force_enabled.clear();
        self.force_disabled.clear();
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub root: PathBuf,
    pub max_output_size: usize,
    pub active_skill: Option<String>,
    pub session_id: Option<String>,
    pub session_tools: SessionToolState,
    pub config: Config,
}

impl ToolContext {
    fn new(
        config: &Config,
        active_skill: Option<&str>,
        session_id: Option<&str>,
        session_tools: &SessionToolState,
        max_output_size: usize,
    ) -> Result<Self> {
        Ok(Self {
            root: workspace_root()?,
            max_output_size,
            active_skill: active_skill.map(String::from),
            session_id: session_id.map(String::from),
            session_tools: session_tools.clone(),
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

        let skills_root = expand_tilde(&self.config.app.skills_dir);
        let canon_skills = dunce::canonicalize(&skills_root).unwrap_or(skills_root);

        if canon.starts_with(&canon_skills) {
            let allowed = self
                .active_skill
                .as_deref()
                .and_then(|name| crate::skills::find_skill(&self.config, name))
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

        if !candidate.starts_with(&canon_skills) {
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

pub fn auto_approve_match_specificity(call: &ToolCall, rule: &str) -> Option<u8> {
    let r = rule.trim();
    if r.is_empty() {
        return None;
    }
    let name = call.function.name.as_str();
    let sig = call_signature(call);

    if r == "*" {
        Some(1)
    } else if r == sig {
        Some(3)
    } else if r == name {
        // When `sig != name`, the call has a subaction, so bare `name` matches the whole tool family.
        Some(if sig == name { 3 } else { 2 })
    } else if let Some(p) = r.strip_suffix(":*")
        && (name == p || sig.starts_with(&format!("{p}:")))
    {
        Some(2)
    } else {
        None
    }
}

pub fn is_auto_approved<'a, I>(call: &ToolCall, rules: I) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    rules
        .into_iter()
        .any(|r| auto_approve_match_specificity(call, r).is_some())
}

pub fn is_tool_enabled(name: &str, config: &Config) -> bool {
    crate::config::tool_list_contains(&config.app.enabled_tools, name, &config.tool_groups)
}

pub fn is_core_tool(name: &str, config: &Config) -> bool {
    crate::config::tool_list_contains(&config.app.core_tools, name, &config.tool_groups)
}

pub fn is_tool_session_enabled(
    name: &str,
    config: &Config,
    session_tools: &SessionToolState,
) -> bool {
    if session_tools.is_force_disabled(name) {
        return false;
    }
    session_tools.is_force_enabled(name) || is_tool_enabled(name, config)
}

pub fn is_tool_enabled_for_turn(
    name: &str,
    config: &Config,
    session_tools: &SessionToolState,
    active_skill: Option<&str>,
) -> bool {
    if name == LOAD_TOOLS_NAME {
        return ALL_TOOL_NAMES
            .iter()
            .any(|&t| is_tool_enabled_for_turn(t, config, session_tools, active_skill));
    }
    if !is_tool_session_enabled(name, config, session_tools) {
        return false;
    }
    let Some(skill) = active_skill.and_then(|s| crate::skills::find_skill(config, s)) else {
        return true;
    };
    skill.allows_tool(name)
}

/// Regular tools that are immediately active on this turn without calling `load_tools`:
/// turn-eligible `core_tools` plus any tools explicitly declared by an active `@skill`.
pub fn core_regular_tools_for_turn(
    config: &Config,
    session_tools: &SessionToolState,
    active_skill: Option<&str>,
) -> Vec<String> {
    let skill = active_skill.and_then(|s| crate::skills::find_skill(config, s));
    ALL_TOOL_NAMES
        .iter()
        .filter(|&&name| {
            is_tool_enabled_for_turn(name, config, session_tools, active_skill)
                && (is_core_tool(name, config)
                    || skill.is_some_and(|s| s.explicitly_declares_tool(name)))
        })
        .map(|&name| name.to_string())
        .collect()
}

/// All regular tools whose full JSON schemas are active on this turn:
/// core/skill-declared tools followed by dynamically loaded session tools in insertion order.
pub fn active_regular_tools_for_turn(
    config: &Config,
    session_tools: &SessionToolState,
    active_skill: Option<&str>,
) -> Vec<String> {
    let mut active = core_regular_tools_for_turn(config, session_tools, active_skill);
    for loaded in &session_tools.loaded {
        if is_tool_enabled_for_turn(loaded, config, session_tools, active_skill)
            && !active.iter().any(|a| a == loaded)
        {
            active.push(loaded.clone());
        }
    }
    active
}

/// Regular tools enabled for this turn that are not core/skill-declared tools,
/// independent of `session_tools.loaded`, returned in canonical `ALL_TOOL_NAMES` order.
///
/// Keeping this set independent of dynamically loaded tools ensures the
/// `<deferred_tools>` system-prompt catalog and `load_tools` declaration stay
/// byte-stable across `load_tools` calls for prompt-cache stability.
pub fn catalog_deferred_tool_names_for_turn(
    config: &Config,
    session_tools: &SessionToolState,
    active_skill: Option<&str>,
) -> Vec<String> {
    let core = core_regular_tools_for_turn(config, session_tools, active_skill);
    ALL_TOOL_NAMES
        .iter()
        .filter(|&&name| {
            is_tool_enabled_for_turn(name, config, session_tools, active_skill)
                && !core.iter().any(|c| c == name)
        })
        .map(|&name| name.to_string())
        .collect()
}

/// Regular tools enabled for this turn but currently deferred (available via `load_tools`),
/// returned in canonical `ALL_TOOL_NAMES` order.
pub fn deferred_tool_names_for_turn(
    config: &Config,
    session_tools: &SessionToolState,
    active_skill: Option<&str>,
) -> Vec<String> {
    let active = active_regular_tools_for_turn(config, session_tools, active_skill);
    ALL_TOOL_NAMES
        .iter()
        .filter(|&&name| {
            is_tool_enabled_for_turn(name, config, session_tools, active_skill)
                && !active.iter().any(|a| a == name)
        })
        .map(|&name| name.to_string())
        .collect()
}

/// Ordered tool names whose schemas are sent to the model on this turn.
///
/// Placement of `load_tools` right after `core_tools` (before dynamically `loaded` tools)
/// — and keeping it present whenever any non-core enabled tools exist — keeps tool
/// declarations strictly append-only across `load_tools` calls for prompt-cache stability.
pub fn active_tool_names_for_turn(
    config: &Config,
    session_tools: &SessionToolState,
    active_skill: Option<&str>,
) -> Vec<String> {
    let mut names = core_regular_tools_for_turn(config, session_tools, active_skill);
    let has_non_core =
        !catalog_deferred_tool_names_for_turn(config, session_tools, active_skill).is_empty();
    if has_non_core {
        names.push(LOAD_TOOLS_NAME.to_string());
    }
    for loaded in &session_tools.loaded {
        if is_tool_enabled_for_turn(loaded, config, session_tools, active_skill)
            && !names.iter().any(|a| a == loaded)
        {
            names.push(loaded.clone());
        }
    }
    names
}

pub fn schemas_for_turn(
    config: &Config,
    session_tools: &SessionToolState,
    active_skill: Option<&str>,
) -> Vec<Value> {
    let names = active_tool_names_for_turn(config, session_tools, active_skill);
    schemas_for_tool_names(&names)
}

macro_rules! register_tools {
    ($( $kind:ident : $mod:ident ),* $(,)?) => {
        pub const ALL_TOOL_NAMES: &[&str] = &[ $( $mod::Tool::NAME ),* ];

        pub fn tool_subactions(tool_name: &str) -> &'static [&'static str] {
            match tool_name {
                LOAD_TOOLS_NAME => register_tools!(@subactions blocking, load_tools),
                $( $mod::Tool::NAME => register_tools!(@subactions $kind, $mod), )*
                _ => &[],
            }
        }

        pub fn tool_param_hint(tool_name: &str) -> &'static str {
            match tool_name {
                LOAD_TOOLS_NAME => register_tools!(@param_hint blocking, load_tools),
                $( $mod::Tool::NAME => register_tools!(@param_hint $kind, $mod), )*
                _ => "[key=value | JSON]",
            }
        }

        /// Concise one-line summary for each tool, used in the `<deferred_tools>` catalog.
        pub fn tool_short_summary(tool_name: &str) -> &'static str {
            match tool_name {
                LOAD_TOOLS_NAME => register_tools!(@short_summary blocking, load_tools),
                $( $mod::Tool::NAME => register_tools!(@short_summary $kind, $mod), )*
                _ => "Built-in tool.",
            }
        }

        pub(super) fn tool_default_action(tool_name: &str) -> Option<&'static str> {
            match tool_name {
                LOAD_TOOLS_NAME => register_tools!(@default_action blocking, load_tools),
                $( $mod::Tool::NAME => register_tools!(@default_action $kind, $mod), )*
                _ => None,
            }
        }

        pub(super) fn apply_positional_manual_arg(
            tool_name: &str,
            token: &str,
            map: &mut serde_json::Map<String, Value>,
        ) {
            match tool_name {
                LOAD_TOOLS_NAME => {
                    register_tools!(@apply_positional blocking, load_tools, token, map)
                }
                $( $mod::Tool::NAME => register_tools!(@apply_positional $kind, $mod, token, map), )*
                _ => {}
            }
        }

        pub(super) fn format_tool_approval_body(
            tool_name: &str,
            out: &mut String,
            args: &Value,
            obj: &serde_json::Map<String, Value>,
            config: &Config,
            active_skill: Option<&str>,
        ) {
            match tool_name {
                LOAD_TOOLS_NAME => register_tools!(
                    @format_approval blocking, load_tools, out, args, obj, config, active_skill
                ),
                $( $mod::Tool::NAME => register_tools!(
                    @format_approval $kind, $mod, out, args, obj, config, active_skill
                ), )*
                _ => executor::approval::format_generic_approval(out, obj),
            }
        }

        pub fn tool_schema_by_name(name: &str) -> Option<Value> {
            match name {
                LOAD_TOOLS_NAME => Some(register_tools!(@schema blocking, load_tools)),
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
                LOAD_TOOLS_NAME => {
                    let _: register_tools!(@args_ty blocking, load_tools) =
                        parse_args(args, LOAD_TOOLS_NAME)?;
                    Ok(())
                }
                $( $mod::Tool::NAME => {
                    let _: register_tools!(@args_ty $kind, $mod) = parse_args(args, $mod::Tool::NAME)?;
                    Ok(())
                } )*
                _ => bail!("Unknown tool: '{name}'"),
            }
        }

        #[allow(clippy::too_many_arguments)]
        async fn dispatch_tool(
            name: &str,
            args: &Value,
            config: &Config,
            active_skill: Option<&str>,
            session_id: Option<&str>,
            session_tools: &SessionToolState,
            max_output_size: usize,
            filter: ToolFilter,
        ) -> Result<String> {
            let allowed = match filter {
                ToolFilter::Turn => {
                    is_tool_enabled_for_turn(name, config, session_tools, active_skill)
                }
                ToolFilter::Enabled => is_tool_enabled(name, config),
                ToolFilter::Manual => name == LOAD_TOOLS_NAME || ALL_TOOL_NAMES.contains(&name),
            };
            if !allowed {
                bail!("Unknown tool: '{name}'");
            }
            let ctx = ToolContext::new(
                config,
                active_skill,
                session_id,
                session_tools,
                max_output_size,
            )?;
            match name {
                LOAD_TOOLS_NAME => register_tools!(@run blocking, load_tools::Tool, args, ctx),
                $( $mod::Tool::NAME => register_tools!(@run $kind, $mod::Tool, args, ctx), )*
                _ => bail!("Unknown tool: '{name}'"),
            }
        }
    };
    (@subactions blocking, $mod:ident) => {
        <$mod::Tool as BlockingTool>::SUBACTIONS
    };
    (@subactions async, $mod:ident) => {
        <$mod::Tool as AsyncTool>::SUBACTIONS
    };
    (@default_action blocking, $mod:ident) => {
        <$mod::Tool as BlockingTool>::DEFAULT_ACTION
    };
    (@default_action async, $mod:ident) => {
        <$mod::Tool as AsyncTool>::DEFAULT_ACTION
    };
    (@param_hint blocking, $mod:ident) => {
        <$mod::Tool as BlockingTool>::PARAM_HINT
    };
    (@param_hint async, $mod:ident) => {
        <$mod::Tool as AsyncTool>::PARAM_HINT
    };
    (@short_summary blocking, $mod:ident) => {
        <$mod::Tool as BlockingTool>::SHORT_SUMMARY
    };
    (@short_summary async, $mod:ident) => {
        <$mod::Tool as AsyncTool>::SHORT_SUMMARY
    };
    (@apply_positional blocking, $mod:ident, $token:ident, $map:ident) => {
        <$mod::Tool as BlockingTool>::apply_positional_arg($token, $map)
    };
    (@apply_positional async, $mod:ident, $token:ident, $map:ident) => {
        <$mod::Tool as AsyncTool>::apply_positional_arg($token, $map)
    };
    (@format_approval blocking, $mod:ident, $out:ident, $args:ident, $obj:ident, $config:ident, $active_skill:ident) => {
        <$mod::Tool as BlockingTool>::format_approval($out, $args, $obj, $config, $active_skill)
    };
    (@format_approval async, $mod:ident, $out:ident, $args:ident, $obj:ident, $config:ident, $active_skill:ident) => {
        <$mod::Tool as AsyncTool>::format_approval($out, $args, $obj, $config, $active_skill)
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
    blocking: plan,
    async: process,
    blocking: read_file,
    blocking: repo_map,
    blocking: schedule,
    blocking: search,
    blocking: session,
    blocking: skill,
    blocking: sqlite,
    async: system,
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
    let empty_tools = SessionToolState::default();
    let ctx = match ToolContext::new(config, None, None, &empty_tools, capture_limit) {
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

#[allow(clippy::too_many_arguments)]
async fn execute_with_filter(
    call: &ToolCall,
    config: &Config,
    active_skill: Option<&str>,
    session_id: Option<&str>,
    session_tools: &SessionToolState,
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
        session_tools,
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
    session_tools: &SessionToolState,
    max_size: usize,
    full_cap: usize,
) -> ToolExecutionOutput {
    execute_with_filter(
        call,
        config,
        active_skill,
        session_id,
        session_tools,
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
    let empty_tools = SessionToolState::default();
    execute_with_filter(
        call,
        config,
        active_skill,
        session_id,
        &empty_tools,
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
    session_tools: &SessionToolState,
    max_size: usize,
    full_cap: usize,
) -> ToolExecutionOutput {
    execute_with_filter(
        call,
        config,
        active_skill,
        session_id,
        session_tools,
        max_size,
        full_cap,
        ToolFilter::Manual,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{SessionToolState, ToolContext, diff_paths};
    use crate::config::Config;
    use std::path::Path;

    fn test_tool_ctx(config: &Config) -> ToolContext {
        ToolContext::new(config, None, None, &SessionToolState::default(), 1024).unwrap()
    }

    #[test]
    fn read_root_file_search_anchors_base_root_to_read_root() {
        // Keep the `TempDir` guard alive so the directory is not deleted.
        let root_dir = tempfile::tempdir().unwrap();
        let read_root = dunce::canonicalize(root_dir.path()).unwrap();
        let file = read_root.join("dep.rs");
        std::fs::write(&file, "fn hello() {}\n").unwrap();

        let mut config = Config::default();
        config.app.read_roots = vec![read_root.to_string_lossy().into_owned()];
        let ctx = test_tool_ctx(&config);

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
        let ctx = test_tool_ctx(&config);

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
        let ctx = test_tool_ctx(&config);

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
        let ctx = test_tool_ctx(&config);

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
        let ctx = test_tool_ctx(&config);

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
        let ctx = test_tool_ctx(&config);

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
        let ctx = test_tool_ctx(&config);

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

    #[test]
    fn all_registered_tools_have_dedicated_summaries() {
        for &tool in super::ALL_TOOL_NAMES {
            assert_ne!(
                super::tool_short_summary(tool),
                "Built-in tool.",
                "tool '{tool}' should have a dedicated one-line summary"
            );
        }
    }

    #[test]
    fn schemas_for_turn_keeps_load_tools_append_only_for_non_core_tools() {
        let mut cfg = Config::default();
        cfg.app.enabled_tools = vec![
            "read_file".into(),
            "edit_file".into(),
            "git".into(),
            "cargo".into(),
        ];
        cfg.app.core_tools = vec!["read_file".into(), "edit_file".into()];

        let mut session_tools = SessionToolState::default();

        let schema_names = |cfg: &Config, st: &SessionToolState, skill: Option<&str>| {
            super::schemas_for_turn(cfg, st, skill)
                .into_iter()
                .map(|s| s["function"]["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        // Turn 1: core_tools + load_tools (git & cargo are deferred).
        let t1 = schema_names(&cfg, &session_tools, None);
        assert_eq!(t1, vec!["edit_file", "read_file", "load_tools"]);

        // Turn 2: load `git` -> append-only after `load_tools`, `cargo` still deferred.
        session_tools.apply_loaded(&["git".into()], 2, cfg.app.max_loaded_tools);
        let t2 = schema_names(&cfg, &session_tools, None);
        assert_eq!(t2, vec!["edit_file", "read_file", "load_tools", "git"]);
        // Prompt-cache stability: prefix of Turn 2 matches Turn 1 verbatim.
        assert_eq!(&t2[..t1.len()], &t1[..]);

        // Turn 3: load `cargo` -> `load_tools` stays in place so declarations remain
        // strictly append-only across `load_tools` calls for prompt-cache stability.
        session_tools.apply_loaded(&["cargo".into()], 2, cfg.app.max_loaded_tools);
        let t3 = schema_names(&cfg, &session_tools, None);
        assert_eq!(
            t3,
            vec!["edit_file", "read_file", "load_tools", "git", "cargo"]
        );
        assert_eq!(&t3[..t2.len()], &t2[..]);
        assert!(super::deferred_tool_names_for_turn(&cfg, &session_tools, None).is_empty());

        // When `core_tools = ["*"]`, all enabled tools are active and `load_tools` is omitted.
        cfg.app.core_tools = vec!["*".into()];
        let all_core = schema_names(&cfg, &SessionToolState::default(), None);
        assert_eq!(all_core, vec!["cargo", "edit_file", "git", "read_file"]);
    }

    #[test]
    fn active_skill_declared_tools_bypass_deferred_loading() {
        let mut cfg = Config::default();
        cfg.app.enabled_tools = vec![
            "read_file".into(),
            "git".into(),
            "cargo".into(),
            "sqlite".into(),
        ];
        cfg.app.core_tools = vec!["read_file".into()];

        let explicit_skill = crate::skills::parse_skill_markdown(
            "+++\nname = \"git-cargo\"\ntools = [\"git\", \"cargo\"]\n+++\nUse git and cargo.",
            Path::new("/tmp/git-cargo"),
        )
        .unwrap();
        let prompt_only_skill = crate::skills::parse_skill_markdown(
            "+++\nname = \"helper\"\n+++\nBe helpful.",
            Path::new("/tmp/helper"),
        )
        .unwrap();
        cfg.skills = crate::skills::resolve_skills(vec![explicit_skill, prompt_only_skill]);

        let st = SessionToolState::default();

        // Explicit `tools = ["git", "cargo"]`: both immediately loaded, no `load_tools`, no deferred.
        let names: Vec<String> = super::schemas_for_turn(&cfg, &st, Some("git-cargo"))
            .into_iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["cargo", "git"]);
        assert!(super::deferred_tool_names_for_turn(&cfg, &st, Some("git-cargo")).is_empty());

        let sys_explicit = crate::skills::compose_system_prompt_for_turn(
            "",
            Some("git-cargo"),
            None,
            None,
            &std::collections::BTreeMap::new(),
            None,
            &cfg,
            true,
            &st,
        );
        assert!(!sys_explicit.contains("<deferred_tools>"));

        // Prompt-only skill (unrestricted): normal deferred loading still applies.
        let names_prompt_only: Vec<String> = super::schemas_for_turn(&cfg, &st, Some("helper"))
            .into_iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names_prompt_only, vec!["read_file", "load_tools"]);
    }

    #[test]
    fn deferred_tools_catalog_is_cache_stable_and_advertises_custom_groups() {
        let mut cfg = Config::default();
        cfg.app.enabled_tools = vec!["read_file".into(), "git".into(), "cargo".into()];
        cfg.app.core_tools = vec!["read_file".into()];
        cfg.tool_groups
            .insert("backend".into(), vec!["git".into(), "cargo".into()]);

        let mut st = SessionToolState::default();
        let empty = std::collections::BTreeMap::new();

        // Initially both `cargo` and `git` appear in `<deferred_tools>`, while core `read_file` does not.
        let sys1 = crate::skills::compose_system_prompt_for_turn(
            "Base.", None, None, None, &empty, None, &cfg, true, &st,
        );
        assert!(sys1.contains("<deferred_tools>"));
        assert!(sys1.contains("`@backend`"));
        assert!(sys1.contains("- cargo: "));
        assert!(sys1.contains("- git: "));
        assert!(!sys1.contains("- read_file: "));
        assert!(!sys1.contains("- sqlite: ")); // disabled

        // When `enable_tools = false`, no `<deferred_tools>` block is injected.
        let sys_no_tools = crate::skills::compose_system_prompt_for_turn(
            "Base.", None, None, None, &empty, None, &cfg, false, &st,
        );
        assert!(!sys_no_tools.contains("<deferred_tools>"));

        // After loading `git`, `deferred_tool_names_for_turn` (used by sidebar / `/tools`)
        // removes `git`, while the system prompt stays byte-identical for prompt-cache stability.
        st.apply_loaded(&["git".into()], 1, cfg.app.max_loaded_tools);
        assert_eq!(
            super::deferred_tool_names_for_turn(&cfg, &st, None),
            vec!["cargo"]
        );
        let sys2 = crate::skills::compose_system_prompt_for_turn(
            "Base.", None, None, None, &empty, None, &cfg, true, &st,
        );
        assert_eq!(sys2, sys1);

        // After loading `cargo` too, UI deferred list is empty while system prompt remains stable.
        st.apply_loaded(&["cargo".into()], 1, cfg.app.max_loaded_tools);
        assert!(super::deferred_tool_names_for_turn(&cfg, &st, None).is_empty());
        let sys3 = crate::skills::compose_system_prompt_for_turn(
            "Base.", None, None, None, &empty, None, &cfg, true, &st,
        );
        assert_eq!(sys3, sys1);
    }

    #[tokio::test]
    async fn load_tools_rejects_disabled_and_unknown_and_activates_enabled_with_fifo_eviction() {
        let mut cfg = Config::default();
        cfg.app.enabled_tools = vec![
            "read_file".into(),
            "git".into(),
            "cargo".into(),
            "fetch_url".into(),
            "browser".into(), // note: `curl` from `@web` is disabled
        ];
        cfg.app.core_tools = vec!["read_file".into()];
        cfg.app.max_loaded_tools = 3; // 1 core + 2 dynamic slots
        cfg.tool_groups.insert(
            "web".into(),
            vec!["fetch_url".into(), "curl".into(), "browser".into()],
        );

        let mut st = SessionToolState::default();

        // 1. Rejects disabled individual tool (`curl`).
        let call_disabled = super::ToolCall {
            function: super::ToolFunction {
                name: super::LOAD_TOOLS_NAME.into(),
                arguments: serde_json::json!({ "names": ["curl"] }),
            },
            thought_signature: None,
        };
        let out_disabled = super::execute(&call_disabled, &cfg, None, None, &st, 4096, 8192).await;
        assert!(out_disabled.is_error);
        assert!(out_disabled.truncated_output.contains("Disabled tool(s)"));
        assert!(out_disabled.truncated_output.contains("curl"));

        // 2. Rejects unknown tool/group.
        let call_unknown = super::ToolCall {
            function: super::ToolFunction {
                name: super::LOAD_TOOLS_NAME.into(),
                arguments: serde_json::json!({ "names": ["@no_such_group"] }),
            },
            thought_signature: None,
        };
        let out_unknown = super::execute(&call_unknown, &cfg, None, None, &st, 4096, 8192).await;
        assert!(out_unknown.is_error);
        assert!(out_unknown.truncated_output.contains("Unknown tool/group"));

        // 3. Activates enabled `@web` members (`fetch_url`, `browser`) and skips disabled `curl`.
        let call_web = super::ToolCall {
            function: super::ToolFunction {
                name: super::LOAD_TOOLS_NAME.into(),
                arguments: serde_json::json!({ "names": ["@web"] }),
            },
            thought_signature: None,
        };
        let out_web = super::execute(&call_web, &cfg, None, None, &st, 4096, 8192).await;
        assert!(!out_web.is_error, "{}", out_web.truncated_output);
        assert!(out_web.truncated_output.contains("fetch_url"));
        assert!(out_web.truncated_output.contains("browser"));
        assert!(out_web.truncated_output.contains("Skipped disabled"));
        assert!(out_web.truncated_output.contains("curl"));

        // Apply loaded to session state and verify FIFO eviction when loading `git`.
        let evicted1 = st.apply_loaded(
            &["fetch_url".into(), "browser".into()],
            1,
            cfg.app.max_loaded_tools,
        );
        assert!(evicted1.is_empty());
        assert_eq!(st.loaded, vec!["fetch_url", "browser"]);

        let call_git = super::ToolCall {
            function: super::ToolFunction {
                name: super::LOAD_TOOLS_NAME.into(),
                arguments: serde_json::json!({ "names": ["git"] }),
            },
            thought_signature: None,
        };
        let out_git = super::execute(&call_git, &cfg, None, None, &st, 4096, 8192).await;
        assert!(!out_git.is_error, "{}", out_git.truncated_output);
        assert!(out_git.truncated_output.contains("Evicted oldest"));
        assert!(out_git.truncated_output.contains("fetch_url"));

        let evicted2 = st.apply_loaded(&["git".into()], 1, cfg.app.max_loaded_tools);
        assert_eq!(evicted2, vec!["fetch_url"]);
        assert_eq!(st.loaded, vec!["browser", "git"]);
    }

    #[tokio::test]
    async fn load_tools_handles_single_string_oversized_budget_zero_limit_and_nested_group_errors()
    {
        let mut cfg = Config::default();
        cfg.app.enabled_tools = vec![
            "read_file".into(),
            "git".into(),
            "cargo".into(),
            "fetch_url".into(),
            "browser".into(),
        ];
        cfg.app.core_tools = vec!["read_file".into()];
        cfg.app.max_loaded_tools = 3; // 1 core + 2 dynamic slots
        cfg.tool_groups.insert(
            "bad_nested".into(),
            vec!["git".into(), "@typo_group".into()],
        );
        cfg.tool_groups
            .insert("bare_at".into(), vec!["git".into(), "@".into()]);
        cfg.tool_groups
            .insert("mixed_case".into(), vec!["Git".into(), "Cargo".into()]);

        let mut st = SessionToolState::default();

        // 1. Single-string `"names": "git"` is tolerated via `deserialize_string_list`.
        let call_single = super::ToolCall {
            function: super::ToolFunction {
                name: super::LOAD_TOOLS_NAME.into(),
                arguments: serde_json::json!({ "names": "git" }),
            },
            thought_signature: None,
        };
        let out_single = super::execute(&call_single, &cfg, None, None, &st, 4096, 8192).await;
        assert!(!out_single.is_error, "{}", out_single.truncated_output);
        assert!(out_single.truncated_output.contains("git"));

        // 2. Mixed-case custom group members canonicalize cleanly.
        let expanded_mixed =
            super::expand_tool_spec_checked(&["@mixed_case".into()], &cfg.tool_groups).unwrap();
        assert_eq!(expanded_mixed, vec!["git", "cargo"]);

        // 3. Unknown nested `@group` (including bare `@`) inside a custom group is surfaced as an error.
        let err_nested =
            super::expand_tool_spec_checked(&["@bad_nested".into()], &cfg.tool_groups).unwrap_err();
        assert_eq!(err_nested, vec!["@typo_group"]);
        let err_bare_at =
            super::expand_tool_spec_checked(&["@bare_at".into()], &cfg.tool_groups).unwrap_err();
        assert_eq!(err_bare_at, vec!["@"]);

        let call_nested = super::ToolCall {
            function: super::ToolFunction {
                name: super::LOAD_TOOLS_NAME.into(),
                arguments: serde_json::json!({ "names": ["@bad_nested"] }),
            },
            thought_signature: None,
        };
        let out_nested = super::execute(&call_nested, &cfg, None, None, &st, 4096, 8192).await;
        assert!(out_nested.is_error);
        assert!(out_nested.truncated_output.contains("@typo_group"));

        // 4. Requesting more tools than `dynamic_cap` (3 > 2) fails cleanly rather than
        //    immediately evicting part of the request.
        let call_oversized = super::ToolCall {
            function: super::ToolFunction {
                name: super::LOAD_TOOLS_NAME.into(),
                arguments: serde_json::json!({ "names": ["git", "cargo", "browser"] }),
            },
            thought_signature: None,
        };
        let out_oversized =
            super::execute(&call_oversized, &cfg, None, None, &st, 4096, 8192).await;
        assert!(out_oversized.is_error);
        assert!(
            out_oversized
                .truncated_output
                .contains("exceeds the dynamic tool slot budget")
        );

        // 5. Already-active no-op `load_tools` does not report phantom evictions even when
        //    `/tools +` previously pushed `loaded` past `dynamic_cap`.
        let mut over_cap = SessionToolState::default();
        over_cap.enable_tools(&["git".into(), "cargo".into(), "browser".into()], &cfg);
        let out_noop = super::execute(&call_single, &cfg, None, None, &over_cap, 4096, 8192).await;
        assert!(!out_noop.is_error, "{}", out_noop.truncated_output);
        assert!(out_noop.truncated_output.contains("Already active: git"));
        assert!(!out_noop.truncated_output.contains("Evicted"));

        // 6. `max_loaded_tools = 0` disables eviction and allows arbitrary batch sizes.
        cfg.app.max_loaded_tools = 0;
        let out_unbounded =
            super::execute(&call_oversized, &cfg, None, None, &st, 4096, 8192).await;
        assert!(
            !out_unbounded.is_error,
            "{}",
            out_unbounded.truncated_output
        );
        let evicted = st.apply_loaded(
            &["git".into(), "cargo".into(), "browser".into()],
            1,
            cfg.app.max_loaded_tools,
        );
        assert!(evicted.is_empty());
        assert_eq!(st.loaded, vec!["git", "cargo", "browser"]);
    }
}
