mod cargo;
mod create_file;
mod dotfiles;
mod edit_file;
mod execute_command;
mod fetch_url;
mod fs;
mod git;
mod read_file;
mod repo_map;
mod search;
mod system;
mod utils;
mod web_search;
mod workspace_snapshot;

use crate::core::config::{
    Config, ExecuteCommandSettings, WorkspaceSnapshotSettings, expand_tilde,
};
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
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::timeout};
use tracing::{debug, error, info};
use utils::{
    fs::read_capped,
    path::{
        build_globset, ensure_within_root, has_git_dir_component, resolve_existing_under,
        validate_relative_path, workspace_root,
    },
};

const COMMAND_TIMEOUT_SECS: u64 = 30;

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
    pub snapshot: WorkspaceSnapshotSettings,
    pub execute_command: ExecuteCommandSettings,
    pub active_skill: Option<String>,
    pub config: Config,
}

impl ToolContext {
    fn new(config: &Config, active_skill: Option<&str>, max_output_size: usize) -> Result<Self> {
        Ok(Self {
            root: workspace_root()?,
            max_output_size,
            snapshot: config.workspace_snapshot.clone(),
            execute_command: config.execute_command.clone(),
            active_skill: active_skill.map(String::from),
            config: config.clone(),
        })
    }

    fn from_cwd() -> Result<Self> {
        Ok(Self {
            root: workspace_root()?,
            max_output_size: 0,
            snapshot: WorkspaceSnapshotSettings::default(),
            execute_command: ExecuteCommandSettings::default(),
            active_skill: None,
            config: Config::default(),
        })
    }

    pub fn ensure_within_workspace(&self, path: &Path, label: &str) -> Result<PathBuf> {
        ensure_within_root(path, &self.root, label)
    }

    pub fn ensure_path_or_ancestor_within_workspace(
        &self,
        target: &Path,
        label: &str,
    ) -> Result<()> {
        if let Some(existing) = target.ancestors().find(|p| p.exists()) {
            self.ensure_within_workspace(existing, label)?;
        }
        Ok(())
    }

    pub fn validate_scoped_relative_path<'a>(&self, raw: &'a str) -> Result<&'a Path> {
        let rel = validate_relative_path(raw)?;
        self.ensure_path_or_ancestor_within_workspace(&self.root.join(rel), raw.trim())?;
        Ok(rel)
    }

    pub fn resolve_existing_under_root(&self, raw: &str, escape_label: &str) -> Result<PathBuf> {
        resolve_existing_under(&self.root, raw, escape_label)
    }

    pub fn resolve_existing_path(&self, raw: &str) -> Result<PathBuf> {
        let trimmed = raw.trim();
        let target = self.root.join(validate_relative_path(trimmed)?);
        if !target.exists() {
            bail!("Target path does not exist: '{trimmed}'");
        }
        self.ensure_within_workspace(&target, trimmed)
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

        self.ensure_within_workspace(&canon, trimmed)
    }

    pub fn resolve_new_path(&self, raw: &str) -> Result<PathBuf> {
        let trimmed = raw.trim();
        let target = self.root.join(validate_relative_path(trimmed)?);
        if target.exists() {
            bail!("File already exists: {trimmed}. Use edit_file instead.");
        }
        self.ensure_path_or_ancestor_within_workspace(&target, trimmed)?;
        Ok(target)
    }

    pub fn resolve_scan_roots(
        &self,
        target_path: &str,
        escape_label: &str,
    ) -> Result<(PathBuf, PathBuf)> {
        let base_root = if self.snapshot.git_root {
            self.root
                .ancestors()
                .find(|a| a.join(".git").exists())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.clone())
        } else {
            self.root.clone()
        };
        let base_root = dunce::canonicalize(&base_root).unwrap_or(base_root);
        let scan_root = resolve_existing_under(&base_root, target_path, escape_label)?;
        Ok((base_root, scan_root))
    }

    pub fn build_walker(&self, scan_root: &Path) -> impl Iterator<Item = DirEntry> {
        let use_git_ignore = self.snapshot.git_root;
        WalkBuilder::new(scan_root)
            .hidden(false)
            .git_ignore(use_git_ignore)
            .git_global(use_git_ignore)
            .git_exclude(use_git_ignore)
            .build()
            .flatten()
    }

    pub fn build_exclude_globset(
        &self,
        call_exclude: &[String],
        allow_recursive_shorthand: bool,
    ) -> Result<GlobSet> {
        let patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
            .iter()
            .map(|&s| String::from(s))
            .chain(self.snapshot.exclude.iter().cloned())
            .chain(call_exclude.iter().cloned())
            .collect();
        build_globset(&patterns, allow_recursive_shorthand)
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
        let exclude_set = self.build_exclude_globset(call_exclude, true)?;

        if scan_root.is_file() {
            let rel = diff_paths(scan_root, base_root).unwrap_or_else(|| scan_root.to_path_buf());
            if exclude_set.is_match(&rel) {
                bail!(
                    "Security violation: '{}' matches an excluded pattern",
                    rel.display()
                );
            }
            return Ok(vec![(
                scan_root.to_path_buf(),
                rel.to_string_lossy().into_owned(),
            )]);
        }

        let mut files = Vec::new();
        for entry in self.build_walker(scan_root) {
            if files.len() >= self.snapshot.max_files {
                break;
            }
            let path = entry.path();
            if !path.is_file() || has_git_dir_component(path) {
                continue;
            }
            let Some(relative) = diff_paths(path, base_root) else {
                continue;
            };
            if exclude_set.is_match(&relative)
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
        let mut child = Command::new(program)
            .current_dir(&self.root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Error executing {program}"))?;

        let cap = (self.max_output_size as u64).saturating_add(1);
        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        let stderr = child.stderr.take().context("Failed to capture stderr")?;

        let (out_bytes, err_bytes, status) = timeout(Duration::from_secs(timeout_secs), async {
            let (r1, r2, status) = tokio::join!(
                read_capped(stdout, cap),
                read_capped(stderr, cap),
                child.wait()
            );
            Ok::<_, std::io::Error>((r1?, r2?, status?))
        })
        .await
        .map_err(|_| anyhow::anyhow!("Command '{program}' timed out after {timeout_secs} seconds"))?
        .with_context(|| format!("Error reading output from {program}"))?;

        let mut combined = String::from_utf8_lossy(&out_bytes).into_owned();
        if !err_bytes.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str("[STDERR]\n");
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub function: ToolFunction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: Value,
}

pub fn call_signature(call: &ToolCall) -> String {
    let name = call.function.name.as_str();
    let args = &call.function.arguments;

    let sub = match name {
        "git" | "cargo" | "fs" | "repo_map" | "dotfiles" | "system" => args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        "execute_command" => {
            shlex::split(args.get("command").and_then(Value::as_str).unwrap_or(""))
                .and_then(|parts| parts.into_iter().next())
                .unwrap_or_default()
        }
        _ => String::new(),
    };

    if sub.is_empty() {
        name.to_string()
    } else {
        format!("{name}:{sub}")
    }
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

fn format_edit_approval(mut out: String, args: &Value, obj: &Map<String, Value>) -> String {
    let file_path = obj.get("file_path").and_then(Value::as_str).unwrap_or("");
    out.push_str(&format!("  file_path: {file_path}\n"));
    for (k, v) in obj
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "file_path" | "edits"))
    {
        out.push_str(&format_kv(k, v));
    }

    let preview =
        parse_args::<edit_file::EditFileArgs>(args, edit_file::Tool::NAME).and_then(|parsed| {
            let ctx = ToolContext::from_cwd()?;
            edit_file::preview_edits(&parsed, &ctx)
        });

    match preview {
        Ok((_, original, updated, hunk_count)) => {
            out.push_str(&format!("  hunks: {hunk_count}\n"));
            append_unified_diff(&mut out, file_path, &original, &updated);
        }
        Err(e) => {
            out.push_str(&format!("\n--- WARNING ---\n{e}! Execution will fail.\n"));
            let (raw_search, raw_replace) =
                parse_args::<edit_file::EditFileArgs>(args, edit_file::Tool::NAME)
                    .map(|parsed| edit_file::fallback_hunk_preview(&parsed))
                    .unwrap_or_default();
            append_unified_diff(&mut out, file_path, &raw_search, &raw_replace);
        }
    }
    out.trim_end().to_string()
}

fn format_fs_approval(mut out: String, args: &Value, obj: &Map<String, Value>) -> String {
    for (k, v) in obj {
        out.push_str(&format_kv(k, v));
    }
    let preview = parse_args::<fs::FsArgs>(args, fs::Tool::NAME).and_then(|parsed| {
        let ctx = ToolContext::from_cwd()?;
        fs::preview_action(&parsed, &ctx)
    });
    match preview {
        Ok((summary, maybe_diff)) => {
            out.push_str(&format!("\n--- OPERATION ---\n{summary}\n"));
            if let Some((file_path, deleted_content)) = maybe_diff {
                append_unified_diff(&mut out, &file_path, &deleted_content, "");
            }
        }
        Err(e) => out.push_str(&format!("\n--- WARNING ---\n{e}! Execution will fail.\n")),
    }
    out.trim_end().to_string()
}

fn format_dotfiles_approval(mut out: String, args: &Value, obj: &Map<String, Value>) -> String {
    for (k, v) in obj
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "edits" | "content"))
    {
        out.push_str(&format_kv(k, v));
    }

    if let Ok(parsed) = parse_args::<dotfiles::DotfilesArgs>(args, dotfiles::Tool::NAME) {
        match parsed.action {
            dotfiles::DotfilesAction::Edit => match dotfiles::preview_edit(&parsed) {
                Ok((_, display, original, updated, hunk_count)) => {
                    out.push_str(&format!("  hunks: {hunk_count}\n"));
                    append_unified_diff(&mut out, &display, &original, &updated);
                }
                Err(e) => {
                    out.push_str(&format!("\n--- WARNING ---\n{e}! Execution will fail.\n"));
                }
            },
            dotfiles::DotfilesAction::Create => match dotfiles::preview_create(&parsed) {
                Ok((_, display)) => {
                    append_unified_diff(&mut out, &display, "", &parsed.content);
                }
                Err(e) => {
                    out.push_str(&format!("\n--- WARNING ---\n{e}! Execution will fail.\n"));
                }
            },
            _ => {}
        }
    }
    out.trim_end().to_string()
}

pub fn format_approval(call: &ToolCall) -> String {
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
        "edit_file" => return format_edit_approval(out, args, obj),
        "fs" => return format_fs_approval(out, args, obj),
        "dotfiles" => return format_dotfiles_approval(out, args, obj),
        _ => {}
    }

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
    if is_tool_enabled(name, config) {
        return true;
    }
    active_skill
        .and_then(|s| crate::core::skills::find_skill(config, s))
        .is_some_and(|skill| {
            skill
                .tools
                .iter()
                .map(|t| t.trim())
                .any(|t| t == "*" || t == name)
        })
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
        ) -> Result<String> {
            if !is_tool_enabled_for_turn(name, config, active_skill) {
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
    async: execute_command,
    async: fetch_url,
    blocking: fs,
    async: git,
    blocking: read_file,
    blocking: repo_map,
    blocking: search,
    async: system,
    async: web_search,
    blocking: workspace_snapshot,
}

pub async fn execute(
    call: &ToolCall,
    config: &Config,
    active_skill: Option<&str>,
    max_size: usize,
) -> (String, bool) {
    let name = &call.function.name;
    let args = &call.function.arguments;
    info!("Executing tool: {name}");
    debug!("Tool arguments: {args}");

    match dispatch_tool(name, args, config, active_skill, max_size).await {
        Ok(mut out) => {
            let truncated = out.len() > max_size;
            if truncated {
                let truncate_at = out.floor_char_boundary(max_size);
                let dropped = out.len() - truncate_at;
                out.truncate(truncate_at);
                out.push_str(&format!(
                    "\n\n[TRUNCATED: Output exceeded the {max_size} byte limit — {dropped} bytes were dropped. Content is incomplete.]"
                ));
            }
            info!("Tool '{name}' executed successfully. Truncated: {truncated}");
            debug!(
                "Tool output snippet: {}",
                out.replace('\n', " ").chars().take(200).collect::<String>()
            );
            (out, truncated)
        }
        Err(e) => {
            error!("Tool '{name}' failed: {e}");
            (format!("Error: {e}"), false)
        }
    }
}
