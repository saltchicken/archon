use super::{ActiveModal, TuiApp, create_text_area};
use crate::core::EngineCommand;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;
use std::path::PathBuf;

impl TuiApp {
    fn find_msg_at(&self, visual_y: usize) -> Option<(usize, bool)> {
        self.msg_visual_ranges
            .iter()
            .find(|(_, start, end, _)| (*start..*end).contains(&visual_y))
            .map(|&(i, _, _, truncated)| (i, truncated))
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

        if !self.chat_area.contains(Position::new(x, y)) {
            return;
        }

        let is_on_scrollbar = x >= self.chat_area.x + self.chat_area.width.saturating_sub(2);
        let inner_y = usize::from(y.saturating_sub(self.chat_area.y + 1));
        let visual_y = inner_y + self.chat_scroll;

        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let is_up = mouse.kind == MouseEventKind::ScrollUp;
                if !is_on_scrollbar
                    && let Some((i, true)) = self.find_msg_at(visual_y)
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
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((i, _)) = self.find_msg_at(visual_y)
                    && let Some(state) = self.msg_states.get_mut(i)
                {
                    state.unfolded = !state.unfolded;
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('q') {
            return true;
        }

        match self.modal {
            ActiveModal::AddFile => self.handle_add_file_key(key.code),
            ActiveModal::Sessions => self.handle_sessions_key(key.code),
            ActiveModal::ToolApproval => self.handle_tool_approval_key(key.code, ctrl),
            ActiveModal::ModelSelect => self.handle_model_select_key(key.code),
            ActiveModal::ContextWindow => self.handle_context_window_key(key.code),
            ActiveModal::None => self.handle_main_key(key, ctrl),
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
            KeyCode::Char('?') => self.show_help_bar = !self.show_help_bar,
            KeyCode::Up => self.session_idx = self.session_idx.saturating_sub(1),
            KeyCode::Down if self.session_idx + 1 < self.sessions.len() => self.session_idx += 1,
            KeyCode::Enter => {
                if let Some(s) = self.sessions.get(self.session_idx) {
                    let _ = self
                        .cmd_tx
                        .send(EngineCommand::LoadSession(s.id.clone()));
                    self.modal = ActiveModal::None;
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(s) = self.sessions.get(self.session_idx) {
                    let _ = self
                        .cmd_tx
                        .send(EngineCommand::DeleteSession(s.id.clone()));
                }
            }
            _ => {}
        }
    }

    fn handle_tool_approval_key(&mut self, code: KeyCode, ctrl: bool) {
        match code {
            KeyCode::Enter => self.resolve_modal_tool(true, false),
            KeyCode::Char('a') if ctrl => self.resolve_modal_tool(true, true),
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

    fn handle_context_window_key(&mut self, code: KeyCode) {
        let presets = super::modals::CTX_PRESETS;
        match code {
            KeyCode::Esc => self.modal = ActiveModal::None,
            KeyCode::Up => {
                self.ctx_input.clear();
                self.ctx_preset_idx = self.ctx_preset_idx.saturating_sub(1);
            }
            KeyCode::Down if self.ctx_preset_idx + 1 < presets.len() => {
                self.ctx_input.clear();
                self.ctx_preset_idx += 1;
            }
            KeyCode::Backspace => {
                self.ctx_input.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                self.ctx_input.push(c);
            }
            KeyCode::Enter => {
                let target_ctx = if !self.ctx_input.is_empty() {
                    self.ctx_input.parse::<u32>().ok()
                } else {
                    presets.get(self.ctx_preset_idx).map(|(v, _)| *v)
                };
                if let Some(ctx) = target_ctx {
                    let _ = self.cmd_tx.send(EngineCommand::SetContextWindow(ctx));
                }
                self.modal = ActiveModal::None;
            }
            _ => {}
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent, ctrl: bool) {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page_step = self.chat_area.height.saturating_sub(4).max(3) as isize;

        match key.code {
            KeyCode::Esc if self.is_streaming => {
                let _ = self.cmd_tx.send(EngineCommand::CancelStream);
            }
            KeyCode::PageUp => self.scroll_chat(-page_step),
            KeyCode::PageDown => self.scroll_chat(page_step),
            KeyCode::Up if alt => self.scroll_chat(-3),
            KeyCode::Down if alt => self.scroll_chat(3),
            KeyCode::Home if ctrl => self.scroll_chat(isize::MIN),
            KeyCode::End if ctrl => self.scroll_to_bottom(),
            KeyCode::Char('?') if !ctrl && self.text_area.lines().iter().all(|l| l.is_empty()) => {
                self.show_help_bar = !self.show_help_bar;
            }
            KeyCode::Enter if shift => {
                self.text_area.insert_newline();
            }
            KeyCode::Enter => self.submit_prompt(),
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
            KeyCode::Char('m') if ctrl && !self.config.models.is_empty() => {
                if let Some(idx) = self.config.models.iter().position(|m| m.name == self.model) {
                    self.model_idx = idx;
                }
                self.modal = ActiveModal::ModelSelect;
            }
            KeyCode::Char('w') if ctrl => {
                self.ctx_input.clear();
                self.ctx_preset_idx = super::modals::CTX_PRESETS
                    .iter()
                    .position(|(v, _)| *v == self.num_ctx)
                    .unwrap_or(3);
                self.modal = ActiveModal::ContextWindow;
            }
            KeyCode::Char('r') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::ResetChat);
            }
            KeyCode::Char('t') if ctrl => self.enable_tools = !self.enable_tools,
            KeyCode::Char('b') if ctrl => self.sidebar_expanded = !self.sidebar_expanded,
            KeyCode::Char('p') if ctrl && !self.config.presets.is_empty() => {
                self.preset_idx = (self.preset_idx + 1) % self.config.presets.len();
                let prompt = &self.config.presets[self.preset_idx].prompt;
                if !prompt.is_empty() {
                    self.text_area = create_text_area();
                    self.text_area.insert_str(prompt);
                }
            }
            KeyCode::Char('y') if ctrl && !self.config.system_presets.is_empty() => {
                self.sys_preset_idx = (self.sys_preset_idx + 1) % self.config.system_presets.len();
                self.custom_system_prompt = None;
            }
            _ => {
                self.text_area.input(key);
            }
        }
    }
}
