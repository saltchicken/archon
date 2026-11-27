//! Core data types for the skill system: triggers, hooks, argument definitions,
//! and the `SkillDefinition` struct.

use crate::core::config::MemoryScope;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const RESERVED_ARG_NAMES: &[&str] = &["args", "raw_args", "last_response", "super"];

pub const KNOWN_FRONTMATTER_KEYS: &[&str] = &[
    "name",
    "description",
    "command",
    "extends",
    "abstract",
    "tools",
    "auto_approve",
    "hooks",
    "args",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookTrigger {
    /// Run once when the skill is first activated in a session (default).
    #[default]
    #[serde(alias = "once", alias = "activate")]
    OnActivate,
    /// Run before every user turn while this skill is active.
    #[serde(alias = "always", alias = "turn", alias = "before_turn")]
    EveryTurn,
    /// Run after every user turn while this skill is active.
    #[serde(alias = "after", alias = "post_turn", alias = "on_complete")]
    AfterTurn,
}

impl HookTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnActivate => "on_activate",
            Self::EveryTurn => "every_turn",
            Self::AfterTurn => "after_turn",
        }
    }
}

fn default_hook_args() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct SkillHook {
    /// When to execute the hook: `on_activate` (default, once per session), `every_turn` (before every turn), or `after_turn` (after every turn).
    #[serde(default)]
    pub trigger: HookTrigger,
    /// Built-in tool name to execute (e.g. `"workspace_snapshot"`, `"repo_map"`, `"git"`).
    pub tool: String,
    /// Tool arguments object (e.g. `{"tree_only": true}`). String values may include `{{last_response}}`, `{{args}}`, or declared `{{arg_name}}` placeholders.
    #[serde(default = "default_hook_args")]
    pub args: Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillArgType {
    /// Arbitrary string value (default).
    #[default]
    #[serde(alias = "str", alias = "text")]
    String,
    /// Relative or tilde-expanded file/directory path.
    #[serde(alias = "file", alias = "dir")]
    Path,
    /// 64-bit signed integer.
    #[serde(alias = "integer", alias = "i64")]
    Int,
    /// Floating-point number.
    #[serde(alias = "number", alias = "f64")]
    Float,
    /// Boolean (`true`/`false`, `yes`/`no`, `1`/`0`).
    #[serde(alias = "boolean")]
    Bool,
    /// One of the allowed values declared in `choices`.
    #[serde(alias = "choice")]
    Enum,
}

impl SkillArgType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Path => "path",
            Self::Int => "int",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::Enum => "enum",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct SkillArgDef {
    /// Argument identifier (used in `key=value` and `{{name}}` template placeholders).
    pub name: String,
    /// Expected input type: `string` (default), `path`, `int`, `float`, `bool`, or `enum`.
    #[serde(default, rename = "type")]
    pub arg_type: SkillArgType,
    /// Whether the user must provide this argument when invoking the skill (default: `false`).
    #[serde(default)]
    pub required: bool,
    /// Optional default value used when the argument is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Allowed values when `type = "enum"` (or to constrain string/path values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// Short human-readable description shown in usage hints and error messages.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl SkillArgDef {
    pub fn usage_token(&self) -> String {
        let rhs = if !self.choices.is_empty() {
            self.choices.join("|")
        } else {
            format!("<{}>", self.arg_type.as_str())
        };
        if self.required {
            format!("{}={rhs}", self.name)
        } else {
            format!("[{}={rhs}]", self.name)
        }
    }

    pub fn sample_value(&self) -> Value {
        if let Some(ref def) = self.default
            && let Ok(valid) = super::args::validate_arg_json_value(self, def)
        {
            return valid;
        }
        match self.arg_type {
            SkillArgType::String => self
                .choices
                .first()
                .map(|c| Value::String(c.clone()))
                .unwrap_or_else(|| Value::String("sample".into())),
            SkillArgType::Path => self
                .choices
                .first()
                .map(|c| Value::String(c.clone()))
                .unwrap_or_else(|| Value::String("src".into())),
            SkillArgType::Int => Value::Number(1.into()),
            SkillArgType::Float => serde_json::Number::from_f64(1.0)
                .map(Value::Number)
                .unwrap_or(Value::Number(1.into())),
            SkillArgType::Bool => Value::Bool(true),
            SkillArgType::Enum => self
                .choices
                .first()
                .map(|c| Value::String(c.clone()))
                .unwrap_or_else(|| Value::String("sample".into())),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub command: String,
    pub extends: Vec<String>,
    pub is_abstract: bool,
    pub tools: Vec<String>,
    pub excluded_tools: Vec<String>,
    pub tools_restricted: bool,
    pub auto_approve: Vec<String>,
    pub excluded_auto_approve: Vec<String>,
    pub hooks: Vec<SkillHook>,
    pub args: Vec<SkillArgDef>,
    pub prompt: String,
    pub dir_path: Option<PathBuf>,
    pub inherited_dirs: Vec<PathBuf>,
    pub scope: MemoryScope,
    pub overrides_global: bool,
    pub resolution_errors: Vec<String>,
}

impl SkillDefinition {
    pub fn trigger(&self) -> &str {
        if self.command.is_empty() {
            &self.name
        } else {
            &self.command
        }
    }

    pub fn allows_tool(&self, name: &str) -> bool {
        if self
            .excluded_tools
            .iter()
            .any(|t| t.eq_ignore_ascii_case(name))
        {
            return false;
        }
        if self.tools.is_empty() {
            return !self.tools_restricted;
        }
        self.tools
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .any(|t| t == "*" || t == name)
    }

    /// Returns `true` if the skill explicitly names `name` in `tools` (not via `"*"`)
    /// and does not exclude it. Explicitly declared skill tools bypass deferred loading.
    pub fn explicitly_declares_tool(&self, name: &str) -> bool {
        if self
            .excluded_tools
            .iter()
            .any(|t| t.eq_ignore_ascii_case(name))
        {
            return false;
        }
        self.tools
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .any(|t| t != "*" && t == name)
    }

    pub fn auto_approve_allows(&self, call: &crate::tools::ToolCall) -> bool {
        let Some(incl) = self
            .auto_approve
            .iter()
            .filter_map(|r| crate::tools::auto_approve_match_specificity(call, r))
            .max()
        else {
            return false;
        };
        let excl = self
            .excluded_auto_approve
            .iter()
            .filter_map(|r| crate::tools::auto_approve_match_specificity(call, r))
            .max();
        excl.is_none_or(|e| incl > e)
    }

    pub fn all_dirs(&self) -> impl Iterator<Item = &Path> {
        self.dir_path
            .as_deref()
            .into_iter()
            .chain(self.inherited_dirs.iter().map(PathBuf::as_path))
    }

    pub fn args_usage_hint(&self) -> String {
        self.args
            .iter()
            .map(SkillArgDef::usage_token)
            .collect::<Vec<_>>()
            .join(" ")
    }
}
