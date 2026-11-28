//! Typed argument parsing, coercion, and skill input resolution.

use super::types::{SkillArgDef, SkillArgType, SkillDefinition};
use crate::config::Config;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub fn format_arg_value(val: &Value) -> String {
    match val {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn match_choice(def: &SkillArgDef, candidate: &str) -> Result<String, String> {
    if def.choices.is_empty() {
        return Err(format!(
            "Argument '{}' has type 'enum' but declares no `choices`.",
            def.name
        ));
    }
    if let Some(exact) = def.choices.iter().find(|c| c.as_str() == candidate) {
        return Ok(exact.clone());
    }
    if let Some(ci) = def
        .choices
        .iter()
        .find(|c| c.eq_ignore_ascii_case(candidate))
    {
        return Ok(ci.clone());
    }
    Err(format!(
        "Invalid value '{candidate}' for argument '{}'. Expected one of: {}",
        def.name,
        def.choices.join(", ")
    ))
}

pub fn coerce_arg_value(def: &SkillArgDef, raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    match def.arg_type {
        SkillArgType::String => {
            if !def.choices.is_empty() {
                return Ok(Value::String(match_choice(def, trimmed)?));
            }
            Ok(Value::String(raw.to_string()))
        }
        SkillArgType::Path => {
            if trimmed.is_empty() {
                return Err(format!("Argument '{}' expects a non-empty path.", def.name));
            }
            if !def.choices.is_empty() {
                return Ok(Value::String(match_choice(def, trimmed)?));
            }
            Ok(Value::String(trimmed.to_string()))
        }
        SkillArgType::Int => trimmed
            .parse::<i64>()
            .map(|n| Value::Number(n.into()))
            .map_err(|_| {
                format!(
                    "Invalid value '{trimmed}' for argument '{}': expected an integer.",
                    def.name
                )
            }),
        SkillArgType::Float => {
            let f = trimmed.parse::<f64>().map_err(|_| {
                format!(
                    "Invalid value '{trimmed}' for argument '{}': expected a number.",
                    def.name
                )
            })?;
            if !f.is_finite() {
                return Err(format!(
                    "Invalid value '{trimmed}' for argument '{}': expected a finite number.",
                    def.name
                ));
            }
            serde_json::Number::from_f64(f)
                .map(Value::Number)
                .ok_or_else(|| {
                    format!(
                        "Invalid value '{trimmed}' for argument '{}': expected a finite number.",
                        def.name
                    )
                })
        }
        SkillArgType::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Ok(Value::Bool(true)),
            "false" | "no" | "0" | "off" => Ok(Value::Bool(false)),
            _ => Err(format!(
                "Invalid value '{trimmed}' for argument '{}': expected a boolean (true/false).",
                def.name
            )),
        },
        SkillArgType::Enum => Ok(Value::String(match_choice(def, trimmed)?)),
    }
}

pub fn validate_arg_json_value(def: &SkillArgDef, val: &Value) -> Result<Value, String> {
    match (def.arg_type, val) {
        (SkillArgType::String, Value::String(s)) => coerce_arg_value(def, s),
        (SkillArgType::Path, Value::String(s)) => coerce_arg_value(def, s),
        (SkillArgType::Enum, Value::String(s)) => coerce_arg_value(def, s),
        (SkillArgType::Int, Value::Number(n)) if n.is_i64() => Ok(val.clone()),
        (SkillArgType::Int, Value::Number(n))
            if n.as_u64().is_some_and(|u| u <= i64::MAX as u64) =>
        {
            Ok(Value::Number((n.as_u64().unwrap() as i64).into()))
        }
        (SkillArgType::Float, Value::Number(n)) if n.as_f64().is_some_and(f64::is_finite) => {
            Ok(val.clone())
        }
        (SkillArgType::Bool, Value::Bool(_)) => Ok(val.clone()),
        _ => Err(format!(
            "Default value {val} for argument '{}' does not match declared type '{}'.",
            def.name,
            def.arg_type.as_str()
        )),
    }
}

#[derive(Debug)]
struct ArgTokenSpan {
    start: usize,
    end: usize,
    key: Option<String>,
    value: String,
}

fn scan_quoted_segment(s: &str, start: usize) -> Option<(usize, String)> {
    let mut chars = s[start..].char_indices();
    let (_, quote) = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    let mut out = String::new();
    let mut escaped = false;
    for (rel_idx, ch) in chars {
        if escaped {
            match ch {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                other => out.push(other),
            }
            escaped = false;
            continue;
        }
        if quote == '"' && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some((start + rel_idx + ch.len_utf8(), out));
        }
        out.push(ch);
    }
    None
}

fn scan_arg_tokens(raw: &str) -> Vec<ArgTokenSpan> {
    let mut tokens = Vec::new();
    let len = raw.len();
    let mut pos = 0usize;

    while pos < len {
        let trimmed = raw[pos..].trim_start();
        pos = len - trimmed.len();
        if pos >= len {
            break;
        }

        let token_start = pos;
        if trimmed.starts_with(['"', '\''])
            && let Some((end, unquoted)) = scan_quoted_segment(raw, pos)
        {
            tokens.push(ArgTokenSpan {
                start: token_start,
                end,
                key: None,
                value: unquoted,
            });
            pos = end;
            continue;
        }

        let word_end = trimmed.find(char::is_whitespace).map_or(len, |i| pos + i);
        let word = &raw[pos..word_end];

        if let Some(eq_rel) = word.find('=')
            && eq_rel > 0
            && word[..eq_rel]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            let key = word[..eq_rel].to_string();
            let val_start = pos + eq_rel + 1;
            let (end, value) = if raw[val_start..].starts_with(['"', '\''])
                && let Some((q_end, unquoted)) = scan_quoted_segment(raw, val_start)
            {
                (q_end, unquoted)
            } else {
                (word_end, raw[val_start..word_end].to_string())
            };
            tokens.push(ArgTokenSpan {
                start: token_start,
                end,
                key: Some(key),
                value,
            });
            pos = end;
            continue;
        }

        tokens.push(ArgTokenSpan {
            start: token_start,
            end: word_end,
            key: None,
            value: word.to_string(),
        });
        pos = word_end;
    }

    tokens
}

fn excise_spans(raw: &str, consumed: &[(usize, usize)]) -> String {
    if consumed.is_empty() {
        return raw.trim().to_string();
    }
    let mut spans = consumed.to_vec();
    spans.sort_by_key(|(s, _)| *s);

    let mut out = String::with_capacity(raw.len());
    let mut cursor = 0usize;
    for (start, end) in spans {
        if start > cursor {
            out.push_str(&raw[cursor..start]);
        }
        cursor = end;
        if out.ends_with(' ')
            && let Some(rest) = raw.get(cursor..)
            && rest.starts_with(' ')
        {
            cursor += 1;
        }
    }
    if cursor < raw.len() {
        out.push_str(&raw[cursor..]);
    }
    out.trim().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkillInvocation {
    pub inputs: BTreeMap<String, Value>,
    pub remaining_prompt: String,
    pub raw_args: String,
}

pub fn resolve_skill_inputs(
    skill: &SkillDefinition,
    raw_args: &str,
    explicit_overrides: &[(String, String)],
) -> Result<ResolvedSkillInvocation, String> {
    let raw_trimmed = raw_args.trim().to_string();

    if skill.args.is_empty() {
        if let Some((k, _)) = explicit_overrides.first() {
            return Err(format!(
                "Skill '@{}' does not declare any typed arguments (unexpected argument '{k}').",
                skill.name
            ));
        }
        return Ok(ResolvedSkillInvocation {
            inputs: BTreeMap::new(),
            remaining_prompt: raw_trimmed.clone(),
            raw_args: raw_trimmed,
        });
    }

    let declared: std::collections::HashMap<String, &SkillArgDef> = skill
        .args
        .iter()
        .map(|a| (a.name.to_ascii_lowercase(), a))
        .collect();

    let mut inputs: BTreeMap<String, Value> = BTreeMap::new();

    for (k, v) in explicit_overrides {
        let Some(&def) = declared.get(&k.trim().to_ascii_lowercase()) else {
            let valid: Vec<&str> = skill.args.iter().map(|a| a.name.as_str()).collect();
            return Err(format!(
                "Unknown argument '{k}' for skill '@{}'. Declared arguments: {}",
                skill.name,
                valid.join(", ")
            ));
        };
        let coerced = coerce_arg_value(def, v)?;
        inputs.insert(def.name.clone(), coerced);
    }

    let tokens = scan_arg_tokens(raw_args);
    let mut consumed_indices = HashSet::new();
    for (idx, tok) in tokens.iter().enumerate() {
        if tok.key.is_none() && tok.value == "--" {
            consumed_indices.insert(idx);
            break;
        }
        if let Some(ref key) = tok.key
            && let Some(&def) = declared.get(&key.to_ascii_lowercase())
        {
            if inputs.contains_key(&def.name) {
                break;
            }
            let coerced = coerce_arg_value(def, &tok.value)?;
            inputs.insert(def.name.clone(), coerced);
            consumed_indices.insert(idx);
            continue;
        }
        if let Some(def) = skill
            .args
            .iter()
            .find(|d| d.required && !inputs.contains_key(&d.name))
        {
            let positional_raw = if tok.key.is_some() {
                &raw_args[tok.start..tok.end]
            } else {
                &tok.value
            };
            let coerced = coerce_arg_value(def, positional_raw)?;
            inputs.insert(def.name.clone(), coerced);
            consumed_indices.insert(idx);
        } else {
            break;
        }
    }

    let mut missing = Vec::new();
    for def in &skill.args {
        if inputs.contains_key(&def.name) {
            continue;
        }
        if def.required {
            let desc = if def.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", def.description)
            };
            missing.push(format!("'{}' ({}){desc}", def.name, def.arg_type.as_str()));
        } else if let Some(ref default_val) = def.default {
            let valid = validate_arg_json_value(def, default_val)?;
            inputs.insert(def.name.clone(), valid);
        }
    }

    if !missing.is_empty() {
        return Err(format!(
            "Skill '@{}' is missing required argument(s): {}. Usage: @{} {}",
            skill.name,
            missing.join("; "),
            skill.trigger(),
            skill.args_usage_hint()
        ));
    }

    let consumed_spans: Vec<(usize, usize)> = consumed_indices
        .into_iter()
        .map(|i| (tokens[i].start, tokens[i].end))
        .collect();
    let remaining_prompt = excise_spans(raw_args, &consumed_spans);

    Ok(ResolvedSkillInvocation {
        inputs,
        remaining_prompt,
        raw_args: raw_trimmed,
    })
}

pub fn activate_skill_for_turn(
    skill_name: &str,
    raw_args: &str,
    explicit_overrides: &[(String, String)],
    turn: &mut crate::engine::TurnConfig,
    config: &Config,
) -> Result<String, String> {
    let canonical_name =
        super::resolve::check_skill_activation(skill_name, turn.enable_tools, config)?;
    let skill = super::parse::find_skill(config, &canonical_name)
        .ok_or_else(|| format!("Skill '@{canonical_name}' not found."))?;
    let invocation = resolve_skill_inputs(skill, raw_args, explicit_overrides)?;

    turn.active_skill = Some(canonical_name);
    turn.skill_args =
        (!invocation.remaining_prompt.is_empty()).then(|| invocation.remaining_prompt.clone());
    turn.raw_skill_args = (!invocation.raw_args.is_empty()).then_some(invocation.raw_args);
    turn.skill_inputs = invocation.inputs;

    Ok(if invocation.remaining_prompt.is_empty() {
        format!("@{}", skill.trigger())
    } else {
        invocation.remaining_prompt
    })
}

pub fn apply_at_skill_to_turn(
    raw_prompt: &str,
    turn: &mut crate::engine::TurnConfig,
    config: &Config,
) -> Result<String, String> {
    if let Some((cmd_token, args_str)) = super::parse::parse_at_command(raw_prompt) {
        let prompt = activate_skill_for_turn(cmd_token, args_str, &[], turn, config)?;
        Ok(if args_str.is_empty() {
            raw_prompt.trim().to_string()
        } else {
            prompt
        })
    } else {
        Ok(raw_prompt.to_string())
    }
}

/// Validates (and normalizes defaults for) a `TurnConfig` that already carries `active_skill`.
pub fn validate_turn_skill(
    turn: &mut crate::engine::TurnConfig,
    config: &Config,
) -> Result<(), String> {
    let Some(skill_name) = turn.active_skill.clone() else {
        return Ok(());
    };
    let canonical_name =
        super::resolve::check_skill_activation(&skill_name, turn.enable_tools, config)?;
    let skill = super::parse::find_skill(config, &canonical_name)
        .ok_or_else(|| format!("Skill '@{canonical_name}' not found."))?;
    turn.active_skill = Some(canonical_name);

    if skill.args.is_empty() {
        if let Some(k) = turn.skill_inputs.keys().next() {
            return Err(format!(
                "Skill '@{}' does not declare any typed arguments (unexpected argument '{k}').",
                skill.name
            ));
        }
        return Ok(());
    }

    if turn.skill_inputs.is_empty() && turn.raw_skill_args.is_none() {
        let raw_args = turn.skill_args.as_deref().unwrap_or("");
        let resolved = resolve_skill_inputs(skill, raw_args, &[])?;
        turn.skill_inputs = resolved.inputs;
        turn.raw_skill_args = (!resolved.raw_args.is_empty()).then_some(resolved.raw_args);
        turn.skill_args =
            (!resolved.remaining_prompt.is_empty()).then_some(resolved.remaining_prompt);
    } else {
        let overrides: Vec<(String, String)> = turn
            .skill_inputs
            .iter()
            .map(|(k, v)| (k.clone(), format_arg_value(v)))
            .collect();
        let resolved = resolve_skill_inputs(skill, "", &overrides)?;
        turn.skill_inputs = resolved.inputs;
    }

    Ok(())
}
