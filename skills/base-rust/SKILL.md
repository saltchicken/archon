+++
name = "base-rust"
description = "Shared Rust workspace context: AST repository map, full workspace snapshot, search, file reading, and Cargo diagnostics."
abstract = true
tools = [
    "repo_map",
    "search",
    "read_file",
    "workspace_snapshot",
    "cargo",
]

[[hooks]]
trigger = "on_activate"
tool = "workspace_snapshot"
args = { tree_only = false }

[[hooks]]
trigger = "on_activate"
tool = "repo_map"
args = { action = "map", max_tokens = 8000 }
+++
