use crate::core::{config::expand_tilde, tools::ToolCall};
use anyhow::{Context, Result};
use chrono::Local;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{IgnoredAny, SeqAccess, Visitor},
};
use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

pub fn truncate_str(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}...", &s[..idx]),
        None => s.to_string(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThoughtPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thoughts: Option<Vec<ThoughtPart>>,
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
            thoughts: None,
        }
    }

    pub fn without_images(&self) -> Self {
        Self {
            images: None,
            ..self.clone()
        }
    }

    pub fn with_ui_image_placeholders(&self) -> Self {
        let img_count = self.images.as_ref().map_or(0, Vec::len);
        let path_count = self.image_paths.as_ref().map_or(0, Vec::len);
        let count = img_count.max(path_count);

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

fn count_sequence<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<usize, D::Error> {
    struct CountVisitor;
    impl<'de> Visitor<'de> for CountVisitor {
        type Value = usize;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a sequence of messages")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<usize, A::Error> {
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
        Local::now().format("%Y%m%d_%H%M%S_%3f").to_string()
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
            .and_then(|m| m.content.lines().next())
            .map_or_else(|| "Untitled Session".into(), |line| truncate_str(line, 50));

        let path = self.history_dir.join(format!("session_{id}.json"));
        let tmp_path = self.history_dir.join(format!(".session_{id}.json.tmp"));

        let data = SessionData {
            id: id.into(),
            title,
            model: model.into(),
            updated_at: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            system_prompt: system_prompt.into(),
            num_ctx,
            messages: clean_msgs,
        };

        fs::write(&tmp_path, serde_json::to_string_pretty(&data)?)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        let mut list: Vec<SessionSummary> = fs::read_dir(&self.history_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
                    && p.extension().is_some_and(|ext| ext == "json")
            })
            .filter_map(|path| {
                let file = fs::File::open(&path).ok()?;
                let mut summary: SessionSummary =
                    serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
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
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse session file: {}", path.display()))
    }

    pub fn delete(&self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        if let Some(id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("session_"))
        {
            let _ = fs::remove_dir_all(self.asset_dir(id));
        }
        fs::remove_file(path)?;
        Ok(())
    }
}
