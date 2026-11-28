mod memory;
mod provenance;

pub use memory::{
    Memory, MemoryScope, archon_config_root, clean_memory_item, global_memory_path, load_memories,
    load_scoped_memory, load_scoped_memory_file, load_workspace_memory_db, reload_memories,
    save_memory_file, save_scoped_memory, save_workspace_memory_db,
};
pub use provenance::{
    ConfigProvenance, ConfigSource, ToolProvenance, build_provenance, tool_list_contains,
};

use crate::provider::Provider;
use crate::skills::SkillDefinition;
use anyhow::{Context, Result, bail};
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

pub fn expand_tilde(path: &str) -> PathBuf {
    shellexpand::full(path)
        .unwrap_or_else(|_| shellexpand::tilde(path))
        .into_owned()
        .into()
}

pub fn format_tilde_path(path: &Path) -> String {
    if path.is_relative() {
        return path.to_string_lossy().into_owned();
    }
    if let Some(home) = dirs::home_dir()
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        let s = stripped.to_string_lossy();
        return if s.is_empty() {
            "~".into()
        } else {
            format!("~/{s}")
        };
    }
    path.to_string_lossy().into_owned()
}

/// Loads `.env` from the current directory into process environment variables
/// without overwriting variables already set in the shell environment.
pub fn load_dotenv(path: impl AsRef<Path>) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    for line in content.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() || std::env::var_os(k).is_some() {
            continue;
        }
        let unquoted = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        unsafe { std::env::set_var(k, unquoted) };
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub on_calendar: String,
    #[serde(default = "default_true")]
    pub persistent: bool,
    pub skill: Option<String>,
    pub prompt: String,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub auto_approve: bool,
    pub no_tools: bool,
    #[serde(alias = "allowed_tools", alias = "tools")]
    pub enabled_tools: Option<Vec<String>>,
    pub save_session: bool,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            on_calendar: String::new(),
            persistent: true,
            skill: None,
            prompt: String::new(),
            model: None,
            system_prompt: None,
            auto_approve: false,
            no_tools: false,
            enabled_tools: None,
            save_session: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub app: AppSettings,
    pub ui: UiSettings,
    pub dotfiles: DotfilesSettings,
    #[serde(skip)]
    pub global_memory: Memory,
    #[serde(skip)]
    pub workspace_memory: Memory,
    pub workspace_snapshot: WorkspaceSnapshotSettings,
    pub browser: BrowserSettings,
    pub ollama: EndpointSettings,
    pub gemini: EndpointSettings,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_groups: BTreeMap<String, Vec<String>>,
    pub models: Vec<ModelPreset>,
    pub system_presets: Vec<Preset>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<ScheduleConfig>,
    #[serde(skip)]
    pub global_schedules: Vec<ScheduleConfig>,
    #[serde(skip)]
    pub skills: Vec<SkillDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip)]
    pub workspace_name: Option<String>,
    #[serde(skip)]
    pub provenance: ConfigProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DotfilesSettings {
    pub allowed_dirs: Vec<String>,
    pub repo_url: String,
}

impl Default for DotfilesSettings {
    fn default() -> Self {
        Self {
            allowed_dirs: vec!["*".into()],
            repo_url: "saltchicken/dotfiles.git".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserSettings {
    /// CDP port to connect to (or spawn Chrome on if none is listening).
    pub cdp_port: u16,
    /// Profile directory used when Archon has to spawn its own CDP Chrome
    /// instance (i.e. no Chrome with `--remote-debugging-port` is listening on
    /// `cdp_port` yet). A dedicated profile keeps it from clobbering your
    /// daily-driver Chrome session. Tilde is expanded.
    pub cdp_user_data_dir: String,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            cdp_port: 9222,
            cdp_user_data_dir: "~/.config/chrome-cdp".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub default_model: String,
    pub max_msg_lines: usize,
    pub history_dir: String,
    pub skills_dir: String,
    /// Optional default compaction-guide skill name used by `/compact` when no
    /// explicit `@name` is given (e.g. `compact_skill = "compact"`).
    pub compact_skill: Option<String>,
    /// When `true`, automatically compact conversation history before sending a
    /// model turn whose estimated pre-flight context usage meets or exceeds
    /// `auto_compact_threshold * num_ctx`. Default is `false`.
    pub auto_compact: bool,
    /// Fraction of `num_ctx` at which pre-flight auto-compaction triggers
    /// (clamped to `0.10..=0.98`). Default is `0.75`.
    pub auto_compact_threshold: f64,
    /// Minimum number of model turns between automatic compactions to avoid
    /// thrashing. Default is `5`.
    pub auto_compact_cooldown_turns: u32,
    #[serde(alias = "include_hidden")]
    pub allow_hidden: bool,
    pub enabled_tools: Vec<String>,
    /// Tools whose full JSON schemas are eagerly included on every turn.
    /// Other tools in `enabled_tools` are exposed via `<deferred_tools>` and
    /// activated on demand with `load_tools`. Supports `@group` entries and `"*"`.
    pub core_tools: Vec<String>,
    /// Maximum number of active tool schemas (core + dynamically loaded) per
    /// session before oldest dynamically loaded tools are evicted FIFO. Set to
    /// `0` to disable eviction. Default is `16`.
    pub max_loaded_tools: usize,
    pub auto_approve: Vec<String>,
    /// When `true`, the `curl` and `fetch_url` tools may access private,
    /// loopback, and otherwise reserved IP addresses (e.g. `127.0.0.1`,
    /// `10.x.x.x`, `192.168.x.x`). Default is `false` (SSRF protection on).
    pub http_allow_private: bool,
    /// Extra directories (absolute, or relative to CWD) that `read_file` and
    /// `search` are additionally allowed to read, beyond the workspace root.
    /// Entries that do not resolve to an existing directory are ignored, but
    /// listed in rejection messages (e.g. `~/.cargo/registry/src` does not
    /// exist until the first dependency fetch). Default is empty.
    ///
    /// Example: `read_roots = ["~/.cargo/registry/src"]`
    pub read_roots: Vec<String>,

    /// Fine-grained CIDR ranges to allow (e.g. `["192.168.1.0/24", "10.0.0.0/8"]`).
    /// When non-empty, only IPs within these ranges are permitted;
    /// `http_allow_private` is ignored.
    pub http_allowed_cidrs: Vec<String>,
    /// When `true`, automatically continue into the next model turn when a
    /// response finishes with no tool calls while active session plan tasks remain.
    /// Default is `false`.
    #[serde(alias = "task_autoloop")]
    pub plan_autoloop: bool,
    /// When `true`, `plan_autoloop` ignores `blocked` tasks and keeps running
    /// through the plan instead of pausing for user input. Default is `false`.
    #[serde(alias = "task_autoloop_ignore_blocked")]
    pub plan_autoloop_ignore_blocked: bool,
    /// Maximum number of consecutive auto-plan-loop turns before the loop stops.
    /// Default is `25`.
    #[serde(alias = "task_autoloop_max_turns")]
    pub plan_autoloop_max_turns: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub chat_padding_left: u16,
    pub chat_padding_right: u16,
    pub fork_icon_hover_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceSnapshotSettings {
    pub git_root: bool,
    pub tree_only: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_in_tree: Vec<String>,
    pub file: Vec<String>,
    #[serde(skip)]
    pub global_files: Vec<String>,
    pub max_size: u64,
    pub max_files: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointSettings {
    pub api_key: String,
    pub base_url: String,
    pub chat_timeout: u64,
    pub status_timeout: u64,
}

impl EndpointSettings {
    pub fn resolve_api_key(&self) -> Option<String> {
        [
            Some(self.api_key.clone()),
            std::env::var("GEMINI_API_KEY").ok(),
            std::env::var("GOOGLE_API_KEY").ok(),
        ]
        .into_iter()
        .flatten()
        .map(|k| k.trim().to_string())
        .find(|k| !k.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPreset {
    pub name: String,
    pub context: u32,
    pub max_tool_output_size: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_batch: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub prompt: String,
}

impl AppSettings {
    /// Returns `auto_compact_threshold` clamped to `[0.10, 0.98]`, falling back
    /// to `0.75` if non-finite.
    pub fn effective_auto_compact_threshold(&self) -> f64 {
        if self.auto_compact_threshold.is_finite() {
            self.auto_compact_threshold.clamp(0.10, 0.98)
        } else {
            0.75
        }
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            default_model: "qwen3.8:27b".into(),
            max_msg_lines: 25,
            history_dir: "~/.local/share/archon/sessions".into(),
            skills_dir: "~/.config/archon/skills".into(),
            compact_skill: None,
            auto_compact: false,
            auto_compact_threshold: 0.75,
            auto_compact_cooldown_turns: 5,
            allow_hidden: false,
            enabled_tools: crate::tools::ALL_TOOL_NAMES
                .iter()
                .map(|&s| String::from(s))
                .collect(),
            core_tools: [
                "read_file",
                "edit_file",
                "create_file",
                "search",
                "repo_map",
                "plan",
            ]
            .map(String::from)
            .into(),
            max_loaded_tools: 16,
            auto_approve: [
                "load_tools",
                "read_file",
                "search",
                "repo_map",
                "github_repo:list",
                "github_repo:read",
                "github_repo:search",
                "workspace_snapshot",
                "fs:list",
                "git:status",
                "git:diff",
                "git:log",
                "git:show",
                "git:blame",
                "cargo:check",
                "cargo:clippy",
                "cargo:test",
                "cargo:fmt_check",
                "cargo:metadata",
                "system:*",
                "process:list",
                "process:output",
                "process:poll",
                "dotfiles:list",
                "dotfiles:read",
                "dotfiles:search",
                "dotfiles:validate",
                "dotfiles:status",
                "dotfiles:diff",
                "memory:list",
                "memory:read",
                "notify",
                "session:list",
                "session:search",
                "session:read",
                "session:stats",
                "skill:list",
                "skill:read",
                "skill:check",
                "schedule:list",
                "schedule:logs",
                "sqlite:tables",
                "sqlite:schema",
                "sqlite:query",
                "plan:*",
                "todo:*",
            ]
            .map(String::from)
            .into(),
            http_allow_private: false,
            read_roots: Vec::new(),
            http_allowed_cidrs: Vec::new(),
            plan_autoloop: false,
            plan_autoloop_ignore_blocked: false,
            plan_autoloop_max_turns: 25,
        }
    }
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            chat_padding_left: 2,
            chat_padding_right: 2,
            fork_icon_hover_only: true,
        }
    }
}

impl Default for WorkspaceSnapshotSettings {
    fn default() -> Self {
        Self {
            git_root: true,
            tree_only: false,
            include: vec!["**/*".into()],
            exclude: ["target/**", "node_modules/**", "Cargo.lock", "!.gitignore"]
                .map(String::from)
                .into(),
            include_in_tree: vec!["Cargo.lock".into()],
            file: Vec::new(),
            global_files: Vec::new(),
            max_size: 1_048_576,
            max_files: 10_000,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let preset = |name: &str, prompt: &str| Preset {
            name: name.into(),
            prompt: prompt.into(),
        };
        Self {
            app: AppSettings::default(),
            ui: UiSettings::default(),
            dotfiles: DotfilesSettings::default(),
            global_memory: Memory::default(),
            workspace_memory: Memory::empty(),
            workspace_snapshot: WorkspaceSnapshotSettings::default(),
            browser: BrowserSettings::default(),
            ollama: EndpointSettings {
                api_key: String::new(),
                base_url: "http://localhost:11434".into(),
                chat_timeout: 180,
                status_timeout: 2,
            },
            gemini: EndpointSettings {
                api_key: String::new(),
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                chat_timeout: 600,
                status_timeout: 5,
            },
            tool_groups: BTreeMap::new(),
            models: vec![
                ModelPreset {
                    name: "qwen3.8:27b".into(),
                    context: 32768,
                    max_tool_output_size: 131_072,
                    num_predict: None,
                    temperature: None,
                    repeat_penalty: None,
                    num_batch: None,
                },
                ModelPreset {
                    name: "gemini-4-hydrogen".into(),
                    context: 1_048_576,
                    max_tool_output_size: 524_288,
                    num_predict: None,
                    temperature: None,
                    repeat_penalty: None,
                    num_batch: None,
                },
            ],
            system_presets: vec![
                preset("None", ""),
                preset(
                    "Helpful Assistant",
                    "You are a highly capable and helpful AI visual assistant. Provide concise and accurate answers.",
                ),
                preset(
                    "JSON Only",
                    "You are a data extractor. Only output valid JSON. Do not include markdown or explanations.",
                ),
            ],
            schedules: Vec::new(),
            global_schedules: Vec::new(),
            skills: Vec::new(),
            workspace: None,
            workspace_name: None,
            provenance: ConfigProvenance::default(),
        }
    }
}

const DEFAULT_WORKSPACES_TOML: &str = concat!(
    "# Archon per-project workspace configuration\n",
    "# Declare project-specific overrides to ~/.config/archon/config.toml here.\n",
    "# Directories without a matching entry use the global config.toml defaults.\n",
    "#\n",
    "# Example:\n",
    "# [my-project]\n",
    "# workspace = \"~/Projects/my-project\"\n",
    "#\n",
    "# [my-project.app]\n",
    "# allow_hidden = false\n",
    "# enabled_tools = [\"read_file\", \"edit_file\", \"create_file\", \"search\", \"repo_map\", \"plan\"]\n",
    "#\n",
    "# [my-project.workspace_snapshot]\n",
    "# include = [\"**/*.rs\", \"**/*.toml\"]\n",
    "# exclude = [\"target/**\", \"Cargo.lock\"]\n",
    "#\n",
    "# [[my-project.schedules]]\n",
    "# name = \"daily-check\"\n",
    "# on_calendar = \"daily\"\n",
    "# prompt = \"Summarize recent changes\"\n",
);

pub fn workspaces_config_path() -> PathBuf {
    archon_config_root().join("workspaces.toml")
}

#[derive(Debug, Clone)]
struct WorkspaceEntry {
    name: Option<String>,
    path: PathBuf,
    table: toml::Table,
}

fn looks_like_path(s: &str) -> bool {
    let t = s.trim();
    t.starts_with('/')
        || t.starts_with('~')
        || t.starts_with("./")
        || t.starts_with("../")
        || t == "."
}

fn canonicalize_workspace_path(raw: &str) -> PathBuf {
    let expanded = expand_tilde(raw.trim());
    dunce::canonicalize(&expanded).unwrap_or(expanded)
}

fn build_workspace_entry(key_name: Option<&str>, mut tbl: toml::Table) -> Result<WorkspaceEntry> {
    let explicit_name = tbl
        .remove("name")
        .and_then(|v| v.as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty());

    let raw_ws = tbl
        .get("workspace")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            tbl.remove("path")
                .and_then(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            key_name
                .filter(|k| looks_like_path(k))
                .map(|k| k.trim().to_string())
        })
        .with_context(|| match key_name {
            Some(k) => {
                format!(
                    "Workspace entry '[{k}]' in workspaces.toml is missing `workspace = \"...\"`"
                )
            }
            None => "A `[[workspaces]]` entry in workspaces.toml is missing `workspace = \"...\"`"
                .to_string(),
        })?;

    tbl.insert("workspace".to_string(), toml::Value::String(raw_ws.clone()));

    let name = explicit_name.or_else(|| {
        key_name
            .map(str::trim)
            .filter(|k| !k.is_empty() && !looks_like_path(k))
            .map(String::from)
    });

    Ok(WorkspaceEntry {
        name,
        path: canonicalize_workspace_path(&raw_ws),
        table: tbl,
    })
}

fn parse_workspaces_toml(raw: &str) -> Result<Vec<WorkspaceEntry>> {
    let root: toml::Table = toml::from_str(raw).context("Failed to parse workspaces.toml")?;
    let mut entries = Vec::new();

    for (key, val) in root {
        if key == "workspaces" {
            match val {
                toml::Value::Array(arr) => {
                    for item in arr {
                        let toml::Value::Table(tbl) = item else {
                            bail!("Each `[[workspaces]]` item in workspaces.toml must be a table");
                        };
                        entries.push(build_workspace_entry(None, tbl)?);
                    }
                }
                toml::Value::Table(tbl) => {
                    if tbl.get("workspace").and_then(toml::Value::as_str).is_some()
                        || tbl.get("path").and_then(toml::Value::as_str).is_some()
                    {
                        entries.push(build_workspace_entry(Some(&key), tbl)?);
                    } else {
                        for (sub_key, sub_val) in tbl {
                            let toml::Value::Table(sub_tbl) = sub_val else {
                                bail!(
                                    "Entry 'workspaces.{sub_key}' in workspaces.toml must be a table"
                                );
                            };
                            entries.push(build_workspace_entry(Some(&sub_key), sub_tbl)?);
                        }
                    }
                }
                _ => {
                    bail!(
                        "`workspaces` in workspaces.toml must be an array of tables (`[[workspaces]]`) or a table of project configs"
                    );
                }
            }
        } else {
            let toml::Value::Table(tbl) = val else {
                bail!("Top-level entry '{key}' in workspaces.toml must be a table (e.g. [{key}])");
            };
            entries.push(build_workspace_entry(Some(&key), tbl)?);
        }
    }

    Ok(entries)
}

impl Config {
    pub fn set_cli_model_override(&mut self) {
        self.provenance.default_model = ConfigSource::Cli;
    }

    pub fn tool_provenance(&self, tool_name: &str) -> ToolProvenance {
        let has_global = self.provenance.global_enabled_tools.is_some();
        let in_global = self
            .provenance
            .global_enabled_tools
            .as_ref()
            .is_none_or(|g| tool_list_contains(g, tool_name, &self.tool_groups));
        let in_ws = self
            .provenance
            .workspace_enabled_tools
            .as_ref()
            .map(|ws| tool_list_contains(ws, tool_name, &self.tool_groups));

        if let Some(ref cli) = self.provenance.cli_allowed_tools
            && in_ws.unwrap_or(in_global)
            && !tool_list_contains(cli, tool_name, &self.tool_groups)
        {
            return ToolProvenance::CliDisabled;
        }

        match (has_global, in_global, in_ws) {
            (true, true, Some(true)) => ToolProvenance::GlobalAndWorkspaceEnabled,
            (false, true, Some(true)) => ToolProvenance::WorkspaceEnabled,
            (_, false, Some(true)) => ToolProvenance::WorkspaceAdded,
            (_, true, Some(false)) => ToolProvenance::WorkspaceRemoved,
            (_, false, Some(false)) => ToolProvenance::GlobalAndWorkspaceDisabled,
            (true, true, None) => ToolProvenance::GlobalEnabled,
            (false, true, None) => ToolProvenance::DefaultEnabled,
            (_, false, None) => ToolProvenance::GlobalDisabled,
        }
    }

    /// Validates `--allowed-tools` entries against built-in tools and both
    /// built-in and custom `[tool_groups]`.
    pub fn validate_allowed_tools(&self, allowed: &[String]) -> Result<()> {
        if let Err(unknown) = crate::tools::expand_tool_spec_checked(allowed, &self.tool_groups) {
            bail!(
                "Unknown tool/group(s) in --allowed-tools: {}. {}",
                unknown.join(", "),
                crate::tools::format_valid_tools_and_groups(&self.tool_groups)
            );
        }
        Ok(())
    }

    /// Returns `@group` entries in `enabled_tools` or `core_tools` that do not
    /// resolve against the current `[tool_groups]` definitions.
    ///
    /// An empty result means all group references are valid (or no groups are used).
    pub fn unresolved_group_refs(&self) -> Vec<String> {
        let mut unknown = Vec::new();
        for list in [&self.app.enabled_tools, &self.app.core_tools] {
            if let Err(entries) = crate::tools::expand_tool_spec_checked(list, &self.tool_groups) {
                for e in entries {
                    if e.starts_with('@') && !unknown.iter().any(|u| u == &e) {
                        unknown.push(e);
                    }
                }
            }
        }
        unknown
    }

    pub fn restrict_enabled_tools(&mut self, allowed: &[String]) {
        self.provenance.cli_allowed_tools = Some(
            allowed
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        );
        self.provenance.enabled_tools = ConfigSource::Cli;
        let allow_all = allowed.iter().map(|s| s.trim()).any(|t| t == "*");
        if allow_all {
            return;
        }
        let expanded_allowed = crate::tools::expand_tool_list(allowed, &self.tool_groups);
        let expanded_enabled =
            crate::tools::expand_tool_list(&self.app.enabled_tools, &self.tool_groups);
        let had_wildcard = expanded_enabled.iter().any(|t| t == "*");
        if had_wildcard {
            self.app.enabled_tools = expanded_allowed;
        } else {
            self.app.enabled_tools = expanded_enabled
                .into_iter()
                .filter(|tool| expanded_allowed.iter().any(|a| a == tool))
                .collect();
        }
    }

    pub fn scoped_memory(&self, scope: MemoryScope) -> &Memory {
        match scope {
            MemoryScope::Global => &self.global_memory,
            MemoryScope::Workspace => &self.workspace_memory,
        }
    }

    pub fn set_scoped_memory(&mut self, scope: MemoryScope, memory: Memory) {
        match scope {
            MemoryScope::Global => self.global_memory = memory,
            MemoryScope::Workspace => self.workspace_memory = memory,
        }
    }

    pub fn effective_memory(&self) -> Memory {
        let mut combined_items = Vec::new();
        if self.global_memory.enabled {
            combined_items.extend(self.global_memory.items.iter().cloned());
        }
        if self.workspace_memory.enabled {
            combined_items.extend(self.workspace_memory.items.iter().cloned());
        }
        let enabled = self.global_memory.enabled
            || (self.workspace_memory.enabled && self.workspace_memory.count() > 0);
        Memory::new(combined_items, enabled)
    }

    pub fn workspace_key(&self) -> String {
        self.workspace
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(expand_tilde)
            .or_else(|| std::env::current_dir().ok())
            .map(|p| {
                dunce::canonicalize(&p)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default()
    }

    pub fn find_model(&self, model: &str) -> Option<&ModelPreset> {
        self.models.iter().find(|m| m.name == model)
    }

    pub fn startup_model(&self) -> String {
        let def = self.app.default_model.trim();
        if !def.is_empty() {
            return def.to_string();
        }
        self.models
            .first()
            .map_or_else(|| "qwen3.8:27b".into(), |m| m.name.clone())
    }

    pub fn context_for_model(&self, model: &str) -> u32 {
        self.find_model(model)
            .map_or_else(|| Provider::from_model(model).fallback_ctx(), |m| m.context)
            .max(512)
    }

    pub fn max_tool_output_for_model(&self, model: &str) -> usize {
        self.find_model(model)
            .map_or_else(
                || Provider::from_model(model).fallback_max_tool_output(),
                |m| m.max_tool_output_size,
            )
            .max(1024)
    }

    pub fn load(
        base_path: impl AsRef<Path>,
        workspaces_path: impl AsRef<Path>,
        workspace_dir: impl AsRef<Path>,
    ) -> (Self, Option<String>) {
        let base_path = base_path.as_ref();
        if !base_path.exists() {
            if let Some(parent) = base_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(toml_str) = toml::to_string_pretty(&Self::default()) {
                let _ = fs::write(base_path, toml_str);
            }
        }

        let workspaces_path = workspaces_path.as_ref();
        if !workspaces_path.exists() {
            if let Some(parent) = workspaces_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(workspaces_path, DEFAULT_WORKSPACES_TOML);
        }

        let mem_path = global_memory_path();
        if !mem_path.exists() {
            let _ = save_memory_file(&mem_path, &Memory::default());
        }

        let canon_ws = dunce::canonicalize(workspace_dir.as_ref())
            .unwrap_or_else(|_| workspace_dir.as_ref().to_path_buf());
        let canon_ws_str = canon_ws.to_string_lossy().into_owned();

        let global_table = fs::read_to_string(base_path)
            .ok()
            .and_then(|s| toml::from_str::<toml::Table>(&s).ok());

        let mut ws_parse_err: Option<String> = None;
        let matched_ws = match fs::read_to_string(workspaces_path) {
            Ok(raw) => match parse_workspaces_toml(&raw) {
                Ok(entries) => entries.into_iter().find(|e| e.path == canon_ws),
                Err(e) => {
                    ws_parse_err = Some(format!(
                        "workspaces.toml error (ignoring workspace overrides): {e}"
                    ));
                    None
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                ws_parse_err = Some(format!("Failed to read workspaces.toml: {e}"));
                None
            }
        };

        let (ws_name, ws_table, ws_toml_str) = match matched_ws {
            Some(entry) => {
                let serialized = match toml::to_string(&entry.table) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        ws_parse_err = Some(format!(
                            "Failed to serialize workspace entry for '{}': {e}",
                            canon_ws.display()
                        ));
                        None
                    }
                };
                (entry.name, Some(entry.table), serialized)
            }
            None => (None, None, None),
        };

        let provenance = build_provenance(global_table.as_ref(), ws_table.as_ref());

        let base_figment =
            Figment::from(Serialized::defaults(Self::default())).merge(Toml::file(base_path));
        let (global_files, global_schedules) = base_figment
            .clone()
            .extract::<Self>()
            .map(|c| (c.workspace_snapshot.file, c.schedules))
            .unwrap_or_default();
        let local_schedules = ws_toml_str
            .as_deref()
            .and_then(|s| {
                Figment::from(Serialized::defaults(Self::default()))
                    .merge(Toml::string(s))
                    .extract::<Self>()
                    .ok()
            })
            .map(|c| c.schedules)
            .unwrap_or_default();

        let mut merged_figment = base_figment;
        if let Some(ref s) = ws_toml_str {
            merged_figment = merged_figment.merge(Toml::string(s));
        }

        let (mut cfg, figment_err) = match merged_figment
            .merge(Env::prefixed("ARCHON_"))
            .extract::<Self>()
        {
            Ok(mut cfg) => {
                cfg.workspace_snapshot.global_files = global_files;
                cfg.schedules = local_schedules;
                cfg.global_schedules = global_schedules;
                cfg.workspace_name = ws_name;
                if cfg.workspace.as_deref().is_none_or(|s| s.trim().is_empty()) {
                    cfg.workspace = Some(canon_ws_str);
                } else if let Some(ref ws) = cfg.workspace {
                    let expanded = expand_tilde(ws);
                    cfg.workspace = Some(
                        dunce::canonicalize(&expanded)
                            .unwrap_or(expanded)
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                (cfg, None)
            }
            Err(e) => (
                Self {
                    workspace: Some(canon_ws_str),
                    workspace_name: ws_name,
                    ..Self::default()
                },
                Some(format!("Config error (using defaults): {e}")),
            ),
        };

        let err = match (figment_err, ws_parse_err) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        let ws_key = cfg.workspace_key();
        let (global_memory, workspace_memory) = load_memories(&cfg.app.history_dir, &ws_key);
        cfg.global_memory = global_memory;
        cfg.workspace_memory = workspace_memory;
        cfg.provenance = provenance;
        cfg.skills =
            crate::skills::discover_skills_with_groups(&cfg.app.skills_dir, &cfg.tool_groups);
        (cfg, err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_settings_cdp_user_data_dir_default() {
        let settings = BrowserSettings::default();
        assert_eq!(settings.cdp_port, 9222);
        assert_eq!(settings.cdp_user_data_dir, "~/.config/chrome-cdp");

        // Missing key falls back to the default via #[serde(default)].
        let toml_str = "cdp_port = 9223\n";
        let deserialized: BrowserSettings = toml::from_str(toml_str).unwrap();
        assert_eq!(deserialized.cdp_port, 9223);
        assert_eq!(deserialized.cdp_user_data_dir, "~/.config/chrome-cdp");
    }

    #[test]
    fn browser_settings_cdp_user_data_dir_overridable() {
        let toml_str = "cdp_user_data_dir = \"/tmp/my-chrome-profile\"\n";
        let settings: BrowserSettings = toml::from_str(toml_str).unwrap();
        assert_eq!(settings.cdp_user_data_dir, "/tmp/my-chrome-profile");
    }

    #[test]
    fn auto_compact_settings_defaults_and_clamping() {
        let mut settings = AppSettings::default();
        assert!(!settings.auto_compact);
        assert!((settings.auto_compact_threshold - 0.75).abs() < 1e-9);
        assert_eq!(settings.auto_compact_cooldown_turns, 5);
        assert!((settings.effective_auto_compact_threshold() - 0.75).abs() < 1e-9);

        settings.auto_compact_threshold = 0.01;
        assert!((settings.effective_auto_compact_threshold() - 0.10).abs() < 1e-9);

        settings.auto_compact_threshold = 1.5;
        assert!((settings.effective_auto_compact_threshold() - 0.98).abs() < 1e-9);

        settings.auto_compact_threshold = f64::NAN;
        assert!((settings.effective_auto_compact_threshold() - 0.75).abs() < 1e-9);

        let partial: AppSettings = toml::from_str("auto_compact = true\n").unwrap();
        assert!(partial.auto_compact);
        assert!((partial.auto_compact_threshold - 0.75).abs() < 1e-9);
        assert_eq!(partial.auto_compact_cooldown_turns, 5);
    }

    #[test]
    fn auto_compact_provenance_tracks_overrides_across_keys() {
        let g: toml::Table = toml::from_str("[app]\nauto_compact = true\n").unwrap();
        let ws_same: toml::Table = toml::from_str("[app]\nauto_compact_threshold = 0.8\n").unwrap();
        let ws_override: toml::Table = toml::from_str("[app]\nauto_compact = false\n").unwrap();

        let p_def = provenance::build_provenance(None, None);
        assert_eq!(p_def.auto_compact, ConfigSource::Default);

        let p_global = provenance::build_provenance(Some(&g), None);
        assert_eq!(p_global.auto_compact, ConfigSource::Global);

        let p_ws = provenance::build_provenance(Some(&g), Some(&ws_same));
        assert_eq!(p_ws.auto_compact, ConfigSource::Workspace);

        let p_ov = provenance::build_provenance(Some(&g), Some(&ws_override));
        assert_eq!(p_ov.auto_compact, ConfigSource::WorkspaceOverridesGlobal);
    }

    #[test]
    fn unresolved_group_refs_detects_missing_groups() {
        let mut cfg = Config::default();
        // No [tool_groups] defined — any @group reference is unresolvable.
        cfg.app.enabled_tools = vec!["@code".into(), "read_file".into()];
        cfg.app.core_tools = vec!["@rust".into()];

        let unresolved = cfg.unresolved_group_refs();
        assert_eq!(unresolved, vec!["@code".to_string(), "@rust".to_string()]);

        // Defining the groups makes them resolve cleanly.
        cfg.tool_groups
            .insert("code".into(), vec!["read_file".into()]);
        cfg.tool_groups.insert("rust".into(), vec!["cargo".into()]);
        assert!(cfg.unresolved_group_refs().is_empty());
    }

    #[test]
    fn unresolved_group_refs_ignores_wildcard_and_plain_tools() {
        let mut cfg = Config::default();
        cfg.app.enabled_tools = vec!["*".into(), "read_file".into(), "search".into()];
        cfg.app.core_tools = vec!["edit_file".into()];
        assert!(cfg.unresolved_group_refs().is_empty());
    }

    #[test]
    fn parse_workspaces_toml_supports_named_array_nested_and_path_keyed_entries() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let dir_c = tempfile::tempdir().unwrap();
        let dir_d = tempfile::tempdir().unwrap();

        let a = dunce::canonicalize(dir_a.path()).unwrap();
        let b = dunce::canonicalize(dir_b.path()).unwrap();
        let c = dunce::canonicalize(dir_c.path()).unwrap();
        let d = dunce::canonicalize(dir_d.path()).unwrap();

        // 1. Top-level named project table + path-keyed table.
        let toml_named = format!(
            r#"
            [proj-a]
            workspace = "{}"
            [proj-a.app]
            allow_hidden = true

            ["{}"]
            [ "{}" .ui ]
            chat_padding_left = 6
            "#,
            a.display(),
            d.display(),
            d.display(),
        );
        let entries = parse_workspaces_toml(&toml_named).unwrap();
        assert_eq!(entries.len(), 2);
        let ea = entries.iter().find(|e| e.path == a).unwrap();
        assert_eq!(ea.name.as_deref(), Some("proj-a"));
        assert_eq!(ea.table["app"]["allow_hidden"], toml::Value::Boolean(true));
        let ed = entries.iter().find(|e| e.path == d).unwrap();
        assert_eq!(ed.name, None);
        assert_eq!(ed.table["ui"]["chat_padding_left"], toml::Value::Integer(6));

        // 2. `[[workspaces]]` array of tables.
        let toml_array = format!(
            r#"
            [[workspaces]]
            name = "custom-b"
            workspace = "{}"

            [[workspaces]]
            workspace = "{}"
            "#,
            b.display(),
            c.display(),
        );
        let entries_arr = parse_workspaces_toml(&toml_array).unwrap();
        assert_eq!(entries_arr.len(), 2);
        assert_eq!(entries_arr[0].name.as_deref(), Some("custom-b"));
        assert_eq!(entries_arr[0].path, b);
        assert!(!entries_arr[0].table.contains_key("name"));
        assert_eq!(entries_arr[1].name, None);
        assert_eq!(entries_arr[1].path, c);

        // 3. `[workspaces.<name>]` nested tables.
        let toml_nested = format!(
            r#"
            [workspaces.proj-b]
            workspace = "{}"
            "#,
            b.display(),
        );
        let entries_nested = parse_workspaces_toml(&toml_nested).unwrap();
        assert_eq!(entries_nested.len(), 1);
        assert_eq!(entries_nested[0].name.as_deref(), Some("proj-b"));
        assert_eq!(entries_nested[0].path, b);
    }

    #[test]
    fn parse_workspaces_toml_rejects_missing_workspace_path() {
        let err =
            parse_workspaces_toml("[broken]\n[broken.app]\nallow_hidden = true\n").unwrap_err();
        assert!(err.to_string().contains("missing `workspace"), "{err}");
    }
}
