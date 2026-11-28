use crate::{
    attachments::ImageAttachment,
    config::{Memory, MemoryScope},
    session::{ChatMessage, SessionSummary, ThoughtPart},
    skills::SkillDefinition,
    tools::{SessionToolState, ToolCall, ToolExecutionOutput},
};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionToolsAction {
    List,
    Update {
        add: Vec<String>,
        remove: Vec<String>,
    },
    Reset,
}

#[derive(Debug, Clone)]
pub enum ToolExecSource {
    Model {
        tool_name: String,
        summary: String,
        arguments: serde_json::Value,
    },
    ManualShell {
        command: String,
        include_in_context: bool,
    },
    ManualTool {
        tool_name: String,
        summary: String,
        arguments: serde_json::Value,
        include_in_context: bool,
    },
}

impl ToolExecSource {
    pub fn include_in_context(&self) -> bool {
        match self {
            Self::Model { .. } => true,
            Self::ManualShell {
                include_in_context, ..
            }
            | Self::ManualTool {
                include_in_context, ..
            } => *include_in_context,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub system_prompt: String,
    pub num_ctx: u32,
    pub enable_tools: bool,
    pub active_skill: Option<String>,
    pub skill_args: Option<String>,
    pub raw_skill_args: Option<String>,
    pub skill_inputs: BTreeMap<String, serde_json::Value>,
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
    CompactChat {
        skill: Option<String>,
        instructions: Option<String>,
    },
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
    ManageSessionTools(SessionToolsAction),
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
    ToolCompleted,
    ToolExecFinished(ToolExecSource, ToolExecutionOutput),
    HookCompleted {
        tool_name: String,
        summary: String,
        output: String,
        truncated: bool,
        after_turn: bool,
    },
    HooksFinished {
        after_turn: bool,
    },
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
    ChatCompacted {
        res: Result<String, String>,
        prompt_tokens: u64,
        eval_tokens: u64,
    },
    TokenUsageUpdated {
        tokens: u64,
        is_estimate: bool,
    },
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
    SessionToolsUpdated(SessionToolState),
}
