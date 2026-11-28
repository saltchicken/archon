use crate::config::expand_tilde;
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::fs;
use std::path::PathBuf;

pub fn archon_db_path(raw_history_dir: &str) -> PathBuf {
    expand_tilde(raw_history_dir).join("archon.db")
}

pub fn open_archon_db(raw_history_dir: &str) -> Result<Connection> {
    let history_dir = expand_tilde(raw_history_dir);
    fs::create_dir_all(&history_dir)?;
    let db_path = archon_db_path(raw_history_dir);
    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open SQLite database at {}", db_path.display()))?;

    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
    .context("Failed to configure SQLite pragmas")?;

    Ok(conn)
}
