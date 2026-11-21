+++
command = "rust-app"
description = "Build the most efficient, correct Rust program that fulfills a given task, then verify with cargo until it is clean."
extends = "base-notify"
name = "rust-app-scaffold"
tools = [
    "cargo",
    "repo_map",
    "create_file",
    "edit_file",
    "read_file",
    "fs",
    "search",
]

[[args]]
description = "What the program must do — the purpose, inputs, and behavior to implement."
name = "task"
required = true
type = "string"
+++
Build **the most efficient program that fulfills the task** — the smallest amount of code that does exactly what was asked, nothing more.

**Task:** {{task}}

## Approach
1. **Understand before writing.** Check the target directory (`fs` `action: "list"`); if Rust source already exists, skim it with `repo_map` (`action: "map"`) so you extend rather than duplicate.
2. **Design the simplest thing that works.**
   - Prefer a small number of focused modules — or a single `main.rs` for small tasks. Do **not** add layers, engines, protocols, managers, or abstractions the task does not require.
   - Use the **fewest dependencies** that get the job done; prefer std first.
   - No speculative config systems, persistence, plugin/feature layers, or generic machinery unless the task explicitly needs them.
3. **Write idiomatic Rust.** `?` propagation over `unwrap`, pattern matching over index loops, small focused functions.
4. **Never hallucinate crate APIs.** Before using an external crate's types, read its real signatures with `repo_map` (`crate_name: "<crate>"`, `action: "map"` or `"definition"`).

## Verify (fix until clean)
Run in order, fixing every issue with `edit_file` before moving on:
1. `cargo` `action: "check"` — clear all type/borrow errors.
2. `cargo` `action: "clippy"` — zero warnings.
3. `cargo` `action: "fmt"`

> If `cargo check` reports that `Cargo.lock` needs updating after a dependency change, ask the user to run `!!cargo generate-lockfile` (or `!!cargo check`) via the shell so the lock file syncs, then re-run.

## Deliver
- A one-line summary of what was built and where the code lives.
- The exact run command (e.g. `!cargo run`).
- Any dependency you added and why it was necessary.
