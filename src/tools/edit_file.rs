use crate::tools::{auto_schema, parse_args, resolve_existing_path};
use anyhow::{bail, Context, Result};
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

fn to_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.matches('\n').count());
    let mut prev_cr = false;
    for ch in s.chars() {
        if ch == '\n' && !prev_cr {
            out.push('\r');
        }
        out.push(ch);
        prev_cr = ch == '\r';
    }
    out
}

fn to_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
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
    let mut effective_search: Cow<'_, str> = Cow::Borrowed(search);
    let mut effective_replace: Cow<'_, str> = Cow::Borrowed(replace);

    // Fallback: normalize line endings if exact match failed due to CRLF vs LF mismatch
    if match_count == 0 {
        if content.contains("\r\n") && !search.contains("\r\n") && search.contains('\n') {
            let crlf_search = to_crlf(search);
            let crlf_matches = content.matches(&crlf_search).count();
            if crlf_matches > 0 {
                match_count = crlf_matches;
                effective_search = Cow::Owned(crlf_search);
                effective_replace = Cow::Owned(to_crlf(replace));
            }
        } else if !content.contains("\r\n") && search.contains("\r\n") {
            let lf_search = to_lf(search);
            let lf_matches = content.matches(&lf_search).count();
            if lf_matches > 0 {
                match_count = lf_matches;
                effective_search = Cow::Owned(lf_search);
                effective_replace = Cow::Owned(to_lf(replace));
            }
        }
    }

    if match_count == 0 {
        bail!("{hunk_label}: exact search_string not found in {file_path}");
    }
    if match_count > 1 {
        bail!(
            "{hunk_label}: search_string matched {match_count} times in {file_path}. Provide more surrounding context to make it unique."
        );
    }

    Ok(content.replacen(effective_search.as_ref(), effective_replace.as_ref(), 1))
}

/// Applies all requested hunks sequentially in memory and returns:
/// `(resolved_path, original_content, final_updated_content, hunk_count)`
pub fn apply_edits_from_value(json_args: &Value) -> Result<(PathBuf, String, String, usize)> {
    let args: EditFileArgs = parse_args(json_args, "edit_file")?;
    if args.edits.is_empty() {
        bail!("'edits' array cannot be empty");
    }

    let (_, target_path) = resolve_existing_path(&args.file_path)?;
    let original = fs::read_to_string(&target_path).context("Error reading file")?;
    let mut current = original.clone();
    let total = args.edits.len();

    for (idx, hunk) in args.edits.iter().enumerate() {
        let label = if total == 1 {
            "Edit".to_string()
        } else {
            format!("Hunk {}/{total}", idx + 1)
        };
        current = apply_single_hunk(
            &current,
            args.file_path.trim(),
            &hunk.search_string,
            &hunk.replace_string,
            &label,
        )?;
    }

    Ok((target_path, original, current, total))
}

/// Builds a fallback before/after string pair from raw arguments when `apply_edits_from_value`
/// fails (e.g. search string not found), so the approval modal can still show what the model attempted.
pub fn fallback_hunk_preview(json_args: &Value) -> (String, String) {
    let Ok(args) = parse_args::<EditFileArgs>(json_args, "edit_file") else {
        return (String::new(), String::new());
    };

    let mut all_search = String::new();
    let mut all_replace = String::new();
    for (idx, h) in args.edits.iter().enumerate() {
        if idx > 0 {
            all_search.push_str("\n// ---\n");
            all_replace.push_str("\n// ---\n");
        }
        all_search.push_str(&h.search_string);
        all_replace.push_str(&h.replace_string);
    }
    (all_search, all_replace)
}

pub fn execute(json_args: &Value) -> Result<String> {
    let args: EditFileArgs = parse_args(json_args, "edit_file")?;
    let (target_path, _, updated, hunk_count) = apply_edits_from_value(json_args)?;

    fs::write(&target_path, updated).context("Error writing file")?;
    if hunk_count == 1 {
        Ok(format!("Successfully edited {}", args.file_path.trim()))
    } else {
        Ok(format!(
            "Successfully applied {hunk_count} edits to {}",
            args.file_path.trim()
        ))
    }
}
