use super::{BlockingTool, ToolContext};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{fmt::Write as _, time::Duration};

const DEFAULT_ROW_LIMIT: usize = 50;
const MAX_ROW_LIMIT: usize = 500;
const MAX_CELL_CHARS: usize = 200;

pub struct Tool;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SqliteAction {
    /// List all tables and views in the database.
    Tables,
    /// Show the SQL schema (CREATE statements) for the database or a specific table.
    Schema,
    /// Execute a read-only SQL query (SELECT, WITH, EXPLAIN, PRAGMA) and return results as a Markdown table.
    Query,
}

#[derive(Deserialize, JsonSchema)]
pub struct SqliteArgs {
    /// Action to perform: `tables`, `schema`, or `query`.
    pub action: SqliteAction,
    /// Path to the SQLite database file (relative to workspace root, or an absolute path under `app.read_roots`).
    ///
    /// Note: the workspace boundary applies to this main database only; an
    /// `ATTACH DATABASE` inside a query could open other files (still read-only).
    pub path: String,
    /// Optional table/view name to filter `schema` output.
    #[serde(default)]
    pub table: Option<String>,
    /// SQL query to execute (required for `query`; strictly read-only).
    #[serde(default)]
    pub sql: Option<String>,
    /// Maximum number of rows to return for `query` (default: 50, max: 500).
    #[serde(default)]
    pub limit: Option<usize>,
}

fn open_readonly(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("Failed to open SQLite database '{}'", path.display()))?;
    conn.busy_timeout(Duration::from_secs(3))?;
    // Defense-in-depth: enforce read-only at the SQLite engine level as well.
    conn.execute_batch("PRAGMA query_only = ON;")?;
    Ok(conn)
}

fn format_cell(val: ValueRef<'_>) -> String {
    let raw = match val {
        ValueRef::Null => "NULL".to_string(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(b) => String::from_utf8_lossy(b).into_owned(),
        ValueRef::Blob(b) => format!("<BLOB {} bytes>", b.len()),
    };

    let single_line = raw
        .replace('\r', "")
        .replace('\n', "\\n")
        .replace('|', "\\|");
    match single_line.char_indices().nth(MAX_CELL_CHARS) {
        Some((idx, _)) => format!("{}...", &single_line[..idx]),
        None => single_line,
    }
}

fn run_tables(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare(
        "SELECT type, name FROM sqlite_master
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'
         ORDER BY type, name",
    )?;

    let entries: Vec<(String, String)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if entries.is_empty() {
        return Ok("No tables or views found.".to_string());
    }

    let mut out = String::from("| Type | Name |\n| :--- | :--- |\n");
    for (kind, name) in entries {
        let _ = writeln!(out, "| {kind} | {name} |");
    }
    Ok(out.trim_end().to_string())
}

fn run_schema(conn: &Connection, table: Option<&str>) -> Result<String> {
    let rows: Vec<String> = match table.map(str::trim).filter(|s| !s.is_empty()) {
        Some(tbl) => {
            let mut stmt = conn.prepare(
                "SELECT sql FROM sqlite_master
                 WHERE sql IS NOT NULL AND (name = ?1 OR tbl_name = ?1)
                 ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'view' THEN 1 ELSE 2 END, name",
            )?;
            stmt.query_map([tbl], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT sql FROM sqlite_master
                 WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'
                 ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'view' THEN 1 ELSE 2 END, name",
            )?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
    };

    if rows.is_empty() {
        return match table {
            Some(tbl) => bail!("No schema found for table/view '{tbl}'."),
            None => Ok("Database schema is empty.".to_string()),
        };
    }

    Ok(rows
        .into_iter()
        .map(|s| format!("{};", s.trim_end_matches(';')))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn run_query(conn: &Connection, sql: &str, limit: usize) -> Result<String> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        bail!("'sql' is required for sqlite:query");
    }

    // Reject statements that are not read-only at the application level for
    // a clearer error message (defense-in-depth alongside SQLite's own
    // `query_only` + `SQLITE_OPEN_READ_ONLY` enforcement).
    let first_word = trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_uppercase();
    if !matches!(
        first_word.as_str(),
        "SELECT" | "WITH" | "EXPLAIN" | "PRAGMA"
    ) {
        bail!(
            "sqlite:query only allows read-only statements (SELECT, WITH, EXPLAIN, PRAGMA). Got: {first_word}"
        );
    }
    // PRAGMA assignment form (e.g. `PRAGMA cache_size = …`) can allocate
    // connection memory or flip internal state before the read-only flag
    // rejects it, so block it at the keyword level. Query-form PRAGMAs
    // (`PRAGMA table_info(…)`, `PRAGMA index_list(…)`, …) remain allowed.
    if first_word == "PRAGMA" && trimmed.contains('=') {
        bail!(
            "sqlite:query only allows read-only PRAGMAs (e.g. `PRAGMA table_info(…)`); assignment form is not permitted."
        );
    }

    let mut stmt = conn
        .prepare(trimmed)
        .with_context(|| "Failed to prepare SQL query")?;

    let col_names: Vec<String> = stmt.column_names().map(str::to_string).collect();
    if col_names.is_empty() {
        // No result columns: still execute so that SQLite enforces read-only
        // for any DML/DDL that slipped past the keyword check, and so
        // legitimate no-result PRAGMAs (e.g. `PRAGMA cache_size = 2000`) run.
        stmt.execute([])
            .with_context(|| "Failed to execute statement (read-only violation?)")?;
        return Ok("Statement executed (no result rows).".to_string());
    }

    let mut rows = stmt.query([])?;
    let mut rendered_rows: Vec<Vec<String>> = Vec::new();
    let mut truncated = false;

    while let Some(row) = rows.next()? {
        if rendered_rows.len() >= limit {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(col_names.len());
        for i in 0..col_names.len() {
            cells.push(format_cell(row.get_ref(i)?));
        }
        rendered_rows.push(cells);
    }

    let mut out = String::new();
    let _ = writeln!(out, "| {} |", col_names.join(" | "));
    let _ = writeln!(out, "| {} |", vec!["---"; col_names.len()].join(" | "));
    for r in &rendered_rows {
        let _ = writeln!(out, "| {} |", r.join(" | "));
    }

    if rendered_rows.is_empty() {
        let _ = writeln!(out, "\n(0 rows)");
    } else if truncated {
        let _ = writeln!(
            out,
            "\n(Showing first {limit} rows; more rows available. Refine your query or increase `limit` up to {MAX_ROW_LIMIT}.)"
        );
    } else {
        let _ = writeln!(out, "\n({} row(s))", rendered_rows.len());
    }

    Ok(out.trim_end().to_string())
}

impl BlockingTool for Tool {
    const NAME: &'static str = "sqlite";
    const DESCRIPTION: &'static str = "Inspect SQLite databases strictly read-only: list tables/views, view CREATE schemas, or run SELECT/PRAGMA/EXPLAIN queries formatted as Markdown tables.";
    type Args = SqliteArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        let db_path = ctx.resolve_readable_path(&args.path)?;
        let conn = open_readonly(&db_path)?;

        match args.action {
            SqliteAction::Tables => run_tables(&conn),
            SqliteAction::Schema => run_schema(&conn, args.table.as_deref()),
            SqliteAction::Query => {
                let sql = args
                    .sql
                    .as_deref()
                    .context("'sql' is required for sqlite:query")?;
                let limit = args
                    .limit
                    .unwrap_or(DEFAULT_ROW_LIMIT)
                    .clamp(1, MAX_ROW_LIMIT);
                run_query(&conn, sql, limit)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_sample_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT);
             INSERT INTO users (name, note) VALUES ('Alice', 'hello'), ('Bob', NULL);",
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn lists_tables_and_reads_schema() {
        let (_dir, path) = create_sample_db();
        let conn = open_readonly(&path).unwrap();

        let tables = run_tables(&conn).unwrap();
        assert!(tables.contains("| table | users |"), "{tables}");

        let schema = run_schema(&conn, Some("users")).unwrap();
        assert!(schema.contains("CREATE TABLE users"), "{schema}");
    }

    #[test]
    fn executes_readonly_query_and_blocks_writes() {
        let (_dir, path) = create_sample_db();
        let conn = open_readonly(&path).unwrap();

        let res = run_query(&conn, "SELECT id, name, note FROM users ORDER BY id", 10).unwrap();
        assert!(res.contains("| 1 | Alice | hello |"), "{res}");
        assert!(res.contains("| 2 | Bob | NULL |"), "{res}");
        assert!(res.contains("(2 row(s))"), "{res}");

        let write_err = run_query(&conn, "DELETE FROM users", 10).unwrap_err();
        assert!(
            write_err.to_string().to_lowercase().contains("readonly")
                || write_err.to_string().to_lowercase().contains("read-only"),
            "unexpected error: {write_err}"
        );
    }

    #[test]
    fn blocks_pragma_assignment_but_allows_pragma_query() {
        let (_dir, path) = create_sample_db();
        let conn = open_readonly(&path).unwrap();

        // Assignment form must be rejected at the keyword level.
        let err = run_query(&conn, "PRAGMA cache_size = 1000", 10).unwrap_err();
        assert!(
            err.to_string().contains("assignment form is not permitted"),
            "unexpected error: {err}"
        );

        // Query form (read-only) must succeed.
        let ok = run_query(&conn, "PRAGMA table_info(users)", 10).unwrap();
        assert!(ok.contains("id"), "should list column 'id': {ok}");
        assert!(ok.contains("name"), "should list column 'name': {ok}");
    }
}
