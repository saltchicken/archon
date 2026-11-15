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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    pub enabled: bool,
    pub items: Vec<String>,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            enabled: true,
            items: vec!["The user is running CachyOS.".into()],
        }
    }
}

pub fn clean_memory_item(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.starts_with('#') {
        return "";
    }
    let unbulleted = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("• "))
        .map_or(trimmed, str::trim);
    if unbulleted.starts_with('#') {
        return "";
    }
    unbulleted
}

impl Memory {
    pub fn new(raw_items: Vec<String>, enabled: bool) -> Self {
        let mut items: Vec<String> = Vec::new();
        for raw in raw_items {
            for line in raw.lines() {
                let cleaned = clean_memory_item(line);
                if !cleaned.is_empty()
                    && !items
                        .iter()
                        .any(|existing| existing.eq_ignore_ascii_case(cleaned))
                {
                    items.push(cleaned.to_string());
                }
            }
        }
        Self { enabled, items }
    }

    pub fn count(&self) -> usize {
        self.items
            .iter()
            .filter(|s| !clean_memory_item(s).is_empty())
            .count()
    }

    pub fn to_editor_text(&self) -> String {
        self.items
            .iter()
            .map(|s| clean_memory_item(s))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn from_editor_text(raw: &str, enabled: bool) -> Self {
        Self::new(vec![raw.to_string()], enabled)
    }

    pub fn from_markdown(raw: &str) -> Self {
        let (fm_opt, body) = crate::core::skills::split_toml_frontmatter(raw);
        let mut enabled = true;

        if let Some(fm) = fm_opt {
            #[derive(Deserialize)]
            struct MemoryFrontmatter {
                #[serde(default = "default_true")]
                enabled: bool,
            }
            fn default_true() -> bool {
                true
            }
            if let Ok(parsed) = toml::from_str::<MemoryFrontmatter>(fm) {
                enabled = parsed.enabled;
            }
        }

        Self::new(vec![body.to_string()], enabled)
    }

    pub fn to_markdown_file(&self) -> String {
        let mut out = String::new();
        if !self.enabled {
            out.push_str("+++\nenabled = false\n+++\n\n");
        }
        for item in self
            .items
            .iter()
            .map(|s| clean_memory_item(s))
            .filter(|s| !s.is_empty())
        {
            out.push_str("- ");
            out.push_str(item);
            out.push('\n');
        }
        out
    }

    pub fn format_for_prompt(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let today = chrono::Local::now().format("%A, %B %-d, %Y");
        let mut bullets = vec![format!("- Current date: {today}")];
        bullets.extend(
            self.items
                .iter()
                .map(|s| clean_memory_item(s))
                .filter(|s| !s.is_empty())
                .map(|s| format!("- {s}")),
        );
        Some(format!(
            "<memory>\nPersistent user context and environment facts (always apply):\n{}\n</memory>",
            bullets.join("\n")
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub app: AppSettings,
    #[serde(skip)]
    pub memory: Memory,
    pub execute_command: ExecuteCommandSettings,
    pub workspace_snapshot: WorkspaceSnapshotSettings,
    pub ollama: EndpointSettings,
    pub gemini: EndpointSettings,
    pub models: Vec<ModelPreset>,
    pub system_presets: Vec<Preset>,
    #[serde(skip)]
    pub skills: Vec<SkillDefinition>,
    #[serde(skip)]
    pub workspace_skills_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub default_model: String,
    pub max_msg_lines: usize,
    pub history_dir: String,
    pub skills_dir: String,
    pub enabled_tools: Vec<String>,
    pub auto_approve: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecuteCommandSettings {
    pub allowed_commands: Vec<String>,
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
                "cargo:fmt_check",
                "cargo:metadata",
                "system:*",
                "dotfiles:list",
                "dotfiles:read",
                "dotfiles:search",
                "dotfiles:validate",
                "memory:list",
                "memory:read",
                "skill:list",
                "skill:read",
            ]
            .map(String::from)
            .into(),
        }
    }
}

impl Default for ExecuteCommandSettings {
    fn default() -> Self {
        Self {
            allowed_commands: ["ls", "cat", "echo", "pwd", "grep", "head", "tail", "wc"]
                .map(String::from)
                .into(),
        }
    }
}

impl Default for WorkspaceSnapshotSettings {
    fn default() -> Self {
        Self {
            git_root: true,
            tree_only: false,
            include: vec!["**/*".into()],
            exclude: ["target/**", "node_modules/**", "Cargo.lock"]
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
            memory: Memory::default(),
            execute_command: ExecuteCommandSettings::default(),
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
                chat_timeout: 180,
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
            skills: Vec::new(),
            workspace_skills_dir: None,
            workspace: None,
        }
    }
}

pub fn archon_config_root() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("archon")
}

pub fn global_memory_path() -> PathBuf {
    archon_config_root().join("MEMORY.md")
}

pub fn workspace_memory_path(ws_dir: &Path) -> PathBuf {
    ws_dir.join("MEMORY.md")
}

pub fn load_scoped_memory_file(mem_path: &Path, is_global: bool) -> Result<(String, Memory)> {
    if !mem_path.exists() {
        let mem = if is_global {
            Memory::default()
        } else {
            Memory::new(Vec::new(), true)
        };
        return Ok((String::new(), mem));
    }

    let raw = fs::read_to_string(mem_path)
        .with_context(|| format!("Failed to read {}", mem_path.display()))?;
    let mem = Memory::from_markdown(&raw);
    Ok((raw, mem))
}

pub fn save_memory_file(mem_path: &Path, mem: &Memory) -> Result<String> {
    if let Some(parent) = mem_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }

    let rendered = mem.to_markdown_file();
    crate::tools::utils::fs::atomic_write_file(mem_path, &rendered)
        .with_context(|| format!("Failed to write {}", mem_path.display()))?;
    Ok(rendered)
}

pub fn save_global_memory(mem: &Memory) -> Result<String> {
    save_memory_file(&global_memory_path(), mem)
}

pub fn load_merged_memory(ws_dir: Option<&Path>) -> Memory {
    let global_path = global_memory_path();
    let (_, global_mem) = load_scoped_memory_file(&global_path, true)
        .unwrap_or_else(|_| (String::new(), Memory::default()));

    let Some(ws_dir) = ws_dir else {
        return global_mem;
    };
    let ws_path = workspace_memory_path(ws_dir);
    if !ws_path.exists() {
        return global_mem;
    }

    let Ok((_, ws_mem)) = load_scoped_memory_file(&ws_path, false) else {
        return global_mem;
    };

    let mut combined_items = global_mem.items;
    combined_items.extend(ws_mem.items);
    Memory::new(combined_items, global_mem.enabled && ws_mem.enabled)
}

pub fn reload_memory(current_config: &Config) -> Memory {
    let ws_dir = current_config
        .workspace_skills_dir
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| {
            current_config
                .workspace
                .as_deref()
                .map(Path::new)
                .map(workspace_config_dir_for)
        });
    load_merged_memory(ws_dir.as_deref())
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
            "# enabled_tools = [\"cargo\", \"create_file\", \"edit_file\", \"execute_command\", \"fetch_url\", \"fs\", \"git\", \"memory\", \"read_file\", \"repo_map\", \"search\", \"skill\", \"web_search\", \"workspace_snapshot\"]\n\n",
            "[workspace_snapshot]\n",
            "# git_root = true\n",
            "# tree_only = false\n",
            "# include = [\"**/*\"]\n",
            "# exclude = [\"target/**\", \"node_modules/**\", \"Cargo.lock\"]\n",
        ),
        escaped_ws = escaped_ws
    );

    fs::write(&cfg_path, default_ws_toml)
        .with_context(|| format!("Failed to write workspace config to {}", cfg_path.display()))?;

    Ok(Some(ws_dir))
}

impl Config {
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
        let merged_memory = load_merged_memory(Some(ws_dir));

        let base_figment =
            Figment::from(Serialized::defaults(Self::default())).merge(Toml::file(base_path));
        let global_files = base_figment
            .clone()
            .extract::<Self>()
            .map(|c| c.workspace_snapshot.file)
            .unwrap_or_default();

        let bound_ws = read_bound_workspace(&local_config_path)
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| dunce::canonicalize(&p).unwrap_or(p))
            })
            .map(|p| p.to_string_lossy().into_owned());

        match base_figment
            .merge(Toml::file(&local_config_path))
            .merge(Env::prefixed("ARCHON_"))
            .extract::<Self>()
        {
            Ok(mut cfg) => {
                cfg.workspace_snapshot.global_files = global_files;
                cfg.memory = merged_memory;
                cfg.skills = crate::core::skills::discover_skills(
                    &cfg.app.skills_dir,
                    Some(ws_skills_dir.as_path()),
                );
                cfg.workspace_skills_dir = Some(ws_skills_dir);
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
            Err(e) => {
                let mut def = Self::default();
                def.memory = merged_memory;
                def.skills = crate::core::skills::discover_skills(
                    &def.app.skills_dir,
                    Some(ws_skills_dir.as_path()),
                );
                def.workspace_skills_dir = Some(ws_skills_dir);
                def.workspace = bound_ws;
                (def, Some(format!("Config error (using defaults): {e}")))
            }
        }
    }
}
