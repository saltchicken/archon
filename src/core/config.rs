use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub app: AppSettings,
    #[serde(default)]
    pub ollama: OllamaSettings,
    #[serde(default = "default_presets")]
    pub presets: Vec<Preset>,
    #[serde(default = "default_sys_presets")]
    pub system_presets: Vec<Preset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default = "default_ctx")]
    pub default_context: u32,
    #[serde(default = "default_history_dir")]
    pub history_dir: String,
    #[serde(default = "default_req_cmds")]
    pub required_commands: Vec<String>,
    #[serde(default = "default_allowed_cmds")]
    pub allowed_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaSettings {
    #[serde(default = "default_url")]
    pub base_url: String,
    #[serde(default = "default_chat_timeout")]
    pub chat_timeout: u64,
    #[serde(default = "default_status_timeout")]
    pub status_timeout: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub prompt: String,
}

fn default_model() -> String {
    "qwen3.8:27b".into()
}
fn default_ctx() -> u32 {
    32768
}
fn default_history_dir() -> String {
    "~/.local/share/archon/sessions".into()
}
fn default_req_cmds() -> Vec<String> {
    vec!["grim".into(), "slurp".into(), "wl-paste".into()]
}
fn default_allowed_cmds() -> Vec<String> {
    ["ls", "cat", "echo", "pwd", "grep", "head", "tail", "wc"]
        .iter()
        .map(|s| (*s).into())
        .collect()
}
fn default_url() -> String {
    "http://localhost:11434".into()
}
fn default_chat_timeout() -> u64 {
    180
}
fn default_status_timeout() -> u64 {
    2
}
fn default_presets() -> Vec<Preset> {
    vec![
        Preset {
            name: "Custom".into(),
            prompt: "".into(),
        },
        Preset {
            name: "Describe in detail".into(),
            prompt: "Describe this image in detail.".into(),
        },
        Preset {
            name: "Extract text".into(),
            prompt: "Extract all the text from this image.".into(),
        },
    ]
}
fn default_sys_presets() -> Vec<Preset> {
    vec![
        Preset { name: "None".into(), prompt: "".into() },
        Preset {
            name: "Helpful Assistant".into(),
            prompt: "You are a highly capable and helpful AI visual assistant. Provide concise and accurate answers.".into(),
        },
        Preset {
            name: "JSON Only".into(),
            prompt: "You are a data extractor. Only output valid JSON. Do not include markdown or explanations.".into(),
        },
    ]
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            default_model: default_model(),
            default_context: default_ctx(),
            history_dir: default_history_dir(),
            required_commands: default_req_cmds(),
            allowed_commands: default_allowed_cmds(),
        }
    }
}

impl Default for OllamaSettings {
    fn default() -> Self {
        Self {
            base_url: default_url(),
            chat_timeout: default_chat_timeout(),
            status_timeout: default_status_timeout(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            app: AppSettings::default(),
            ollama: OllamaSettings::default(),
            presets: default_presets(),
            system_presets: default_sys_presets(),
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
