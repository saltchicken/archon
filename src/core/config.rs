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
    match shellexpand::full(path) {
        Ok(expanded) => PathBuf::from(expanded.into_owned()),
        Err(_) => PathBuf::from(shellexpand::tilde(path).into_owned()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub app: AppSettings,
    pub ollama: OllamaSettings,
    pub presets: Vec<Preset>,
    pub system_presets: Vec<Preset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub default_model: String,
    pub default_context: u32,
    pub max_msg_lines: usize,
    pub max_tool_output_size: usize,
    pub history_dir: String,
    pub allowed_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OllamaSettings {
    pub base_url: String,
    pub chat_timeout: u64,
    pub status_timeout: u64,
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
            default_context: 32768,
            max_msg_lines: 25,
            max_tool_output_size: 131072,
            history_dir: "~/.local/share/archon/sessions".into(),
            allowed_commands: ["ls", "cat", "echo", "pwd", "grep", "head", "tail", "wc"]
                .into_iter()
                .map(String::from)
                .collect(),
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

impl Default for Config {
    fn default() -> Self {
        Self {
            app: AppSettings::default(),
            ollama: OllamaSettings::default(),
            presets: vec![
                Preset::new("Custom", ""),
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
    pub fn load(base_path: impl AsRef<Path>, local_path: impl AsRef<Path>) -> Self {
        let base_path = base_path.as_ref();

        if !base_path.exists() {
            if let Some(parent) = base_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(toml_str) = toml::to_string_pretty(&Self::default()) {
                let _ = fs::write(base_path, toml_str);
            }
        }

        Figment::from(Serialized::defaults(Self::default()))
            .merge(Toml::file(base_path))
            .merge(Toml::file(local_path.as_ref()))
            .merge(Env::prefixed("ARCHON_"))
            .extract()
            .unwrap_or_default()
    }
}
