//! Manual tool invocation parsing: converts `#tool args` user input into a
//! `ToolCall` with positional argument mapping.

use super::{
    ALL_TOOL_NAMES, LOAD_TOOLS_NAME, ToolCall, ToolFunction, apply_positional_manual_arg,
    format_call_summary, tool_default_action,
};
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

    if !map.contains_key("action")
        && let Some(act) = tool_default_action(tool_name)
    {
        map.insert("action".into(), Value::String(act.into()));
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

    #[test]
    fn manual_clipboard_positional_actions_and_args_map_correctly() {
        for action in ["read", "write", "read_image", "write_image", "clear"] {
            assert_eq!(
                parsed_action(&format!("clipboard {action}")),
                Some(action.to_string()),
                "positional token '{action}' should map to action"
            );
        }

        let (write_call, _) = parse_manual_tool_call("clipboard write 'hello world'")
            .expect("clipboard write should parse");
        assert_eq!(
            write_call.function.arguments,
            serde_json::json!({ "action": "write", "text": "hello world" })
        );

        let (read_img_call, _) = parse_manual_tool_call("clipboard read_image shots/clip.png")
            .expect("clipboard read_image should parse");
        assert_eq!(
            read_img_call.function.arguments,
            serde_json::json!({ "action": "read_image", "file_path": "shots/clip.png" })
        );

        let (write_img_call, _) = parse_manual_tool_call("clipboard:write_image assets/logo.png")
            .expect("clipboard:write_image should parse");
        assert_eq!(
            write_img_call.function.arguments,
            serde_json::json!({ "action": "write_image", "file_path": "assets/logo.png" })
        );
    }
}
