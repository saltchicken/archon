use ratatui::style::Color;

// ─── Borders & Chrome ────────────────────────────────────────────────────────
pub const BORDER_PROMPT: Color = Color::Cyan;
pub const BORDER_PROMPT_HINT: Color = Color::Yellow;
pub const BORDER_MODAL_ACCENT: Color = Color::Cyan;
pub const BORDER_MODAL_WARN: Color = Color::Yellow;
pub const BORDER_MODAL_DANGER: Color = Color::Red;

// ─── Message Headers & Badges ────────────────────────────────────────────────
pub const HEADER_USER: Color = Color::LightGreen;
pub const HEADER_ASSISTANT: Color = Color::Green;
pub const HEADER_STREAMING: Color = Color::Yellow;
pub const HEADER_QUEUED: Color = Color::LightCyan;
pub const HEADER_TOOL: Color = Color::Yellow;
pub const HEADER_HOOK: Color = Color::Yellow;
pub const HEADER_LOCAL: Color = Color::Blue;
pub const HEADER_ERROR: Color = Color::LightRed;
pub const HEADER_SYSTEM: Color = Color::LightGreen;

pub const BADGE_TRUNCATED: Color = Color::LightRed;
pub const BADGE_COLLAPSED: Color = Color::Blue;
pub const SCROLL_ELLIPSIS: Color = Color::Blue;
pub const BADGE_FORK: Color = Color::Yellow;
pub const BADGE_CODE_COPY: Color = Color::Yellow;
pub const BADGE_FLASH_SUCCESS: Color = Color::Green;

// ─── Sidebar ─────────────────────────────────────────────────────────────────
pub const SIDEBAR_DIR: Color = Color::Cyan;
pub const SIDEBAR_MODEL: Color = Color::Green;
pub const SIDEBAR_EMPTY_IMAGES: Color = Color::LightGreen;

pub const CTX_USAGE_OK: Color = Color::Cyan;
pub const CTX_USAGE_WARN: Color = Color::Yellow;
pub const CTX_USAGE_CRIT: Color = Color::LightRed;

// ─── Modals, Lists & Tabs ────────────────────────────────────────────────────
pub const SELECTION_HIGHLIGHT: Color = Color::Green;
pub const SELECTION_ACTIVE: Color = Color::Cyan;

pub const MODAL_TITLE_ACCENT: Color = Color::Cyan;
pub const MODAL_SECTION_LABEL: Color = Color::Blue;
pub const MODAL_HINT: Color = Color::Yellow;
pub const MODAL_MUTED: Color = Color::LightGreen;

pub const TAB_RECENT_ACTIVE: Color = Color::Cyan;
pub const TAB_PINNED_ACTIVE: Color = Color::Yellow;
pub const TAB_GLOBAL_ACTIVE: Color = Color::Cyan;
pub const TAB_WORKSPACE_ACTIVE: Color = Color::Magenta;
pub const TAB_INACTIVE: Color = Color::LightGreen;

pub const STATUS_ENABLED: Color = Color::Green;
pub const STATUS_DISABLED: Color = Color::LightRed;
pub const PRESET_ACTIVE: Color = Color::Green;

// ─── Truncated Tool Modal ────────────────────────────────────────────────────
pub const TRUNCATED_SUMMARY: Color = Color::White;
pub const TRUNCATED_TOOL_NAME: Color = Color::LightGreen;
pub const TRUNCATED_WARNING: Color = Color::Red;
pub const TRUNCATED_PREVIEW_TEXT: Color = Color::LightGreen;

// ─── Tool Approval & Payload Diff Preview ────────────────────────────────────
pub const PREVIEW_DEFAULT_SECTION: Color = Color::Blue;
pub const PREVIEW_HEADER_WARN: Color = Color::LightRed;
pub const PREVIEW_SECTION_WARN: Color = Color::Red;
pub const PREVIEW_HEADER_CONTENT: Color = Color::Green;
pub const PREVIEW_SECTION_CONTENT: Color = Color::Cyan;
pub const PREVIEW_HEADER_DEFAULT: Color = Color::Cyan;
pub const PREVIEW_SECTION_DEFAULT: Color = Color::White;

pub const DIFF_FILE_HEADER: Color = Color::Yellow;
pub const DIFF_HUNK_HEADER: Color = Color::Cyan;
pub const DIFF_ADD: Color = Color::Green;
pub const DIFF_DELETE: Color = Color::Red;
pub const DIFF_CONTEXT: Color = Color::Cyan;

// ─── Markdown Rendering ──────────────────────────────────────────────────────
pub const MD_INLINE_CODE: Color = Color::Yellow;
pub const MD_CODE_BORDER: Color = Color::Blue;
pub const MD_CODE_TEXT: Color = Color::Cyan;
pub const MD_CODE_KEYWORD: Color = Color::Green;
pub const MD_CODE_TYPE: Color = Color::Yellow;
pub const MD_CODE_FUNCTION: Color = Color::Blue;
pub const MD_CODE_STRING: Color = Color::Cyan;
pub const MD_CODE_NUMBER: Color = Color::Magenta;
pub const MD_CODE_COMMENT: Color = Color::DarkGray;
pub const MD_CODE_KEY: Color = Color::Blue;
pub const MD_CODE_MACRO: Color = Color::Magenta;
pub const MD_CODE_PUNCT: Color = Color::Green;

pub const MD_TABLE_HEADER: Color = Color::Cyan;
pub const MD_TABLE_BORDER: Color = Color::LightGreen;

pub const MD_H1: Color = Color::LightCyan;
pub const MD_H2: Color = Color::Cyan;
pub const MD_H3: Color = Color::LightMagenta;
pub const MD_H4: Color = Color::LightGreen;
pub const MD_HEADER_ACCENT: Color = Color::Blue;
pub const MD_HEADER_SUB_ACCENT: Color = Color::LightGreen;

pub const MD_QUOTE_TEXT: Color = Color::LightGreen;
pub const MD_QUOTE_BAR_0: Color = Color::Blue;
pub const MD_QUOTE_BAR_1: Color = Color::Cyan;
pub const MD_QUOTE_BAR_2: Color = Color::LightMagenta;
pub const MD_QUOTE_BAR_3: Color = Color::LightGreen;

pub const MD_LIST_DEPTH_0: Color = Color::Cyan;
pub const MD_LIST_DEPTH_1: Color = Color::LightMagenta;
pub const MD_LIST_DEPTH_2: Color = Color::Blue;
pub const MD_LIST_DEPTH_FALLBACK: Color = Color::LightGreen;

pub const MD_TASK_CHECKED: Color = Color::Green;
pub const MD_TASK_UNCHECKED: Color = Color::LightGreen;

pub const MD_TOOL_PARAM: Color = Color::Yellow;
pub const MD_THOUGHT: Color = Color::LightCyan;
