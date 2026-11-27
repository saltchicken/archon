+++
name = "coder"
description = "Software engineering and refactoring mode"
command = "coder"
extends = "base-rust"
tools = ["edit_file", "create_file"]
auto_approve = ["edit_file", "create_file"]
+++
# Rust Engineering & Refactoring Mode

Follow these rules when writing or modifying Rust code in this workspace:

## Dependencies & Crate APIs
1. **Use `cargo:add` for new dependencies (never hand-write versions in `Cargo.toml`).**
   - Call `cargo` (`action: "add"`, `package: "<crate>"`, optional `features`, `dev`, `build_dep`) without pinning `version` so Cargo resolves the latest compatible version and updates `Cargo.lock` automatically.
   - Only add a new dependency when `std` or an existing crate in `Cargo.toml` cannot cleanly cover the need.
2. **Use `cargo:info` and `cargo:update` when checking or bumping existing crates.**
   - Run `cargo` (`action: "info"`, `package: "<crate>"`) to check the latest version on crates.io and available features.
   - Run `cargo` (`action: "update"`, optional `package` / `version`) to refresh `Cargo.lock` when updating dependencies.
3. **Never guess external crate APIs.**
   - After adding or updating a crate, inspect its real type and function signatures with `repo_map` (`crate_name: "<crate>"`, `action: "map"` or `"definition"`) or `read_file` under `~/.cargo/registry/src` before writing code against it.

## Verification
Before finishing any code change, run in order and fix all issues:
1. `cargo` `action: "check"` — resolve all compiler errors.
2. `cargo` `action: "clippy"` — resolve all warnings.
3. `cargo` `action: "test"` — ensure existing and new tests pass.
4. `cargo` `action: "fmt"` — format modified files.
