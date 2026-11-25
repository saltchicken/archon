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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThoughtPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_in_context: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
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
            ..Self::default()
        }
    }

    pub fn new_tool(
        tool_name: impl Into<String>,
        content: impl Into<String>,
        truncated: bool,
        is_error: bool,
    ) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            truncated,
            is_error,
            tool_name: Some(tool_name.into()),
            ..Self::default()
        }
    }

    pub fn new_hook(
        summary: impl Into<String>,
        content: impl Into<String>,
        truncated: bool,
    ) -> Self {
        Self {
            role: "hook".into(),
            content: content.into(),
            truncated,
            include_in_context: true,
            tool_name: Some(summary.into()),
            ..Self::default()
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
            ..self.clone()
        }
    }

    pub fn is_forkable(&self) -> bool {
        self.role == "assistant"
            && (!self.content.trim().is_empty()
                || (self.tool_calls.as_ref().is_none_or(Vec::is_empty)
                    && self.thoughts.as_ref().is_some_and(|t| !t.is_empty())))
    }
}

pub const FORK_TITLE_PREFIX: &str = "󰘬 ";

pub fn strip_fork_prefix(title: &str) -> &str {
    title.strip_prefix(FORK_TITLE_PREFIX).unwrap_or(title)
}

pub fn default_title_for_messages(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .find(|m| m.role == "user" && !m.content.trim().is_empty())
        .and_then(|m| m.content.lines().next())
        .map_or_else(|| "Untitled Session".into(), |line| truncate_str(line, 50))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub id: String,
    pub title: String,
    pub model: String,
    pub updated_at: String,
    pub system_prompt: String,
    pub num_ctx: u32,
    #[serde(default)]
    pub pinned: bool,
    pub messages: Vec<ChatMessage>,
}

impl SessionData {
    pub fn is_fork(&self) -> bool {
        self.title.starts_with(FORK_TITLE_PREFIX)
    }

    pub fn custom_title(&self) -> Option<String> {
        let raw_title = strip_fork_prefix(&self.title);
        let default_title = default_title_for_messages(&self.messages);
        (raw_title != default_title && raw_title != "Untitled Session")
            .then(|| raw_title.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub messages: usize,
    pub pinned: bool,
}

#[derive(Debug, Clone)]
pub struct SessionDetails {
    pub id: String,
    pub workspace: String,
    pub title: String,
    pub model: String,
    pub updated_at: String,
    pub messages: usize,
    pub pinned: bool,
}

#[derive(Debug, Clone)]
pub struct SessionMatchSnippet {
    pub message_index: usize,
    pub role: String,
    pub excerpt: String,
}

#[derive(Debug, Clone)]
pub struct SessionSearchHit {
    pub session: SessionDetails,
    pub title_matched: bool,
    pub snippets: Vec<SessionMatchSnippet>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub total_sessions: usize,
    pub pinned_sessions: usize,
    pub total_messages: usize,
    pub models: Vec<(String, usize)>,
}

pub struct SessionManager {
    conn: Mutex<Connection>,
    workspace: String,
}

impl SessionManager {
    pub fn new(raw_dir: &str, workspace: Option<&str>) -> Result<Self> {
        let history_dir = expand_tilde(raw_dir);
        fs::create_dir_all(&history_dir)?;

        let workspace = workspace
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(expand_tilde)
            .or_else(|| std::env::current_dir().ok())
            .map(|p| {
                dunce::canonicalize(&p)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default();

        let db_path = history_dir.join("archon.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to open SQLite database at {}", db_path.display()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;

             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 workspace TEXT NOT NULL DEFAULT '',
                 title TEXT NOT NULL,
                 model TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 system_prompt TEXT NOT NULL,
                 num_ctx INTEGER NOT NULL,
                 pinned INTEGER NOT NULL DEFAULT 0,
                 message_count INTEGER NOT NULL DEFAULT 0,
                 messages_json TEXT NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_sessions_updated_at
                 ON sessions(updated_at DESC);

             CREATE INDEX IF NOT EXISTS idx_sessions_workspace_updated_at
                 ON sessions(workspace, updated_at DESC);

             CREATE TABLE IF NOT EXISTS session_images (
                 session_id TEXT NOT NULL,
                 name TEXT NOT NULL,
                 data BLOB NOT NULL,
                 PRIMARY KEY (session_id, name),
                 FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
             );",
        )
        .context("Failed to initialize database schema")?;

        // Migrate existing databases that predate the `pinned` column.
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sessions_workspace_pinned_updated_at
             ON sessions(workspace, pinned, updated_at DESC)",
            [],
        );

        Ok(Self {
            conn: Mutex::new(conn),
            workspace,
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

    #[allow(clippy::too_many_arguments)]
    pub fn save(
        &self,
        id: &str,
        custom_title: Option<&str>,
        model: &str,
        system_prompt: &str,
        num_ctx: u32,
        messages: &[ChatMessage],
        new_images: &[(String, Vec<u8>)],
        is_fork: bool,
        is_pinned: bool,
    ) -> Result<()> {
        if messages.is_empty() && new_images.is_empty() && custom_title.is_none() && !is_pinned {
            let conn = self.lock_conn();
            let _ = conn.execute(
                "DELETE FROM sessions WHERE id = ?1 AND message_count = 0",
                params![id],
            );
            return Ok(());
        }

        let clean_msgs: Vec<ChatMessage> = messages
            .iter()
            .map(|m| ChatMessage {
                images: None,
                ..m.clone()
            })
            .collect();
        let base_title = custom_title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| truncate_str(s, 50))
            .unwrap_or_else(|| default_title_for_messages(&clean_msgs));
        let title = if is_fork {
            format!("{FORK_TITLE_PREFIX}{base_title}")
        } else {
            base_title
        };

        let updated_at = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let messages_json = serde_json::to_string(&clean_msgs)?;
        let msg_count = clean_msgs.len() as i64;
        let pinned_int = i64::from(is_pinned);

        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO sessions (id, workspace, title, model, updated_at, system_prompt, num_ctx, pinned, message_count, messages_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                 workspace = excluded.workspace,
                 title = excluded.title,
                 model = excluded.model,
                 updated_at = excluded.updated_at,
                 system_prompt = excluded.system_prompt,
                 num_ctx = excluded.num_ctx,
                 pinned = excluded.pinned,
                 message_count = excluded.message_count,
                 messages_json = excluded.messages_json",
            params![
                id,
                self.workspace,
                title,
                model,
                updated_at,
                system_prompt,
                num_ctx,
                pinned_int,
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

    pub fn toggle_pinned(&self, id: &str) -> Result<bool> {
        let conn = self.lock_conn();
        let current: i64 = conn
            .query_row(
                "SELECT pinned FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .with_context(|| format!("Session '{id}' not found in database"))?;
        let next = current == 0;
        conn.execute(
            "UPDATE sessions SET pinned = ?1 WHERE id = ?2",
            params![i64::from(next), id],
        )?;
        Ok(next)
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<bool> {
        let conn = self.lock_conn();
        let updated = conn.execute(
            "UPDATE sessions SET pinned = ?1 WHERE id = ?2",
            params![i64::from(pinned), id],
        )?;
        if updated == 0 {
            anyhow::bail!("Session '{id}' not found in database");
        }
        Ok(pinned)
    }

    pub fn rename(&self, id: &str, new_title: &str) -> Result<String> {
        let cleaned = strip_fork_prefix(new_title.trim()).trim();
        if cleaned.is_empty() {
            anyhow::bail!("Session title cannot be empty.");
        }
        let conn = self.lock_conn();
        let current_title: String = conn
            .query_row(
                "SELECT title FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .with_context(|| format!("Session '{id}' not found in database"))?;

        let base = truncate_str(cleaned, 50);
        let final_title = if current_title.starts_with(FORK_TITLE_PREFIX) {
            format!("{FORK_TITLE_PREFIX}{base}")
        } else {
            base
        };

        conn.execute(
            "UPDATE sessions SET title = ?1 WHERE id = ?2",
            params![final_title, id],
        )?;
        Ok(final_title)
    }

    pub fn list_filtered(
        &self,
        all_workspaces: bool,
        pinned_only: bool,
        limit: usize,
    ) -> Vec<SessionDetails> {
        let conn = self.lock_conn();
        let sql = match (all_workspaces, pinned_only) {
            (true, true) => {
                "SELECT id, workspace, title, model, updated_at, message_count, pinned
                 FROM sessions WHERE pinned = 1
                 ORDER BY updated_at DESC, id DESC LIMIT ?1"
            }
            (true, false) => {
                "SELECT id, workspace, title, model, updated_at, message_count, pinned
                 FROM sessions
                 ORDER BY pinned DESC, updated_at DESC, id DESC LIMIT ?1"
            }
            (false, true) => {
                "SELECT id, workspace, title, model, updated_at, message_count, pinned
                 FROM sessions WHERE workspace = ?1 AND pinned = 1
                 ORDER BY updated_at DESC, id DESC LIMIT ?2"
            }
            (false, false) => {
                "SELECT id, workspace, title, model, updated_at, message_count, pinned
                 FROM sessions WHERE workspace = ?1
                 ORDER BY pinned DESC, updated_at DESC, id DESC LIMIT ?2"
            }
        };

        let Ok(mut stmt) = conn.prepare(sql) else {
            return Vec::new();
        };

        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(SessionDetails {
                id: row.get(0)?,
                workspace: row.get(1)?,
                title: row.get(2)?,
                model: row.get(3)?,
                updated_at: row.get(4)?,
                messages: row.get::<_, i64>(5)?.max(0) as usize,
                pinned: row.get::<_, i64>(6).unwrap_or(0) != 0,
            })
        };

        let limit_i64 = limit as i64;
        if all_workspaces {
            stmt.query_map(params![limit_i64], map_row)
                .into_iter()
                .flatten()
                .flatten()
                .collect()
        } else {
            stmt.query_map(params![self.workspace, limit_i64], map_row)
                .into_iter()
                .flatten()
                .flatten()
                .collect()
        }
    }

    pub fn search(
        &self,
        query: &str,
        is_regex: bool,
        all_workspaces: bool,
        pinned_only: bool,
        limit: usize,
    ) -> Result<Vec<SessionSearchHit>> {
        const MAX_SNIPPETS_PER_SESSION: usize = 3;

        let re = if is_regex {
            Some(regex::Regex::new(query).with_context(|| format!("Invalid regex: '{query}'"))?)
        } else {
            None
        };
        let query_lower = query.to_ascii_lowercase();

        let matches_text = |text: &str| -> Option<usize> {
            if let Some(ref r) = re {
                r.find(text).map(|m| m.start())
            } else {
                text.to_ascii_lowercase().find(&query_lower)
            }
        };

        let conn = self.lock_conn();
        let sql = match (all_workspaces, pinned_only) {
            (true, true) => {
                "SELECT id, workspace, title, model, updated_at, message_count, pinned, messages_json
                 FROM sessions WHERE pinned = 1
                 ORDER BY updated_at DESC, id DESC"
            }
            (true, false) => {
                "SELECT id, workspace, title, model, updated_at, message_count, pinned, messages_json
                 FROM sessions
                 ORDER BY pinned DESC, updated_at DESC, id DESC"
            }
            (false, true) => {
                "SELECT id, workspace, title, model, updated_at, message_count, pinned, messages_json
                 FROM sessions WHERE workspace = ?1 AND pinned = 1
                 ORDER BY updated_at DESC, id DESC"
            }
            (false, false) => {
                "SELECT id, workspace, title, model, updated_at, message_count, pinned, messages_json
                 FROM sessions WHERE workspace = ?1
                 ORDER BY pinned DESC, updated_at DESC, id DESC"
            }
        };

        let mut stmt = conn.prepare(sql)?;
        let rows = if all_workspaces {
            stmt.query_map([], |row| {
                Ok((
                    SessionDetails {
                        id: row.get(0)?,
                        workspace: row.get(1)?,
                        title: row.get(2)?,
                        model: row.get(3)?,
                        updated_at: row.get(4)?,
                        messages: row.get::<_, i64>(5)?.max(0) as usize,
                        pinned: row.get::<_, i64>(6).unwrap_or(0) != 0,
                    },
                    row.get::<_, String>(7)?,
                ))
            })?
            .flatten()
            .collect::<Vec<_>>()
        } else {
            stmt.query_map(params![self.workspace], |row| {
                Ok((
                    SessionDetails {
                        id: row.get(0)?,
                        workspace: row.get(1)?,
                        title: row.get(2)?,
                        model: row.get(3)?,
                        updated_at: row.get(4)?,
                        messages: row.get::<_, i64>(5)?.max(0) as usize,
                        pinned: row.get::<_, i64>(6).unwrap_or(0) != 0,
                    },
                    row.get::<_, String>(7)?,
                ))
            })?
            .flatten()
            .collect::<Vec<_>>()
        };

        let mut hits = Vec::new();
        for (details, messages_json) in rows {
            if hits.len() >= limit {
                break;
            }
            let title_matched = matches_text(&details.title).is_some();
            let mut snippets = Vec::new();

            if let Ok(msgs) = serde_json::from_str::<Vec<ChatMessage>>(&messages_json) {
                for (idx, msg) in msgs.iter().enumerate() {
                    if msg.role == "error" || msg.content.trim().is_empty() {
                        continue;
                    }
                    if let Some(byte_pos) = matches_text(&msg.content) {
                        let start = msg.content.floor_char_boundary(byte_pos.saturating_sub(60));
                        let end = msg
                            .content
                            .ceil_char_boundary((byte_pos + 120).min(msg.content.len()));
                        let raw_excerpt = msg.content[start..end].replace('\n', " ");
                        let prefix = if start > 0 { "..." } else { "" };
                        let suffix = if end < msg.content.len() { "..." } else { "" };
                        snippets.push(SessionMatchSnippet {
                            message_index: idx + 1,
                            role: msg.role.clone(),
                            excerpt: format!("{prefix}{}{suffix}", raw_excerpt.trim()),
                        });
                        if snippets.len() >= MAX_SNIPPETS_PER_SESSION {
                            break;
                        }
                    }
                }
            }

            if title_matched || !snippets.is_empty() {
                hits.push(SessionSearchHit {
                    session: details,
                    title_matched,
                    snippets,
                });
            }
        }

        Ok(hits)
    }

    pub fn stats(&self, all_workspaces: bool) -> Result<SessionStats> {
        let conn = self.lock_conn();
        let (total_sessions, pinned_sessions, total_messages): (i64, i64, i64) = if all_workspaces {
            conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(pinned), 0), COALESCE(SUM(message_count), 0) FROM sessions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
        } else {
            conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(pinned), 0), COALESCE(SUM(message_count), 0)
                 FROM sessions WHERE workspace = ?1",
                params![self.workspace],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
        };

        let model_sql = if all_workspaces {
            "SELECT model, COUNT(*) FROM sessions GROUP BY model ORDER BY COUNT(*) DESC, model ASC"
        } else {
            "SELECT model, COUNT(*) FROM sessions WHERE workspace = ?1 GROUP BY model ORDER BY COUNT(*) DESC, model ASC"
        };
        let mut stmt = conn.prepare(model_sql)?;
        let models: Vec<(String, usize)> = if all_workspaces {
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?.max(0) as usize))
            })?
            .flatten()
            .collect()
        } else {
            stmt.query_map(params![self.workspace], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?.max(0) as usize))
            })?
            .flatten()
            .collect()
        };

        Ok(SessionStats {
            total_sessions: total_sessions.max(0) as usize,
            pinned_sessions: pinned_sessions.max(0) as usize,
            total_messages: total_messages.max(0) as usize,
            models,
        })
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        let conn = self.lock_conn();
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, title, message_count, pinned
             FROM sessions
             WHERE workspace = ?1
             ORDER BY updated_at DESC, id DESC",
        ) else {
            return Vec::new();
        };

        stmt.query_map(params![self.workspace], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                messages: row.get::<_, i64>(2)?.max(0) as usize,
                pinned: row.get::<_, i64>(3).unwrap_or(0) != 0,
            })
        })
        .into_iter()
        .flatten()
        .flatten()
        .collect()
    }

    pub fn load(&self, id: &str) -> Result<SessionData> {
        let conn = self.lock_conn();
        let (mut session, messages_json) = conn
            .query_row(
                "SELECT title, model, updated_at, system_prompt, num_ctx, pinned, messages_json
                 FROM sessions
                 WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        SessionData {
                            id: id.to_string(),
                            title: row.get(0)?,
                            model: row.get(1)?,
                            updated_at: row.get(2)?,
                            system_prompt: row.get(3)?,
                            num_ctx: row.get(4)?,
                            pinned: row.get::<_, i64>(5).unwrap_or(0) != 0,
                            messages: Vec::new(),
                        },
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .with_context(|| format!("Session '{id}' not found in database"))?;

        session.messages = serde_json::from_str(&messages_json)
            .with_context(|| format!("Failed to parse messages JSON for session '{id}'"))?;
        Ok(session)
    }

    pub fn load_images(&self, session_id: &str, names: &[String]) -> Vec<(String, Vec<u8>)> {
        if names.is_empty() {
            return Vec::new();
        }
        let conn = self.lock_conn();
        let Ok(mut stmt) =
            conn.prepare("SELECT data FROM session_images WHERE session_id = ?1 AND name = ?2")
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
