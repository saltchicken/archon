use super::{BlockingTool, ToolContext};
use crate::core::skills::{find_skill, list_skill_resources, read_skill_resource};
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use std::fmt::Write as _;

pub struct Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillAction {
    /// List all available skills with their descriptions, slash commands, and associated tools.
    List,
    /// Load the full instructions and resource manifest for a named skill.
    Load,
    /// Read a supplementary reference file from a skill's directory on disk.
    ReadResource,
}

#[derive(Deserialize, JsonSchema)]
pub struct SkillArgs {
    /// The skill operation to perform (`list`, `load`, or `read_resource`).
    pub action: SkillAction,
    /// Name or command of the skill (required when `action` is `load` or `read_resource`).
    #[serde(default)]
    pub name: String,
    /// Relative path to an auxiliary resource file inside the skill directory (required when `action` is `read_resource`).
    #[serde(default)]
    pub resource_path: String,
}

impl BlockingTool for Tool {
    const NAME: &'static str = "skill";
    const DESCRIPTION: &'static str = "Discovers and loads specialized domain skills, workflows, and reference files on demand (`list`, `load`, `read_resource`).";
    type Args = SkillArgs;

    fn execute(args: Self::Args, ctx: &ToolContext) -> Result<String> {
        match args.action {
            SkillAction::List => {
                if ctx.config.skills.is_empty() {
                    return Ok("No skills configured or discovered.".into());
                }
                let mut out = format!("=== Available Skills ({}) ===\n", ctx.config.skills.len());
                for s in &ctx.config.skills {
                    let cmd = if s.command.is_empty() {
                        String::new()
                    } else {
                        format!(" [/{}]", s.command)
                    };
                    let tools = if s.tools.is_empty() {
                        String::new()
                    } else {
                        format!(" (tools: {})", s.tools.join(", "))
                    };
                    let _ = writeln!(out, "• {}{}{}: {}", s.name, cmd, tools, s.description);
                }
                Ok(out.trim_end().to_string())
            }
            SkillAction::Load => {
                let target = args.name.trim();
                if target.is_empty() {
                    bail!("'name' is required when `action` is `load`");
                }
                let Some(skill) = find_skill(&ctx.config, target) else {
                    let available: Vec<&str> =
                        ctx.config.skills.iter().map(|s| s.name.as_str()).collect();
                    bail!(
                        "Skill '{}' not found. Available skills: {}",
                        target,
                        available.join(", ")
                    );
                };

                let mut out = format!(
                    "=== Skill: {} ===\nDescription: {}\n",
                    skill.name, skill.description
                );
                if !skill.command.is_empty() {
                    let _ = writeln!(out, "Command: /{}", skill.command);
                }
                if !skill.tools.is_empty() {
                    let _ = writeln!(out, "Recommended Tools: {}", skill.tools.join(", "));
                }
                let resources = list_skill_resources(skill);
                if !resources.is_empty() {
                    let _ = writeln!(
                        out,
                        "Supplementary Resources (load with `skill` `action: \"read_resource\"`): {}",
                        resources.join(", ")
                    );
                }
                let _ = write!(out, "\n{}\n", skill.prompt.trim());
                Ok(out.trim_end().to_string())
            }
            SkillAction::ReadResource => {
                let target = args.name.trim();
                if target.is_empty() {
                    bail!("'name' is required when `action` is `read_resource`");
                }
                let Some(skill) = find_skill(&ctx.config, target) else {
                    bail!("Skill '{}' not found", target);
                };
                read_skill_resource(skill, &args.resource_path, ctx.max_output_size)
            }
        }
    }
}
