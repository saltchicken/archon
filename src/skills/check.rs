use super::parse::{ScannedRawSkill, scan_raw_dir};
use crate::{
    cli::SkillCommand,
    config::{Config, expand_tilde, format_tilde_path},
    schema_utils,
    skills::{
        HookTrigger, RESERVED_ARG_NAMES, SkillArgDef, SkillArgType, SkillDefinition, SkillHook,
        TemplateContext, inspect_template, interpolate_hook_args, resolve_skills_with_groups,
        validate_arg_json_value,
    },
    tools::{self, ALL_TOOL_NAMES},
};
use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
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
    if let Some(ref_str) = obj.get("$ref").and_then(Value::as_str)
        && let Some(name) = schema_utils::resolve_ref_name(ref_str)
        && let Some(target) = defs.get(name)
    {
        validate_value_against_schema(value, target, defs, path, errors, depth + 1);
        // If there are sibling keywords besides `$ref`, validate them too.
        if obj.len() == 1 {
            return;
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
                if let Some(choices) = schema_utils::extract_enum_strings(branches) {
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

    let defs = schema_utils::extract_root_defs(params_schema);

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

fn is_known_template_var(
    var: &str,
    args: &[SkillArgDef],
    allow_last_response: bool,
    allow_super: bool,
) -> bool {
    if matches!(var, "args" | "raw_args" | "skill.name") {
        return true;
    }
    if allow_super && var == "super" {
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
            } else if !is_known_template_var(var, resolved_args, true, false) {
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

fn check_scanned_registry(
    scanned: Vec<ScannedRawSkill>,
    config: &Config,
    check_enabled: bool,
    name_filter: Option<&str>,
) -> Vec<SkillCheckResult> {
    let defs: Vec<SkillDefinition> = scanned
        .iter()
        .filter_map(|item| item.parsed.as_ref()?.definition.clone())
        .collect();
    let resolved_all = resolve_skills_with_groups(defs, &config.tool_groups);
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

        let resolved = raw_def.as_ref().and_then(|d| {
            resolved_all.iter().find(|r| {
                r.name.eq_ignore_ascii_case(&d.name)
                    && r.dir_path.as_deref() == Some(item.skill_dir.as_path())
            })
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
                crate::skills::KNOWN_FRONTMATTER_KEYS.join(", ")
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
        let mut warned_disabled_tools = HashSet::new();
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
                if neg_name.is_empty()
                    || !tools::is_known_tool_or_group(neg_name, &config.tool_groups)
                {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Unknown tool '{neg_name}' in negated `tools` entry '{trimmed}' ({})",
                        tools::format_valid_tools_and_groups(&config.tool_groups)
                    )));
                } else if let Some(neg_group) = neg_name.strip_prefix('@')
                    && let Some(members) = tools::resolve_tool_group(neg_group, &config.tool_groups)
                {
                    for member in members {
                        if !ALL_TOOL_NAMES.contains(&member.as_str()) {
                            diagnostics.push(SkillDiagnostic::error(format!(
                                "Unknown tool/group '{member}' (from negated group '{trimmed}') in `tools` ({})",
                                tools::format_valid_tools_and_groups(&config.tool_groups)
                            )));
                        }
                    }
                }
            } else if let Some(group_key) = trimmed.strip_prefix('@') {
                if let Some(members) = tools::resolve_tool_group(group_key, &config.tool_groups) {
                    for member in members {
                        if !ALL_TOOL_NAMES.contains(&member.as_str()) {
                            diagnostics.push(SkillDiagnostic::error(format!(
                                "Unknown tool/group '{member}' (from group '{trimmed}') in `tools` ({})",
                                tools::format_valid_tools_and_groups(&config.tool_groups)
                            )));
                        } else if check_enabled
                            && !tools::is_tool_enabled(&member, config)
                            && warned_disabled_tools.insert(member.clone())
                        {
                            diagnostics.push(SkillDiagnostic::warning(format!(
                                "Tool '{member}' (from group '{trimmed}') is not enabled in current config (`app.enabled_tools`)"
                            )));
                        }
                    }
                } else {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Unknown tool group '{trimmed}' in `tools` ({})",
                        tools::format_valid_tools_and_groups(&config.tool_groups)
                    )));
                }
            } else if !ALL_TOOL_NAMES.contains(&trimmed) {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Unknown tool '{trimmed}' in `tools` ({})",
                    tools::format_valid_tools_and_groups(&config.tool_groups)
                )));
            } else if check_enabled
                && !tools::is_tool_enabled(trimmed, config)
                && warned_disabled_tools.insert(trimmed.to_string())
            {
                diagnostics.push(SkillDiagnostic::warning(format!(
                    "Tool '{trimmed}' is not enabled in current config (`app.enabled_tools`)"
                )));
            }
        }
        if check_enabled && !raw_skill.extends.is_empty() {
            for tool in &effective.tools {
                let t = tool.trim();
                if t.is_empty() || t == "*" || !ALL_TOOL_NAMES.contains(&t) {
                    continue;
                }
                if !tools::is_tool_enabled(t, config) && warned_disabled_tools.insert(t.to_string())
                {
                    diagnostics.push(SkillDiagnostic::warning(format!(
                        "Inherited tool '{t}' is not enabled in current config (`app.enabled_tools`)"
                    )));
                }
            }
        }

        // 4b. Auto-approve rules verification
        for raw_rule in &parsed.raw_auto_approve {
            let trimmed = raw_rule.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (is_neg, rule) = match trimmed.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, trimmed),
            };
            if is_neg && raw_skill.extends.is_empty() {
                diagnostics.push(SkillDiagnostic::warning(format!(
                    "`auto_approve` negation '{trimmed}' has no effect on a skill without `extends`"
                )));
            }
            if rule == "*" {
                continue;
            }
            let (tool_name, subaction) = match rule.split_once(':') {
                Some((t, sub)) => {
                    if t != t.trim() || sub != sub.trim() {
                        diagnostics.push(SkillDiagnostic::error(format!(
                            "Unexpected whitespace around ':' in `auto_approve` entry '{trimmed}'"
                        )));
                        continue;
                    }
                    (t, Some(sub))
                }
                None => (rule, None),
            };
            if tool_name.is_empty()
                || (tool_name != tools::LOAD_TOOLS_NAME && !ALL_TOOL_NAMES.contains(&tool_name))
            {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Unknown tool '{tool_name}' in `auto_approve` entry '{trimmed}' (valid tools: {})",
                    ALL_TOOL_NAMES.join(", ")
                )));
                continue;
            }
            if let Some(sub) = subaction
                && sub != "*"
            {
                let valid_subs = tools::tool_subactions(tool_name);
                if valid_subs.is_empty() {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Tool '{tool_name}' in `auto_approve` entry '{trimmed}' does not support subactions"
                    )));
                } else if !valid_subs.contains(&sub) {
                    diagnostics.push(SkillDiagnostic::error(format!(
                        "Unknown subaction '{sub}' for tool '{tool_name}' in `auto_approve` entry '{trimmed}' (valid subactions: {})",
                        valid_subs.join(", ")
                    )));
                }
            }
            if !is_neg && tool_name != tools::LOAD_TOOLS_NAME && !effective.allows_tool(tool_name) {
                diagnostics.push(SkillDiagnostic::warning(format!(
                    "`auto_approve` entry '{trimmed}' targets tool '{tool_name}', which is not enabled by this skill's `tools`"
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
            if !is_known_template_var(&var, &effective.args, false, !raw_skill.extends.is_empty()) {
                diagnostics.push(SkillDiagnostic::error(format!(
                    "Prompt references undeclared template variable `{{{{{var}}}}}`"
                )));
            }
        }

        results.push(SkillCheckResult {
            name: effective.name.clone(),
            scope_label: item.scope_label,
            file_path: item.file_path,
            extends: effective.extends.clone(),
            is_abstract: effective.is_abstract,
            tool_count: effective.tools.len(),
            hook_count: effective.hooks.len(),
            arg_count: effective.args.len(),
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
    let scanned = scan_raw_dir(dir, scope_label);
    let results = check_scanned_registry(scanned, config, check_enabled, name_filter);
    SkillCheckReport { results }
}

/// Validates skills in the configured skills directory (and optional repo `./skills`).
pub fn check_configured_skills(
    config: &Config,
    name_filter: Option<&str>,
    dir_override: Option<&Path>,
) -> SkillCheckReport {
    if let Some(dir) = dir_override {
        return check_skills_in_dir(dir, "dir", config, false, name_filter);
    }

    let skills_dir = expand_tilde(&config.app.skills_dir);
    let mut results = check_skills_in_dir(&skills_dir, "global", config, true, name_filter).results;

    let cwd_skills = std::env::current_dir()
        .ok()
        .map(|cwd| cwd.join("skills"))
        .filter(|p| dir_contains_skill_md(p));
    if let Some(repo_skills) = cwd_skills {
        let canon_repo = dunce::canonicalize(&repo_skills).unwrap_or(repo_skills.clone());
        let canon_skills = dunce::canonicalize(&skills_dir).unwrap_or(skills_dir);
        if canon_repo != canon_skills {
            results.extend(
                check_skills_in_dir(&repo_skills, "repo", config, true, name_filter).results,
            );
        }
    }

    SkillCheckReport { results }
}

pub fn handle_command(cmd: SkillCommand, config: &Config) -> Result<()> {
    match cmd {
        SkillCommand::Check { name, dir, strict } => {
            let dir = dir.map(|d| expand_tilde(&d.to_string_lossy()));
            if let Some(ref d) = dir
                && !d.exists()
            {
                bail!("Skills directory does not exist: {}", d.display());
            }
            let report = check_configured_skills(config, name.as_deref(), dir.as_deref());
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
        SkillCommand::List { dir } => {
            let dir = dir.map(|d| expand_tilde(&d.to_string_lossy()));
            let skills = if let Some(ref d) = dir {
                if !d.exists() {
                    bail!("Skills directory does not exist: {}", d.display());
                }
                crate::skills::discover_skills_with_groups(
                    &d.to_string_lossy(),
                    &config.tool_groups,
                )
            } else {
                config.skills.clone()
            };

            if skills.is_empty() {
                println!("No skills found.");
                return Ok(());
            }

            println!("Loaded skills ({}):", skills.len());
            for s in &skills {
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
                    "  - {} ({}){}{} | {} tools, {} hooks",
                    s.name,
                    kind,
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
