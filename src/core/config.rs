use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

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
    pub history_dir: String,
    pub required_commands: Vec<String>,
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
            history_dir: "~/.local/share/archon/sessions".into(),
            required_commands: vec!["wl-paste".into()],
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
    pub fn load(path: impl AsRef<Path>) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }
}
