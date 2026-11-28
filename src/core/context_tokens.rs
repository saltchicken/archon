//! Pre-flight context token estimation and provider-calibrated tracking.
//!
//! Measures the actual inputs that `Provider::build_turn_payload` will send to
//! the model — composed system prompt (base + memory + active tasks + deferred
//! tools + active skill), sanitized context messages (content, tool calls,
//! thoughts, image allowance), and serialized active tool JSON schemas — before
//! any network request is dispatched.

use crate::{
    core::{config::Config, protocol::TurnConfig, session::ChatMessage, skills},
    tools::{self, SessionToolState},
};
use serde_json::Value;

/// Conservative token allowance per attached image when estimating pre-flight
/// context usage (vision encoders typically consume ~258–1024 tokens per image).
pub const ESTIMATED_TOKENS_PER_IMAGE: u64 = 512;

/// Approximate bytes per token used for raw pre-flight estimation before any
/// provider-specific calibration is observed.
const BYTES_PER_TOKEN: usize = 4;

/// Approximate per-message framing/role overhead in bytes (~4 tokens).
const MESSAGE_OVERHEAD_BYTES: usize = 16;

/// Breakdown of an uncalibrated pre-flight context token estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextTokenBreakdown {
    pub system_tokens: u64,
    pub tool_schema_tokens: u64,
    pub message_tokens: u64,
    pub image_tokens: u64,
}

impl ContextTokenBreakdown {
    pub fn total(&self) -> u64 {
        self.system_tokens
            .saturating_add(self.tool_schema_tokens)
            .saturating_add(self.message_tokens)
            .saturating_add(self.image_tokens)
    }
}

/// Lightweight online calibration factor that aligns raw byte-based estimates
/// with the active model's actual reported `prompt_tokens`.
#[derive(Debug, Clone)]
pub struct TokenCalibration {
    scale: f64,
    samples: u32,
}

impl Default for TokenCalibration {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenCalibration {
    const MIN_SCALE: f64 = 0.5;
    const MAX_SCALE: f64 = 2.5;
    const EMA_ALPHA: f64 = 0.5;
    /// Ignore tiny prompts when calibrating so fixed framing noise doesn't skew
    /// the ratio for large contexts.
    const MIN_OBSERVATION_TOKENS: u64 = 64;

    pub fn new() -> Self {
        Self {
            scale: 1.0,
            samples: 0,
        }
    }

    pub fn reset(&mut self) {
        self.scale = 1.0;
        self.samples = 0;
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Updates the calibration factor from a completed turn's raw pre-flight
    /// estimate and the provider's exact `prompt_tokens`.
    pub fn observe(&mut self, raw_estimate: u64, actual_prompt_tokens: u64) {
        if raw_estimate < Self::MIN_OBSERVATION_TOKENS
            || actual_prompt_tokens < Self::MIN_OBSERVATION_TOKENS
        {
            return;
        }
        let ratio = (actual_prompt_tokens as f64 / raw_estimate as f64)
            .clamp(Self::MIN_SCALE, Self::MAX_SCALE);
        if self.samples == 0 {
            self.scale = ratio;
        } else {
            self.scale = (Self::EMA_ALPHA * ratio + (1.0 - Self::EMA_ALPHA) * self.scale)
                .clamp(Self::MIN_SCALE, Self::MAX_SCALE);
        }
        self.samples = self.samples.saturating_add(1);
    }

    /// Applies the current calibration factor to a raw token estimate.
    pub fn apply(&self, raw_estimate: u64) -> u64 {
        if raw_estimate == 0 {
            return 0;
        }
        ((raw_estimate as f64) * self.scale).round().max(1.0) as u64
    }
}

#[inline]
fn bytes_to_tokens(bytes: usize) -> u64 {
    if bytes == 0 {
        0
    } else {
        bytes.div_ceil(BYTES_PER_TOKEN) as u64
    }
}

/// Zero-allocation estimate of the compact JSON serialized byte length of `val`.
pub fn estimate_json_bytes(val: &Value) -> usize {
    match val {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                // Decimal digits plus optional minus sign.
                if i == 0 {
                    1
                } else {
                    let sign = usize::from(i < 0);
                    (i.unsigned_abs().ilog10() as usize) + 1 + sign
                }
            } else if let Some(u) = n.as_u64() {
                if u == 0 { 1 } else { (u.ilog10() as usize) + 1 }
            } else {
                // Float fallback.
                8
            }
        }
        Value::String(s) => s.len() + 2,
        Value::Array(items) => {
            if items.is_empty() {
                2
            } else {
                2 + items.len().saturating_sub(1)
                    + items.iter().map(estimate_json_bytes).sum::<usize>()
            }
        }
        Value::Object(map) => {
            if map.is_empty() {
                2
            } else {
                2 + map.len().saturating_sub(1)
                    + map
                        .iter()
                        .map(|(k, v)| k.len() + 3 + estimate_json_bytes(v))
                        .sum::<usize>()
            }
        }
    }
}

/// Estimates `(message_text_and_struct_tokens, image_tokens)` for `messages`
/// using the same inclusion/exclusion semantics as
/// `provider::sanitize_messages_for_turn`.
pub fn estimate_messages_tokens(messages: &[ChatMessage]) -> (u64, u64) {
    let mut total_bytes: usize = 0;
    let mut total_images: u64 = 0;

    for m in messages {
        match m.role.as_str() {
            "assistant" => {
                let has_content = !m.content.trim().is_empty();
                let has_tools = m.tool_calls.as_ref().is_some_and(|t| !t.is_empty());
                let has_thoughts = m.thoughts.as_ref().is_some_and(|t| !t.is_empty());
                if !(has_content || has_tools || has_thoughts) {
                    continue;
                }
                total_bytes += MESSAGE_OVERHEAD_BYTES + m.content.len();
                if let Some(thoughts) = &m.thoughts {
                    for tp in thoughts {
                        if let Some(text) = &tp.text {
                            total_bytes += text.len() + 8;
                        }
                    }
                }
                if let Some(calls) = &m.tool_calls {
                    for call in calls {
                        total_bytes += 16
                            + call.function.name.len()
                            + estimate_json_bytes(&call.function.arguments);
                    }
                }
            }
            "local" if m.include_in_context => {
                let label_len = m.tool_name.as_deref().map_or(0, str::len);
                // Accounts for `<tool_output tool="...">...</tool_output>` or
                // `<shell_command_output>...</shell_command_output>` wrapper.
                total_bytes += MESSAGE_OVERHEAD_BYTES + 48 + label_len + m.content.len();
            }
            "hook" if m.include_in_context => {
                let label_len = m.tool_name.as_deref().unwrap_or("hook").len();
                total_bytes += MESSAGE_OVERHEAD_BYTES + 40 + label_len + m.content.len();
            }
            "local" | "hook" | "error" => {}
            _ => {
                total_bytes += MESSAGE_OVERHEAD_BYTES + m.content.len();
                if let Some(name) = m.tool_name.as_deref().filter(|n| !n.is_empty()) {
                    total_bytes += name.len() + 12;
                }
                let img_count = m
                    .image_names
                    .as_ref()
                    .map(Vec::len)
                    .or_else(|| m.images.as_ref().map(Vec::len))
                    .unwrap_or(0);
                total_images = total_images.saturating_add(img_count as u64);
            }
        }
    }

    (
        bytes_to_tokens(total_bytes),
        total_images.saturating_mul(ESTIMATED_TOKENS_PER_IMAGE),
    )
}

/// Computes an uncalibrated pre-flight token breakdown for the exact payload
/// that `Provider::build_turn_payload` would construct for this turn.
pub fn estimate_turn_tokens(
    config: &Config,
    messages: &[ChatMessage],
    turn: &TurnConfig,
    session_id: Option<&str>,
    session_tools: &SessionToolState,
) -> ContextTokenBreakdown {
    let active_skill = turn.active_skill.as_deref();
    let effective_system_prompt = skills::compose_system_prompt_for_turn(
        &turn.system_prompt,
        active_skill,
        turn.skill_args.as_deref(),
        turn.raw_skill_args.as_deref(),
        &turn.skill_inputs,
        session_id,
        config,
        turn.enable_tools,
        session_tools,
    );

    let system_tokens = if effective_system_prompt.trim().is_empty() {
        0
    } else {
        bytes_to_tokens(effective_system_prompt.len() + MESSAGE_OVERHEAD_BYTES)
    };

    let tool_schema_tokens = if turn.enable_tools {
        let schemas = tools::schemas_for_turn(config, session_tools, active_skill);
        let schema_bytes: usize = schemas.iter().map(estimate_json_bytes).sum();
        bytes_to_tokens(schema_bytes)
    } else {
        0
    };

    let (message_tokens, image_tokens) = estimate_messages_tokens(messages);

    ContextTokenBreakdown {
        system_tokens,
        tool_schema_tokens,
        message_tokens,
        image_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::session::ThoughtPart,
        tools::{ToolCall, ToolFunction},
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    fn sample_turn(enable_tools: bool, system_prompt: &str) -> TurnConfig {
        TurnConfig {
            system_prompt: system_prompt.into(),
            num_ctx: 32_768,
            enable_tools,
            active_skill: None,
            skill_args: None,
            raw_skill_args: None,
            skill_inputs: BTreeMap::new(),
        }
    }

    #[test]
    fn estimate_json_bytes_matches_serialized_lengths() {
        let samples = [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(42),
            json!(-12345),
            json!("hello world"),
            json!([]),
            json!({}),
            json!({
                "path": "src/main.rs",
                "lines": [1, 20, 300],
                "nested": { "flag": true, "empty": null }
            }),
        ];
        for val in samples {
            let exact = serde_json::to_string(&val).unwrap().len();
            assert_eq!(estimate_json_bytes(&val), exact, "mismatch for {val}");
        }
    }

    #[test]
    fn estimate_messages_filters_excluded_roles_and_counts_tools_thoughts_and_images() {
        let messages = vec![
            ChatMessage::new("error", "x".repeat(4000)),
            ChatMessage {
                include_in_context: false,
                ..ChatMessage::new("local", "y".repeat(4000))
            },
            ChatMessage {
                include_in_context: false,
                ..ChatMessage::new_hook("after_hook", "z".repeat(4000), false)
            },
            // Empty assistant message is skipped by sanitize_messages_for_turn.
            ChatMessage::new("assistant", "   "),
        ];
        assert_eq!(estimate_messages_tokens(&messages), (0, 0));

        let rich_messages = vec![
            ChatMessage {
                image_names: Some(vec!["a.png".into(), "b.png".into()]),
                ..ChatMessage::new("user", "Inspect these screenshots")
            },
            ChatMessage {
                thoughts: Some(vec![ThoughtPart {
                    text: Some("Let me check the files carefully.".into()),
                    thought_signature: Some("opaque_sig".into()),
                }]),
                tool_calls: Some(vec![ToolCall {
                    function: ToolFunction {
                        name: "read_file".into(),
                        arguments: json!({ "file_path": "src/core/turns.rs", "start_line": 1 }),
                    },
                    thought_signature: None,
                }]),
                ..ChatMessage::new("assistant", "Checking now.")
            },
            ChatMessage::new_tool("read_file", "a".repeat(800), false, false),
            ChatMessage {
                include_in_context: true,
                tool_name: Some("cargo".into()),
                ..ChatMessage::new("local", "b".repeat(400))
            },
            ChatMessage {
                include_in_context: true,
                ..ChatMessage::new_hook("pre_hook", "c".repeat(400), false)
            },
        ];

        let (msg_tokens, img_tokens) = estimate_messages_tokens(&rich_messages);
        assert_eq!(img_tokens, 2 * ESTIMATED_TOKENS_PER_IMAGE);
        // 800 + 400 + 400 = 1600 content bytes alone => >400 tokens.
        assert!(
            msg_tokens > 420,
            "expected >420 msg tokens, got {msg_tokens}"
        );

        // Fallback to hydrated `images` when `image_names` is None.
        let hydrated_only = vec![ChatMessage {
            images: Some(vec!["base64data".into()]),
            image_names: None,
            ..ChatMessage::new("user", "hi")
        }];
        let (_, hydrated_img_tokens) = estimate_messages_tokens(&hydrated_only);
        assert_eq!(hydrated_img_tokens, ESTIMATED_TOKENS_PER_IMAGE);
    }

    #[test]
    fn estimate_turn_tokens_includes_system_prompt_and_tool_schemas() {
        let cfg = Config::default();
        let mut session_tools = SessionToolState::default();
        let no_tools_turn = sample_turn(false, "");
        let empty = estimate_turn_tokens(&cfg, &[], &no_tools_turn, None, &session_tools);
        assert_eq!(empty.tool_schema_tokens, 0);
        assert_eq!(empty.message_tokens, 0);
        assert_eq!(empty.image_tokens, 0);

        let with_sys = sample_turn(false, &"System prompt text. ".repeat(20));
        let sys_est = estimate_turn_tokens(&cfg, &[], &with_sys, None, &session_tools);
        assert!(sys_est.system_tokens > empty.system_tokens);

        let with_tools = sample_turn(true, "");
        let tools_est = estimate_turn_tokens(&cfg, &[], &with_tools, None, &session_tools);
        assert!(
            tools_est.tool_schema_tokens > 0,
            "expected positive tool schema tokens when enable_tools is true"
        );

        // Dynamically loading additional deferred tools increases tool schema tokens.
        session_tools.apply_loaded(&["git".into(), "cargo".into()], 6, 16);
        let more_tools_est = estimate_turn_tokens(&cfg, &[], &with_tools, None, &session_tools);
        assert!(
            more_tools_est.tool_schema_tokens > tools_est.tool_schema_tokens,
            "expected loaded tools to increase tool_schema_tokens ({} > {})",
            more_tools_est.tool_schema_tokens,
            tools_est.tool_schema_tokens
        );
    }

    #[test]
    fn token_calibration_converges_clamps_and_resets() {
        let mut cal = TokenCalibration::new();
        assert_eq!(cal.apply(0), 0);
        assert_eq!(cal.apply(1000), 1000);

        // Ignores tiny samples below threshold.
        cal.observe(10, 500);
        assert!((cal.scale() - 1.0).abs() < 1e-9);

        // First valid sample sets scale directly.
        cal.observe(1000, 1200);
        assert!((cal.scale() - 1.2).abs() < 1e-9);
        assert_eq!(cal.apply(1000), 1200);

        // Subsequent sample blends via EMA (alpha = 0.5).
        cal.observe(1000, 1400);
        assert!((cal.scale() - 1.3).abs() < 1e-9);

        // Extreme ratios are clamped to [0.5, 2.5].
        let mut high = TokenCalibration::new();
        high.observe(100, 10_000);
        assert!((high.scale() - 2.5).abs() < 1e-9);

        let mut low = TokenCalibration::new();
        low.observe(10_000, 100);
        assert!((low.scale() - 0.5).abs() < 1e-9);

        high.reset();
        assert!((high.scale() - 1.0).abs() < 1e-9);
    }
}
