mod memory;
mod provenance;

pub use memory::{
    Memory, MemoryScope, archon_config_root, clean_memory_item, global_memory_path, load_memories,
    load_scoped_memory_file, reload_memories, save_memory_file, save_scoped_memory,
    workspace_memory_path,
};
pub use provenance::{
    ConfigProvenance, ConfigSource, ToolProvenance, build_provenance, tool_list_contains,
};

use crate::core::{provider::Provider, skills::SkillDefinition};
use anyhow::{Context, Result};
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
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
    pub ollama: EndpointSettings,
    pub gemini: EndpointSettings,
    pub models: Vec<ModelPreset>,
    pub system_presets: Vec<Preset>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<ScheduleConfig>,
    #[serde(skip)]
    pub global_schedules: Vec<ScheduleConfig>,
    #[serde(skip)]
    pub skills: Vec<SkillDefinition>,
    #[serde(skip)]
    pub workspace_skills_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
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
pub struct AppSettings {
    pub default_model: String,
    pub max_msg_lines: usize,
    pub history_dir: String,
    pub skills_dir: String,
    /// Optional default compaction-guide skill name used by `/compact` when no
    /// explicit `@name` is given (e.g. `compact_skill = "compact"`).
    pub compact_skill: Option<String>,
    #[serde(alias = "include_hidden")]
    pub allow_hidden: bool,
    pub enabled_tools: Vec<String>,
    pub auto_approve: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub chat_padding_left: u16,
    pub chat_padding_right: u16,
    pub fork_icon_hover_only: bool,
    pub style_code_blocks: bool,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub prompt: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            default_model: "qwen3.8:27b".into(),
            max_msg_lines: 25,
            history_dir: "~/.local/share/archon/sessions".into(),
            skills_dir: "~/.config/archon/skills".into(),
            compact_skill: None,
            allow_hidden: false,
            enabled_tools: crate::tools::ALL_TOOL_NAMES
                .iter()
                .map(|&s| String::from(s))
                .collect(),
            auto_approve: [
                "read_file",
                "search",
                "repo_map",
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
                "dotfiles:list",
                "dotfiles:read",
                "dotfiles:search",
                "dotfiles:validate",
                "dotfiles:status",
                "dotfiles:diff",
                "memory:list",
                "memory:read",
                "notify",
                "skill:list",
                "skill:read",
                "skill:check",
            ]
            .map(String::from)
            .into(),
        }
    }
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            chat_padding_left: 2,
            chat_padding_right: 2,
            fork_icon_hover_only: true,
            style_code_blocks: true,
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
            models: vec![
                ModelPreset {
                    name: "qwen3.8:27b".into(),
                    context: 32768,
                    max_tool_output_size: 131_072,
                },
                ModelPreset {
                    name: "gemini-4-hydrogen".into(),
                    context: 1_048_576,
                    max_tool_output_size: 524_288,
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
            workspace_skills_dir: None,
            workspace: None,
            provenance: ConfigProvenance::default(),
        }
    }
}

fn read_bound_workspace(cfg_path: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(cfg_path).ok()?;
    let val: toml::Value = toml::from_str(&raw).ok()?;
    let ws = val.get("workspace")?.as_str()?;
    let expanded = expand_tilde(ws);
    Some(dunce::canonicalize(&expanded).unwrap_or(expanded))
}

pub fn workspace_config_dir_for(workspace: &Path) -> PathBuf {
    let canon = dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let base_root = archon_config_root().join("workspaces");

    let name = canon
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("root");

    let parent = canon
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty());

    let candidates = std::iter::once(name.to_string())
        .chain(parent.map(|p| format!("{p}-{name}")))
        .chain((2..1000).map(|idx| format!("{name}-{idx}")));

    for folder in candidates {
        let dir = base_root.join(folder);
        let cfg = dir.join("config.toml");
        if !cfg.exists() || read_bound_workspace(&cfg).is_none_or(|bound| bound == canon) {
            return dir;
        }
    }

    base_root.join(name)
}

pub fn ensure_workspace_config(workspace: &Path) -> Result<Option<PathBuf>> {
    let canon = dunce::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let ws_dir = workspace_config_dir_for(&canon);
    let cfg_path = ws_dir.join("config.toml");
    if cfg_path.exists() {
        return Ok(Some(ws_dir));
    }

    println!(
        "No Archon workspace configuration found for '{}'.",
        canon.display()
    );
    println!("Expected config at: {}", cfg_path.display());
    print!("Would you like to create a workspace configuration here? [y/N]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer != "y" && answer != "yes" {
        return Ok(None);
    }

    let skills_dir = ws_dir.join("skills");
    fs::create_dir_all(&skills_dir)
        .with_context(|| format!("Failed to create directory {}", skills_dir.display()))?;

    let escaped_ws = canon
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let default_ws_toml = format!(
        concat!(
            "# Archon workspace configuration\n",
            "# Overrides ~/.config/archon/config.toml for this directory.\n",
            "workspace = \"{escaped_ws}\"\n\n",
            "[app]\n",
            "# allow_hidden = false\n",
            "enabled_tools = [\"cargo\", \"create_file\", \"edit_file\", \"fetch_url\", \"fs\", \"git\", \"memory\", \"notify\", \"read_file\", \"repo_map\", \"search\", \"skill\", \"workspace_snapshot\"]\n\n",
            "[ui]\n",
            "# chat_padding_left = 2\n",
            "# chat_padding_right = 2\n",
            "# fork_icon_hover_only = true\n# style_code_blocks = true\n\n",
            "[workspace_snapshot]\n",
            "# git_root = true\n",
            "# tree_only = false\n",
            "# include = [\"**/*\"]\n",
            "# exclude = [\"target/**\", \"node_modules/**\", \"Cargo.lock\", \"!.gitignore\"]\n",
        ),
        escaped_ws = escaped_ws
    );

    fs::write(&cfg_path, default_ws_toml)
        .with_context(|| format!("Failed to write workspace config to {}", cfg_path.display()))?;

    Ok(Some(ws_dir))
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
            .is_none_or(|g| tool_list_contains(g, tool_name));
        let in_ws = self
            .provenance
            .workspace_enabled_tools
            .as_ref()
            .map(|ws| tool_list_contains(ws, tool_name));

        if let Some(ref cli) = self.provenance.cli_allowed_tools
            && in_ws.unwrap_or(in_global)
            && !tool_list_contains(cli, tool_name)
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
        let had_wildcard = self
            .app
            .enabled_tools
            .iter()
            .map(|s| s.trim())
            .any(|t| t == "*");
        if had_wildcard {
            self.app.enabled_tools = allowed
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else {
            self.app.enabled_tools.retain(|tool| {
                let t = tool.trim();
                allowed.iter().map(|a| a.trim()).any(|a| a == t)
            });
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

    pub fn workspace_config_dir(&self) -> Option<PathBuf> {
        self.workspace_skills_dir
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| {
                self.workspace
                    .as_deref()
                    .map(Path::new)
                    .map(workspace_config_dir_for)
            })
    }

    pub fn workspace_memory_path(&self) -> PathBuf {
        let ws_dir = self.workspace_config_dir().unwrap_or_else(|| {
            let cwd = std::env::current_dir().unwrap_or_default();
            workspace_config_dir_for(&cwd)
        });
        workspace_memory_path(&ws_dir)
    }

    fn find_model(&self, model: &str) -> Option<&ModelPreset> {
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
        workspace_config_dir: impl AsRef<Path>,
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

        let mem_path = global_memory_path();
        if !mem_path.exists() {
            let _ = save_memory_file(&mem_path, &Memory::default());
        }

        let ws_dir = workspace_config_dir.as_ref();
        let local_config_path = ws_dir.join("config.toml");
        let ws_skills_dir = ws_dir.join("skills");
        let (global_memory, workspace_memory) = load_memories(Some(ws_dir));

        let global_table = fs::read_to_string(base_path)
            .ok()
            .and_then(|s| toml::from_str::<toml::Table>(&s).ok());
        let ws_table = fs::read_to_string(&local_config_path)
            .ok()
            .and_then(|s| toml::from_str::<toml::Table>(&s).ok());
        let provenance = build_provenance(global_table.as_ref(), ws_table.as_ref());

        let base_figment =
            Figment::from(Serialized::defaults(Self::default())).merge(Toml::file(base_path));
        let (global_files, global_schedules) = base_figment
            .clone()
            .extract::<Self>()
            .map(|c| (c.workspace_snapshot.file, c.schedules))
            .unwrap_or_default();
        let local_schedules = Figment::from(Serialized::defaults(Self::default()))
            .merge(Toml::file(&local_config_path))
            .extract::<Self>()
            .map(|c| c.schedules)
            .unwrap_or_default();

        let bound_ws = read_bound_workspace(&local_config_path)
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| dunce::canonicalize(&p).unwrap_or(p))
            })
            .map(|p| p.to_string_lossy().into_owned());

        let (mut cfg, err) = match base_figment
            .merge(Toml::file(&local_config_path))
            .merge(Env::prefixed("ARCHON_"))
            .extract::<Self>()
        {
            Ok(mut cfg) => {
                cfg.workspace_snapshot.global_files = global_files;
                cfg.schedules = local_schedules;
                cfg.global_schedules = global_schedules;
                if cfg.workspace.as_deref().is_none_or(|s| s.trim().is_empty()) {
                    cfg.workspace = bound_ws;
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
                    workspace: bound_ws,
                    ..Self::default()
                },
                Some(format!("Config error (using defaults): {e}")),
            ),
        };

        cfg.global_memory = global_memory;
        cfg.workspace_memory = workspace_memory;
        cfg.provenance = provenance;
        cfg.skills = crate::core::skills::discover_skills(
            &cfg.app.skills_dir,
            Some(ws_skills_dir.as_path()),
        );
        cfg.workspace_skills_dir = Some(ws_skills_dir);
        (cfg, err)
    }
}
