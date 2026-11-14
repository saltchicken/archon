use crate::core::provider::Provider;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn expand_tilde(path: &str) -> PathBuf {
    shellexpand::full(path)
        .unwrap_or_else(|_| shellexpand::tilde(path))
        .into_owned()
        .into()
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub app: AppSettings,
    pub workspace_snapshot: WorkspaceSnapshotSettings,
    pub ollama: EndpointSettings,
    pub gemini: EndpointSettings,
    pub models: Vec<ModelPreset>,
    pub system_presets: Vec<Preset>,
    pub skills: Vec<SkillDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub default_model: String,
    pub sub_agent_model: String,
    pub max_msg_lines: usize,
    pub history_dir: String,
    pub skills_dir: String,
    pub default_skills: Vec<String>,
    pub enabled_tools: Vec<String>,
    pub allowed_commands: Vec<String>,
    pub auto_approve: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceSnapshotSettings {
    pub git_root: bool,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub command: String,
    pub tools: Vec<String>,
    pub auto_approve: Vec<String>,
    pub prompt: String,
    #[serde(skip)]
    pub dir_path: Option<PathBuf>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            default_model: "qwen3.8:27b".into(),
            sub_agent_model: "qwen3.8:27b".into(),
            max_msg_lines: 25,
            history_dir: "~/.local/share/archon/sessions".into(),
            skills_dir: "~/.config/archon/skills".into(),
            default_skills: Vec::new(),
            enabled_tools: crate::tools::ALL_TOOL_NAMES
                .iter()
                .map(|&s| String::from(s))
                .collect(),
            allowed_commands: ["ls", "cat", "echo", "pwd", "grep", "head", "tail", "wc"]
                .map(String::from)
                .into(),
            auto_approve: [
                "read_file",
                "search",
                "repo_map",
                "workspace_snapshot",
                "delegate_task",
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
            ]
            .map(String::from)
            .into(),
        }
    }
}

impl Default for WorkspaceSnapshotSettings {
    fn default() -> Self {
        Self {
            git_root: true,
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
            skills: vec![
                SkillDefinition {
                    name: "codebase-architect".into(),
                    description: "Navigate and modify workspaces using AST repo_map, targeted reads, atomic edits, and sub-agent delegation.".into(),
                    command: "architect".into(),
                    tools: vec![
                        "repo_map".into(),
                        "search".into(),
                        "workspace_snapshot".into(),
                        "read_file".into(),
                        "edit_file".into(),
                        "create_file".into(),
                        "cargo".into(),
                        "delegate_task".into(),
                    ],
                    auto_approve: vec![
                        "repo_map".into(),
                        "search".into(),
                        "workspace_snapshot".into(),
                        "read_file".into(),
                        "cargo:check".into(),
                        "cargo:clippy".into(),
                        "cargo:fmt_check".into(),
                        "delegate_task".into(),
                    ],
                    prompt: "Navigate and modify the workspace efficiently:\n1. Architecture & Symbols (`repo_map`): Start with `repo_map` (`action: \"map\"`) to inspect the PageRank-ranked AST symbol graph. Use `action: \"definition\"` to read a symbol's exact implementation by name without needing file paths or line numbers, and `action: \"references\"` to find all AST call sites grouped by enclosing scope.\n2. Text & Configs (`search` & `workspace_snapshot`): Use `search` for regex/literal strings, error messages, or comments (`files_with_matches: true` for broad surveys). Use `workspace_snapshot` (`tree_only: true`) for directory structure, or scoped with `target_path`/`include` to inspect manifests (`Cargo.toml`) and small modules.\n3. Targeted Reads & Edits: Use `read_file` with `start_line`/`end_line` for non-code files or surrounding context, apply minimal atomic hunks via `edit_file`, and delegate routine formatting/lint verification (`cargo fmt`, `cargo clippy`, `cargo check`) to the local model via `delegate_task`.".into(),
                    dir_path: None,
                },
                SkillDefinition {
                    name: "cachyos-optimizer".into(),
                    description: "Diagnose and optimize CachyOS kernel, sched-ext (scx), systemd services/logs, Btrfs, packages, Hyprland, and dotfiles.".into(),
                    command: "cachyos".into(),
                    tools: vec!["system".into(), "dotfiles".into()],
                    auto_approve: vec![
                        "system:*".into(),
                        "dotfiles:list".into(),
                        "dotfiles:read".into(),
                        "dotfiles:search".into(),
                        "dotfiles:validate".into(),
                    ],
                    prompt: "Operate as a CachyOS & Arch Linux system optimization and dotfiles architect:\n1. System Telemetry (`system`): Start with `system` (`action: \"overview\"`) to inspect the CachyOS kernel, sched-ext (`scx`) scheduler, CPU governor/EPP, PSI resource pressure, ZRAM, and thermals. Use `action: \"journal\"` and `action: \"services\"` (`failed`, `blame`, `timers`) to diagnose errors and boot bottlenecks. Use `action: \"btrfs\"` for mount options/compression/usage, `action: \"packages\"` for `paru`/`pacman` orphans/foreign packages/modified configs, `action: \"hyprland\"` for compositor state, and `action: \"fish\"` (`recent`, `frequent`, `abbreviations`) to optimize shell workflows.\n2. Dotfiles Management (`dotfiles`): Use `dotfiles` (`list`, `read`, `search`) to inspect `~/.config` (`hypr/`, `fish/`, `waybar/`, etc.). Apply targeted atomic edits with `dotfiles` (`action: \"edit\"` or `\"create\"`) and verify syntax with `action: \"validate\"`.".into(),
                    dir_path: None,
                },
                SkillDefinition {
                    name: "git-commit".into(),
                    description: "Inspect repository status/diffs, stage relevant changes, and create a conventional commit.".into(),
                    command: "commit".into(),
                    tools: vec!["git".into()],
                    auto_approve: vec!["git:status".into(), "git:diff".into(), "git:log".into()],
                    prompt: "Inspect the repository using the `git` tool (`status`, `diff` with `target: \"--staged\"`, and `log` with `limit: 5`). If no changes are staged, inspect the unstaged `diff` and stage the relevant files using `git` (`action: \"add\"`). Then commit the staged changes using `git` (`action: \"commit\"`) with a clear, conventional commit message (concise summary line followed by bullet points explaining key changes).".into(),
                    dir_path: None,
                },
                SkillDefinition {
                    name: "describe-image".into(),
                    description: "Describe attached image(s) in comprehensive visual and technical detail.".into(),
                    command: "describe".into(),
                    tools: Vec::new(),
                    auto_approve: Vec::new(),
                    prompt: "Describe this image in detail. {{args}}".into(),
                    dir_path: None,
                },
                SkillDefinition {
                    name: "extract-text".into(),
                    description: "Extract all visible text from attached image(s) verbatim.".into(),
                    command: "extract".into(),
                    tools: Vec::new(),
                    auto_approve: Vec::new(),
                    prompt: "Extract all the text from this image. {{args}}".into(),
                    dir_path: None,
                },
            ],
        }
    }
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
        local_path: impl AsRef<Path>,
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

        let base_figment =
            Figment::from(Serialized::defaults(Self::default())).merge(Toml::file(base_path));
        let global_files = base_figment
            .clone()
            .extract::<Self>()
            .map(|c| c.workspace_snapshot.file)
            .unwrap_or_default();

        match base_figment
            .merge(Toml::file(local_path.as_ref()))
            .merge(Env::prefixed("ARCHON_"))
            .extract::<Self>()
        {
            Ok(mut cfg) => {
                cfg.workspace_snapshot.global_files = global_files;
                let skills_dir = cfg.app.skills_dir.clone();
                cfg.skills = crate::core::skills::discover_skills(cfg.skills, &skills_dir);
                (cfg, None)
            }
            Err(e) => {
                let mut def = Self::default();
                let skills_dir = def.app.skills_dir.clone();
                def.skills = crate::core::skills::discover_skills(def.skills, &skills_dir);
                (def, Some(format!("Config error (using defaults): {e}")))
            }
        }
    }
}
