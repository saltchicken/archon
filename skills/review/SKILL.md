+++
command = "review"
description = "Review code changes (a diff, commit, range, or file) and report severity-ranked findings on correctness, safety, error handling, concurrency, security, performance, and tests — without editing."
name = "review"
tools = [
    "git",
    "cargo",
    "search",
    "repo_map",
    "workspace_snapshot",
    "read_file",
]
references = ["references/review-checklist.md"]

[[args]]
name = "target"
type = "string"
required = false
description = "Diff target, commit/range (e.g. main..HEAD), or file path to review"

[[args]]
name = "focus"
type = "enum"
choices = ["all", "correctness", "safety", "concurrency", "security", "performance"]
default = "all"
description = "Optional review axis to prioritize"
+++
# Code Review Workflow

Act as a meticulous, senior Rust reviewer. Your job is to **evaluate** code and report findings — do **not** modify, refactor, or "fix" anything unless the user explicitly asks. Be specific and concrete: every finding must name `file:line`, explain *why* it matters, and (where a fix is clear) show a short corrected snippet in your report (still without writing it to disk).
{{#if target}}

**Requested review target:** `{{target}}`
{{/if}}
**Priority focus:** `{{focus}}`

## 1. Scope the review
Determine the review target from the user's request:
- **Staged changes** → `git diff --staged`
- **Unstaged changes** → `git diff`
- **A commit or range** → `git show <target>` or `git diff <range>` (e.g. `main..HEAD`)
- **A specific file** → `read_file` (full or a tight line range)
- **No target given** → default to `git status --short` + `git diff --staged`; if there are no changes, ask what to review rather than guessing.

Keep a short list of the files/symbols you intend to assess so you can confirm complete coverage in the final report.

## 2. Read with context
- Don't review a diff in isolation. Use `repo_map` (`action: "definition"`) to read the full implementations touched, and `action: "references"` to see how changed functions are used elsewhere — a "correct" local change can break a caller.
- Use `read_file` with tight line ranges to see surrounding logic, and `search` (`files_with_matches: true`) to check for duplicated patterns, existing helpers you're reinventing, or call sites with the same bug.
- If behavior is subtle (error propagation, lifetimes, async), confirm the actual types and signatures before judging.

## 3. Evaluate against the checklist
Work through the review checklist in `~/.config/archon/workspaces/archon/skills/review/references/review-checklist.md` (read it via `read_file` while this skill is active) across these axes, in priority order:
1. **Correctness** — logic errors, off-by-ones, wrong assumptions, untested edge cases, broken invariants.
2. **Safety & error handling** — `unwrap`/`expect`/`?` misuse, swallowed errors, indexing, integer overflow, `unsafe` blocks (justification + invariants), panics on user input.
3. **Concurrency & async** — data races, lock ordering/deadlocks, `Send`/`Sync` holes, blocking calls inside `async`, missing `await`, unbounded tasks/channels.
4. **Security** — injection, path traversal, command construction, secrets in code/logs, unsafe deserialization, weak crypto.
5. **Performance** — needless `clone()`/allocation, O(n²) in hot paths, missing bounds checks, repeated work per iteration.
6. **Design & API** — unclear names, leakage of internals, surprising defaults, missing/incorrect doc comments and intra-doc links.
7. **Tests** — do the changes have tests? Do they assert behavior (not just "doesn't panic")? Are edge/error paths covered?

Use `cargo clippy` and `cargo check` when they would provide objective evidence (new lints, warnings, type errors). Treat their output as supporting data, not a substitute for your own analysis.

## 4. Report
Group findings by severity. Use this scale and keep it honest — don't inflate:
- 🔴 **Critical** — a bug, security hole, or data-corruption risk that should block merging.
- 🟠 **High** — likely incorrect behavior, resource leak, or concurrency bug in a realistic scenario.
- 🟡 **Medium** — real but bounded issue, missing error handling, or a notable performance cost.
- 🔵 **Low** — clarity, naming, minor perf, or maintainability.
- ⚪ **Nit / Suggestion** — optional preference, not a defect.

For each finding: `file:line` → **severity** → one-line issue → concrete rationale → suggested fix (snippet, not applied).

End with a short **verdict**: `✅ Approve`, `🟡 Approve with changes`, or `🔴 Request changes`, plus a one-sentence summary of the most important risk. Also state explicitly whether the change set is missing tests and whether the build/lints pass.

## Rules
- Never edit, reformat, or stage files — reporting only.
- If you cannot determine whether something is a bug, say so and mark it "needs verification" rather than asserting it.
- Prefer the few high-signal findings over a long list of nits; lead with the most important issues.
