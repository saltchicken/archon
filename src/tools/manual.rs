//! Manual tool invocation parsing: converts `#tool args` user input into a
//! `ToolCall` with positional argument mapping.

use super::{ALL_TOOL_NAMES, LOAD_TOOLS_NAME, ToolCall, ToolFunction, format_call_summary};
use anyhow::{Context, bail};
use serde_json::{Map, Value};

fn parse_manual_arg_value(raw: &str) -> Value {
    match raw {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => {
            if let Ok(n) = raw.parse::<i64>() {
                Value::Number(n.into())
            } else if let Ok(n) = raw.parse::<u64>() {
                Value::Number(n.into())
            } else if let Ok(f) = raw.parse::<f64>()
                && let Some(num) = serde_json::Number::from_f64(f)
            {
                Value::Number(num)
            } else if (raw.starts_with('[') && raw.ends_with(']'))
                || (raw.starts_with('{') && raw.ends_with('}'))
            {
                serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
            } else {
                Value::String(raw.to_string())
            }
        }
    }
}

fn apply_positional_manual_arg(tool_name: &str, token: &str, map: &mut Map<String, Value>) {
    if tool_name == "read_file" && map.contains_key("file_path") {
        if let Ok(n) = token.parse::<usize>()
            && let Some(&k) = ["start_line", "end_line"]
                .iter()
                .find(|&&k| !map.contains_key(k))
        {
            map.insert(k.into(), Value::Number(n.into()));
        }
        return;
    }
    if tool_name == "workspace_snapshot" {
        if matches!(token, "tree" | "tree_only") && !map.contains_key("tree_only") {
            map.insert("tree_only".into(), Value::Bool(true));
        }
        return;
    }
    if tool_name == LOAD_TOOLS_NAME {
        let entry = map
            .entry("names".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(arr) = entry {
            arr.push(Value::String(token.to_string()));
        }
        return;
    }

    let keys: &[&str] = match tool_name {
        "repo_map" if matches!(token, "map" | "definition" | "references") => {
            &["action", "symbol", "target_path"]
        }
        "repo_map" => &["symbol", "target_path"],
        "cargo" => &["action", "package"],
        "git" if !map.contains_key("action") => &["action"],
        "git" => match map.get("action").and_then(Value::as_str).unwrap_or("") {
            "blame" | "add" | "unstage" => &["path"],
            "commit" => &["message", "target", "path"],
            _ => &["target", "path"],
        },
        "fs" => &["action", "path", "destination"],
        "github_repo"
            if !map.contains_key("action")
                && matches!(token, "fetch" | "list" | "read" | "search") =>
        {
            &["action"]
        }
        "github_repo" => match map.get("action").and_then(Value::as_str).unwrap_or("fetch") {
            "fetch" => &["repo", "git_ref"],
            "search" => &["repo", "pattern", "path"],
            _ => &["repo", "path"],
        },
        "dotfiles" if map.get("action").and_then(Value::as_str) == Some("sync") => &["content"],
        "dotfiles" => &["action", "path", "pattern"],
        "skill" => &["action", "name"],
        "memory" if !map.contains_key("action") => &["action"],
        "memory" if matches!(token, "global" | "workspace") => &["scope", "item"],
        "memory" => &["item"],
        "notify" if matches!(token, "low" | "normal" | "critical") => {
            &["urgency", "summary", "body"]
        }
        "notify" => &["summary", "body"],
        "system" => &["action", "query", "target"],
        "read_file" => &["file_path"],
        "search" => &["pattern", "path"],
        "fetch_url" => &["url"],
        "curl" => &["url", "method", "body", "json"],
        "process"
            if !map.contains_key("action")
                && matches!(token, "list" | "output" | "poll" | "kill" | "clean") =>
        {
            &["action"]
        }
        "process" => &["id"],
        "browser"
            if !map.contains_key("action")
                && matches!(
                    token,
                    "goto"
                        | "read"
                        | "click"
                        | "type"
                        | "paste"
                        | "eval"
                        | "extract"
                        | "find"
                        | "screenshot"
                        | "snapshot"
                        | "status"
                        | "close"
                ) =>
        {
            &["action"]
        }
        "browser" => match map.get("action").and_then(Value::as_str).unwrap_or("") {
            "goto" => &["url", "selector"],
            "read" | "click" => &["selector"],
            "type" => &["selector", "text"],
            "paste" => &["selector", "text", "source"],
            "eval" => &["script"],
            "screenshot" => &["selector", "path"],
            _ => &[],
        },
        "create_file" => &["file_path", "content"],
        _ => &[],
    };

    if let Some(&k) = keys.iter().find(|&&k| !map.contains_key(k)) {
        map.insert(k.into(), Value::String(token.to_string()));
    }
}

pub fn parse_manual_tool_call(raw_after_hash: &str) -> Result<(ToolCall, String), anyhow::Error> {
    let trimmed = raw_after_hash.trim();
    if trimmed.is_empty() {
        bail!("Manual tool invocation cannot be empty");
    }

    let (tool_token, args_str) = match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim()),
        None => (trimmed, ""),
    };

    let (raw_name, colon_action) = match tool_token.split_once(':') {
        Some((n, a)) => (n.trim(), Some(a.trim()).filter(|s| !s.is_empty())),
        None => (tool_token.trim(), None),
    };

    let tool_name = if raw_name.eq_ignore_ascii_case(LOAD_TOOLS_NAME) {
        LOAD_TOOLS_NAME
    } else if let Some(&found) = ALL_TOOL_NAMES
        .iter()
        .find(|&&name| name.eq_ignore_ascii_case(raw_name))
    {
        found
    } else {
        bail!(
            "Unknown tool '{}'. Available tools: {}",
            raw_name,
            ALL_TOOL_NAMES.join(", ")
        );
    };

    let mut map = if args_str.starts_with('{') {
        serde_json::from_str::<Map<String, Value>>(args_str)
            .with_context(|| format!("Invalid JSON arguments for #{tool_name}"))?
    } else {
        let mut m = Map::new();
        if let Some(act) = colon_action {
            m.insert("action".into(), Value::String(act.to_string()));
        }
        if !args_str.is_empty() {
            let tokens = shlex::split(args_str)
                .ok_or_else(|| anyhow::anyhow!("Unmatched quotes in arguments for #{tool_name}"))?;
            for token in tokens {
                if let Some((k, v)) = token.split_once('=')
                    && !k.is_empty()
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    m.insert(k.to_string(), parse_manual_arg_value(v));
                } else {
                    apply_positional_manual_arg(tool_name, &token, &mut m);
                }
            }
        }
        m
    };

    if let Some(act) = colon_action {
        map.entry("action".to_string())
            .or_insert_with(|| Value::String(act.to_string()));
    }

    if !map.contains_key("action") {
        let default_action = match tool_name {
            "git" => Some("status"),
            "cargo" => Some("check"),
            "fs" | "memory" | "skill" | "dotfiles" | "process" => Some("list"),
            "github_repo" => Some("fetch"),
            "system" => Some("overview"),
            _ => None,
        };
        if let Some(act) = default_action {
            map.insert("action".into(), Value::String(act.into()));
        }
    }

    let call = ToolCall {
        function: ToolFunction {
            name: tool_name.to_string(),
            arguments: Value::Object(map),
        },
        thought_signature: None,
    };
    let summary = format_call_summary(&call);
    Ok((call, summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_action(raw: &str) -> Option<String> {
        let (call, _) = parse_manual_tool_call(raw).expect("manual invocation should parse");
        call.function
            .arguments
            .get("action")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    #[test]
    fn manual_browser_positional_actions_map_to_action_key() {
        for action in [
            "goto",
            "read",
            "click",
            "type",
            "paste",
            "eval",
            "extract",
            "find",
            "screenshot",
            "snapshot",
            "status",
            "close",
        ] {
            assert_eq!(
                parsed_action(&format!("browser {action}")),
                Some(action.to_string()),
                "positional token '{action}' should map to action"
            );
        }
    }

    #[test]
    fn manual_browser_positional_extract_keeps_selector() {
        let (call, _) = parse_manual_tool_call("browser extract selector=#main")
            .expect("manual invocation should parse");
        assert_eq!(
            call.function
                .arguments
                .get("action")
                .and_then(Value::as_str),
            Some("extract")
        );
        assert_eq!(
            call.function
                .arguments
                .get("selector")
                .and_then(Value::as_str),
            Some("#main")
        );
    }

    #[test]
    fn manual_browser_positional_paste_maps_selector_text_source() {
        let (call, _) =
            parse_manual_tool_call("browser paste '#inp' hello workspace_snapshot_tree")
                .expect("manual invocation should parse");
        assert_eq!(
            call.function
                .arguments
                .get("action")
                .and_then(Value::as_str),
            Some("paste")
        );
        assert_eq!(
            call.function
                .arguments
                .get("selector")
                .and_then(Value::as_str),
            Some("#inp")
        );
        assert_eq!(
            call.function.arguments.get("text").and_then(Value::as_str),
            Some("hello")
        );
        assert_eq!(
            call.function
                .arguments
                .get("source")
                .and_then(Value::as_str),
            Some("workspace_snapshot_tree")
        );
    }

    #[test]
    fn manual_browser_colon_action_still_works() {
        assert_eq!(parsed_action("browser:snapshot"), Some("snapshot".into()));
    }

    #[test]
    fn manual_load_tools_positional_args_collect_into_names_array() {
        let (call, _) =
            parse_manual_tool_call("load_tools git @web").expect("manual invocation should parse");
        assert_eq!(call.function.name, "load_tools");
        assert_eq!(
            call.function.arguments,
            serde_json::json!({ "names": ["git", "@web"] })
        );
    }
}
