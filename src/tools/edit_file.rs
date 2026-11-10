use crate::tools::{auto_schema, parse_args, resolve_existing_path};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fs;

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

pub fn execute(json_args: &Value) -> Result<String> {
    let args: EditFileArgs = parse_args(json_args, "edit_file")?;

    if args.search_string.is_empty() {
        bail!("search_string cannot be empty");
    }

    let (_, target_path) = resolve_existing_path(&args.file_path)?;
    let content = fs::read_to_string(&target_path).context("Error reading file")?;

    let match_count = content.matches(&args.search_string).count();
    if match_count == 0 {
        bail!("Exact search_string not found in {}", args.file_path);
    }
    if match_count > 1 {
        bail!(
            "search_string matched {} times in {}. Provide more surrounding context to make it unique.",
            match_count,
            args.file_path
        );
    }

    let updated = content.replacen(&args.search_string, &args.replace_string, 1);
    fs::write(&target_path, updated).context("Error writing file")?;

    Ok(format!("Successfully edited {}", args.file_path))
}
