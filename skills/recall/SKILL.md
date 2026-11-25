+++
name = "recall"
command = "recall"
description = "Search past Archon sessions to recall prior decisions, fixes, and context, and cross-reference them against the current codebase."
tools = [
    "session",
    "read_file",
    "search",
    "repo_map",
]

[[args]]
name = "all_workspaces"
type = "bool"
default = false
description = "Search sessions across all workspaces instead of only the current workspace"

[[args]]
name = "pinned_only"
type = "bool"
default = false
description = "Restrict session listing and search to pinned sessions"

[[hooks]]
trigger = "on_activate"
tool = "session"
args = { action = "list", all_workspaces = "{{all_workspaces}}", pinned_only = "{{pinned_only}}", limit = 15 }
+++
# Session Recall & Context Synthesis

Your job is to recover past context from saved Archon sessions and verify it against the current state of the repository — **read-only** (do not modify files).

**Search scope:** {{#if all_workspaces}}all workspaces{{else}}current workspace{{/if}}{{#if pinned_only}} (pinned sessions only){{/if}}.
{{#if args}}
**User query:** {{args}}
{{/if}}

## Workflow

1. **Search past sessions first.**
   - Review the preloaded session list from the activation hook.
   - Run `session` (`action: "search"`) with targeted keywords (and `all_workspaces: {{all_workspaces}}`, `pinned_only: {{pinned_only}}`). Try 1–2 synonyms or symbol names if the first query misses.
2. **Read only the relevant slices.**
   - Use `session` (`action: "read"`) with tight `start_message` / `end_message` ranges around the matching message indices from `search`.
   - Keep `include_tool_outputs: false` unless a specific tool output (e.g., an error trace or diff) is essential to answer the question.
3. **Verify against current code.**
   - Past sessions can be stale. Use `repo_map`, `search`, or `read_file` to confirm whether the files, symbols, or behaviors discussed still exist in the current workspace, and explicitly flag anything that has since changed.

## Response Format

- **Answer directly** with the recalled decision, root cause, or context.
- **Cite sources** by session ID, title, and message index (e.g., `20261118_093104_412 #14`) alongside current `file:line` references where verified.
- **Note drift** if the current codebase differs from what was discussed in the past session.
