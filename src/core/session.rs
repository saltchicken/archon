use crate::core::{config::expand_tilde, tools::ToolCall};
use anyhow::{Context, Result};
use chrono::Local;
use serde::{
    de::{IgnoredAny, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_paths: Option<Vec<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

impl ChatMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            truncated: false,
            images: None,
            image_paths: None,
            tool_calls: None,
        }
    }

    pub fn without_images(&self) -> Self {
        Self {
            images: None,
            ..self.clone()
        }
    }

    pub fn with_ui_image_placeholders(&self) -> Self {
        let count = self
            .images
            .as_ref()
            .map(Vec::len)
            .or_else(|| self.image_paths.as_ref().map(Vec::len))
            .unwrap_or(0);

        Self {
            images: (count > 0).then(|| vec![String::new(); count]),
            ..self.without_images()
        }
    }
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

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSummary {
    pub title: String,
    pub updated_at: String,
    #[serde(default, deserialize_with = "count_sequence")]
    pub messages: usize,
    #[serde(skip)]
    pub file_path: PathBuf,
}

fn count_sequence<'de, D>(deserializer: D) -> std::result::Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    struct CountVisitor;

    impl<'de> Visitor<'de> for CountVisitor {
        type Value = usize;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a sequence of messages")
        }

        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut count = 0;
            while seq.next_element::<IgnoredAny>()?.is_some() {
                count += 1;
            }
            Ok(count)
        }
    }

    deserializer.deserialize_seq(CountVisitor)
}

pub struct SessionManager {
    history_dir: PathBuf,
}

impl SessionManager {
    pub fn new(raw_dir: &str) -> Result<Self> {
        let history_dir = expand_tilde(raw_dir);
        fs::create_dir_all(&history_dir)?;
        Ok(Self { history_dir })
    }

    pub fn new_id() -> String {
        Local::now().format("%Y%m%d_%H%M%S").to_string()
    }

    pub fn asset_dir(&self, id: &str) -> PathBuf {
        self.history_dir.join("assets").join(id)
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

        let clean_msgs: Vec<ChatMessage> =
            messages.iter().map(ChatMessage::without_images).collect();

        let title = clean_msgs
            .iter()
            .find(|m| m.role == "user" && !m.content.trim().is_empty())
            .map(|m| {
                let first_line = m.content.lines().next().unwrap_or("Untitled");
                let mut chars = first_line.chars();
                let preview: String = chars.by_ref().take(50).collect();
                if chars.next().is_some() {
                    format!("{preview}...")
                } else {
                    preview
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

    pub fn list(&self) -> Vec<SessionSummary> {
        let mut list: Vec<SessionSummary> = fs::read_dir(&self.history_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .filter_map(|path| {
                let file = fs::File::open(&path).ok()?;
                let reader = std::io::BufReader::new(file);
                let mut summary: SessionSummary = serde_json::from_reader(reader).ok()?;
                summary.file_path = path;
                Some(summary)
            })
            .collect();

        list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        list
    }

    pub fn load(&self, path: &Path) -> Result<SessionData> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read session file: {}", path.display()))?;
        let mut data: SessionData = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse session file: {}", path.display()))?;
        data.file_path = path.to_path_buf();
        Ok(data)
    }

    pub fn delete(&self, path: &Path) -> Result<()> {
        if path.exists() {
            if let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("session_"))
            {
                let _ = fs::remove_dir_all(self.asset_dir(id));
            }
            fs::remove_file(path)?;
        }
        Ok(())
    }
}
