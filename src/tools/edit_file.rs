use crate::tools::{atomic_write_file, auto_schema, parse_args, resolve_existing_path};
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
    /// Disjoint search/replace hunks to apply atomically to the file in a single pass.
    edits: Vec<EditHunk>,
}

pub fn schema() -> Value {
    auto_schema::<EditFileArgs>(
        "edit_file",
        "Edit a file by applying one or more disjoint, exact, unique search/replace hunks atomically.",
    )
}

fn normalize_eol(s: &str, use_crlf: bool) -> String {
    let lf = s.replace("\r\n", "\n");
    if use_crlf {
        lf.replace('\n', "\r\n")
    } else {
        lf
    }
}

fn locate_hunk_span(
    content: &str,
    file_path: &str,
    search: &str,
    replace: &str,
    hunk_label: &str,
) -> Result<(usize, usize, String)> {
    if search.is_empty() {
        bail!("{hunk_label}: search_string cannot be empty");
    }

    let mut matches: Vec<usize> = content.match_indices(search).map(|(i, _)| i).collect();
    let mut effective_search = Cow::Borrowed(search);
    let mut effective_replace = Cow::Borrowed(replace);

    if matches.is_empty() && search.contains('\n') {
        let target_crlf = content.contains("\r\n");
        let alt_search = normalize_eol(search, target_crlf);
        let alt_matches: Vec<usize> = content.match_indices(&alt_search).map(|(i, _)| i).collect();
        if !alt_matches.is_empty() {
            matches = alt_matches;
            effective_search = Cow::Owned(alt_search);
            effective_replace = Cow::Owned(normalize_eol(replace, target_crlf));
        }
    }

    match matches.as_slice() {
        [] => bail!("{hunk_label}: exact search_string not found in {file_path}"),
        &[start] => Ok((
            start,
            start + effective_search.len(),
            effective_replace.into_owned(),
        )),
        n => bail!(
            "{hunk_label}: search_string matched {} times in {file_path}. Provide more surrounding context to make it unique.",
            n.len()
        ),
    }
}

pub fn apply_hunks_to_str(content: &str, file_path: &str, edits: &[EditHunk]) -> Result<String> {
    if edits.is_empty() {
        bail!("'edits' array cannot be empty");
    }

    let total = edits.len();
    let label_for = |idx: usize| {
        if total == 1 {
            "Edit".into()
        } else {
            format!("Hunk {}/{total}", idx + 1)
        }
    };

    let mut resolved: Vec<(usize, usize, usize, String)> = Vec::with_capacity(total);
    for (idx, hunk) in edits.iter().enumerate() {
        let (start, end, replacement) = locate_hunk_span(
            content,
            file_path,
            &hunk.search_string,
            &hunk.replace_string,
            &label_for(idx),
        )?;
        for &(prev_idx, prev_start, prev_end, _) in &resolved {
            if start < prev_end && prev_start < end {
                bail!(
                    "{} and {} overlap in {file_path}. Edits must be disjoint.",
                    label_for(prev_idx),
                    label_for(idx)
                );
            }
        }
        resolved.push((idx, start, end, replacement));
    }

    resolved.sort_by_key(|&(_, start, _, _)| start);

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0;
    for (_, start, end, replacement) in resolved {
        out.push_str(&content[cursor..start]);
        out.push_str(&replacement);
        cursor = end;
    }
    out.push_str(&content[cursor..]);
    Ok(out)
}

pub fn apply_edits_from_value(json_args: &Value) -> Result<(PathBuf, String, String, usize)> {
    let args: EditFileArgs = parse_args(json_args, "edit_file")?;
    if args.edits.is_empty() {
        bail!("'edits' array cannot be empty");
    }

    let (_, target_path) = resolve_existing_path(&args.file_path)?;
    let original = fs::read_to_string(&target_path).context("Error reading file")?;
    let updated = apply_hunks_to_str(&original, args.file_path.trim(), &args.edits)?;

    Ok((target_path, original, updated, args.edits.len()))
}

pub fn fallback_hunk_preview(json_args: &Value) -> (String, String) {
    let Ok(args) = parse_args::<EditFileArgs>(json_args, "edit_file") else {
        return (String::new(), String::new());
    };
    let join_field = |f: fn(&EditHunk) -> &str| {
        args.edits
            .iter()
            .map(f)
            .collect::<Vec<_>>()
            .join("\n// ---\n")
    };
    (
        join_field(|h| &h.search_string),
        join_field(|h| &h.replace_string),
    )
}

pub fn execute(json_args: &Value) -> Result<String> {
    let args: EditFileArgs = parse_args(json_args, "edit_file")?;
    let (target_path, _, updated, hunk_count) = apply_edits_from_value(json_args)?;
    atomic_write_file(&target_path, updated).context("Error writing file")?;

    let path = args.file_path.trim();
    if hunk_count == 1 {
        Ok(format!("Successfully edited {path}"))
    } else {
        Ok(format!("Successfully applied {hunk_count} edits to {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_multiple_disjoint_hunks_out_of_order() {
        let original = "fn alpha() {\n    let a = 1;\n}\n\nfn beta() {\n    let b = 2;\n}\n";
        let edits = vec![
            EditHunk {
                search_string: "let b = 2;".into(),
                replace_string: "let b = 20;".into(),
            },
            EditHunk {
                search_string: "let a = 1;".into(),
                replace_string: "let a = 10;".into(),
            },
        ];

        let updated = apply_hunks_to_str(original, "test.rs", &edits).unwrap();
        assert_eq!(
            updated,
            "fn alpha() {\n    let a = 10;\n}\n\nfn beta() {\n    let b = 20;\n}\n"
        );
    }

    #[test]
    fn disjoint_hunks_do_not_interfere_with_each_other() {
        let original = "first = alpha\nsecond = beta\n";
        let edits = vec![
            EditHunk {
                search_string: "first = alpha".into(),
                replace_string: "first = second = beta".into(),
            },
            EditHunk {
                search_string: "second = beta".into(),
                replace_string: "second = gamma".into(),
            },
        ];

        let updated = apply_hunks_to_str(original, "test.txt", &edits).unwrap();
        assert_eq!(updated, "first = second = beta\nsecond = gamma\n");
    }

    #[test]
    fn rejects_overlapping_hunks() {
        let original = "line 1\nline 2\nline 3\n";
        let edits = vec![
            EditHunk {
                search_string: "line 1\nline 2".into(),
                replace_string: "a\nb".into(),
            },
            EditHunk {
                search_string: "line 2\nline 3".into(),
                replace_string: "c\nd".into(),
            },
        ];

        let err = apply_hunks_to_str(original, "test.txt", &edits).unwrap_err();
        assert!(err.to_string().contains("overlap in test.txt"));
    }

    #[test]
    fn atomic_write_creates_and_updates_without_leaving_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("script.sh");

        atomic_write_file(&file_path, "#!/bin/sh\necho old\n").unwrap();
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "#!/bin/sh\necho old\n"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&file_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&file_path, perms).unwrap();
        }

        atomic_write_file(&file_path, "#!/bin/sh\necho new\n").unwrap();
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "#!/bin/sh\necho new\n"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&file_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }

        let remaining_entries: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(remaining_entries.len(), 1);
    }
}
