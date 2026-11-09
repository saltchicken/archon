use crate::core::tools::ToolCall;
use anyhow::Result;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub id: String,
    pub title: String,
    pub model: String,
    pub updated_at: String,
    pub system_prompt: String,
    pub num_ctx: u32,
    pub messages: Vec<ChatMessage>,
    #[serde(skip)]
    pub file_path: PathBuf,
}

pub struct SessionManager {
    history_dir: PathBuf,
}

impl SessionManager {
    pub fn new(raw_dir: &str) -> Result<Self> {
        let expanded = if let Some(stripped) = raw_dir.strip_prefix("~/") {
            dirs::home_dir().unwrap_or_default().join(stripped)
        } else {
            PathBuf::from(raw_dir)
        };
        fs::create_dir_all(&expanded)?;
        Ok(Self { history_dir: expanded })
    }

    pub fn new_id() -> String {
        Local::now().format("%Y%m%d_%H%M%S").to_string()
    }

    pub fn save(
        &self,
        id: &str,
        model: &str,
        system_prompt: &str,
        num_ctx: u32,
        messages: &[ChatMessage],
    ) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }

        let clean_msgs: Vec<ChatMessage> = messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: m.content.clone(),
                images: None,
                tool_calls: m.tool_calls.clone(),
            })
            .collect();

        let title = clean_msgs
            .iter()
            .find(|m| m.role == "user" && !m.content.trim().is_empty())
            .map(|m| {
                let first_line = m.content.lines().next().unwrap_or("Untitled");
                if first_line.chars().count() > 50 {
                    format!("{}...", first_line.chars().take(50).collect::<String>())
                } else {
                    first_line.to_string()
                }
            })
            .unwrap_or_else(|| "Untitled Session".into());

        let path = self.history_dir.join(format!("session_{id}.json"));
        let data = SessionData {
            id: id.into(),
            title,
            model: model.into(),
            updated_at: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            system_prompt: system_prompt.into(),
            num_ctx,
            messages: clean_msgs,
            file_path: path.clone(),
        };

        fs::write(path, serde_json::to_string_pretty(&data)?)?;
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionData> {
        let mut list: Vec<SessionData> = fs::read_dir(&self.history_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
            .filter_map(|e| {
                let text = fs::read_to_string(e.path()).ok()?;
                let mut data: SessionData = serde_json::from_str(&text).ok()?;
                data.file_path = e.path();
                Some(data)
            })
            .collect();

        list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        list
    }

    pub fn delete(&self, path: &Path) -> Result<()> {
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}
