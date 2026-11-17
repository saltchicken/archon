use super::{TuiApp, create_text_area, create_text_area_with};
use crate::{
    core::{
        EngineCommand,
        config::{
            Memory, MemoryScope, clean_memory_item, global_memory_path, load_scoped_memory_file,
        },
        session::ChatMessage,
        skills,
    },
    tools,
};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum SlashCommand {
    Compact,
    Exit,
    Fork,
    Inspect,
    Memory,
    Pin,
    Rename,
    Reset,
    Run,
}

impl SlashCommand {
    pub const ALL: &[(&str, Self)] = &[
        ("compact", Self::Compact),
        ("exit", Self::Exit),
        ("fork", Self::Fork),
        ("inspect", Self::Inspect),
        ("memory", Self::Memory),
        ("pin", Self::Pin),
        ("rename", Self::Rename),
        ("reset", Self::Reset),
        ("run", Self::Run),
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(cmd, _)| cmd.eq_ignore_ascii_case(name))
            .map(|(_, variant)| *variant)
    }

    pub fn matching_triggers(prefix: &str) -> Vec<String> {
        let lower = prefix.to_ascii_lowercase();
        Self::ALL
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(&lower))
            .map(|(cmd, _)| format!("/{cmd}"))
            .collect()
    }
}

impl TuiApp {
    pub(super) fn is_prompt_empty(&self) -> bool {
        self.text_area.lines().iter().all(String::is_empty)
    }

    fn record_prompt_and_clear(&mut self, raw_prompt: &str) {
        self.prompt_history_idx = None;
        if !raw_prompt.is_empty()
            && self
                .prompt_history
                .last()
                .is_none_or(|last| last != raw_prompt)
        {
            self.prompt_history.push(raw_prompt.to_string());
        }
        self.text_area = create_text_area();
    }

    fn dispatch_shell_command(
        &mut self,
        raw_prompt: &str,
        command: &str,
        include_in_context: bool,
    ) {
        if command.is_empty() {
            return;
        }
        self.record_prompt_and_clear(raw_prompt);
        self.scroll_to_bottom();
        let _ = self.cmd_tx.send(EngineCommand::RunShellCommand {
            command: command.to_string(),
            include_in_context,
        });
    }

    fn dispatch_manual_tool(
        &mut self,
        raw_prompt: &str,
        tool_input: &str,
        include_in_context: bool,
    ) {
        if tool_input.is_empty() {
            return;
        }
        self.record_prompt_and_clear(raw_prompt);
        self.scroll_to_bottom();
        let _ = self.cmd_tx.send(EngineCommand::RunManualTool {
            raw_input: tool_input.to_string(),
            include_in_context,
        });
    }

    pub(super) fn try_tab_complete_prompt(&mut self) -> bool {
        if self.text_area.lines().len() != 1 {
            return false;
        }
        let line = self.text_area.lines()[0].trim_start();
        if line.is_empty() || line.contains(char::is_whitespace) {
            return false;
        }

        let completed = if let Some(cmd_prefix) = line.strip_prefix('/') {
            SlashCommand::matching_triggers(cmd_prefix)
                .into_iter()
                .next()
                .map(|m| format!("{m} "))
        } else if let Some(rest) = line.strip_prefix('@') {
            self.config
                .skills
                .iter()
                .find(|s| {
                    (!s.command.is_empty() && s.command.starts_with(rest))
                        || s.name.starts_with(rest)
                })
                .map(|s| format!("@{} ", s.trigger()))
        } else if !line.starts_with("###")
            && let Some((prefix_mark, rest)) = line
                .strip_prefix("##")
                .map(|r| ("##", r))
                .or_else(|| line.strip_prefix('#').map(|r| ("#", r)))
        {
            tools::matching_tool_triggers(prefix_mark, rest)
                .into_iter()
                .next()
                .map(|m| format!("{m} "))
        } else {
            None
        };

        if let Some(new_text) = completed {
            self.prompt_history_idx = None;
            self.text_area = create_text_area_with(&new_text);
            true
        } else {
            false
        }
    }

    fn handle_memory_slash_command(&mut self, raw_prompt: &str, args: &str) {
        self.record_prompt_and_clear(raw_prompt);
        if args.is_empty() {
            self.open_memory_modal();
            return;
        }
        let lower_args = args.to_ascii_lowercase();
        if matches!(lower_args.as_str(), "global") {
            self.memory_scope = MemoryScope::Global;
            self.open_memory_modal();
            return;
        }
        if matches!(lower_args.as_str(), "workspace" | "ws") {
            self.memory_scope = MemoryScope::Workspace;
            self.open_memory_modal();
            return;
        }

        let (scope, sub_args) = if let Some(rest_args) = args
            .strip_prefix("workspace ")
            .or_else(|| args.strip_prefix("ws "))
        {
            (MemoryScope::Workspace, rest_args.trim())
        } else if let Some(rest_args) = args.strip_prefix("global ") {
            (MemoryScope::Global, rest_args.trim())
        } else {
            (MemoryScope::Global, args)
        };

        let (mem_path, is_global) = match scope {
            MemoryScope::Global => (global_memory_path(), true),
            MemoryScope::Workspace => (self.config.workspace_memory_path(), false),
        };
        let mut mem = load_scoped_memory_file(&mem_path, is_global)
            .map(|(_, m)| m)
            .unwrap_or_else(|_| self.config.scoped_memory(scope).clone());
        match sub_args.to_ascii_lowercase().as_str() {
            "clear" => {
                mem.items.clear();
            }
            "on" | "enable" => {
                mem.enabled = true;
            }
            "off" | "disable" => {
                mem.enabled = false;
            }
            _ => {
                let fact = clean_memory_item(sub_args.strip_prefix("add ").unwrap_or(sub_args));
                if !fact.is_empty() {
                    let mut items = mem.items;
                    items.push(fact.to_string());
                    mem = Memory::new(items, true);
                }
            }
        }
        self.config.set_scoped_memory(scope, mem.clone());
        let _ = self
            .cmd_tx
            .send(EngineCommand::SetMemory { scope, memory: mem });
    }

    fn handle_slash_command(&mut self, raw_prompt: &str, rest: &str) -> bool {
        let (cmd_token, args) = match rest.find(char::is_whitespace) {
            Some(idx) => (&rest[..idx], rest[idx..].trim()),
            None => (rest, ""),
        };
        match SlashCommand::from_name(cmd_token) {
            Some(SlashCommand::Run) => {
                let (include_in_context, cmd_args) = if let Some(rest_args) = args
                    .strip_prefix("-c ")
                    .or_else(|| args.strip_prefix("--context "))
                {
                    (true, rest_args.trim())
                } else {
                    (false, args)
                };
                self.dispatch_shell_command(raw_prompt, cmd_args, include_in_context);
                false
            }
            Some(SlashCommand::Memory) => {
                self.handle_memory_slash_command(raw_prompt, args);
                false
            }
            Some(SlashCommand::Compact) => {
                self.record_prompt_and_clear(raw_prompt);
                self.scroll_to_bottom();
                let instructions = (!args.is_empty()).then(|| args.to_string());
                let _ = self
                    .cmd_tx
                    .send(EngineCommand::CompactChat { instructions });
                false
            }
            Some(SlashCommand::Rename) => {
                self.record_prompt_and_clear(raw_prompt);
                if args.is_empty() {
                    self.push_error_message("Usage: /rename <new_title>".into());
                } else {
                    let _ = self
                        .cmd_tx
                        .send(EngineCommand::RenameSession(args.to_string()));
                }
                false
            }
            Some(_) if !args.is_empty() => false,
            Some(SlashCommand::Exit) => true,
            Some(SlashCommand::Fork) => {
                self.record_prompt_and_clear(raw_prompt);
                if let Some(last_assistant_idx) =
                    self.messages.iter().rposition(ChatMessage::is_forkable)
                {
                    self.fork_from_message(last_assistant_idx);
                } else {
                    self.push_error_message("No assistant response available to fork from.".into());
                }
                false
            }
            Some(SlashCommand::Inspect) => {
                self.record_prompt_and_clear(raw_prompt);
                self.request_payload_inspector();
                false
            }
            Some(SlashCommand::Pin) => {
                self.record_prompt_and_clear(raw_prompt);
                let _ = self.cmd_tx.send(EngineCommand::TogglePinSession(None));
                false
            }
            Some(SlashCommand::Reset) => {
                self.record_prompt_and_clear(raw_prompt);
                self.scroll_to_bottom();
                let _ = self.cmd_tx.send(EngineCommand::ResetChat);
                false
            }
            None => false,
        }
    }

    pub(super) fn submit_prompt(&mut self) -> bool {
        let raw_prompt = self.text_area.lines().join("\n").trim().to_string();
        if raw_prompt.is_empty() {
            return false;
        }

        if let Some((include_in_context, tool_input)) = tools::parse_hash_tool_prefix(&raw_prompt) {
            self.dispatch_manual_tool(&raw_prompt, tool_input, include_in_context);
            return false;
        }

        if let Some(rest) = raw_prompt.strip_prefix('!') {
            let (include_in_context, shell_cmd) = match rest.strip_prefix('!') {
                Some(ctx_cmd) => (true, ctx_cmd.trim()),
                None => (false, rest.trim()),
            };
            self.dispatch_shell_command(&raw_prompt, shell_cmd, include_in_context);
            return false;
        }

        if let Some(rest) = raw_prompt.strip_prefix('/') {
            return self.handle_slash_command(&raw_prompt, rest);
        }

        let mut turn = self.current_turn_config();
        let prompt = if let Some((cmd_token, args_str)) = skills::parse_at_command(&raw_prompt) {
            self.config.skills = skills::discover_skills(
                &self.config.app.skills_dir,
                self.config.workspace_skills_dir.as_deref(),
            );
            match skills::check_skill_activation(cmd_token, self.enable_tools, &self.config) {
                Ok(skill_name) => {
                    turn.active_skill = Some(skill_name);
                    turn.skill_args = (!args_str.is_empty()).then(|| args_str.to_string());
                    if args_str.is_empty() {
                        raw_prompt.trim().to_string()
                    } else {
                        args_str.to_string()
                    }
                }
                Err(_) => {
                    turn.active_skill = Some(cmd_token.to_string());
                    let _ = self.cmd_tx.send(EngineCommand::SendPrompt {
                        prompt: raw_prompt,
                        turn,
                    });
                    return false;
                }
            }
        } else {
            raw_prompt.clone()
        };

        self.record_prompt_and_clear(&raw_prompt);
        self.scroll_to_bottom();
        let _ = self.cmd_tx.send(EngineCommand::SendPrompt { prompt, turn });
        false
    }
}
