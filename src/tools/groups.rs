//! Named tool groups (`@code`, `@git`, `@web`, etc.) and custom `[tool_groups]`
//! expansion helpers.

use super::ALL_TOOL_NAMES;
use std::collections::{BTreeMap, HashSet};

/// Returns the default tool groups that are seeded into `Config::default()`.
/// Users can override or remove any of these via `[tool_groups]` in their config.
pub fn default_tool_groups() -> BTreeMap<String, Vec<String>> {
    let mut m = BTreeMap::new();
    m.insert(
        "code".into(),
        vec![
            "read_file".into(),
            "edit_file".into(),
            "create_file".into(),
            "search".into(),
            "repo_map".into(),
            "fs".into(),
            "workspace_snapshot".into(),
        ],
    );
    m.insert("rust".into(), vec!["cargo".into()]);
    m.insert("git".into(), vec!["git".into(), "github_repo".into()]);
    m.insert(
        "web".into(),
        vec!["fetch_url".into(), "curl".into(), "browser".into()],
    );
    m.insert(
        "sysadmin".into(),
        vec![
            "system".into(),
            "dotfiles".into(),
            "process".into(),
            "schedule".into(),
            "notify".into(),
            "clipboard".into(),
        ],
    );
    m.insert(
        "data".into(),
        vec!["sqlite".into(), "memory".into(), "session".into()],
    );
    m.insert("planning".into(), vec!["task".into(), "todo".into()]);
    m.insert("agent".into(), vec!["skill".into(), "subagent".into()]);
    m
}

fn normalize_group_key(raw: &str) -> &str {
    raw.trim().trim_start_matches('@').trim()
}

fn lookup_raw_group_members<'a>(
    key: &str,
    custom_groups: &'a BTreeMap<String, Vec<String>>,
) -> Option<Vec<&'a str>> {
    custom_groups
        .iter()
        .find(|(k, _)| normalize_group_key(k).eq_ignore_ascii_case(key))
        .map(|(_, members)| members.iter().map(String::as_str).collect())
}

fn canonical_tool_name(raw: &str) -> Option<&'static str> {
    ALL_TOOL_NAMES
        .iter()
        .copied()
        .find(|&name| name.eq_ignore_ascii_case(raw))
}

fn resolve_group_recursive(
    key: &str,
    custom_groups: &BTreeMap<String, Vec<String>>,
    visiting: &mut HashSet<String>,
    out: &mut Vec<String>,
) -> bool {
    let lower = key.to_ascii_lowercase();
    if !visiting.insert(lower.clone()) {
        // Cycle guard: already expanding this group.
        return true;
    }

    let Some(members) = lookup_raw_group_members(key, custom_groups) else {
        visiting.remove(&lower);
        return false;
    };

    for raw in members {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        if let Some(sub) = item.strip_prefix('@') {
            let sub_key = sub.trim();
            if sub_key.is_empty() || !resolve_group_recursive(sub_key, custom_groups, visiting, out)
            {
                let unresolved = format!("@{sub_key}");
                if !out.iter().any(|existing| existing == &unresolved) {
                    out.push(unresolved);
                }
            }
        } else if item == "*" {
            for &name in ALL_TOOL_NAMES {
                if !out.iter().any(|existing| existing == name) {
                    out.push(name.to_string());
                }
            }
        } else {
            let canonical = canonical_tool_name(item).unwrap_or(item);
            if !out.iter().any(|existing| existing == canonical) {
                out.push(canonical.to_string());
            }
        }
    }

    visiting.remove(&lower);
    true
}

/// Resolves a group name (with or without leading `@`) against custom `[tool_groups]`
/// first, then built-in groups. Returns `None` if the top-level group does not exist.
///
/// Note: For custom groups, the returned list may contain unresolved nested `@...`
/// sentinel tokens or unrecognized tool names so validators (`expand_tool_spec_checked`,
/// `load_tools`, `skill_check`) can report them; non-validating callers should filter
/// entries starting with `@` or validate against `ALL_TOOL_NAMES`.
pub fn resolve_tool_group(
    group_name: &str,
    custom_groups: &BTreeMap<String, Vec<String>>,
) -> Option<Vec<String>> {
    let key = normalize_group_key(group_name);
    if key.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let mut visiting = HashSet::new();
    resolve_group_recursive(key, custom_groups, &mut visiting, &mut out).then_some(out)
}

/// Expands any `@group` entries in `specs` into their constituent tool names,
/// preserving `"*"` and individual tool names, deduplicated in insertion order.
pub fn expand_tool_list(
    specs: &[String],
    custom_groups: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut out = Vec::new();
    for raw in specs {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        if let Some(group_key) = item.strip_prefix('@') {
            if let Some(members) = resolve_tool_group(group_key, custom_groups) {
                for m in members {
                    if !m.starts_with('@') && !out.iter().any(|existing| existing == &m) {
                        out.push(m);
                    }
                }
            }
        } else {
            let canonical = canonical_tool_name(item).unwrap_or(item);
            if !out.iter().any(|existing| existing == canonical) {
                out.push(canonical.to_string());
            }
        }
    }
    out
}

/// Expands `specs` into concrete tool names (expanding `*` to all `ALL_TOOL_NAMES`
/// and `@group` to its members), returning `Err(unknown_entries)` if any entry is
/// neither `*`, a known tool in `ALL_TOOL_NAMES`, nor a known `@group`.
pub fn expand_tool_spec_checked(
    specs: &[String],
    custom_groups: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<String>, Vec<String>> {
    let mut out = Vec::new();
    let mut unknown = Vec::new();

    for raw in specs {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        if item == "*" {
            for &name in ALL_TOOL_NAMES {
                if !out.iter().any(|existing| existing == name) {
                    out.push(name.to_string());
                }
            }
        } else if let Some(group_key) = item.strip_prefix('@') {
            match resolve_tool_group(group_key, custom_groups) {
                Some(members) => {
                    for m in members {
                        if ALL_TOOL_NAMES.contains(&m.as_str()) {
                            if !out.iter().any(|existing| existing == &m) {
                                out.push(m);
                            }
                        } else if !unknown.iter().any(|u| u == &m) {
                            unknown.push(m);
                        }
                    }
                }
                None => {
                    if !unknown.iter().any(|u| u == item) {
                        unknown.push(item.to_string());
                    }
                }
            }
        } else if let Some(&canonical) = ALL_TOOL_NAMES
            .iter()
            .find(|&&name| name.eq_ignore_ascii_case(item))
        {
            if !out.iter().any(|existing| existing == canonical) {
                out.push(canonical.to_string());
            }
        } else if !unknown.iter().any(|u| u == item) {
            unknown.push(item.to_string());
        }
    }

    if unknown.is_empty() {
        Ok(out)
    } else {
        Err(unknown)
    }
}

/// Returns `true` if `spec` is a known built-in tool name or a known `@group`.
pub fn is_known_tool_or_group(spec: &str, custom_groups: &BTreeMap<String, Vec<String>>) -> bool {
    let trimmed = spec.trim();
    if let Some(group_key) = trimmed.strip_prefix('@') {
        resolve_tool_group(group_key, custom_groups).is_some()
    } else {
        ALL_TOOL_NAMES.contains(&trimmed)
    }
}

/// Returns sorted list of all available `@group` names from `custom_groups`.
pub fn all_group_names(custom_groups: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for raw_key in custom_groups.keys() {
        let key = normalize_group_key(raw_key);
        if key.is_empty() {
            continue;
        }
        let tagged = format!("@{}", key.to_ascii_lowercase());
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&tagged)) {
            names.push(tagged);
        }
    }
    names.sort();
    names
}

/// Formats a helpful list of valid tools and `@groups` for error messages.
pub fn format_valid_tools_and_groups(custom_groups: &BTreeMap<String, Vec<String>>) -> String {
    format!(
        "Valid tools: {}. Valid groups: {}",
        ALL_TOOL_NAMES.join(", "),
        all_group_names(custom_groups).join(", ")
    )
}
