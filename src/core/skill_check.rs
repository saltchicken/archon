use crate::{
    cli::SkillCommand,
    core::{
        config::{Config, MemoryScope, expand_tilde, format_tilde_path},
        skills::{
            DetailedSkillParse, HookTrigger, RESERVED_ARG_NAMES, SkillArgDef, SkillArgType,
            SkillDefinition, SkillHook, TemplateContext, inspect_template, interpolate_hook_args,
            list_skill_reference_paths, parse_skill_markdown_detailed, resolve_skills,
            scan_skills_dir, validate_arg_json_value,
        },
    },
    tools::{self, ALL_TOOL_NAMES, utils::path::validate_relative_path},
};
use anyhow::{Result, bail};
use regex::Regex;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
}

impl SkillDiagnostic {
    fn error(msg: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            message: msg.into(),
        }
    }

    fn warning(msg: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            message: msg.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillCheckResult {
    pub name: String,
    pub scope_label: String,
    pub file_path: PathBuf,
    pub extends: Vec<String>,
    pub is_abstract: bool,
    pub tool_count: usize,
    pub hook_count: usize,
    pub arg_count: usize,
    pub ref_count: usize,
    pub diagnostics: Vec<SkillDiagnostic>,
}

impl SkillCheckResult {
    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Warning)
            .count()
    }

    pub fn is_ok(&self, strict: bool) -> bool {
        self.error_count() == 0 && (!strict || self.warning_count() == 0)
    }
}

#[derive(Debug, Default)]
pub struct SkillCheckReport {
    pub results: Vec<SkillCheckResult>,
}

impl SkillCheckReport {
    pub fn error_count(&self) -> usize {
        self.results.iter().map(SkillCheckResult::error_count).sum()
    }

    pub fn warning_count(&self) -> usize {
        self.results
            .iter()
            .map(SkillCheckResult::warning_count)
            .sum()
    }

    pub fn failed_count(&self, strict: bool) -> usize {
        self.results.iter().filter(|r| !r.is_ok(strict)).count()
    }

    pub fn passed_count(&self, strict: bool) -> usize {
        self.results.len().saturating_sub(self.failed_count(strict))
    }

    pub fn is_ok(&self, strict: bool) -> bool {
        self.failed_count(strict) == 0
    }

    pub fn format_report(&self, strict: bool) -> String {
        if self.results.is_empty() {
            return "No skills found to check.".to_string();
        }

        let mut out = String::new();
        for res in &self.results {
            let ok = res.is_ok(strict);
            let status_glyph = if !ok {
                "✗"
            } else if res.warning_count() > 0 {
                "⚠"
            } else {
                "✓"
            };

            let mut meta = Vec::new();
            if res.is_abstract {
                meta.push("abstract".to_string());
            }
            if !res.extends.is_empty() {
                meta.push(format!("extends: {}", res.extends.join(", ")));
            }
            if res.tool_count > 0 {
                let s = if res.tool_count == 1 { "" } else { "s" };
                meta.push(format!("{} tool{s}", res.tool_count));
            }
            if res.hook_count > 0 {
                let s = if res.hook_count == 1 { "" } else { "s" };
                meta.push(format!("{} hook{s}", res.hook_count));
            }
            if res.arg_count > 0 {
                let s = if res.arg_count == 1 { "" } else { "s" };
                meta.push(format!("{} arg{s}", res.arg_count));
            }
            if res.ref_count > 0 {
                let s = if res.ref_count == 1 { "" } else { "s" };
                meta.push(format!("{} ref{s}", res.ref_count));
            }
            let meta_str = if meta.is_empty() {
                String::new()
            } else {
                format!(" ({})", meta.join("; "))
            };

            let _ = writeln!(
                out,
                "  {status_glyph} {} [{}]{meta_str}",
                res.name, res.scope_label
            );

            if !res.diagnostics.is_empty() {
                let _ = writeln!(out, "      file: {}", format_tilde_path(&res.file_path));
                for diag in &res.diagnostics {
                    let level = match diag.severity {
                        DiagnosticSeverity::Error => "error",
                        DiagnosticSeverity::Warning => "warning",
                    };
                    let _ = writeln!(out, "      {level}: {}", diag.message);
                }
            }
        }

        let total = self.results.len();
        let passed = self.passed_count(strict);
        let failed = self.failed_count(strict);
        let errs = self.error_count();
        let warns = self.warning_count();
        let err_s = if errs == 1 { "" } else { "s" };
        let warn_s = if warns == 1 { "" } else { "s" };
        let _ = write!(
            out,
            "\nSummary: {total} checked, {passed} passed, {failed} failed ({errs} error{err_s}, {warns} warning{warn_s})"
        );
        out
    }
}

// ============================================================================
// JSON Schema Validation for Hook Arguments
// ============================================================================

fn json_type_name(val: &Value) -> &'static str {
    match val {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn value_matches_json_type(val: &Value, expected: &str) -> bool {
    match expected {
        "null" => val.is_null(),
        "boolean" => val.is_boolean(),
        "integer" => val.as_i64().is_some() || val.as_u64().is_some(),
        "number" => val.is_number(),
        "string" => val.is_string(),
        "array" => val.is_array(),
        "object" => val.is_object(),
        _ => true,
    }
}

fn extract_const_or_enum_strings(branches: &[Value]) -> Option<Vec<String>> {
    let mut vals = Vec::new();
    for b in branches {
        let obj = b.as_object()?;
        if let Some(c) = obj.get("const").and_then(Value::as_str) {
            vals.push(c.to_string());
        } else {
            let arr = obj.get("enum").and_then(Value::as_array)?;
            for item in arr {
                vals.push(item.as_str()?.to_string());
            }
        }
    }
    (!vals.is_empty()).then_some(vals)
}

pub fn validate_value_against_schema(
    value: &Value,
    schema: &Value,
    defs: &Map<String, Value>,
    path: &str,
    errors: &mut Vec<String>,
    depth: usize,
) {
    if depth > 20 {
        return;
    }

    if let Some(b) = schema.as_bool() {
        if !b {
            errors.push(format!("{path}: value is not permitted by schema"));
        }
        return;
    }

    let Some(obj) = schema.as_object() else {
        return;
    };

    // Resolve `$ref`
    if let Some(ref_str) = obj.get("$ref").and_then(Value::as_str) {
        let ref_name = ref_str
            .strip_prefix("#/$defs/")
            .or_else(|| ref_str.strip_prefix("#/definitions/"));
        if let Some(name) = ref_name
            && let Some(target) = defs.get(name)
        {
            validate_value_against_schema(value, target, defs, path, errors, depth + 1);
            // If there are sibling keywords besides `$ref`, validate them too.
            if obj.len() == 1 {
                return;
            }
        }
    }

    // `oneOf` / `anyOf`
    for comb in ["oneOf", "anyOf"] {
        if let Some(branches) = obj.get(comb).and_then(Value::as_array)
            && !branches.is_empty()
        {
            let mut matched = false;
            let mut best_branch_errs: Option<Vec<String>> = None;
            for branch in branches {
                let mut branch_errs = Vec::new();
                validate_value_against_schema(
                    value,
                    branch,
                    defs,
                    path,
                    &mut branch_errs,
                    depth + 1,
                );
                if branch_errs.is_empty() {
                    matched = true;
                    break;
                }
                if best_branch_errs
                    .as_ref()
                    .is_none_or(|prev| branch_errs.len() < prev.len())
                {
                    best_branch_errs = Some(branch_errs);
                }
            }
            if !matched {
                if let Some(choices) = extract_const_or_enum_strings(branches) {
                    errors.push(format!(
                        "{path}: invalid value {value}; expected one of: {}",
                        choices.join(", ")
                    ));
                } else if let Some(errs) = best_branch_errs {
                    errors.extend(errs);
                } else {
                    errors.push(format!("{path}: value {value} did not match schema"));
                }
                return;
            }
        }
    }

    // `const`
    if let Some(const_val) = obj.get("const")
        && value != const_val
    {
        errors.push(format!("{path}: expected {const_val}, got {value}"));
        return;
    }

    // `enum`
    if let Some(enum_vals) = obj.get("enum").and_then(Value::as_array)
        && !enum_vals.iter().any(|e| e == value)
    {
        let rendered: Vec<String> = enum_vals
            .iter()
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            })
            .collect();
        errors.push(format!(
            "{path}: invalid value {value}; expected one of: {}",
            rendered.join(", ")
        ));
        return;
    }

    // `type`
    if let Some(ty_val) = obj.get("type") {
        let valid_type = match ty_val {
            Value::String(s) => value_matches_json_type(value, s),
            Value::Array(arr) => arr
                .iter()
                .filter_map(Value::as_str)
                .any(|s| value_matches_json_type(value, s)),
            _ => true,
        };
        if !valid_type {
            let expected = match ty_val {
                Value::String(s) => s.clone(),
                Value::Array(arr) => arr
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" | "),
                other => other.to_string(),
            };
            errors.push(format!(
                "{path}: expected type {expected}, got {} ({value})",
                json_type_name(value)
            ));
            return;
        }
    }

    // Numeric bounds (`minimum` / `maximum`)
    if let Some(num) = value.as_f64() {
        if let Some(min) = obj.get("minimum").and_then(Value::as_f64)
            && num < min
        {
            errors.push(format!("{path}: value {value} is less than minimum {min}"));
        }
        if let Some(max) = obj.get("maximum").and_then(Value::as_f64)
            && num > max
        {
            errors.push(format!("{path}: value {value} exceeds maximum {max}"));
        }
    }

    // Object validation
    if let Some(val_obj) = value.as_object() {
        if let Some(req_arr) = obj.get("required").and_then(Value::as_array) {
            for req in req_arr.iter().filter_map(Value::as_str) {
                if !val_obj.contains_key(req) {
                    errors.push(format!("{path}: missing required argument '{req}'"));
                }
            }
        }

        if let Some(props) = obj.get("properties").and_then(Value::as_object) {
            let additional = obj.get("additionalProperties");
            let allow_any_additional = additional == Some(&Value::Bool(true));
            let additional_schema = additional.filter(|v| v.is_object());

            let mut valid_keys: Vec<&str> = props.keys().map(String::as_str).collect();
            valid_keys.sort();

            for (k, v) in val_obj {
                let child_path = if path.is_empty() || path == "args" {
                    format!("args.{k}")
                } else {
                    format!("{path}.{k}")
                };
                if let Some(prop_schema) = props.get(k) {
                    validate_value_against_schema(
                        v,
                        prop_schema,
                        defs,
                        &child_path,
                        errors,
                        depth + 1,
                    );
                } else if let Some(add_schema) = additional_schema {
                    validate_value_against_schema(
                        v,
                        add_schema,
                        defs,
                        &child_path,
                        errors,
                        depth + 1,
                    );
                } else if !allow_any_additional {
                    errors.push(format!(
                        "{child_path}: unknown argument '{k}' (valid arguments: {})",
                        valid_keys.join(", ")
                    ));
                }
            }
        }
    }

    // Array validation
    if let Some(val_arr) = value.as_array()
        && let Some(item_schema) = obj.get("items")
    {
        for (i, item) in val_arr.iter().enumerate() {
            let item_path = format!("{path}[{i}]");
            validate_value_against_schema(item, item_schema, defs, &item_path, errors, depth + 1);
        }
    }
}

pub fn validate_tool_args_against_schema(tool_name: &str, args: &Value) -> Vec<String> {
    let Some(full_schema) = tools::tool_schema_by_name(tool_name) else {
        return vec![format!("Unknown tool '{tool_name}'")];
    };
    let Some(params_schema) = full_schema
        .get("function")
        .and_then(|f| f.get("parameters"))
    else {
        return Vec::new();
    };

    let mut defs = Map::new();
    if let Some(obj) = params_schema.as_object() {
        for key in ["$defs", "definitions"] {
            if let Some(Value::Object(d)) = obj.get(key) {
                defs.extend(d.clone());
            }
        }
    }

    let mut errors = Vec::new();
    validate_value_against_schema(args, params_schema, &defs, "args", &mut errors, 0);

    // Final backstop: verify serde deserializes cleanly into the actual tool `Args` struct.
    if errors.is_empty()
        && let Err(e) = tools::deserialize_tool_args(tool_name, args)
    {
        errors.push(format!("{e:#}"));
    }

    errors
}

// ============================================================================
// Skill Validation Checks
// ============================================================================

fn is_valid_slug(raw: &str) -> bool {
    !raw.is_empty()
        && !raw.starts_with(['.', '-'])
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn is_valid_arg_name(raw: &str) -> bool {
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn collect_template_vars_in_json(
    val: &Value,
    vars: &mut Vec<String>,
    syntax_errs: &mut Vec<String>,
) {
    match val {
        Value::String(s) => {
            let (v, e) = inspect_template(s);
            for var in v {
                if !vars.contains(&var) {
                    vars.push(var);
                }
            }
            syntax_errs.extend(e);
        }
        Value::Array(arr) => {
            for item in arr {
                collect_template_vars_in_json(item, vars, syntax_errs);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_template_vars_in_json(v, vars, syntax_errs);
            }
        }
        _ => {}
    }
}

fn is_known_template_var(var: &str, args: &[SkillArgDef], allow_last_response: bool) -> bool {
    if matches!(var, "args" | "raw_args" | "skill.name" | "super") {
        return true;
    }
    if allow_last_response && var == "last_response" {
        return true;
    }
    args.iter().any(|a| a.name.eq_ignore_ascii_case(var))
}

fn validate_args_defs(args: &[SkillArgDef], diagnostics: &mut Vec<SkillDiagnostic>) {
    let mut seen = HashSet::new();
    for (idx, arg) in args.iter().enumerate() {
        let label = if arg.name.is_empty() {
            format!("args[{}]", idx + 1)
        } else {
            format!("arg '{}'", arg.name)
        };

        if arg.name.is_empty() {
            diagnostics.push(SkillDiagnostic::error(format!(
                "[[args]] entry #{} is missing `name`",
                idx + 1
            )));
            continue;
        }
        if !is_valid_arg_name(&arg.name) {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Invalid {label}: must start with a letter or '_' and contain only ASCII letters, digits, '_', or '-'"
            )));
        }
        if RESERVED_ARG_NAMES
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&arg.name))
        {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Invalid {label}: '{}' is a reserved template variable name",
                arg.name
            )));
        }
        if !seen.insert(arg.name.to_ascii_lowercase()) {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Duplicate argument declaration '{label}'"
            )));
        }
        if arg.arg_type == SkillArgType::Enum && arg.choices.is_empty() {
            diagnostics.push(SkillDiagnostic::error(format!(
                "{label} has `type = \"enum\"` but `choices` is empty"
            )));
        }
        if let Some(ref default_val) = arg.default {
            if let Err(err) = validate_arg_json_value(arg, default_val) {
                diagnostics.push(SkillDiagnostic::error(err));
            }
            if arg.required {
                diagnostics.push(SkillDiagnostic::warning(format!(
                    "{label} sets both `required = true` and a `default` value (`default` is never used when required)"
                )));
            }
        }
    }
}

fn validate_hooks(
    skill_name: &str,
    hooks: &[SkillHook],
    resolved_args: &[SkillArgDef],
    config: &Config,
    check_enabled: bool,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    let sample_inputs: BTreeMap<String, Value> = resolved_args
        .iter()
        .map(|a| (a.name.clone(), a.sample_value()))
        .collect();
    let sample_ctx = TemplateContext {
        skill_name,
        args: "sample",
        raw_args: "sample",
        last_response: Some("sample response"),
        inputs: &sample_inputs,
    };
    let has_optional_no_default = resolved_args
        .iter()
        .any(|a| !a.required && a.default.is_none());
    let minimal_inputs: BTreeMap<String, Value> = resolved_args
        .iter()
        .filter(|a| a.required || a.default.is_some())
        .map(|a| (a.name.clone(), a.sample_value()))
        .collect();
    let minimal_ctx = TemplateContext {
        skill_name,
        args: "sample",
        raw_args: "sample",
        last_response: Some("sample response"),
        inputs: &minimal_inputs,
    };

    for (idx, hook) in hooks.iter().enumerate() {
        let h_num = idx + 1;
        let tool = hook.tool.trim();
        if tool.is_empty() {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Hook #{h_num} is missing a `tool` name"
            )));
            continue;
        }
        if !ALL_TOOL_NAMES.contains(&tool) {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Hook #{h_num}: unknown tool '{tool}' (valid tools: {})",
                ALL_TOOL_NAMES.join(", ")
            )));
            continue;
        }
        if check_enabled && !tools::is_tool_enabled(tool, config) {
            diagnostics.push(SkillDiagnostic::warning(format!(
                "Hook #{h_num} (`{tool}`): tool is not enabled in current config (`app.enabled_tools`)"
            )));
        }
        if !hook.args.is_object() {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Hook #{h_num} (`{tool}`): `args` must be a TOML table / JSON object"
            )));
            continue;
        }

        // Validate template variables inside hook args
        let mut hook_vars = Vec::new();
        let mut syntax_errs = Vec::new();
        collect_template_vars_in_json(&hook.args, &mut hook_vars, &mut syntax_errs);
        for err in syntax_errs {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Hook #{h_num} (`{tool}`): template error: {err}"
            )));
        }
        for var in &hook_vars {
            if var == "last_response" && hook.trigger == HookTrigger::OnActivate {
                diagnostics.push(SkillDiagnostic::warning(format!(
                    "Hook #{h_num} (`{tool}`): `{{{{last_response}}}}` is typically empty on `on_activate`"
                )));
            } else if !is_known_template_var(var, resolved_args, true) {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Hook #{h_num} (`{tool}`): references undeclared template variable `{{{{{var}}}}}`"
                )));
            }
        }

        // Interpolate with sample typed values and validate against tool's JSON Schema
        let sample_args = interpolate_hook_args(&hook.args, &sample_ctx);
        let sample_errs = validate_tool_args_against_schema(tool, &sample_args);
        for err in &sample_errs {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Hook #{h_num} (`{tool}`): {err}"
            )));
        }

        // Also validate when optional arguments without defaults are omitted
        if sample_errs.is_empty() && has_optional_no_default {
            let minimal_args = interpolate_hook_args(&hook.args, &minimal_ctx);
            if minimal_args != sample_args {
                for err in validate_tool_args_against_schema(tool, &minimal_args) {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Hook #{h_num} (`{tool}`): when optional argument(s) are omitted: {err}"
                    )));
                }
            }
        }
    }
}

fn prompt_reference_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:^|[\s`"'(\[<])((?:(?:~|/)?[^\s`"'()\[\]<>]*?/)?(references/[A-Za-z0-9._/\-]+))"#,
        )
        .expect("valid prompt reference regex")
    })
}

fn extract_prompt_reference_paths(prompt: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for cap in prompt_reference_regex().captures_iter(prompt) {
        let Some(full) = cap.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let Some(rel) = cap.get(2).map(|m| m.as_str()) else {
            continue;
        };
        let clean_full = full
            .trim_end_matches(['.', ',', ';', ':', ')', ']', '`', '"', '\''])
            .to_string();
        let clean_rel = rel
            .trim_end_matches(['.', ',', ';', ':', ')', ']', '`', '"', '\''])
            .to_string();
        if clean_rel == "references/" || clean_rel.ends_with('/') {
            continue;
        }
        if !found
            .iter()
            .any(|(f, r)| f == &clean_full && r == &clean_rel)
        {
            found.push((clean_full, clean_rel));
        }
    }
    found
}

enum ReferenceLookup {
    Found,
    EscapesDir,
    Missing,
}

fn lookup_reference_in_skill_dirs(rel_ref: &str, skill: &SkillDefinition) -> ReferenceLookup {
    let Ok(rel) = validate_relative_path(rel_ref) else {
        return ReferenceLookup::Missing;
    };
    let mut escaped = false;
    for dir in skill.all_dirs() {
        let candidate = dir.join(rel);
        if candidate.is_file() {
            let canon_dir = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            match dunce::canonicalize(&candidate) {
                Ok(canon) if canon.starts_with(&canon_dir) => return ReferenceLookup::Found,
                Ok(_) => escaped = true,
                Err(_) => {}
            }
        }
    }
    if escaped {
        ReferenceLookup::EscapesDir
    } else {
        ReferenceLookup::Missing
    }
}

fn validate_references(
    skill: &SkillDefinition,
    skill_dir: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    // 1. Validate explicit frontmatter `references = [...]`
    for raw_ref in &skill.references {
        match validate_relative_path(raw_ref) {
            Err(e) => {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Invalid reference path '{raw_ref}' in frontmatter `references`: {e}"
                )));
            }
            Ok(_) => match lookup_reference_in_skill_dirs(raw_ref, skill) {
                ReferenceLookup::Found => {}
                ReferenceLookup::EscapesDir => {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Declared reference file '{raw_ref}' resolves outside the skill directory"
                    )));
                }
                ReferenceLookup::Missing => {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Declared reference file '{raw_ref}' does not exist in skill directory"
                    )));
                }
            },
        }
    }

    // 2. Validate any `references/...` files mentioned in the prompt body
    for (full_match, rel_ref) in extract_prompt_reference_paths(&skill.prompt) {
        match lookup_reference_in_skill_dirs(&rel_ref, skill) {
            ReferenceLookup::Found => continue,
            ReferenceLookup::EscapesDir => {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Prompt reference '{full_match}' resolves outside the skill directory"
                )));
                continue;
            }
            ReferenceLookup::Missing => {}
        }
        // If the prompt gave an explicit `~/` or `/` path, also check whether that exact path exists.
        if (full_match.starts_with("~/") || full_match.starts_with('/'))
            && expand_tilde(&full_match).is_file()
        {
            continue;
        }
        diagnostics.push(SkillDiagnostic::error(format!(
            "Prompt references missing file '{full_match}' (expected '{rel_ref}' in skill directory)"
        )));
    }

    // 3. Walk `<skill_dir>/references` if present and verify files/symlinks are healthy
    let refs_dir = skill_dir.join("references");
    if refs_dir.symlink_metadata().is_ok() {
        if !refs_dir.is_dir() {
            diagnostics.push(SkillDiagnostic::error(
                "'references' exists in skill directory but is not a directory",
            ));
            return;
        }
        let canon_skill_dir =
            dunce::canonicalize(skill_dir).unwrap_or_else(|_| skill_dir.to_path_buf());
        let mut file_count = 0usize;
        let mut visited_dirs = HashSet::new();
        fn walk_refs(
            cur: &Path,
            canon_skill_dir: &Path,
            depth: usize,
            visited_dirs: &mut HashSet<PathBuf>,
            file_count: &mut usize,
            diagnostics: &mut Vec<SkillDiagnostic>,
        ) {
            const MAX_REF_WALK_DEPTH: usize = 16;
            if depth > MAX_REF_WALK_DEPTH {
                diagnostics.push(SkillDiagnostic::warning(format!(
                    "Skipping deeply nested references directory '{}' (depth > {MAX_REF_WALK_DEPTH})",
                    format_tilde_path(cur)
                )));
                return;
            }
            let canon_cur = match dunce::canonicalize(cur) {
                Ok(c) => c,
                Err(e) => {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Cannot canonicalize references directory '{}': {e}",
                        format_tilde_path(cur)
                    )));
                    return;
                }
            };
            if !canon_cur.starts_with(canon_skill_dir) {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Reference directory '{}' resolves outside the skill directory",
                    format_tilde_path(cur)
                )));
                return;
            }
            if !visited_dirs.insert(canon_cur) {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Cyclic symlink detected in references directory '{}'",
                    format_tilde_path(cur)
                )));
                return;
            }
            let Ok(rd) = fs::read_dir(cur) else {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Cannot read references directory '{}'",
                    format_tilde_path(cur)
                )));
                return;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                match fs::metadata(&p) {
                    Err(e) => {
                        diagnostics.push(SkillDiagnostic::error(format!(
                            "Broken or unreadable reference entry '{}': {e}",
                            format_tilde_path(&p)
                        )));
                    }
                    Ok(meta) if meta.is_dir() => walk_refs(
                        &p,
                        canon_skill_dir,
                        depth + 1,
                        visited_dirs,
                        file_count,
                        diagnostics,
                    ),
                    Ok(_) => {
                        *file_count += 1;
                        match dunce::canonicalize(&p) {
                            Ok(canon_p) if !canon_p.starts_with(canon_skill_dir) => {
                                diagnostics.push(SkillDiagnostic::error(format!(
                                    "Reference file '{}' resolves outside the skill directory",
                                    format_tilde_path(&p)
                                )));
                            }
                            Err(e) => {
                                diagnostics.push(SkillDiagnostic::error(format!(
                                    "Cannot resolve reference file '{}': {e}",
                                    format_tilde_path(&p)
                                )));
                            }
                            Ok(_) => {
                                if let Err(e) = fs::File::open(&p) {
                                    diagnostics.push(SkillDiagnostic::error(format!(
                                        "Unreadable reference file '{}': {e}",
                                        format_tilde_path(&p)
                                    )));
                                }
                            }
                        }
                    }
                }
            }
        }
        walk_refs(
            &refs_dir,
            &canon_skill_dir,
            0,
            &mut visited_dirs,
            &mut file_count,
            diagnostics,
        );
        if file_count == 0 {
            diagnostics.push(SkillDiagnostic::warning(
                "'references/' directory exists but contains no files",
            ));
        }
    }
}

struct ScannedRawSkill {
    scope_label: String,
    scope: MemoryScope,
    file_path: PathBuf,
    skill_dir: PathBuf,
    read_error: Option<String>,
    parsed: Option<DetailedSkillParse>,
}

fn scan_raw_dir(dir: &Path, scope_label: &str, scope: MemoryScope) -> Vec<ScannedRawSkill> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();

    let mut out = Vec::new();
    for path in entries {
        let (md_path, skill_dir) = if path.is_dir() {
            let md = path.join("SKILL.md");
            if !md.exists() {
                continue;
            }
            (md, path)
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let stem = path.with_extension("");
            (path, stem)
        } else {
            continue;
        };

        match fs::read_to_string(&md_path) {
            Ok(raw) => {
                let parsed = parse_skill_markdown_detailed(&raw, &skill_dir);
                out.push(ScannedRawSkill {
                    scope_label: scope_label.to_string(),
                    scope,
                    file_path: md_path,
                    skill_dir,
                    read_error: None,
                    parsed: Some(parsed),
                });
            }
            Err(e) => {
                out.push(ScannedRawSkill {
                    scope_label: scope_label.to_string(),
                    scope,
                    file_path: md_path,
                    skill_dir,
                    read_error: Some(e.to_string()),
                    parsed: None,
                });
            }
        }
    }
    out
}

fn check_scanned_registry(
    scanned: Vec<ScannedRawSkill>,
    fallback_parents: &[SkillDefinition],
    config: &Config,
    check_enabled: bool,
    name_filter: Option<&str>,
) -> Vec<SkillCheckResult> {
    // Resolve global skills against the global-only registry so workspace overrides
    // never overwrite global skills during validation.
    let mut global_defs: Vec<SkillDefinition> = fallback_parents.to_vec();
    for item in &scanned {
        if item.scope != MemoryScope::Global {
            continue;
        }
        let Some(ref parsed) = item.parsed else {
            continue;
        };
        let Some(mut def) = parsed.definition.clone() else {
            continue;
        };
        def.scope = MemoryScope::Global;
        if let Some(existing) = global_defs
            .iter_mut()
            .find(|s| s.name.eq_ignore_ascii_case(&def.name))
        {
            *existing = def;
        } else {
            global_defs.push(def);
        }
    }
    let resolved_global = resolve_skills(global_defs.clone());

    // Build merged raw definitions (workspace overrides global by name for workspace `extends` resolution).
    let mut merged_defs: Vec<SkillDefinition> = global_defs;
    for item in &scanned {
        if item.scope == MemoryScope::Global {
            continue;
        }
        let Some(ref parsed) = item.parsed else {
            continue;
        };
        let Some(mut def) = parsed.definition.clone() else {
            continue;
        };
        def.scope = item.scope;
        if let Some(existing) = merged_defs
            .iter_mut()
            .find(|s| s.name.eq_ignore_ascii_case(&def.name))
        {
            def.overrides_global =
                existing.scope == MemoryScope::Global && def.scope == MemoryScope::Workspace;
            *existing = def;
        } else {
            merged_defs.push(def);
        }
    }

    let resolved_merged = resolve_skills(merged_defs);
    let clean_filter = name_filter
        .map(|f| f.trim().trim_start_matches('@'))
        .filter(|f| !f.is_empty());

    // Track duplicate names/triggers within the same scope_label
    let mut seen_names_per_scope: HashSet<(String, String)> = HashSet::new();
    let mut seen_cmds_per_scope: HashSet<(String, String)> = HashSet::new();

    let mut results = Vec::new();
    for item in scanned {
        let fallback_name = item
            .skill_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        if let Some(ref read_err) = item.read_error {
            if clean_filter.is_some_and(|f| !fallback_name.eq_ignore_ascii_case(f)) {
                continue;
            }
            results.push(SkillCheckResult {
                name: fallback_name,
                scope_label: item.scope_label,
                file_path: item.file_path,
                extends: Vec::new(),
                is_abstract: false,
                tool_count: 0,
                hook_count: 0,
                arg_count: 0,
                ref_count: 0,
                diagnostics: vec![SkillDiagnostic::error(format!(
                    "Failed to read SKILL.md: {read_err}"
                ))],
            });
            continue;
        }

        let Some(parsed) = item.parsed else {
            continue;
        };

        let raw_def = parsed.definition.clone();
        let display_name = raw_def
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or(fallback_name);

        let pool = if item.scope == MemoryScope::Global {
            &resolved_global
        } else {
            &resolved_merged
        };
        let resolved = raw_def.as_ref().and_then(|d| {
            pool.iter()
                .find(|r| {
                    r.name.eq_ignore_ascii_case(&d.name)
                        && r.dir_path.as_deref() == Some(item.skill_dir.as_path())
                })
                .or_else(|| pool.iter().find(|r| r.name.eq_ignore_ascii_case(&d.name)))
        });

        if let Some(filter) = clean_filter {
            let matches_name = display_name.eq_ignore_ascii_case(filter);
            let matches_cmd = raw_def
                .as_ref()
                .is_some_and(|d| !d.command.is_empty() && d.command.eq_ignore_ascii_case(filter));
            if !matches_name && !matches_cmd {
                continue;
            }
        }

        let mut diagnostics = Vec::new();

        // 1. Frontmatter syntax & unknown keys
        if parsed.unclosed_frontmatter {
            diagnostics.push(SkillDiagnostic::error(
                "Opening '+++' frontmatter fence is missing a closing '+++' line",
            ));
        }
        if let Some(ref fm_err) = parsed.frontmatter_error {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Invalid TOML frontmatter: {fm_err}"
            )));
        }
        for key in &parsed.unknown_frontmatter_keys {
            diagnostics.push(SkillDiagnostic::warning(format!(
                "Unknown frontmatter key '{key}' (recognized keys: {})",
                crate::core::skills::KNOWN_FRONTMATTER_KEYS.join(", ")
            )));
        }

        let Some(raw_skill) = raw_def else {
            diagnostics.push(SkillDiagnostic::error(
                "Could not determine a valid skill `name`",
            ));
            results.push(SkillCheckResult {
                name: display_name,
                scope_label: item.scope_label,
                file_path: item.file_path,
                extends: Vec::new(),
                is_abstract: false,
                tool_count: 0,
                hook_count: 0,
                arg_count: 0,
                ref_count: 0,
                diagnostics,
            });
            continue;
        };

        // 2. Name / command validity & duplicate checks
        if !is_valid_slug(&raw_skill.name) {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Invalid skill name '{}': only ASCII letters, digits, '-', and '_' are allowed (cannot start with '.' or '-')",
                raw_skill.name
            )));
        }
        if !raw_skill.command.is_empty() && !is_valid_slug(&raw_skill.command) {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Invalid skill command '{}': only ASCII letters, digits, '-', and '_' are allowed",
                raw_skill.command
            )));
        }
        if !seen_names_per_scope.insert((
            item.scope_label.clone(),
            raw_skill.name.to_ascii_lowercase(),
        )) {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Duplicate skill name '{}' in [{}] scope",
                raw_skill.name, item.scope_label
            )));
        }
        if !raw_skill.is_abstract {
            let trig = raw_skill.trigger().to_ascii_lowercase();
            if !seen_cmds_per_scope.insert((item.scope_label.clone(), trig.clone())) {
                diagnostics.push(SkillDiagnostic::warning(format!(
                    "Duplicate '@{trig}' trigger in [{}] scope",
                    item.scope_label
                )));
            }
        }

        // 3. Composition / `extends` errors
        let effective = resolved.unwrap_or(&raw_skill);
        for err in &effective.resolution_errors {
            diagnostics.push(SkillDiagnostic::error(err.clone()));
        }

        // 4. Tools verification (both raw declarations including `!negations`, and resolved)
        for raw_tool in &parsed.raw_tools {
            let trimmed = raw_tool.trim();
            if trimmed.is_empty() || trimmed == "*" {
                continue;
            }
            if let Some(neg) = trimmed.strip_prefix('!') {
                let neg_name = neg.trim();
                if raw_skill.extends.is_empty() {
                    diagnostics.push(SkillDiagnostic::warning(format!(
                        "Tool negation '{trimmed}' has no effect on a skill without `extends`"
                    )));
                }
                if neg_name.is_empty() || !ALL_TOOL_NAMES.contains(&neg_name) {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Unknown tool '{neg_name}' in negated `tools` entry '{trimmed}' (valid tools: {})",
                        ALL_TOOL_NAMES.join(", ")
                    )));
                }
            } else if !ALL_TOOL_NAMES.contains(&trimmed) {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Unknown tool '{trimmed}' in `tools` (valid tools: {})",
                    ALL_TOOL_NAMES.join(", ")
                )));
            } else if check_enabled && !tools::is_tool_enabled(trimmed, config) {
                diagnostics.push(SkillDiagnostic::warning(format!(
                    "Tool '{trimmed}' is not enabled in current config (`app.enabled_tools`)"
                )));
            }
        }

        // 5. Typed `[[args]]` validation
        validate_args_defs(&raw_skill.args, &mut diagnostics);

        // 6. Hook verification & JSON Schema validation (on raw hooks so empty-tool hooks aren't silently dropped,
        //    plus any inherited hooks that might depend on child args)
        let hooks_to_check = if raw_skill.extends.is_empty() {
            &parsed.raw_hooks
        } else {
            // Check raw hooks for empty tool names, then validate full resolved hook list against resolved args.
            for (idx, h) in parsed.raw_hooks.iter().enumerate() {
                if h.tool.trim().is_empty() {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Hook #{} is missing a `tool` name",
                        idx + 1
                    )));
                }
            }
            &effective.hooks
        };
        validate_hooks(
            &effective.name,
            hooks_to_check,
            &effective.args,
            config,
            check_enabled,
            &mut diagnostics,
        );

        // 7. Prompt template syntax & variable validation
        let (prompt_vars, prompt_errs) = inspect_template(&raw_skill.prompt);
        for err in prompt_errs {
            diagnostics.push(SkillDiagnostic::error(format!(
                "Prompt template error: {err}"
            )));
        }
        for var in prompt_vars {
            if !is_known_template_var(&var, &effective.args, false) {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Prompt references undeclared template variable `{{{{{var}}}}}`"
                )));
            }
        }

        // 8. Reference files verification
        validate_references(effective, &item.skill_dir, &mut diagnostics);

        let ref_count = list_skill_reference_paths(effective).len();
        results.push(SkillCheckResult {
            name: effective.name.clone(),
            scope_label: item.scope_label,
            file_path: item.file_path,
            extends: effective.extends.clone(),
            is_abstract: effective.is_abstract,
            tool_count: effective.tools.len(),
            hook_count: effective.hooks.len(),
            arg_count: effective.args.len(),
            ref_count,
            diagnostics,
        });
    }

    results
}

fn dir_contains_skill_md(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    rd.flatten()
        .any(|e| e.path().is_dir() && e.path().join("SKILL.md").is_file())
}

/// Validates skills in a single directory (self-contained registry).
pub fn check_skills_in_dir(
    dir: &Path,
    scope_label: &str,
    config: &Config,
    check_enabled: bool,
    name_filter: Option<&str>,
) -> SkillCheckReport {
    let scanned = scan_raw_dir(dir, scope_label, MemoryScope::Workspace);
    let results = check_scanned_registry(scanned, &[], config, check_enabled, name_filter);
    SkillCheckReport { results }
}

/// Validates workspace skills while allowing `extends` references to resolve against global skills.
pub fn check_workspace_skills(
    ws_dir: &Path,
    global_dir: &Path,
    config: &Config,
    check_enabled: bool,
    name_filter: Option<&str>,
) -> SkillCheckReport {
    let fallback_parents = scan_skills_dir(global_dir);
    let scanned = scan_raw_dir(ws_dir, "workspace", MemoryScope::Workspace);
    let results = check_scanned_registry(
        scanned,
        &fallback_parents,
        config,
        check_enabled,
        name_filter,
    );
    SkillCheckReport { results }
}

/// Validates skills across configured directories (global, workspace, and optional repo `./skills`).
pub fn check_configured_skills(
    config: &Config,
    name_filter: Option<&str>,
    dir_override: Option<&Path>,
    global_only: bool,
    include_repo_dir: bool,
) -> SkillCheckReport {
    if let Some(dir) = dir_override {
        return check_skills_in_dir(dir, "dir", config, false, name_filter);
    }

    let global_dir = expand_tilde(&config.app.skills_dir);
    if global_only {
        return check_skills_in_dir(&global_dir, "global", config, true, name_filter);
    }

    let mut active_scanned = scan_raw_dir(&global_dir, "global", MemoryScope::Global);
    let ws_dir = config.workspace_skills_dir.as_deref();
    if let Some(ws) = ws_dir {
        active_scanned.extend(scan_raw_dir(ws, "workspace", MemoryScope::Workspace));
    }
    let mut results = check_scanned_registry(active_scanned, &[], config, true, name_filter);

    if include_repo_dir {
        let cwd_skills = std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join("skills"))
            .filter(|p| dir_contains_skill_md(p));
        if let Some(repo_skills) = cwd_skills {
            let canon_repo = dunce::canonicalize(&repo_skills).unwrap_or(repo_skills.clone());
            let canon_global = dunce::canonicalize(&global_dir).unwrap_or(global_dir);
            let canon_ws =
                ws_dir.map(|w| dunce::canonicalize(w).unwrap_or_else(|_| w.to_path_buf()));
            if canon_repo != canon_global && canon_ws.as_deref() != Some(canon_repo.as_path()) {
                let repo_scanned = scan_raw_dir(&repo_skills, "repo", MemoryScope::Workspace);
                results.extend(check_scanned_registry(
                    repo_scanned,
                    &[],
                    config,
                    true,
                    name_filter,
                ));
            }
        }
    }

    SkillCheckReport { results }
}

pub fn handle_command(cmd: SkillCommand, config: &Config) -> Result<()> {
    match cmd {
        SkillCommand::Check {
            name,
            dir,
            global,
            strict,
        } => {
            if let Some(ref d) = dir
                && !d.exists()
            {
                bail!("Skills directory does not exist: {}", d.display());
            }
            let report =
                check_configured_skills(config, name.as_deref(), dir.as_deref(), global, true);
            println!("{}", report.format_report(strict));
            if !report.is_ok(strict) {
                bail!(
                    "Skill check failed with {} error(s) and {} warning(s).",
                    report.error_count(),
                    report.warning_count()
                );
            }
            Ok(())
        }
        SkillCommand::List { dir, global } => {
            let is_dir_override = dir.is_some();
            let skills = if let Some(ref d) = dir {
                if !d.exists() {
                    bail!("Skills directory does not exist: {}", d.display());
                }
                crate::core::skills::discover_skills(&d.to_string_lossy(), None)
            } else if global {
                crate::core::skills::discover_skills(&config.app.skills_dir, None)
            } else {
                config.skills.clone()
            };

            if skills.is_empty() {
                println!("No skills found.");
                return Ok(());
            }

            println!("Loaded skills ({}):", skills.len());
            for s in &skills {
                let scope_tag = if is_dir_override {
                    "dir"
                } else if s.overrides_global {
                    "global→workspace"
                } else {
                    match s.scope {
                        MemoryScope::Global => "global",
                        MemoryScope::Workspace => "workspace",
                    }
                };
                let kind = if s.is_abstract {
                    "abstract base".to_string()
                } else {
                    format!("@{}", s.trigger())
                };
                let ext = if s.extends.is_empty() {
                    String::new()
                } else {
                    format!(" | extends: {}", s.extends.join(", "))
                };
                let args_str = if s.args.is_empty() {
                    String::new()
                } else {
                    format!(" | args: {}", s.args_usage_hint())
                };
                println!(
                    "  - {} ({}) [{}]{}{} | {} tools, {} hooks",
                    s.name,
                    kind,
                    scope_tag,
                    ext,
                    args_str,
                    s.tools.len(),
                    s.hooks.len()
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_check_catches_bad_tools_hook_schemas_refs_templates_and_inheritance() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // 1. Valid base skill
        fs::create_dir_all(dir.join("base-rust/references")).unwrap();
        fs::write(dir.join("base-rust/references/guide.md"), "# Guide\n").unwrap();
        fs::write(
            dir.join("base-rust/SKILL.md"),
            r#"+++
name = "base-rust"
abstract = true
tools = ["repo_map", "read_file", "workspace_snapshot", "cargo"]
references = ["references/guide.md"]

[[hooks]]
trigger = "on_activate"
tool = "workspace_snapshot"
args = { tree_only = false }

[[hooks]]
trigger = "on_activate"
tool = "repo_map"
args = { action = "map", max_tokens = 8000 }
+++
Read `references/guide.md`."#,
        )
        .unwrap();

        // 2. Valid child extending base-rust with typed args used in prompt and hook
        fs::create_dir_all(dir.join("good-child")).unwrap();
        fs::write(
            dir.join("good-child/SKILL.md"),
            r#"+++
name = "good-child"
command = "good"
extends = "base-rust"
tools = ["edit_file"]

[[args]]
name = "target"
type = "path"
default = "src"

[[args]]
name = "max_tokens"
type = "int"
default = 4000

[[hooks]]
trigger = "every_turn"
tool = "repo_map"
args = { action = "map", target_path = "{{target}}", max_tokens = "{{max_tokens}}" }
+++
Work on `{{target}}`.
{{#if args}}Notes: {{args}}{{/if}}"#,
        )
        .unwrap();

        // 3. Broken skill exercising all error classes
        fs::create_dir_all(dir.join("broken")).unwrap();
        fs::write(
            dir.join("broken/SKILL.md"),
            r#"+++
name = "broken"
extends = "does-not-exist"
tools = ["fake_tool"]
references = ["references/missing.md"]
typo_key = 123

[[args]]
name = "mode"
type = "enum"
default = "nope"

[[hooks]]
trigger = "on_activate"
tool = "repo_map"
args = { action = "invalid_action", max_tockens = 5000 }

[[hooks]]
trigger = "on_activate"
tool = "cargo"
args = {}
+++
Check `references/also-missing.md` and {{undeclared_var}}.
{{#if mode}}Unclosed block"#,
        )
        .unwrap();

        let cfg = Config::default();
        let report = check_skills_in_dir(dir, "test", &cfg, false, None);
        assert_eq!(report.results.len(), 3);

        let base_res = report
            .results
            .iter()
            .find(|r| r.name == "base-rust")
            .unwrap();
        assert!(
            base_res.is_ok(true),
            "base-rust errors: {:?}",
            base_res.diagnostics
        );

        let child_res = report
            .results
            .iter()
            .find(|r| r.name == "good-child")
            .unwrap();
        assert!(
            child_res.is_ok(true),
            "good-child errors: {:?}",
            child_res.diagnostics
        );

        let broken_res = report.results.iter().find(|r| r.name == "broken").unwrap();
        assert!(!broken_res.is_ok(false));
        let msgs: Vec<&str> = broken_res
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            msgs.iter()
                .any(|m| m.contains("unknown parent skill 'does-not-exist'")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("Unknown tool 'fake_tool'")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("Unknown frontmatter key 'typo_key'")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("`choices` is empty")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("unknown argument 'max_tockens'")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("invalid value \"invalid_action\"")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("missing required argument 'action'")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("references/missing.md")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("references/also-missing.md")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("{{undeclared_var}}")),
            "{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("Unclosed template block")),
            "{msgs:?}"
        );
    }

    #[test]
    fn skill_check_isolates_global_overrides_supports_cross_scope_extends_and_checks_optional_hook_args()
     {
        let tmp = tempfile::tempdir().unwrap();
        let global_dir = tmp.path().join("global");
        let ws_dir = tmp.path().join("workspace");

        // Global: valid base + valid skill `shared`
        fs::create_dir_all(global_dir.join("base-rust")).unwrap();
        fs::write(
            global_dir.join("base-rust/SKILL.md"),
            "+++\nname = \"base-rust\"\nabstract = true\ntools = [\"read_file\"]\n+++\nBase.",
        )
        .unwrap();
        fs::create_dir_all(global_dir.join("shared")).unwrap();
        fs::write(
            global_dir.join("shared/SKILL.md"),
            "+++\nname = \"shared\"\nextends = \"base-rust\"\n+++\nGlobal shared.",
        )
        .unwrap();

        // Workspace: broken override of `shared` + valid `ws-child` extending global `base-rust`
        // + skill with optional no-default arg wired to a required tool parameter
        fs::create_dir_all(ws_dir.join("shared")).unwrap();
        fs::write(
            ws_dir.join("shared/SKILL.md"),
            "+++\nname = \"shared\"\nextends = \"missing-parent\"\n+++\nBroken ws override.",
        )
        .unwrap();
        fs::create_dir_all(ws_dir.join("ws-child")).unwrap();
        fs::write(
            ws_dir.join("ws-child/SKILL.md"),
            "+++\nname = \"ws-child\"\nextends = \"base-rust\"\n+++\nChild.",
        )
        .unwrap();
        fs::create_dir_all(ws_dir.join("opt-hook")).unwrap();
        fs::write(
            ws_dir.join("opt-hook/SKILL.md"),
            r#"+++
name = "opt-hook"
tools = ["read_file"]

[[args]]
name = "file"
type = "path"
required = false

[[hooks]]
trigger = "on_activate"
tool = "read_file"
args = { file_path = "{{file}}" }
+++
Read optional file."#,
        )
        .unwrap();

        let cfg = Config::default();

        // Combined check: global `shared` must stay passing while workspace `shared` fails.
        let mut combined = scan_raw_dir(&global_dir, "global", MemoryScope::Global);
        combined.extend(scan_raw_dir(&ws_dir, "workspace", MemoryScope::Workspace));
        let results = check_scanned_registry(combined, &[], &cfg, false, None);

        let global_shared = results
            .iter()
            .find(|r| r.name == "shared" && r.scope_label == "global")
            .unwrap();
        assert!(
            global_shared.is_ok(true),
            "global shared should not inherit workspace override errors: {:?}",
            global_shared.diagnostics
        );

        let ws_shared = results
            .iter()
            .find(|r| r.name == "shared" && r.scope_label == "workspace")
            .unwrap();
        assert!(!ws_shared.is_ok(false));

        // Workspace-only check resolves `ws-child` against global `base-rust`.
        let ws_report = check_workspace_skills(&ws_dir, &global_dir, &cfg, false, None);
        let ws_child = ws_report
            .results
            .iter()
            .find(|r| r.name == "ws-child")
            .unwrap();
        assert!(
            ws_child.is_ok(true),
            "ws-child should extend global base-rust cleanly: {:?}",
            ws_child.diagnostics
        );

        // Optional no-default arg wired to required `read_file.file_path` must be flagged.
        let opt_hook = ws_report
            .results
            .iter()
            .find(|r| r.name == "opt-hook")
            .unwrap();
        assert!(!opt_hook.is_ok(false));
        assert!(
            opt_hook
                .diagnostics
                .iter()
                .any(|d| d.message.contains("when optional argument(s) are omitted")),
            "{:?}",
            opt_hook.diagnostics
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_check_rejects_escaping_and_cyclic_reference_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("secret.txt");
        fs::write(&outside, "secret").unwrap();

        let skills_dir = tmp.path().join("skills");
        let refs_dir = skills_dir.join("sym-skill/references");
        fs::create_dir_all(&refs_dir).unwrap();
        symlink(&outside, refs_dir.join("leak.md")).unwrap();
        symlink(&refs_dir, refs_dir.join("loop")).unwrap();
        fs::write(
            skills_dir.join("sym-skill/SKILL.md"),
            "+++\nname = \"sym-skill\"\nreferences = [\"references/leak.md\"]\n+++\nUse references/leak.md.",
        )
        .unwrap();

        let cfg = Config::default();
        let report = check_skills_in_dir(&skills_dir, "test", &cfg, false, None);
        let res = &report.results[0];
        assert!(!res.is_ok(false));
        assert!(
            res.diagnostics
                .iter()
                .any(|d| d.message.contains("resolves outside the skill directory")),
            "{:?}",
            res.diagnostics
        );
        assert!(
            res.diagnostics
                .iter()
                .any(|d| d.message.contains("Cyclic symlink")),
            "{:?}",
            res.diagnostics
        );
    }

    #[test]
    fn repo_skills_directory_passes_skill_check() {
        let repo_skills = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        if repo_skills.is_dir() {
            let cfg = Config::default();
            let report = check_skills_in_dir(&repo_skills, "repo", &cfg, false, None);
            assert!(
                report.is_ok(true),
                "Repo skills check failed:\n{}",
                report.format_report(true)
            );
        }
    }
}
