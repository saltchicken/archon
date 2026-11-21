use super::{
    ActiveSessionState, CoreEngine, EngineEvent, TurnConfig,
    attachments::ImageStaging,
    config::{self, Memory, MemoryScope},
    provider::Provider,
    session::{self, ChatMessage, SessionManager},
};
use std::{collections::VecDeque, path::PathBuf, sync::Arc};
use tracing::{error, info, warn};

impl CoreEngine {
    pub(super) async fn flush_save_task(&mut self) {
        if let Some(save_handle) = self.save_task.take() {
            let _ = save_handle.await;
        }
    }

    pub(super) async fn clear_attachments(&self) {
        let staging = Arc::clone(&self.attachments);
        let _ = tokio::task::spawn_blocking(move || staging.clear()).await;
        let _ = self.tx.send(EngineEvent::AttachmentsUpdated(vec![]));
    }

    pub(super) async fn update_attachments<F>(&self, op: F)
    where
        F: FnOnce(&ImageStaging) -> anyhow::Result<PathBuf> + Send + 'static,
    {
        let staging = Arc::clone(&self.attachments);
        if let Ok((res, list)) =
            tokio::task::spawn_blocking(move || (op(&staging), staging.list_attachments())).await
        {
            if let Err(e) = res {
                warn!("Attachment operation failed: {e}");
            }
            let _ = self.tx.send(EngineEvent::AttachmentsUpdated(list));
        }
    }

    pub(super) async fn collect_unsent_images(
        &mut self,
    ) -> (Option<Vec<String>>, Vec<(String, Vec<u8>)>) {
        let staging = Arc::clone(&self.attachments);
        let already_sent = self.session.sent_images.clone();

        let (names, blobs, newly_sent) = tokio::task::spawn_blocking(move || {
            let mut names = Vec::new();
            let mut blobs = Vec::new();
            let mut sent = Vec::new();

            for att in staging.list_attachments() {
                if already_sent.contains(&att.path) {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&att.path) {
                    names.push(att.name.clone());
                    blobs.push((att.name, bytes));
                    sent.push(att.path);
                }
            }
            (names, blobs, sent)
        })
        .await
        .unwrap_or_default();

        self.session.sent_images.extend(newly_sent);
        ((!names.is_empty()).then_some(names), blobs)
    }

    pub(super) async fn emit_sessions(&mut self, delete_first: Option<String>) {
        self.flush_save_task().await;
        let sessions = Arc::clone(&self.sessions);
        if let Ok(list) = tokio::task::spawn_blocking(move || {
            if let Some(id) = delete_first {
                let _ = sessions.delete(&id);
            }
            sessions.list()
        })
        .await
        {
            let _ = self.tx.send(EngineEvent::SessionsListed(list));
        }
    }

    pub(super) async fn save_and_emit_chat(&mut self, new_images: Vec<(String, Vec<u8>)>) {
        self.save_session_bg(new_images).await;
        self.emit_chat();
    }

    pub(super) async fn save_session_bg(&mut self, new_images: Vec<(String, Vec<u8>)>) {
        if !self.save_session {
            return;
        }
        self.flush_save_task().await;

        let sessions = Arc::clone(&self.sessions);
        let id = self.session.id.clone();
        let is_fork = self.session.is_fork;
        let is_pinned = self.session.is_pinned;
        let custom_title = self.session.custom_title.clone();
        let model = self.model.clone();
        let sys = self.turn.system_prompt.clone();
        let ctx = self.turn.num_ctx;
        let msgs = self.session.messages.clone();

        self.save_task = Some(tokio::task::spawn_blocking(move || {
            if let Err(e) = sessions.save(
                &id,
                custom_title.as_deref(),
                &model,
                &sys,
                ctx,
                &msgs,
                &new_images,
                is_fork,
                is_pinned,
            ) {
                error!("Failed to save session {id}: {e}");
            }
        }));
    }

    pub(super) fn emit_chat(&self) {
        let messages = self
            .session
            .messages
            .iter()
            .map(ChatMessage::with_ui_image_placeholders)
            .collect();
        let _ = self.tx.send(EngineEvent::ChatUpdated {
            session_id: self.session.id.clone(),
            is_fork: self.session.is_fork,
            is_pinned: self.session.is_pinned,
            messages,
            queued_messages: self.queued_ui_messages(),
            num_ctx: self.turn.num_ctx,
            system_prompt: self.turn.system_prompt.clone(),
        });
    }

    pub(super) async fn handle_load_session(&mut self, session_id: String) {
        info!("Loading session from database: {session_id}");
        self.flush_save_task().await;

        let sessions = Arc::clone(&self.sessions);
        let staging = Arc::clone(&self.attachments);
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut data = sessions.load(&session_id)?;
            for msg in &mut data.messages {
                msg.images = None;
            }
            let all_image_names: Vec<String> = data
                .messages
                .iter()
                .filter_map(|m| m.image_names.as_ref())
                .flatten()
                .cloned()
                .collect();

            let image_blobs = sessions.load_images(&data.id, &all_image_names);
            let restored_paths = staging.restore_from_bytes(&image_blobs);
            let attachments = staging.list_attachments();
            Ok((data, restored_paths, attachments))
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);

        match res {
            Ok((data, restored_staging_paths, attachments)) => {
                self.abort_active_state();
                self.session.auto_approve.clear();
                self.session.executed_skill_hooks.clear();
                self.session.sent_images.extend(restored_staging_paths);
                let _ = self.tx.send(EngineEvent::AttachmentsUpdated(attachments));

                self.session.custom_title = data.custom_title();
                self.session.is_fork = data.is_fork();
                self.session.id = data.id;
                self.session.is_pinned = data.pinned;
                self.turn.system_prompt = data.system_prompt;

                let mut model_switch_err = None;
                if !data.model.is_empty() && data.model != self.model {
                    let loaded_provider = Provider::from_model(&data.model);
                    match loaded_provider.build_client(&self.config) {
                        Ok(new_client) => {
                            self.model = data.model.clone();
                            self.provider = loaded_provider;
                            self.client = new_client;
                        }
                        Err(e) => {
                            let msg = format!(
                                "Failed to build HTTP client for session model '{}': {e}",
                                data.model
                            );
                            error!("{msg}");
                            model_switch_err = Some(msg);
                        }
                    }
                }

                self.reset_skill_and_sync_model_limits();
                let pending_calls = data
                    .messages
                    .last()
                    .and_then(|m| m.tool_calls.clone())
                    .filter(|c| !c.is_empty());
                self.session.messages = data.messages;
                self.emit_chat();
                if let Some(err) = model_switch_err {
                    self.emit_error(err).await;
                }

                if let Some(calls) = pending_calls {
                    self.inflight.pending_tools = VecDeque::from(calls);
                    self.process_pending_tools().await;
                }
            }
            Err(e) => {
                let msg = format!("Failed to load session: {e}");
                error!("{msg}");
                self.emit_error(msg).await;
            }
        }
    }

    pub(super) async fn handle_fork_session(&mut self, msg_idx: usize) {
        let Some(target_msg) = self.session.messages.get(msg_idx) else {
            self.emit_error("Cannot fork conversation: message index out of range.")
                .await;
            return;
        };
        if !target_msg.is_forkable() {
            self.emit_error(
                "Cannot fork conversation: forks can only be created from assistant responses.",
            )
            .await;
            return;
        }

        info!(
            "Forking session {} from message #{msg_idx}",
            self.session.id
        );
        self.flush_save_task().await;
        self.abort_active_state();
        self.session.auto_approve.clear();
        self.session.executed_skill_hooks.clear();

        self.session.messages.truncate(msg_idx + 1);
        if let Some(last) = self.session.messages.last_mut() {
            last.tool_calls = None;
        }

        let old_session_id = self.session.id.clone();
        let mut new_session_id = SessionManager::new_id();
        if new_session_id == old_session_id {
            new_session_id.push_str("_fork");
        }
        self.session.id = new_session_id;
        self.session.is_fork = true;
        self.session.is_pinned = false;

        let all_image_names: Vec<String> = self
            .session
            .messages
            .iter()
            .filter_map(|m| m.image_names.as_ref())
            .flatten()
            .cloned()
            .collect();

        let sessions = Arc::clone(&self.sessions);
        let staging = Arc::clone(&self.attachments);
        let (image_blobs, restored_paths, attachments) = tokio::task::spawn_blocking(move || {
            let blobs = sessions.load_images(&old_session_id, &all_image_names);
            let paths = staging.restore_from_bytes(&blobs);
            let list = staging.list_attachments();
            (blobs, paths, list)
        })
        .await
        .unwrap_or_default();

        self.session.sent_images.extend(restored_paths);
        let _ = self.tx.send(EngineEvent::AttachmentsUpdated(attachments));
        self.save_and_emit_chat(image_blobs).await;
    }

    pub(super) async fn handle_rename_session(&mut self, new_title: String) {
        let trimmed = session::strip_fork_prefix(new_title.trim()).trim();
        if trimmed.is_empty() {
            self.emit_error("Session title cannot be empty.").await;
            return;
        }
        info!("Renaming session {} to '{trimmed}'", self.session.id);
        self.session.custom_title = Some(trimmed.to_string());
        self.save_session_bg(Vec::new()).await;
        self.emit_sessions(None).await;
    }

    pub(super) async fn handle_toggle_pin_session(&mut self, maybe_id: Option<String>) {
        let target_id = maybe_id.unwrap_or_else(|| self.session.id.clone());
        if target_id == self.session.id {
            self.session.is_pinned = !self.session.is_pinned;
            info!(
                "Toggled pin state for active session {}: {}",
                self.session.id, self.session.is_pinned
            );
            self.save_session_bg(Vec::new()).await;
            self.emit_chat();
            self.emit_sessions(None).await;
        } else {
            self.flush_save_task().await;
            let sessions = Arc::clone(&self.sessions);
            let list_res = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let new_state = sessions.toggle_pinned(&target_id)?;
                info!("Toggled pin state for session {target_id}: {new_state}");
                Ok(sessions.list())
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r);
            match list_res {
                Ok(list) => {
                    let _ = self.tx.send(EngineEvent::SessionsListed(list));
                }
                Err(e) => {
                    let msg = format!("Failed to toggle session pin: {e}");
                    error!("{msg}");
                    self.emit_error(msg).await;
                }
            }
        }
    }

    pub(super) async fn handle_delete_session(&mut self, session_id: String) {
        info!("Deleting session: {session_id}");
        if session_id == self.session.id && self.session.is_pinned {
            self.session.is_pinned = false;
            self.emit_chat();
        }
        self.emit_sessions(Some(session_id)).await;
    }

    pub(super) async fn handle_reset_chat(&mut self) {
        info!("Resetting chat session");
        self.flush_save_task().await;
        self.abort_active_state();
        self.clear_attachments().await;

        self.session = ActiveSessionState::new();
        self.reset_skill_and_sync_model_limits();
        self.emit_chat();
    }

    pub(super) async fn handle_set_turn_config(&mut self, new_turn: TurnConfig) {
        self.update_turn_preserving_skill(new_turn);
        self.save_session_bg(Vec::new()).await;
        self.emit_chat();
    }

    pub(super) async fn handle_set_memory(&mut self, scope: MemoryScope, memory: Memory) {
        info!(
            "Updating {:?} Memory (enabled: {}, items: {})",
            scope,
            memory.enabled,
            memory.items.len()
        );
        let mem_to_save = memory.clone();
        let ws_dir = self.config.workspace_config_dir();
        let save_res = tokio::task::spawn_blocking(move || {
            config::save_scoped_memory(scope, ws_dir.as_deref(), &mem_to_save)
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);

        match save_res {
            Ok(_) => {
                let (global_memory, workspace_memory) = config::reload_memories(&self.config);
                self.config.global_memory = global_memory.clone();
                self.config.workspace_memory = workspace_memory.clone();
                let _ = self.tx.send(EngineEvent::MemoryUpdated {
                    global_memory,
                    workspace_memory,
                });
            }
            Err(e) => {
                error!("Failed to persist Memory: {e}");
                self.config.set_scoped_memory(scope, memory);
            }
        }
    }
}
