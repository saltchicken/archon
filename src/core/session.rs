use crate::{
    core::{attachments::ImageStaging, config::expand_tilde},
    tools::ToolCall,
};
use anyhow::{Context, Result};
use chrono::Local;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{fs, sync::Mutex};

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
    pub image_names: Option<Vec<String>>,
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
            image_names: None,
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
        let count = self
            .images
            .as_ref()
            .map_or(0, Vec::len)
            .max(self.image_names.as_ref().map_or(0, Vec::len));
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

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at: String,
    pub messages: usize,
}

pub struct SessionManager {
    conn: Mutex<Connection>,
}

impl SessionManager {
    pub fn new(raw_dir: &str) -> Result<Self> {
        let history_dir = expand_tilde(raw_dir);
        fs::create_dir_all(&history_dir)?;

        let db_path = history_dir.join("archon.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to open SQLite database at {}", db_path.display()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;

             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 model TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 system_prompt TEXT NOT NULL,
                 num_ctx INTEGER NOT NULL,
                 message_count INTEGER NOT NULL DEFAULT 0,
                 messages_json TEXT NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_sessions_updated_at
                 ON sessions(updated_at DESC);

             CREATE TABLE IF NOT EXISTS session_images (
                 session_id TEXT NOT NULL,
                 name TEXT NOT NULL,
                 data BLOB NOT NULL,
                 PRIMARY KEY (session_id, name),
                 FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
             );",
        )
        .context("Failed to initialize database schema")?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn new_id() -> String {
        Local::now().format("%Y%m%d_%H%M%S_%3f").to_string()
    }

    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn save(
        &self,
        id: &str,
        model: &str,
        system_prompt: &str,
        num_ctx: u32,
        messages: &[ChatMessage],
        new_images: &[(String, Vec<u8>)],
    ) -> Result<()> {
        if messages.is_empty() && new_images.is_empty() {
            return Ok(());
        }

        let clean_msgs: Vec<ChatMessage> =
            messages.iter().map(ChatMessage::without_images).collect();
        let title = clean_msgs
            .iter()
            .find(|m| m.role == "user" && !m.content.trim().is_empty())
            .and_then(|m| m.content.lines().next())
            .map_or_else(|| "Untitled Session".into(), |line| truncate_str(line, 50));

        let updated_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let messages_json = serde_json::to_string(&clean_msgs)?;
        let msg_count = clean_msgs.len() as i64;

        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO sessions (id, title, model, updated_at, system_prompt, num_ctx, message_count, messages_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                 title = excluded.title,
                 model = excluded.model,
                 updated_at = excluded.updated_at,
                 system_prompt = excluded.system_prompt,
                 num_ctx = excluded.num_ctx,
                 message_count = excluded.message_count,
                 messages_json = excluded.messages_json",
            params![
                id,
                title,
                model,
                updated_at,
                system_prompt,
                num_ctx,
                msg_count,
                messages_json
            ],
        )?;

        for (name, bytes) in new_images {
            tx.execute(
                "INSERT OR REPLACE INTO session_images (session_id, name, data)
                 VALUES (?1, ?2, ?3)",
                params![id, name, bytes],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        let conn = self.lock_conn();
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, title, updated_at, message_count
             FROM sessions
             ORDER BY updated_at DESC",
        ) else {
            return Vec::new();
        };

        let rows = stmt.query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                updated_at: row.get(2)?,
                messages: row.get::<_, i64>(3)?.max(0) as usize,
            })
        });

        match rows {
            Ok(iter) => iter.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn load(&self, id: &str) -> Result<SessionData> {
        let conn = self.lock_conn();
        let (title, model, updated_at, system_prompt, num_ctx, messages_json): (
            String,
            String,
            String,
            String,
            u32,
            String,
        ) = conn
            .query_row(
                "SELECT title, model, updated_at, system_prompt, num_ctx, messages_json
                 FROM sessions
                 WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .with_context(|| format!("Session '{id}' not found in database"))?;

        let messages: Vec<ChatMessage> = serde_json::from_str(&messages_json)
            .with_context(|| format!("Failed to parse messages JSON for session '{id}'"))?;

        Ok(SessionData {
            id: id.to_string(),
            title,
            model,
            updated_at,
            system_prompt,
            num_ctx,
            messages,
        })
    }

    pub fn load_images(&self, session_id: &str, names: &[String]) -> Vec<(String, Vec<u8>)> {
        if names.is_empty() {
            return Vec::new();
        }
        let conn = self.lock_conn();
        let Ok(mut stmt) = conn
            .prepare("SELECT data FROM session_images WHERE session_id = ?1 AND name = ?2")
        else {
            return Vec::new();
        };

        names
            .iter()
            .filter_map(|name| {
                let bytes: Option<Vec<u8>> = stmt
                    .query_row(params![session_id, name], |row| row.get(0))
                    .optional()
                    .ok()
                    .flatten();
                bytes.map(|b| (name.clone(), b))
            })
            .collect()
    }

    pub fn load_images_base64(&self, session_id: &str, names: &[String]) -> Vec<String> {
        self.load_images(session_id, names)
            .into_iter()
            .map(|(_, bytes)| ImageStaging::encode_bytes(&bytes))
            .collect()
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let conn = self.lock_conn();
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_loads_and_cascades_delete_in_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(dir.path().to_str().unwrap()).unwrap();

        let mut msg = ChatMessage::new("user", "Hello database");
        msg.image_names = Some(vec!["paste_123.png".into()]);
        let img_bytes = vec![137, 80, 78, 71, 13, 10, 26, 10];

        mgr.save(
            "sess_1",
            "qwen3.8:27b",
            "System prompt",
            32768,
            &[msg],
            &[("paste_123.png".into(), img_bytes.clone())],
        )
        .unwrap();

        let list = mgr.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "sess_1");
        assert_eq!(list[0].title, "Hello database");
        assert_eq!(list[0].messages, 1);

        let loaded = mgr.load("sess_1").unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(
            loaded.messages[0].image_names.as_deref(),
            Some(&["paste_123.png".to_string()][..])
        );

        let imgs = mgr.load_images("sess_1", &["paste_123.png".into()]);
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].1, img_bytes);

        mgr.delete("sess_1").unwrap();
        assert!(mgr.list().is_empty());
        assert!(mgr.load_images("sess_1", &["paste_123.png".into()]).is_empty());
    }
}
