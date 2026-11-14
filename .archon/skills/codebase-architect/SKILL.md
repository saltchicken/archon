+++
name = "codebase-architect"
description = "Navigate and modify workspaces using AST repo_map, targeted reads, atomic edits, and sub-agent delegation."
command = "architect"
tools = [
    "repo_map",
    "search",
    "workspace_snapshot",
    "read_file",
    "edit_file",
    "create_file",
    "cargo",
    "delegate_task",
]
auto_approve = [
    "repo_map",
    "search",
    "workspace_snapshot",
    "read_file",
    "cargo:check",
    "cargo:clippy",
    "cargo:fmt_check",
    "delegate_task",
]
+++
Navigate and modify the workspace efficiently:
1. Architecture & Symbols (`repo_map`): Start with `repo_map` (`action: "map"`) to inspect the PageRank-ranked AST symbol graph. Use `action: "definition"` to read a symbol's exact implementation by name without needing file paths or line numbers, and `action: "references"` to find all AST call sites grouped by enclosing scope.
2. Text & Configs (`search` & `workspace_snapshot`): Use `search` for regex/literal strings, error messages, or comments (`files_with_matches: true` for broad surveys). Use `workspace_snapshot` (`tree_only: true`) for directory structure, or scoped with `target_path`/`include` to inspect manifests (`Cargo.toml`) and small modules.
3. Targeted Reads & Edits: Use `read_file` with `start_line`/`end_line` for non-code files or surrounding context, apply minimal atomic hunks via `edit_file`, and delegate routine formatting/lint verification (`cargo fmt`, `cargo clippy`, `cargo check`) to the local model via `delegate_task`.
