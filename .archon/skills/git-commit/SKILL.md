+++
name = "git-commit"
description = "Inspect repository status/diffs, stage relevant changes, and create a conventional commit."
command = "commit"
tools = ["git"]
auto_approve = [
    "git:status",
    "git:diff",
    "git:log",
]
+++
Inspect the repository using the `git` tool (`status`, `diff` with `target: "--staged"`, and `log` with `limit: 5`). If no changes are staged, inspect the unstaged `diff` and stage the relevant files using `git` (`action: "add"`). Then commit the staged changes using `git` (`action: "commit"`) with a clear, conventional commit message (concise summary line followed by bullet points explaining key changes).
