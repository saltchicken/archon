use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::borrow::Cow;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditHunk {
    /// The exact, unique block of text to search for.
    pub search_string: String,
    /// The replacement text to substitute in place of `search_string`.
    pub replace_string: String,
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
