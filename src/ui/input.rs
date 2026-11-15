use super::{ActiveModal, TuiApp};
use crate::core::{EngineCommand, config::PersonalIntelligence};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;
use std::path::PathBuf;

impl TuiApp {
    fn find_msg_at(&self, visual_y: usize) -> Option<(usize, usize, bool)> {
        self.msg_visual_ranges
            .iter()
            .find(|(_, start, end, _)| (*start..*end).contains(&visual_y))
            .map(|&(i, start, _, truncated)| (i, start, truncated))
    }

    fn find_sticky_header_msg(&self) -> Option<usize> {
        if self.chat_scroll == 0 {
            return None;
        }
        let first_is_body = self
            .msg_visual_ranges
            .iter()
            .find(|(_, start, end, _)| (*start..*end).contains(&self.chat_scroll))
            .is_some_and(|&(_, start, end, _)| {
                self.chat_scroll > start && self.chat_scroll + 1 < end
            });
        if !first_is_body {
            return None;
        }
        self.msg_visual_ranges
            .iter()
            .rev()
            .find(|(_, start, _, _)| *start < self.chat_scroll)
            .map(|&(i, _, _, _)| i)
    }

    fn scroll_chat(&mut self, delta: isize) {
        if delta < 0 {
            self.follow_tail = false;
            self.chat_scroll = self.chat_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.chat_scroll = self.chat_scroll.saturating_add(delta as usize);
            if self.chat_scroll >= self.max_chat_scroll() {
                self.follow_tail = true;
            }
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);

        if self.modal == ActiveModal::ToolApproval {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.tool_scroll = self.tool_scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => self.tool_scroll = self.tool_scroll.saturating_add(3),
                _ => {}
            }
            return;
        }

        let pos = Position::new(x, y);
        if !self.chat_area.contains(pos) {
            return;
        }

        let is_on_scrollbar = x >= self.chat_area.x + self.chat_area.width.saturating_sub(2);
        let inner = self.chat_inner_area();
        let inner_y = usize::from(y.saturating_sub(inner.y));
        let visual_y = inner_y + self.chat_scroll;

        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let is_up = mouse.kind == MouseEventKind::ScrollUp;
                if !is_on_scrollbar
                    && let Some((i, _, true)) = self.find_msg_at(visual_y)
                    && let Some(state) = self.msg_states.get_mut(i)
                {
                    state.scroll = if is_up {
                        state.scroll.saturating_sub(3)
                    } else {
                        state.scroll.saturating_add(3)
                    };
                    return;
                }
                self.scroll_chat(if is_up { -3 } else { 3 });
            }
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
                if is_on_scrollbar =>
            {
                let view_height = self.chat_view_height();
                let max_scroll = self.max_chat_scroll();
                if max_scroll > 0 && view_height > 1 {
                    let track_height = view_height.saturating_sub(1);
                    let fraction = (inner_y.min(track_height) as f64) / (track_height as f64);
                    self.chat_scroll = (fraction * (max_scroll as f64)).round() as usize;
                    self.follow_tail = self.chat_scroll >= max_scroll;
                }
            }
            MouseEventKind::Down(MouseButton::Left) if inner.contains(pos) => {
                if inner_y == 0
                    && let Some(sticky_idx) = self.find_sticky_header_msg()
                    && let Some(state) = self.msg_states.get_mut(sticky_idx)
                {
                    state.collapsed = !state.collapsed;
                } else if let Some((i, start_y, _)) = self.find_msg_at(visual_y)
                    && let Some(state) = self.msg_states.get_mut(i)
                {
                    if visual_y == start_y {
                        state.collapsed = !state.collapsed;
                    } else if !state.collapsed {
                        state.unfolded = !state.unfolded;
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if ctrl && key.code == KeyCode::Char('q') {
            return true;
        }

        match self.modal {
            ActiveModal::AddFile => self.handle_add_file_key(key.code),
            ActiveModal::Sessions => self.handle_sessions_key(key.code),
            ActiveModal::ToolApproval => self.handle_tool_approval_key(key.code, ctrl, shift),
            ActiveModal::ModelSelect => self.handle_model_select_key(key.code),
            ActiveModal::SystemPrompt => self.handle_system_prompt_key(key, ctrl, shift),
            ActiveModal::PersonalIntelligence => {
                self.handle_personal_intelligence_key(key, ctrl, shift)
            }
            ActiveModal::None => return self.handle_main_key(key, ctrl),
        }
        false
    }

    fn handle_add_file_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.modal = ActiveModal::None,
            KeyCode::Enter => {
                let cleaned = self.file_path_input.trim().trim_matches(['\'', '"']);
                if !cleaned.is_empty() {
                    let _ = self
                        .cmd_tx
                        .send(EngineCommand::ImportFile(PathBuf::from(cleaned)));
                }
                self.file_path_input.clear();
                self.modal = ActiveModal::None;
            }
            KeyCode::Backspace => {
                self.file_path_input.pop();
            }
            KeyCode::Char(c) => self.file_path_input.push(c),
            _ => {}
        }
    }

    fn handle_sessions_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.modal = ActiveModal::None,
            KeyCode::Up => self.session_idx = self.session_idx.saturating_sub(1),
            KeyCode::Down if self.session_idx + 1 < self.sessions.len() => self.session_idx += 1,
            KeyCode::Enter => {
                if let Some(s) = self.sessions.get(self.session_idx) {
                    let _ = self.cmd_tx.send(EngineCommand::LoadSession(s.id.clone()));
                    self.modal = ActiveModal::None;
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(s) = self.sessions.get(self.session_idx) {
                    let _ = self.cmd_tx.send(EngineCommand::DeleteSession(s.id.clone()));
                }
            }
            _ => {}
        }
    }

    fn handle_tool_approval_key(&mut self, code: KeyCode, ctrl: bool, shift: bool) {
        match code {
            KeyCode::Enter => self.resolve_modal_tool(true, false),
            KeyCode::Char('a') if ctrl => self.resolve_modal_tool(true, true),
            KeyCode::Esc if shift => {
                self.pending_tool = None;
                self.tool_feedback.clear();
                self.modal = ActiveModal::None;
                self.follow_tail = true;
                let _ = self.cmd_tx.send(EngineCommand::CancelStream);
            }
            KeyCode::Esc => self.resolve_modal_tool(false, false),
            KeyCode::Up => self.tool_scroll = self.tool_scroll.saturating_sub(1),
            KeyCode::Down => self.tool_scroll = self.tool_scroll.saturating_add(1),
            KeyCode::PageUp => self.tool_scroll = self.tool_scroll.saturating_sub(10),
            KeyCode::PageDown => self.tool_scroll = self.tool_scroll.saturating_add(10),
            KeyCode::Backspace => {
                self.tool_feedback.pop();
            }
            KeyCode::Char(c) if !ctrl => self.tool_feedback.push(c),
            _ => {}
        }
    }

    fn handle_model_select_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.modal = ActiveModal::None,
            KeyCode::Up => self.model_idx = self.model_idx.saturating_sub(1),
            KeyCode::Down if self.model_idx + 1 < self.config.models.len() => {
                self.model_idx += 1;
            }
            KeyCode::Enter => {
                if let Some(preset) = self.config.models.get(self.model_idx) {
                    let _ = self.cmd_tx.send(EngineCommand::SwitchModel {
                        model: preset.name.clone(),
                        num_ctx: preset.context,
                        max_tool_output_size: preset.max_tool_output_size,
                    });
                }
                self.modal = ActiveModal::None;
            }
            _ => {}
        }
    }

    fn handle_system_prompt_key(&mut self, key: KeyEvent, ctrl: bool, shift: bool) {
        match key.code {
            KeyCode::Esc => self.modal = ActiveModal::None,
            KeyCode::Char('p') if ctrl && !self.config.system_presets.is_empty() => {
                self.sys_preset_idx = (self.sys_preset_idx + 1) % self.config.system_presets.len();
                let preset_prompt = self.config.system_presets[self.sys_preset_idx]
                    .prompt
                    .clone();
                self.sys_prompt_area = super::create_sys_prompt_area(&preset_prompt);
            }
            KeyCode::Char('x') if ctrl => {
                self.sys_preset_idx = 0;
                self.sys_prompt_area = super::create_sys_prompt_area("");
            }
            KeyCode::Enter if shift => {
                self.sys_prompt_area.insert_newline();
            }
            KeyCode::Enter => {
                let edited = self.sys_prompt_area.lines().join("\n").trim().to_string();
                self.sync_system_prompt(&edited);
                self.modal = ActiveModal::None;
                let _ = self
                    .cmd_tx
                    .send(EngineCommand::SetTurnConfig(self.current_turn_config()));
            }
            _ => {
                self.sys_prompt_area.input(key);
            }
        }
    }

    fn handle_personal_intelligence_key(&mut self, key: KeyEvent, ctrl: bool, shift: bool) {
        match key.code {
            KeyCode::Esc => self.modal = ActiveModal::None,
            KeyCode::Char('e') if ctrl => {
                self.pi_enabled_draft = !self.pi_enabled_draft;
            }
            KeyCode::Char('x') if ctrl => {
                self.pi_area = super::create_pi_area("");
            }
            KeyCode::Enter if shift => {
                self.pi_area.insert_newline();
            }
            KeyCode::Enter => {
                let raw_text = self.pi_area.lines().join("\n");
                let pi = PersonalIntelligence::from_editor_text(&raw_text, self.pi_enabled_draft);
                self.config.personal_intelligence = pi.clone();
                self.modal = ActiveModal::None;
                let _ = self.cmd_tx.send(EngineCommand::SetPersonalIntelligence(pi));
            }
            _ => {
                self.pi_area.input(key);
            }
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent, ctrl: bool) -> bool {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page_step = self.chat_view_height().saturating_sub(2).max(3) as isize;

        match key.code {
            KeyCode::Esc
                if self.is_streaming
                    || self.active_tool_status.is_some()
                    || !self.queued_messages.is_empty() =>
            {
                let _ = self.cmd_tx.send(EngineCommand::CancelStream);
            }
            KeyCode::PageUp => self.scroll_chat(-page_step),
            KeyCode::PageDown => self.scroll_chat(page_step),
            KeyCode::Up if alt => self.scroll_chat(-3),
            KeyCode::Down if alt => self.scroll_chat(3),
            KeyCode::Up if !ctrl && !shift => {
                if self.is_prompt_empty() {
                    if let Some(idx) = self.prompt_history.len().checked_sub(1) {
                        self.prompt_history_idx = Some(idx);
                        self.text_area = super::create_text_area_with(&self.prompt_history[idx]);
                    }
                } else if let Some(idx) = self.prompt_history_idx {
                    let prev_idx = idx.saturating_sub(1);
                    self.prompt_history_idx = Some(prev_idx);
                    self.text_area = super::create_text_area_with(&self.prompt_history[prev_idx]);
                } else {
                    self.text_area.input(key);
                }
            }
            KeyCode::Down if !ctrl && !shift => {
                if !self.is_prompt_empty()
                    && let Some(idx) = self.prompt_history_idx
                {
                    if idx + 1 < self.prompt_history.len() {
                        let next_idx = idx + 1;
                        self.prompt_history_idx = Some(next_idx);
                        self.text_area =
                            super::create_text_area_with(&self.prompt_history[next_idx]);
                    } else {
                        self.prompt_history_idx = None;
                        self.text_area = super::create_text_area();
                    }
                } else {
                    self.text_area.input(key);
                }
            }
            KeyCode::Home if ctrl => self.scroll_chat(isize::MIN),
            KeyCode::End if ctrl => self.scroll_to_bottom(),
            KeyCode::Enter if shift => {
                self.prompt_history_idx = None;
                self.text_area.insert_newline();
            }
            KeyCode::Enter => return self.submit_prompt(),
            KeyCode::Char('c') if ctrl => {
                self.text_area = super::create_text_area();
            }
            KeyCode::Char('v') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::PasteClipboard);
            }
            KeyCode::Char('f') if ctrl => {
                self.file_path_input.clear();
                self.modal = ActiveModal::AddFile;
            }
            KeyCode::Char('x') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::ClearImages);
            }
            KeyCode::Char('s') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::RefreshSessions);
                self.modal = ActiveModal::Sessions;
            }
            KeyCode::Char('m') if ctrl => {
                if self.config.models.is_empty() {
                    self.push_error_message(
                        "No model presets configured in config.toml (`[[models]]`).".into(),
                    );
                } else {
                    if let Some(idx) = self.config.models.iter().position(|m| m.name == self.model)
                    {
                        self.model_idx = idx;
                    }
                    self.modal = ActiveModal::ModelSelect;
                }
            }
            KeyCode::Char('r') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::ResetChat);
            }
            KeyCode::Char('t') if ctrl => self.enable_tools = !self.enable_tools,
            KeyCode::Char('b') if ctrl => self.sidebar_expanded = !self.sidebar_expanded,
            KeyCode::Char('y') if ctrl => {
                let sys_text = self.current_system_prompt_text();
                self.sys_prompt_area = super::create_sys_prompt_area(&sys_text);
                self.modal = ActiveModal::SystemPrompt;
            }
            KeyCode::Char('p') if ctrl => {
                self.open_personal_intelligence_modal();
            }
            _ => {
                self.prompt_history_idx = None;
                self.text_area.input(key);
            }
        }
        false
    }
}
