use super::{create_text_area, ActiveModal, TuiApp};
use crate::core::EngineCommand;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;
use std::path::PathBuf;

impl TuiApp {
    fn find_msg_at(&self, visual_y: usize) -> Option<(usize, bool)> {
        self.msg_visual_ranges
            .iter()
            .find(|(_, start, end, _)| visual_y >= *start as usize && visual_y < *end as usize)
            .map(|&(i, _, _, truncated)| (i, truncated))
    }

    fn scroll_chat_up(&mut self, lines: u16) {
        self.follow_tail = false;
        self.chat_scroll = self.chat_scroll.saturating_sub(lines);
    }

    fn scroll_chat_down(&mut self, lines: u16) {
        self.chat_scroll = self.chat_scroll.saturating_add(lines);
        if self.chat_scroll >= self.max_chat_scroll() {
            self.follow_tail = true;
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
        let inner_y = y.saturating_sub(self.chat_area.y + 1);
        let visual_y = inner_y as usize + self.chat_scroll as usize;

        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let is_up = mouse.kind == MouseEventKind::ScrollUp;
                if !is_on_scrollbar {
                    if let Some((i, true)) = self.find_msg_at(visual_y) {
                        if let Some(state) = self.msg_states.get_mut(i) {
                            state.scroll = if is_up {
                                state.scroll.saturating_sub(3)
                            } else {
                                state.scroll.saturating_add(3)
                            };
                            return;
                        }
                    }
                }
                if is_up {
                    self.scroll_chat_up(3);
                } else {
                    self.scroll_chat_down(3);
                }
            }
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
                if is_on_scrollbar =>
            {
                let view_height = self.chat_view_height();
                let max_scroll = self.max_chat_scroll();
                if max_scroll > 0 && view_height > 1 {
                    let track_height = view_height.saturating_sub(1);
                    let fraction = inner_y.min(track_height) as f32 / track_height as f32;
                    self.chat_scroll = (fraction * max_scroll as f32).round() as u16;
                    self.follow_tail = self.chat_scroll >= max_scroll;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((i, _)) = self.find_msg_at(visual_y) {
                    if let Some(state) = self.msg_states.get_mut(i) {
                        state.unfolded = !state.unfolded;
                    }
                }
            }
            _ => {}
        }
    }

    fn is_prompt_empty(&self) -> bool {
        self.text_area.lines().iter().all(|l| l.is_empty())
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('q') {
            return true;
        }

        match self.modal {
            ActiveModal::AddFile => self.handle_add_file_key(key.code),
            ActiveModal::Sessions => self.handle_sessions_key(key.code),
            ActiveModal::ToolApproval => self.handle_tool_approval_key(key.code),
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
            KeyCode::Down if self.session_idx + 1 < self.sessions.len() => {
                self.session_idx += 1;
            }
            KeyCode::Enter => {
                if let Some(s) = self.sessions.get(self.session_idx) {
                    let _ = self
                        .cmd_tx
                        .send(EngineCommand::LoadSession(s.file_path.clone()));
                    self.modal = ActiveModal::None;
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(s) = self.sessions.get(self.session_idx) {
                    let _ = self
                        .cmd_tx
                        .send(EngineCommand::DeleteSession(s.file_path.clone()));
                }
            }
            _ => {}
        }
    }

    fn handle_tool_approval_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => self.resolve_modal_tool(true),
            KeyCode::Esc => self.resolve_modal_tool(false),
            KeyCode::Up => self.tool_scroll = self.tool_scroll.saturating_sub(1),
            KeyCode::Down => self.tool_scroll = self.tool_scroll.saturating_add(1),
            KeyCode::PageUp => self.tool_scroll = self.tool_scroll.saturating_sub(10),
            KeyCode::PageDown => self.tool_scroll = self.tool_scroll.saturating_add(10),
            KeyCode::Backspace => {
                self.tool_feedback.pop();
            }
            KeyCode::Char(c) => self.tool_feedback.push(c),
            _ => {}
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent, ctrl: bool) {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let page_step = self.chat_area.height.saturating_sub(4).max(3);

        match key.code {
            KeyCode::Esc if self.is_streaming => {
                let _ = self.cmd_tx.send(EngineCommand::CancelStream);
            }
            KeyCode::PageUp => self.scroll_chat_up(page_step),
            KeyCode::PageDown => self.scroll_chat_down(page_step),
            KeyCode::Up if alt => self.scroll_chat_up(3),
            KeyCode::Down if alt => self.scroll_chat_down(3),
            KeyCode::Home if ctrl => self.scroll_chat_up(u16::MAX),
            KeyCode::End if ctrl => self.scroll_to_bottom(),
            KeyCode::Char('?') if !ctrl && self.is_prompt_empty() => {
                self.show_help_bar = !self.show_help_bar;
            }
            KeyCode::Enter if ctrl => self.submit_prompt(),
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
            KeyCode::Char('r') if ctrl => {
                let _ = self.cmd_tx.send(EngineCommand::ResetChat);
            }
            KeyCode::Char('t') if ctrl => self.enable_tools = !self.enable_tools,
            KeyCode::Char('k') if ctrl => {
                self.show_thinking = !self.show_thinking;
                self.last_wrap_width = 0; // force re-wrap on next tick
            }
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
