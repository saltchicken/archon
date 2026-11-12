use crate::tools::{auto_schema, parse_args, resolve_existing_path};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{borrow::Cow, fs, path::PathBuf};

#[derive(Deserialize, JsonSchema)]
struct EditFileArgs {
    file_path: String,
    search_string: String,
    replace_string: String,
}

pub fn schema() -> Value {
    auto_schema::<EditFileArgs>(
        "edit_file",
        "Edit a file by replacing an exact, unique search string with a replacement string.",
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

pub fn apply_edit(
    file_path: &str,
    search: &str,
    replace: &str,
) -> Result<(PathBuf, String, String)> {
    if search.is_empty() {
        bail!("search_string cannot be empty");
    }

    let (_, target_path) = resolve_existing_path(file_path)?;
    let content = fs::read_to_string(&target_path).context("Error reading file")?;

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
        bail!("Exact search_string not found in {file_path}");
    }
    if match_count > 1 {
        bail!(
            "search_string matched {match_count} times in {file_path}. Provide more surrounding context to make it unique."
        );
    }

    let updated = content.replacen(effective_search.as_ref(), effective_replace.as_ref(), 1);
    Ok((target_path, content, updated))
}

pub fn execute(json_args: &Value) -> Result<String> {
    let args: EditFileArgs = parse_args(json_args, "edit_file")?;
    let (target_path, _, updated) =
        apply_edit(&args.file_path, &args.search_string, &args.replace_string)?;

    fs::write(&target_path, updated).context("Error writing file")?;
    Ok(format!("Successfully edited {}", args.file_path))
}
