use crate::tools::{auto_schema, parse_args, resolve_existing_path};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{fs, path::PathBuf};

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

    let match_count = content.matches(search).count();
    if match_count == 0 {
        bail!("Exact search_string not found in {file_path}");
    }
    if match_count > 1 {
        bail!(
            "search_string matched {match_count} times in {file_path}. Provide more surrounding context to make it unique."
        );
    }

    let updated = content.replacen(search, replace, 1);
    Ok((target_path, content, updated))
}

pub fn execute(json_args: &Value) -> Result<String> {
    let args: EditFileArgs = parse_args(json_args, "edit_file")?;
    let (target_path, _, updated) =
        apply_edit(&args.file_path, &args.search_string, &args.replace_string)?;

    fs::write(&target_path, updated).context("Error writing file")?;
    Ok(format!("Successfully edited {}", args.file_path))
}
