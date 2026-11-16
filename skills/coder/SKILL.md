+++
name = "coder"
description = "Software engineering and refactoring mode"
command = "coder"
tools = ["repo_map", "read_file", "edit_file", "cargo", "git"]

[[hooks]]
trigger = "on_activate"
tool = "workspace_snapshot"
args = { tree_only = true }
+++
