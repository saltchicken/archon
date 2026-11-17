use crate::{
    core::{
        attachments::ImageAttachment,
        config::{Memory, MemoryScope},
        session::{ChatMessage, SessionSummary, ThoughtPart},
        skills::SkillDefinition,
    },
    tools::ToolCall,
};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub system_prompt: String,
    pub num_ctx: u32,
    pub enable_tools: bool,
    pub active_skill: Option<String>,
    pub skill_args: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedToolAction {
    ProceedTruncated,
    ProceedFull,
    CondenseTruncated,
    CondenseFull,
    Discard,
}

#[derive(Debug)]
pub enum EngineCommand {
    PasteClipboard,
    ImportFile(PathBuf),
    ClearImages,
    SendPrompt {
        prompt: String,
        turn: TurnConfig,
    },
    CancelStream,
    ResolveTool {
        approved: bool,
        always_approve: bool,
        feedback: String,
        turn: TurnConfig,
    },
    ResolveTruncatedTool {
        action: TruncatedToolAction,
        guide_prompt: String,
        turn: TurnConfig,
    },
    LoadSession(String),
    ForkSession(usize),
    RenameSession(String),
    TogglePinSession(Option<String>),
    DeleteSession(String),
    RefreshSessions,
    ResetChat,
    SwitchModel {
        model: String,
        num_ctx: u32,
        max_tool_output_size: usize,
    },
    SetTurnConfig(TurnConfig),
    SetMemory {
        scope: MemoryScope,
        memory: Memory,
    },
    RunShellCommand {
        command: String,
        include_in_context: bool,
    },
    RunManualTool {
        raw_input: String,
        include_in_context: bool,
    },
    InspectPayload {
        draft_prompt: String,
        turn: TurnConfig,
    },
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    AttachmentsUpdated(Vec<ImageAttachment>),
    ChatUpdated {
        session_id: String,
        is_fork: bool,
        is_pinned: bool,
        messages: Vec<ChatMessage>,
        queued_messages: Vec<ChatMessage>,
        num_ctx: u32,
        system_prompt: String,
    },
    QueuedInterjectionsUpdated(Vec<ChatMessage>),
    StreamStarted,
    StreamThoughtChunk(String),
    StreamChunk(String),
    StreamDone {
        prompt_tokens: u64,
        eval_tokens: u64,
        tool_calls: Vec<ToolCall>,
        thoughts: Vec<ThoughtPart>,
    },
    StreamError(String),
    Error(String),
    ToolStarted {
        summary: String,
        index: usize,
        total: usize,
    },
    ToolCompleted {
        output: String,
        full_output: String,
        raw_bytes: usize,
        truncated: bool,
    },
    ShellCommandCompleted {
        command: String,
        output: String,
        full_output: String,
        raw_bytes: usize,
        truncated: bool,
        include_in_context: bool,
    },
    ManualToolCompleted {
        tool_name: String,
        summary: String,
        output: String,
        full_output: String,
        raw_bytes: usize,
        truncated: bool,
        include_in_context: bool,
    },
    HookCompleted {
        tool_name: String,
        summary: String,
        output: String,
        truncated: bool,
    },
    HooksFinished,
    ToolApprovalRequested {
        formatted_preview: String,
        index: usize,
        total: usize,
    },
    TruncatedToolResolutionRequested {
        tool_name: String,
        summary: String,
        preview: String,
        raw_bytes: usize,
        max_size: usize,
        full_cap_bytes: usize,
        index: usize,
        total: usize,
    },
    ToolCondensed(Result<String, String>),
    SessionsListed(Vec<SessionSummary>),
    SkillsUpdated(Vec<SkillDefinition>),
    MemoryUpdated {
        global_memory: Memory,
        workspace_memory: Memory,
    },
    ModelConfigUpdated {
        model: String,
        num_ctx: u32,
        max_tool_output_size: usize,
    },
    PayloadCaptured(String),
    PayloadInspectorOpened(String),
}
