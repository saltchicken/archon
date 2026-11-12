use crate::core::provider::Provider;
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
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
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed).trim();
        if let Some((key, val)) = trimmed.split_once('=') {
            let key = key.trim();
            if key.is_empty() || std::env::var_os(key).is_some() {
                continue;
            }
            let val = val.trim();
            let unquoted = if (val.starts_with('"') && val.ends_with('"'))
                || (val.starts_with('\'') && val.ends_with('\''))
            {
                &val[1..val.len().saturating_sub(1)]
            } else {
                val
            };
            // Safe here because `load_dotenv` is called once at startup before spawning worker threads.
            unsafe {
                std::env::set_var(key, unquoted);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub app: AppSettings,
    pub context: ContextSettings,
    pub ollama: OllamaSettings,
    pub gemini: GeminiSettings,
    pub models: Vec<ModelPreset>,
    pub presets: Vec<Preset>,
    pub system_presets: Vec<Preset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub default_model: String,
    pub max_msg_lines: usize,
    pub history_dir: String,
    pub allowed_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextSettings {
    pub git_root: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_in_tree: Vec<String>,
    pub file: Vec<String>,
    pub max_size: u64,
    pub max_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OllamaSettings {
    pub base_url: String,
    pub chat_timeout: u64,
    pub status_timeout: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiSettings {
    pub api_key: String,
    pub base_url: String,
    pub chat_timeout: u64,
    pub status_timeout: u64,
}

impl GeminiSettings {
    pub fn resolve_api_key(&self) -> Option<String> {
        if !self.api_key.trim().is_empty() {
            return Some(self.api_key.trim().to_string());
        }
        std::env::var("GEMINI_API_KEY")
            .ok()
            .or_else(|| std::env::var("GOOGLE_API_KEY").ok())
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPreset {
    pub name: String,
    pub context: u32,
    pub max_tool_output_size: usize,
}

impl ModelPreset {
    pub fn new(name: &str, context: u32, max_tool_output_size: usize) -> Self {
        Self {
            name: name.into(),
            context,
            max_tool_output_size,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub prompt: String,
}

impl Preset {
    pub fn new(name: &str, prompt: &str) -> Self {
        Self {
            name: name.into(),
            prompt: prompt.into(),
        }
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            default_model: "qwen3.8:27b".into(),
            max_msg_lines: 25,
            history_dir: "~/.local/share/archon/sessions".into(),
            allowed_commands: ["ls", "cat", "echo", "pwd", "grep", "head", "tail", "wc"]
                .into_iter()
                .map(String::from)
                .collect(),
        }
    }
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            git_root: true,
            include: vec!["**/*".into()],
            exclude: vec![
                "target/**".into(),
                "node_modules/**".into(),
                "Cargo.lock".into(),
            ],
            include_in_tree: vec!["Cargo.lock".into()],
            file: Vec::new(),
            max_size: 1_048_576,
            max_files: 10_000,
        }
    }
}

impl Default for OllamaSettings {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434".into(),
            chat_timeout: 180,
            status_timeout: 2,
        }
    }
}

impl Default for GeminiSettings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            chat_timeout: 180,
            status_timeout: 5,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            app: AppSettings::default(),
            context: ContextSettings::default(),
            ollama: OllamaSettings::default(),
            gemini: GeminiSettings::default(),
            models: vec![
                ModelPreset::new("qwen3.8:27b", 32768, 131_072),
                ModelPreset::new("gemini-2.5-flash", 1_048_576, 524_288),
                ModelPreset::new("gemini-2.5-pro", 1_048_576, 524_288),
            ],
            presets: vec![
                Preset::new("Custom", ""),
                Preset::new(
                    "Git Commit",
                    "Inspect the repository using the `git` tool (`status`, `diff` with `target: \"--staged\"`, and `log` with `limit: 5`). If no changes are staged, inspect the unstaged `diff` and stage the relevant files using `git` (`action: \"add\"`). Then commit the staged changes using `git` (`action: \"commit\"`) with a clear, conventional commit message (concise summary line followed by bullet points explaining key changes).",
                ),
                Preset::new("Describe in detail", "Describe this image in detail."),
                Preset::new("Extract text", "Extract all the text from this image."),
            ],
            system_presets: vec![
                Preset::new("None", ""),
                Preset::new(
                    "Helpful Assistant",
                    "You are a highly capable and helpful AI visual assistant. Provide concise and accurate answers.",
                ),
                Preset::new(
                    "JSON Only",
                    "You are a data extractor. Only output valid JSON. Do not include markdown or explanations.",
                ),
            ],
        }
    }
}

impl Config {
    pub fn startup_model(&self) -> String {
        if !self.app.default_model.trim().is_empty() {
            return self.app.default_model.trim().to_string();
        }
        self.models
            .first()
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "qwen3.8:27b".into())
    }

    pub fn context_for_model(&self, model: &str) -> u32 {
        self.models
            .iter()
            .find(|m| m.name == model)
            .map(|m| m.context)
            .unwrap_or_else(|| Provider::from_model(model).fallback_ctx())
            .max(512)
    }

    pub fn max_tool_output_for_model(&self, model: &str) -> usize {
        self.models
            .iter()
            .find(|m| m.name == model)
            .map(|m| m.max_tool_output_size)
            .unwrap_or_else(|| Provider::from_model(model).fallback_max_tool_output())
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

        match Figment::from(Serialized::defaults(Self::default()))
            .merge(Toml::file(base_path))
            .merge(Toml::file(local_path.as_ref()))
            .merge(Env::prefixed("ARCHON_"))
            .extract()
        {
            Ok(cfg) => (cfg, None),
            Err(e) => (
                Self::default(),
                Some(format!("Config error (using defaults): {e}")),
            ),
        }
    }
}
