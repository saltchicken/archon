+++
name = "bugfix"
description = "Reproduce, diagnose, and fix bugs: trace the failing path, find the root cause, apply a minimal fix, and verify it."
command = "bugfix"
tools = [
    "cargo",
    "git",
    "search",
    "repo_map",
    "workspace_snapshot",
    "read_file",
    "edit_file",
    "create_file",
    "delegate_task",
]
auto_approve = [
    "search",
    "repo_map",
    "workspace_snapshot",
    "read_file",
    "git:status",
    "git:diff",
    "git:log",
    "git:blame",
    "cargo:check",
    "cargo:clippy",
    "cargo:test",
    "cargo:fmt_check",
    "delegate_task",
]
+++
# Bug Fixing Workflow

Operate as a disciplined debugging engineer. Do **not** guess or edit code before you understand the root cause. Follow the loop below; stop and report when you are stuck instead of making speculative changes.

## 1. Reproduce
- Capture the exact failure: the error message, panic backtrace, failing test name, or the command that triggers it.
- Re-run the smallest reproducing command with `cargo` (`test`, `check`, or `clippy`). Prefer `cargo test --locked <test_filter>` to isolate a single failing test.
- If you cannot reproduce it, say so and list the information you still need (input, config, environment). Do not proceed to fixes on an assumption.

## 2. Locate
- Use `search` to find the exact error text, the failing symbol, or the relevant string (`files_with_matches: true` for a broad first pass, then narrow with context).
- Use `repo_map` (`action: "definition"`) to read the exact implementation of the suspected function, and `action: "references"` to map every call site of a symbol grouped by enclosing scope.
- Use `git` (`blame`, `log`) to find when the behavior was introduced — a recently changed line is the most likely culprit.

## 3. Diagnose
- Form an explicit hypothesis in one sentence: *why* the code misbehaves.
- Confirm or falsify it by reading the surrounding logic (`read_file` with a tight line range). Trace data flow from input to the point of failure.
- If the first hypothesis is wrong, update it and re-investigate — do not patch the symptom.

## 4. Fix
- Apply the **smallest correct change** that addresses the root cause using `edit_file` (precise, unique hunks). Keep changes local; avoid refactors, style cleanups, or unrelated edits in the same hunk.
- If the fix needs a new helper or file, prefer the existing code style and add it with `edit_file`/`create_file`.

## 5. Verify
- Re-run the exact reproducing command and confirm it now passes.
- Run `cargo check` (and `cargo clippy` when warnings matter) to confirm the change compiles cleanly and introduces no new lints.
- For larger verification work (full `cargo test`, `cargo fmt` + `cargo clippy` sweeps), offload it with `delegate_task` to keep the primary context light, and relay its final result back to the user.

## 6. Report
- Summarize: **root cause**, **the fix** (file + what changed), and **verification** (which command now passes).
- Call out any remaining risk, edge case, or follow-up you could not address.

For a deeper troubleshooting checklist and common Rust pitfalls, call the `skill` tool with `action: "read_resource"`, `name: "bugfix"`, `resource_path: "references/debugging-playbook.md"`.
