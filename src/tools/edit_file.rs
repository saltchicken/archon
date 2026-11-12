use crate::tools::{auto_schema, parse_args, resolve_existing_path};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{borrow::Cow, fs, path::PathBuf};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditHunk {
    /// The exact, unique block of text to search for.
    pub search_string: String,
    /// The replacement text to substitute in place of `search_string`.
    pub replace_string: String,
}

#[derive(Deserialize, JsonSchema)]
struct EditFileArgs {
    /// Relative path to the file in the workspace.
    file_path: String,
    /// Ordered list of search/replace hunks to apply atomically to the file.
    edits: Vec<EditHunk>,
}

pub fn schema() -> Value {
    auto_schema::<EditFileArgs>(
        "edit_file",
        "Edit a file by applying one or more exact, unique search/replace hunks atomically.",
    )
}

fn normalize_eol(s: &str, use_crlf: bool) -> String {
    let lf = s.replace("\r\n", "\n");
    if use_crlf { lf.replace('\n', "\r\n") } else { lf }
}

fn apply_single_hunk(
    content: &str,
    file_path: &str,
    search: &str,
    replace: &str,
    hunk_label: &str,
) -> Result<String> {
    if search.is_empty() {
        bail!("{hunk_label}: search_string cannot be empty");
    }

    let mut match_count = content.matches(search).count();
    let mut effective_search = Cow::Borrowed(search);
    let mut effective_replace = Cow::Borrowed(replace);

    if match_count == 0 && search.contains('\n') {
        let target_crlf = content.contains("\r\n");
        let alt_search = normalize_eol(search, target_crlf);
        let alt_matches = content.matches(&alt_search).count();
        if alt_matches > 0 {
            match_count = alt_matches;
            effective_search = Cow::Owned(alt_search);
            effective_replace = Cow::Owned(normalize_eol(replace, target_crlf));
        }
    }

    match match_count {
        0 => bail!("{hunk_label}: exact search_string not found in {file_path}"),
        1 => Ok(content.replacen(effective_search.as_ref(), effective_replace.as_ref(), 1)),
        n => bail!("{hunk_label}: search_string matched {n} times in {file_path}. Provide more surrounding context to make it unique."),
    }
}

pub fn apply_edits_from_value(json_args: &Value) -> Result<(PathBuf, String, String, usize)> {
    let args: EditFileArgs = parse_args(json_args, "edit_file")?;
    if args.edits.is_empty() {
        bail!("'edits' array cannot be empty");
    }

    let (_, target_path) = resolve_existing_path(&args.file_path)?;
    let original = fs::read_to_string(&target_path).context("Error reading file")?;
    let mut buf = original.clone();
    let total = args.edits.len();

    for (idx, hunk) in args.edits.iter().enumerate() {
        let label = if total == 1 { "Edit".into() } else { format!("Hunk {}/{total}", idx + 1) };
        buf = apply_single_hunk(&buf, args.file_path.trim(), &hunk.search_string, &hunk.replace_string, &label)?;
    }

    Ok((target_path, original, buf, total))
}

pub fn fallback_hunk_preview(json_args: &Value) -> (String, String) {
    let Ok(args) = parse_args::<EditFileArgs>(json_args, "edit_file") else {
        return (String::new(), String::new());
    };
    let join_field = |f: fn(&EditHunk) -> &str| args.edits.iter().map(f).collect::<Vec<_>>().join("\n// ---\n");
    (join_field(|h| &h.search_string), join_field(|h| &h.replace_string))
}

pub fn execute(json_args: &Value) -> Result<String> {
    let args: EditFileArgs = parse_args(json_args, "edit_file")?;
    let (target_path, _, updated, hunk_count) = apply_edits_from_value(json_args)?;
    fs::write(&target_path, updated).context("Error writing file")?;

    let path = args.file_path.trim();
    if hunk_count == 1 {
        Ok(format!("Successfully edited {path}"))
    } else {
        Ok(format!("Successfully applied {hunk_count} edits to {path}"))
    }
}
