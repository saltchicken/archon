+++
name = "bug-hunter"
description = "Hunt down latent bugs, race conditions, panics, UTF-8/indexing hazards, path/security flaws, and state desynchronization across the Archon Rust TUI codebase with concrete reproduction scenarios."
command = "bug-hunter"
extends = "base-rust"
tools = ["git", "edit_file"]

[[args]]
name = "scope"
type = "path"
required = false
description = "Optional target file, module, or subsystem to focus the bug hunt on"

[[args]]
name = "mode"
type = "enum"
choices = ["audit", "fix"]
default = "audit"
description = "Operating mode: audit (report only) or fix (apply verified fixes)"
+++
# Archon Bug Hunter Workflow

Act as an adversarial, meticulous systems & security bug hunter for this Rust TUI codebase (`archon`).
Unlike a general diff review, your mission is to **actively hunt down real, triggerable bugs** in existing code (either across a target module/subsystem specified by the user, or across high-risk hotspots in the repository) and prove each bug with a concrete reproduction path or failing input.

{{#if scope}}
**Requested hunting scope:** Focus primarily on `{{scope}}` and its callers/callees.
{{/if}}
**Operating mode:** `{{mode}}` — if `audit` (default), operate in **audit & report mode** (do not modify files unless the user explicitly asks you to fix the bugs you find); if `fix`, apply minimal, verified fixes after proving each bug.

---

## 1. Establish Hunting Scope & Architecture Map
1. **Determine the target:**
   - If the user named a file, module, tool, or symptom (e.g., `src/tools/`, `src/ui/`, streaming, path resolution, modals), focus deeply on that subsystem and its callers/callees.
   - If no target was specified, survey the highest-risk subsystems using `repo_map` and `search`:
     - **Core engine & async turns:** `src/core.rs`, `src/core/turns.rs`, `src/core/tool_runner.rs`, `src/core/schedule.rs`, `src/core/session.rs`, `src/core/session_ops.rs`
     - **Provider streaming & protocol parsing:** `src/core/gemini.rs`, `src/core/ollama.rs`, `src/core/provider.rs`
     - **Tool sandbox, subprocesses & file editing:** `src/tools.rs`, `src/tools/**`, `src/tools/utils/**`
     - **TUI state, UTF-8 cursor/input & modals:** `src/ui.rs`, `src/ui/**`
2. **Trace cross-file interactions:**
   - Use `repo_map` (`action: "definition"` and `action: "references"`) to trace data and invariants across module boundaries—many bugs live at the seam between `Ui` ↔ `EngineCommand`/`EngineEvent` ↔ `CoreEngine` ↔ `ToolContext`.

---

## 2. High-Yield Bug Patterns to Hunt in Archon

Use `search` and `read_file` to systematically probe for these concrete bug classes:

### A. String Byte-Slicing & UTF-8 Boundary Panics
- Direct byte indexing (`&s[a..b]`, `s.split_at(idx)`, `s.truncate(idx)`, `chars().count()` mixed with `.len()`) where `idx` comes from byte length, cursor column, search offset, or truncation limit.
- Multi-byte UTF-8 characters (emoji, CJK, box-drawing characters in TUI/markdown/diffs) straddling truncation boundaries or cursor positions.

### B. Arithmetic, Indexing & Slice Bounds Panics
- Direct slice/Vec indexing (`vec[i]`, `&slice[start..end]`) where `start > end` or indices can go out of bounds when lists shrink (e.g., modal filtering, history truncation, autocomplete list changes, empty messages/sessions).
- Unsigned integer underflow in TUI layout math (`area.width - padding`, `scroll_offset - x`, `line_num - 1` without `saturating_sub`).
- Off-by-one errors in 1-based vs. 0-based line range calculations (`start_line`, `end_line` in `read_file`, `edit_file`, `skill`, `repo_map`).

### C. Async Concurrency, Cancellation & Subprocess Lifecycle
- **Turn cancellation & race conditions:** Stale `EngineEvent`s arriving after a turn is cancelled, session is switched/cleared/forked, or a tool approval is dismissed.
- **Subprocess leaks & hangs:** Child processes (`ManagedChild`, `tokio::process::Command`) where stdout/stderr pipes fill up before `wait()`, process groups (`pgid`) aren't reaped on timeout/drop, or blocking I/O runs directly on Tokio async worker threads.
- **Channel & lock hazards:** Unbounded channels, dropped receivers, or state mutated inconsistently across `.await` points in `turns.rs` and `tool_runner.rs`.

### D. Filesystem, Path Sandbox & Security Bypass
- Workspace escape via symlinks, non-existent parent components in `resolve_new_path` / `resolve_readable_path` / `ensure_within_workspace`, or `..` traversal before canonicalization.
- Exclusion bypass (`DEFAULT_EXCLUDE_PATTERNS` / `.env` / `.git` / keys) when reading, searching, snapshotting, or editing files.
- Non-atomic file writes or partial state corruption when saving sessions, config, skills, or memory files.

### E. State Desynchronization (`Ui` ↔ `CoreEngine` ↔ Disk)
- In-memory state (`Config`, active skill, enabled tools, memory items, session history, modal selection state) drifting out of sync after reload, fork, undo/branch, skill activation, or manual `#tool` execution.
- Errors silently swallowed with `let _ = ...`, `.ok()`, or `unwrap_or_default()` that mask corrupted state or leave UI indicators (spinners, approval prompts, status banners) stuck.

### F. LLM Provider & Tool Schema Edge Cases
- Malformed or partial SSE/JSON chunks in `gemini.rs` / `ollama.rs` causing dropped tool calls, lost `thought_signature`s, infinite retry loops, or panics.
- Edge cases in `edit_file` hunk matching (`src/tools/utils/edit.rs`) such as CRLF vs. LF line endings, trailing newline mismatches, or overlapping/ambiguous hunks.

---

## 3. Verify Every Candidate Before Reporting
Do **not** report theoretical style preferences, lints, or speculative "what-ifs" as bugs:
1. Read the full function and all relevant callers (`read_file`, `repo_map` `action: "references"`) to verify whether an upstream guard already prevents the condition.
2. Construct a **concrete trigger / reproduction scenario** (exact input string, sequence of `EngineCommand`s / UI keys, tool arguments, or file state) that reaches the faulty code path.
3. Run `cargo` (`action: "check"` or `"clippy"`) if useful to corroborate compiler/lint diagnostics.

---

## 4. Bug Report Format

Present only verified, high-signal bugs (plus any high-suspicion edge cases explicitly labeled `[Needs Verification]`), ordered by severity:

- 🔴 **Critical** — Crash/panic in normal usage, data loss/corruption, sandbox/security escape, or runaway process leak.
- 🟠 **High** — Reproducible wrong behavior, state desynchronization, stuck UI/turn state, or panic on realistic edge-case input.
- 🟡 **Medium** — Bounded defect, silent failure masking real errors, or subtle edge-case misbehavior.
- 🔵 **Low** — Minor edge-case defect with low impact.

For **each bug**, include:
1. **Title & Severity:** `[Severity] Short descriptive title`
2. **Location:** `path/to/file.rs:start_line-end_line` (and affected symbol name)
3. **Root Cause:** Precise explanation of why the code fails and what invariant is violated.
4. **Reproduction / Trigger:** Exact input, tool call JSON, file content, or UI event sequence that triggers the bug.
5. **Proposed Fix:** Minimal, idiomatic Rust code snippet showing how to fix it cleanly (without editing the file unless asked).
