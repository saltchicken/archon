//! Task-local progress channel for tool execution visibility.
//!
//! Set by `CoreEngine::spawn_with_tx` so that any tool (or sub-call) running
//! within the spawned task can emit live progress events to the UI without
//! threading a sender through every function signature.

use tokio::sync::mpsc::UnboundedSender;

tokio::task_local! {
    /// The event channel for the currently executing tool task.
    pub static TOOL_PROGRESS_TX: UnboundedSender<crate::engine::EngineEvent>;
}
