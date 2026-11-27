use super::{BlockingTool, ToolContext, utils::path::ensure_parent_dir};
use crate::core::attachments::{
    clear_clipboard, copy_image_to_clipboard, copy_text_to_clipboard, read_image_from_clipboard,
    read_text_from_clipboard,
};
use anyhow::{Context, Result, bail};
use image::ImageFormat;
use schemars::JsonSchema;
use serde::Deserialize;
use std::fs;

pub struct Tool;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardAction {
    /// Read current plain-text contents from the system clipboard (default).
    #[default]
    Read,
    /// Copy `text` to the system clipboard.
    Write,
    /// Read an image from the system clipboard and save it as PNG to `file_path` in the workspace.
    ReadImage,
    /// Copy an image file at `file_path` to the system clipboard.
    WriteImage,
    /// Clear the system clipboard.
    Clear,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipboardArgs {
    /// Operation to perform: `read` (default), `write`, `read_image`, `write_image`, or `clear`.
    #[serde(default)]
    pub action: ClipboardAction,
    /// Text to copy to the clipboard (required for `write`).
    #[serde(default)]
    pub text: Option<String>,
    /// Workspace-relative destination path for `read_image`, or readable image path for `write_image`.
    #[serde(default)]
    pub file_path: Option<String>,
}

impl BlockingTool for Tool {
    const NAME: &'static str = "clipboard";
    const DESCRIPTION: &'static str = "Read/write text or images with the system clipboard (`read`, `write`, `read_image`, `write_image`, `clear`).";
    type Args = ClipboardArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        match args.action {
            ClipboardAction::Read => read_text_from_clipboard(),
            ClipboardAction::Write => {
                let text = args
                    .text
                    .as_deref()
                    .context("`clipboard:write` requires `text`.")?;
                if text.is_empty() {
                    bail!(
                        "`clipboard:write` cannot write empty text; use `action: \"clear\"` instead."
                    );
                }
                copy_text_to_clipboard(text)?;
                Ok(format!("Copied {} byte(s) to clipboard.", text.len()))
            }
            ClipboardAction::ReadImage => {
                let raw_path = args
                    .file_path
                    .as_deref()
                    .context("`clipboard:read_image` requires `file_path`.")?;
                let dest = ctx.resolve_new_path(raw_path)?;
                let img = read_image_from_clipboard()?;
                ensure_parent_dir(&dest, &ctx.root, "clipboard image path")?;
                img.save_with_format(&dest, ImageFormat::Png)
                    .with_context(|| {
                        format!("Failed to save clipboard image to {}", dest.display())
                    })?;
                let size_kb = (fs::metadata(&dest).map_or(0, |m| m.len()) / 1024).max(1);
                Ok(format!(
                    "Saved clipboard image ({}x{}, {} KB) to '{}'.",
                    img.width(),
                    img.height(),
                    size_kb,
                    raw_path.trim()
                ))
            }
            ClipboardAction::WriteImage => {
                let raw_path = args
                    .file_path
                    .as_deref()
                    .context("`clipboard:write_image` requires `file_path`.")?;
                let src = ctx.resolve_readable_path(raw_path)?;
                let (w, h) = copy_image_to_clipboard(&src)?;
                Ok(format!(
                    "Copied image '{}' ({w}x{h}) to clipboard.",
                    raw_path.trim()
                ))
            }
            ClipboardAction::Clear => {
                clear_clipboard()?;
                Ok("Clipboard cleared.".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::tools::{
        SessionToolState, ToolCall, ToolFunction, call_signature, is_auto_approved,
    };
    use serde_json::json;

    fn test_ctx(tmp: &tempfile::TempDir) -> ToolContext {
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let cfg = Config {
            workspace: Some(root.to_string_lossy().into_owned()),
            ..Config::default()
        };
        ToolContext {
            root,
            max_output_size: 32_768,
            active_skill: None,
            session_id: None,
            session_tools: SessionToolState::default(),
            config: cfg,
        }
    }

    fn make_call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            function: ToolFunction {
                name: "clipboard".into(),
                arguments,
            },
            thought_signature: None,
        }
    }

    #[test]
    fn clipboard_args_deserialization_defaults_and_actions() {
        let default_args: ClipboardArgs = serde_json::from_value(json!({})).unwrap();
        assert_eq!(default_args.action, ClipboardAction::Read);
        assert!(default_args.text.is_none());
        assert!(default_args.file_path.is_none());

        for (raw, expected) in [
            ("read", ClipboardAction::Read),
            ("write", ClipboardAction::Write),
            ("read_image", ClipboardAction::ReadImage),
            ("write_image", ClipboardAction::WriteImage),
            ("clear", ClipboardAction::Clear),
        ] {
            let parsed: ClipboardArgs = serde_json::from_value(json!({ "action": raw })).unwrap();
            assert_eq!(parsed.action, expected);
        }
    }

    #[test]
    fn clipboard_write_validates_required_and_non_empty_text() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&tmp);

        let missing = Tool::execute(
            ClipboardArgs {
                action: ClipboardAction::Write,
                text: None,
                file_path: None,
            },
            &ctx,
        );
        assert!(missing.unwrap_err().to_string().contains("requires `text`"));

        let empty = Tool::execute(
            ClipboardArgs {
                action: ClipboardAction::Write,
                text: Some(String::new()),
                file_path: None,
            },
            &ctx,
        );
        assert!(empty.unwrap_err().to_string().contains("clear"));
    }

    #[test]
    fn clipboard_image_actions_validate_paths_before_touching_clipboard() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&tmp);
        fs::write(tmp.path().join("existing.png"), b"not-an-image").unwrap();
        fs::write(tmp.path().join(".env"), b"SECRET=1").unwrap();

        let missing_dest = Tool::execute(
            ClipboardArgs {
                action: ClipboardAction::ReadImage,
                text: None,
                file_path: None,
            },
            &ctx,
        );
        assert!(
            missing_dest
                .unwrap_err()
                .to_string()
                .contains("requires `file_path`")
        );

        let traversal = Tool::execute(
            ClipboardArgs {
                action: ClipboardAction::ReadImage,
                text: None,
                file_path: Some("../escape.png".into()),
            },
            &ctx,
        );
        assert!(traversal.is_err());

        let existing = Tool::execute(
            ClipboardArgs {
                action: ClipboardAction::ReadImage,
                text: None,
                file_path: Some("existing.png".into()),
            },
            &ctx,
        );
        assert!(existing.unwrap_err().to_string().contains("already exists"));

        let missing_src = Tool::execute(
            ClipboardArgs {
                action: ClipboardAction::WriteImage,
                text: None,
                file_path: Some("does-not-exist.png".into()),
            },
            &ctx,
        );
        assert!(
            missing_src
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );

        let excluded_src = Tool::execute(
            ClipboardArgs {
                action: ClipboardAction::WriteImage,
                text: None,
                file_path: Some(".env".into()),
            },
            &ctx,
        );
        assert!(
            excluded_src
                .unwrap_err()
                .to_string()
                .contains("Security violation")
        );
    }

    #[test]
    fn clipboard_call_signature_normalizes_omitted_action_and_is_not_default_auto_approved() {
        let omitted = make_call(json!({}));
        let explicit_read = make_call(json!({ "action": "read" }));
        let write = make_call(json!({ "action": "write", "text": "hi" }));
        let clear = make_call(json!({ "action": "clear" }));

        assert_eq!(call_signature(&omitted), "clipboard:read");
        assert_eq!(call_signature(&explicit_read), "clipboard:read");
        assert_eq!(call_signature(&write), "clipboard:write");
        assert_eq!(call_signature(&clear), "clipboard:clear");

        // Opt-in `clipboard:read` approves omitted/explicit reads, never writes or clear.
        assert!(is_auto_approved(&omitted, ["clipboard:read"]));
        assert!(is_auto_approved(&explicit_read, ["clipboard:read"]));
        assert!(!is_auto_approved(&write, ["clipboard:read"]));
        assert!(!is_auto_approved(&clear, ["clipboard:read"]));

        // Default config does NOT auto-approve any clipboard action.
        let cfg = Config::default();
        let defaults: Vec<&str> = cfg.app.auto_approve.iter().map(String::as_str).collect();
        assert!(!is_auto_approved(&omitted, defaults.iter().copied()));
        assert!(!is_auto_approved(&explicit_read, defaults.iter().copied()));
        assert!(!is_auto_approved(&write, defaults.iter().copied()));
    }
}
