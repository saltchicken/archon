//! Mustache-style template engine for skill prompts and hook argument
//! interpolation. Supports `{{var}}`, `{{#if var}}…{{else}}…{{/if}}`, and
//! `{{#unless var}}…{{/unless}}` blocks.


use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateToken<'a> {
    Literal(&'a str),
    Var(&'a str),
    If(&'a str),
    Unless(&'a str),
    Else,
    EndIf,
    EndUnless,
}

pub(super) fn is_valid_template_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn tokenize_template(src: &str) -> Vec<TemplateToken<'_>> {
    let mut out = Vec::new();
    let mut pos = 0usize;

    while let Some(rel_open) = src[pos..].find("{{") {
        let open = pos + rel_open;
        let Some(rel_close) = src[open + 2..].find("}}") else {
            break;
        };
        let close = open + 2 + rel_close;
        let inner = src[open + 2..close].trim();

        let parsed = if inner == "else" {
            Some(TemplateToken::Else)
        } else if inner == "/if" {
            Some(TemplateToken::EndIf)
        } else if inner == "/unless" {
            Some(TemplateToken::EndUnless)
        } else if let Some(var) = inner
            .strip_prefix("#if")
            .and_then(|r| r.strip_prefix(char::is_whitespace))
            .map(str::trim)
            && is_valid_template_ident(var)
        {
            Some(TemplateToken::If(var))
        } else if let Some(var) = inner
            .strip_prefix("#unless")
            .and_then(|r| r.strip_prefix(char::is_whitespace))
            .map(str::trim)
            && is_valid_template_ident(var)
        {
            Some(TemplateToken::Unless(var))
        } else if is_valid_template_ident(inner) {
            Some(TemplateToken::Var(inner))
        } else {
            None
        };

        if let Some(tok) = parsed {
            if open > pos {
                out.push(TemplateToken::Literal(&src[pos..open]));
            }
            out.push(tok);
            pos = close + 2;
        } else {
            out.push(TemplateToken::Literal(&src[pos..open + 1]));
            pos = open + 1;
        }
    }

    if pos < src.len() {
        out.push(TemplateToken::Literal(&src[pos..]));
    }
    out
}

/// Context variables available to the template renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateContext<'a> {
    pub skill_name: &'a str,
    pub args: &'a str,
    pub raw_args: &'a str,
    pub last_response: Option<&'a str>,
    pub inputs: &'a BTreeMap<String, Value>,
}

impl<'a> TemplateContext<'a> {
    /// Resolves `var` against the typed skill inputs, falling back to a
    /// case-insensitive key match.
    fn input_value(&self, var: &str) -> Option<&Value> {
        self.inputs.get(var).or_else(|| {
            self.inputs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(var))
                .map(|(_, v)| v)
        })
    }

    fn lookup_str(&self, var: &str) -> Option<String> {
        match var {
            "args" => Some(self.args.to_string()),
            "raw_args" => Some(self.raw_args.to_string()),
            "skill.name" => Some(self.skill_name.to_string()),
            "last_response" => self.last_response.map(str::to_string),
            _ => self.input_value(var).map(super::args::format_arg_value),
        }
    }

    fn is_truthy(&self, var: &str) -> bool {
        match var {
            "args" => !self.args.trim().is_empty(),
            "raw_args" => !self.raw_args.trim().is_empty(),
            "skill.name" => !self.skill_name.trim().is_empty(),
            "last_response" => self.last_response.is_some_and(|s| !s.trim().is_empty()),
            _ => match self.input_value(var) {
                Some(Value::Bool(b)) => *b,
                Some(Value::String(s)) => !s.trim().is_empty(),
                Some(Value::Null) | None => false,
                Some(_) => true,
            },
        }
    }
}

/// Which block a rendered branch belongs to; determines the `else`/terminator
/// boundary that ends the branch body.
#[derive(Clone, Copy)]
enum Block {
    /// Top level: no boundary; stray `else`/`/if`/`/unless` tokens are skipped.
    Top,
    /// Inside a `{{#if ...}}` block (bounded by `else` / `{{/if}}`).
    If,
    /// Inside a `{{#unless ...}}` block (bounded by `else` / `{{/unless}}`).
    Unless,
}

impl Block {
    fn in_block(self) -> bool {
        matches!(self, Self::If | Self::Unless)
    }

    fn is_terminator(self, tok: &TemplateToken) -> bool {
        matches!(
            (self, tok),
            (Self::If, TemplateToken::EndIf) | (Self::Unless, TemplateToken::EndUnless)
        )
    }
}

/// Renders a `{{#if ...}}` / `{{#unless ...}}` block (primary branch, optional
/// `else` branch, then the closing terminator) starting at its opening token.
fn render_block(
    tokens: &[TemplateToken<'_>],
    idx: &mut usize,
    ctx: &TemplateContext<'_>,
    emit: bool,
    cond: bool,
    block: Block,
    out: &mut String,
) {
    // `idx` points at the `#if`/`#unless` token; advance past it.
    *idx += 1;
    // Render the primary branch up to the `else` or the block terminator.
    render_tokens(tokens, idx, ctx, emit && cond, block, out);
    // Render the `else` branch when one is present.
    if matches!(tokens.get(*idx), Some(TemplateToken::Else)) {
        *idx += 1;
        render_tokens(tokens, idx, ctx, emit && !cond, block, out);
    }
    // Consume the block terminator.
    if tokens.get(*idx).is_some_and(|t| block.is_terminator(t)) {
        *idx += 1;
    }
}

fn render_tokens(
    tokens: &[TemplateToken<'_>],
    idx: &mut usize,
    ctx: &TemplateContext<'_>,
    emit: bool,
    block: Block,
    out: &mut String,
) {
    while *idx < tokens.len() {
        match tokens[*idx] {
            TemplateToken::Literal(s) => {
                if emit {
                    out.push_str(s);
                }
                *idx += 1;
            }
            TemplateToken::Var(var) => {
                if emit && let Some(val) = ctx.lookup_str(var) {
                    out.push_str(&val);
                }
                *idx += 1;
            }
            TemplateToken::If(var) => {
                render_block(tokens, idx, ctx, emit, ctx.is_truthy(var), Block::If, out)
            }
            TemplateToken::Unless(var) => render_block(
                tokens,
                idx,
                ctx,
                emit,
                !ctx.is_truthy(var),
                Block::Unless,
                out,
            ),
            TemplateToken::Else => {
                if block.in_block() {
                    return;
                }
                *idx += 1;
            }
            TemplateToken::EndIf | TemplateToken::EndUnless => {
                if block.is_terminator(&tokens[*idx]) {
                    return;
                }
                *idx += 1;
            }
        }
    }
}

/// Renders a template string using the provided context.
pub fn render_template(src: &str, ctx: &TemplateContext<'_>) -> String {
    if !src.contains("{{") {
        return src.to_string();
    }
    let tokens = tokenize_template(src);
    let mut out = String::with_capacity(src.len());
    let mut idx = 0usize;
    render_tokens(&tokens, &mut idx, ctx, true, Block::Top, &mut out);
    out
}

/// Validates template block nesting (`#if`/`#unless`/`else`/`/if`/`/unless`) and returns
/// `(referenced_variables, syntax_errors)`.
pub fn inspect_template(src: &str) -> (Vec<String>, Vec<String>) {
    let mut vars = Vec::new();
    let mut errors = Vec::new();
    if !src.contains("{{") {
        return (vars, errors);
    }

    let tokens = tokenize_template(src);
    let mut stack: Vec<(bool, bool, &str)> = Vec::new();

    for tok in tokens {
        match tok {
            TemplateToken::Literal(_) => {}
            TemplateToken::Var(v) => push_unique_string(&mut vars, v),
            TemplateToken::If(v) => {
                push_unique_string(&mut vars, v);
                stack.push((true, false, v));
            }
            TemplateToken::Unless(v) => {
                push_unique_string(&mut vars, v);
                stack.push((false, false, v));
            }
            TemplateToken::Else => match stack.last_mut() {
                Some((_, seen_else, _)) if !*seen_else => *seen_else = true,
                Some((_, _, v)) => {
                    errors.push(format!("Duplicate `{{{{else}}}}` inside block for '{v}'"));
                }
                None => {
                    errors.push("Unexpected `{{else}}` outside any `#if`/`#unless` block".into());
                }
            },
            TemplateToken::EndIf => match stack.pop() {
                Some((true, _, _)) => {}
                Some((false, _, v)) => {
                    errors.push(format!(
                        "Mismatched closing tag `{{{{/if}}}}` for `{{{{#unless {v}}}}}`"
                    ));
                }
                None => errors.push("Unexpected `{{/if}}` with no matching `{{#if}}`".into()),
            },
            TemplateToken::EndUnless => match stack.pop() {
                Some((false, _, _)) => {}
                Some((true, _, v)) => {
                    errors.push(format!(
                        "Mismatched closing tag `{{{{/unless}}}}` for `{{{{#if {v}}}}}`"
                    ));
                }
                None => {
                    errors.push("Unexpected `{{/unless}}` with no matching `{{#unless}}`".into());
                }
            },
        }
    }

    for (is_if, _, v) in stack.into_iter().rev() {
        let open = if is_if { "#if" } else { "#unless" };
        let close = if is_if { "/if" } else { "/unless" };
        errors.push(format!(
            "Unclosed template block `{{{{{open} {v}}}}}` (missing `{{{{{close}}}}}`)"
        ));
    }

    (vars, errors)
}

fn push_unique_string(vec: &mut Vec<String>, item: &str) {
    if !vec.iter().any(|existing| existing == item) {
        vec.push(item.to_string());
    }
}
