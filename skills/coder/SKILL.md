+++
name = "coder"
description = "Software engineering and refactoring mode"
command = "coder"
tools = ["repo_map", "read_file", "edit_file", "cargo"]

[[hooks]]
trigger = "on_activate"
tool = "workspace_snapshot"
args = { tree_only = false }

[[hooks]]
trigger = "on_activate"
tool = "repo_map"
args = { action = "map", max_tokens = 8000 }

+++
