use super::{
    ActiveModal, ContextMenu, ContextMenuAction, TuiApp, render::format_assistant_display,
};
use crate::core::{
    EngineCommand, TruncatedToolAction, attachments::copy_text_to_clipboard, session::ChatMessage,
};
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

    fn message_text_for(m: &ChatMessage) -> String {
        if !m.content.trim().is_empty() {
            return m.content.clone();
        }
        if m.role == "assistant" {
            return format_assistant_display(m);
        }
        m.content.clone()
    }

    fn message_block_text(&self, idx: usize) -> Option<String> {
        if idx < self.messages.len() {
            Some(Self::message_text_for(&self.messages[idx]))
        } else if idx == self.messages.len() {
            if self.is_streaming {
                if !self.streaming_text.is_empty() {
                    Some(self.streaming_text.clone())
                } else if !self.streaming_thought.is_empty() {
                    Some(self.streaming_thought.clone())
                } else {
                    None
                }
            } else {
                self.active_tool_status.clone()
            }
        } else {
            self.queued_messages
                .get(idx.saturating_sub(self.messages.len() + 1))
                .map(|m| m.content.clone())
        }
    }

    fn chat_msg_idx_at(&self, pos: Position) -> Option<usize> {
        let inner = self.chat_inner_area();
        let inner_y = usize::from(pos.y.saturating_sub(inner.y));
        let visual_y = inner_y + self.chat_scroll;

        if inner.contains(pos) && inner_y == 0 {
            self.find_sticky_header_msg()
                .or_else(|| self.find_msg_at(visual_y).map(|(i, _, _)| i))
        } else {
            self.find_msg_at(visual_y).map(|(i, _, _)| i)
        }
    }

    fn context_target_text(&self, x: u16, y: u16) -> String {
        match self.modal {
            ActiveModal::ToolApproval => {
                return self
                    .pending_tool
                    .as_ref()
                    .map(|(p, _, _)| p.clone())
                    .unwrap_or_default();
            }
            ActiveModal::TruncatedTool => {
                return self
                    .pending_truncated
                    .as_ref()
                    .map(|p| p.preview.clone())
                    .unwrap_or_default();
            }
            ActiveModal::SystemPrompt => return self.sys_prompt_area.lines().join("\n"),
            ActiveModal::Memory => return self.memory_area.lines().join("\n"),
            ActiveModal::AddFile => return self.file_path_input.clone(),
            ActiveModal::Sessions => {
                return self
                    .filtered_sessions()
                    .get(self.session_idx)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
            }
            ActiveModal::ModelSelect => {
                return self
                    .config
                    .models
                    .get(self.model_idx)
                    .map(|m| m.name.clone())
                    .unwrap_or_default();
            }
            ActiveModal::None => {}
        }

        let pos = Position::new(x, y);
        if self.prompt_area.contains(pos) {
            let prompt_text = self.text_area.lines().join("\n");
            if !prompt_text.is_empty() {
                return prompt_text;
            }
        }

        if self.chat_area.contains(pos) {
            let inner = self.chat_inner_area();
            let inner_x = usize::from(x.saturating_sub(inner.x));
            let inner_y = usize::from(y.saturating_sub(inner.y));
            let visual_y = inner_y + self.chat_scroll;

            if inner.contains(pos)
                && !(inner_y == 0 && self.find_sticky_header_msg().is_some())
                && let Some(btn) = self
                    .code_copy_buttons
                    .iter()
                    .find(|b| b.visual_y == visual_y && b.x_range.contains(&inner_x))
            {
                return btn.code.clone();
            }

            if let Some(idx) = self.chat_msg_idx_at(pos)
                && let Some(text) = self.message_block_text(idx)
            {
                return text;
            }
        }

        if self.is_streaming {
            if !self.streaming_text.is_empty() {
                return self.streaming_text.clone();
            }
            if !self.streaming_thought.is_empty() {
                return self.streaming_thought.clone();
            }
        }
        if let Some(last_msg) = self.messages.last() {
            return Self::message_text_for(last_msg);
        }
        self.text_area.lines().join("\n")
    }

    fn context_target_fork_idx(&self, x: u16, y: u16) -> Option<usize> {
        let pos = Position::new(x, y);
        if self.modal != ActiveModal::None || !self.chat_area.contains(pos) {
            return None;
        }
        let target_idx = self.chat_msg_idx_at(pos)?;
        self.messages
            .get(target_idx)
            .is_some_and(ChatMessage::is_forkable)
            .then_some(target_idx)
    }

    pub(super) fn header_msg_at(&self, pos: Position) -> Option<usize> {
        if self.modal != ActiveModal::None || self.context_menu.is_some() {
            return None;
        }
        let inner = self.chat_inner_area();
        if !inner.contains(pos) {
            return None;
        }
        let inner_y = usize::from(pos.y.saturating_sub(inner.y));
        let visual_y = inner_y + self.chat_scroll;

        let target_idx = if inner_y == 0
            && let Some(sticky_idx) = self.find_sticky_header_msg()
        {
            sticky_idx
        } else if let Some((i, start_y, _)) = self.find_msg_at(visual_y)
            && visual_y == start_y
        {
            i
        } else {
            return None;
        };

        self.messages
            .get(target_idx)
            .is_some_and(ChatMessage::is_forkable)
            .then_some(target_idx)
    }

    fn execute_context_menu_action(&mut self, action: ContextMenuAction, target_text: &str) {
        match action {
            ContextMenuAction::CopyToClipboard => {
                if let Err(e) = copy_text_to_clipboard(target_text) {
                    self.push_error_message(format!("Failed to copy to clipboard: {e}"));
                }
            }
            ContextMenuAction::ForkConversation(msg_idx) => {
                self.fork_from_message(msg_idx);
            }
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);
        self.last_mouse_pos = Some(Position::new(x, y));

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) {
            let target_text = self.context_target_text(x, y);
            let mut actions = vec![ContextMenuAction::CopyToClipboard];
            if let Some(fork_idx) = self.context_target_fork_idx(x, y) {
                actions.push(ContextMenuAction::ForkConversation(fork_idx));
            }
            self.context_menu = Some(ContextMenu {
                x,
                y,
                selected: 0,
                actions,
                target_text,
            });
            return;
        }

        if self.context_menu.is_some() {
            let screen = self.term_area;
            match mouse.kind {
                MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                    if let Some(menu) = self.context_menu.as_mut()
                        && let Some(idx) = menu.item_at(screen, x, y)
                    {
                        menu.selected = idx;
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(menu) = self.context_menu.take()
                        && let Some(idx) = menu.item_at(screen, x, y)
                        && let Some(&action) = menu.actions.get(idx)
                    {
                        self.execute_context_menu_action(action, &menu.target_text);
                    }
                }
                MouseEventKind::ScrollUp => {
                    if let Some(menu) = self.context_menu.as_mut() {
                        menu.selected = menu.selected.saturating_sub(1);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(menu) = self.context_menu.as_mut()
                        && menu.selected + 1 < menu.actions.len()
                    {
                        menu.selected += 1;
                    }
                }
                _ => {}
            }
            return;
        }

        match self.modal {
            ActiveModal::Sessions => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.session_idx = self.session_idx.saturating_sub(1);
                    }
                    MouseEventKind::ScrollDown
                        if self.session_idx + 1 < self.filtered_sessions().len() =>
                    {
                        self.session_idx += 1;
                    }
                    _ => {}
                }
                return;
            }
            ActiveModal::ToolApproval => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.tool_scroll = self.tool_scroll.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown => {
                        self.tool_scroll = self.tool_scroll.saturating_add(3);
                    }
                    _ => {}
                }
                return;
            }
            ActiveModal::TruncatedTool => {
                if let Some(ref mut pending) = self.pending_truncated {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            pending.scroll = pending.scroll.saturating_sub(3);
                        }
                        MouseEventKind::ScrollDown => {
                            pending.scroll = pending.scroll.saturating_add(3);
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            if pending.collapsed {
                                pending.collapsed = false;
                            } else {
                                pending.unfolded = !pending.unfolded;
                            }
                        }
                        _ => {}
                    }
                }
                return;
            }
            _ => {}
        }

        let pos = Position::new(x, y);
        if self.sidebar_expanded && self.sidebar_area.contains(pos) {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.sidebar_scroll = self.sidebar_scroll.saturating_sub(1);
                }
                MouseEventKind::ScrollDown => {
                    self.sidebar_scroll = self.sidebar_scroll.saturating_add(1);
                }
                _ => {}
            }
            return;
        }
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
                let inner_x = usize::from(x.saturating_sub(inner.x));
                if inner_y == 0
                    && let Some(sticky_idx) = self.find_sticky_header_msg()
                {
                    if self
                        .fork_buttons
                        .iter()
                        .any(|b| b.msg_idx == sticky_idx && b.x_range.contains(&inner_x))
                    {
                        self.fork_from_message(sticky_idx);
                    } else if let Some(state) = self.msg_states.get_mut(sticky_idx) {
                        state.collapsed = !state.collapsed;
                    }
                } else if let Some(btn) = self
                    .fork_buttons
                    .iter()
                    .find(|b| b.visual_y == visual_y && b.x_range.contains(&inner_x))
                {
                    let target_idx = btn.msg_idx;
                    self.fork_from_message(target_idx);
                } else if let Some(btn) = self
                    .code_copy_buttons
                    .iter()
                    .find(|b| b.visual_y == visual_y && b.x_range.contains(&inner_x))
                {
                    let code = btn.code.clone();
                    let target = (btn.msg_idx, btn.content_idx);
                    match copy_text_to_clipboard(&code) {
                        Ok(()) => {
                            self.copied_code_flash = Some((
                                target,
                                std::time::Instant::now() + std::time::Duration::from_millis(1500),
                            ));
                        }
                        Err(e) => {
                            self.push_error_message(format!("Failed to copy to clipboard: {e}"));
                        }
                    }
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

        if self.context_menu.is_some() {
            self.handle_context_menu_key(key.code);
            return false;
        }

        match self.modal {
            ActiveModal::AddFile => self.handle_add_file_key(key.code),
            ActiveModal::Sessions => self.handle_sessions_key(key.code, ctrl),
            ActiveModal::ToolApproval => self.handle_tool_approval_key(key.code, ctrl, shift),
            ActiveModal::TruncatedTool => self.handle_truncated_tool_key(key.code, ctrl, shift),
            ActiveModal::ModelSelect => self.handle_model_select_key(key.code),
            ActiveModal::SystemPrompt => self.handle_system_prompt_key(key, ctrl, shift),
            ActiveModal::Memory => self.handle_memory_key(key, ctrl, shift),
            ActiveModal::None => return self.handle_main_key(key, ctrl),
        }
        false
    }

    fn handle_context_menu_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.context_menu = None,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(menu) = self.context_menu.as_mut() {
                    menu.selected = menu.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(menu) = self.context_menu.as_mut()
                    && menu.selected + 1 < menu.actions.len()
                {
                    menu.selected += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('c') => {
                if let Some(menu) = self.context_menu.take()
                    && let Some(&action) = menu.actions.get(menu.selected)
                {
                    self.execute_context_menu_action(action, &menu.target_text);
                }
            }
            _ => {}
        }
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

    fn selected_session_id(&self) -> Option<String> {
        self.filtered_sessions()
            .get(self.session_idx)
            .map(|s| s.id.clone())
    }

    fn handle_sessions_key(&mut self, code: KeyCode, ctrl: bool) {
        match code {
            KeyCode::Esc => self.modal = ActiveModal::None,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.toggle_session_view();
            }
            KeyCode::Char('s') | KeyCode::Char('t') if ctrl => {
                self.toggle_session_view();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.session_idx = self.session_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.session_idx + 1 < self.filtered_sessions().len() {
                    self.session_idx += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(id) = self.selected_session_id() {
                    self.fork_flash = None;
                    let _ = self.cmd_tx.send(EngineCommand::LoadSession(id));
                    self.modal = ActiveModal::None;
                }
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                if let Some(id) = self.selected_session_id() {
                    let _ = self.cmd_tx.send(EngineCommand::TogglePinSession(Some(id)));
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Delete => {
                if let Some(id) = self.selected_session_id() {
                    let _ = self.cmd_tx.send(EngineCommand::DeleteSession(id));
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

    fn handle_truncated_tool_key(&mut self, code: KeyCode, ctrl: bool, shift: bool) {
        match code {
            KeyCode::Enter => {
                self.resolve_truncated_modal(TruncatedToolAction::ProceedTruncated);
            }
            KeyCode::Char('f') if ctrl => {
                self.resolve_truncated_modal(TruncatedToolAction::ProceedFull);
            }
            KeyCode::Char('c') if ctrl => {
                self.resolve_truncated_modal(TruncatedToolAction::CondenseTruncated);
            }
            KeyCode::Char('g') if ctrl => {
                self.resolve_truncated_modal(TruncatedToolAction::CondenseFull);
            }
            KeyCode::Char('d') if ctrl => {
                self.resolve_truncated_modal(TruncatedToolAction::Discard);
            }
            KeyCode::Char('x') if ctrl => {
                self.truncated_guide_prompt.clear();
            }
            KeyCode::Tab => {
                if let Some(ref mut pending) = self.pending_truncated {
                    if pending.collapsed {
                        pending.collapsed = false;
                    } else if !pending.unfolded {
                        pending.unfolded = true;
                    } else {
                        pending.collapsed = true;
                        pending.unfolded = false;
                        pending.scroll = 0;
                    }
                }
            }
            KeyCode::Esc if shift => {
                self.pending_truncated = None;
                self.truncated_guide_prompt.clear();
                self.modal = ActiveModal::None;
                self.follow_tail = true;
                let _ = self.cmd_tx.send(EngineCommand::CancelStream);
            }
            KeyCode::Esc => {
                self.resolve_truncated_modal(TruncatedToolAction::Discard);
            }
            KeyCode::Up => {
                if let Some(ref mut pending) = self.pending_truncated {
                    pending.scroll = pending.scroll.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(ref mut pending) = self.pending_truncated {
                    pending.scroll = pending.scroll.saturating_add(1);
                }
            }
            KeyCode::PageUp => {
                if let Some(ref mut pending) = self.pending_truncated {
                    pending.scroll = pending.scroll.saturating_sub(10);
                }
            }
            KeyCode::PageDown => {
                if let Some(ref mut pending) = self.pending_truncated {
                    pending.scroll = pending.scroll.saturating_add(10);
                }
            }
            KeyCode::Backspace => {
                self.truncated_guide_prompt.pop();
            }
            KeyCode::Char(c) if !ctrl => {
                self.truncated_guide_prompt.push(c);
            }
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

    fn handle_memory_key(&mut self, key: KeyEvent, ctrl: bool, shift: bool) {
        match key.code {
            KeyCode::Esc => self.modal = ActiveModal::None,
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_memory_scope();
            }
            KeyCode::Char('s') | KeyCode::Char('t') if ctrl => {
                self.toggle_memory_scope();
            }
            KeyCode::Char('e') if ctrl => {
                self.memory_enabled_draft = !self.memory_enabled_draft;
            }
            KeyCode::Char('x') if ctrl => {
                self.memory_area = super::create_memory_area(self.memory_scope, "");
            }
            KeyCode::Enter if shift => {
                self.memory_area.insert_newline();
            }
            KeyCode::Enter => {
                self.save_memory_modal();
            }
            _ => {
                self.memory_area.input(key);
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
                self.active_turn_skill = None;
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
            KeyCode::Tab if !ctrl && !shift && !alt => {
                if !self.try_tab_complete_prompt() {
                    self.prompt_history_idx = None;
                    self.text_area.input(key);
                }
            }
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
                self.sessions_modal_just_opened = true;
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
                self.open_memory_modal();
            }
            _ => {
                self.prompt_history_idx = None;
                self.text_area.input(key);
            }
        }
        false
    }
}
