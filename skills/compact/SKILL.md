+++
abstract = true
description = "Prompt-only guide that steers `/compact` on what to preserve. Abstract (not a turn skill); only its prompt body is consumed by compaction."
name = "compact"
tools = []
+++
Additional project rules to apply on top of the base compactor instructions when producing this checkpoint:

1. Identifiers are sacred. Preserve exact, unparaphrased names of anything discussed, touched, or planned - enum/struct variants (e.g. EngineCommand, EngineEvent, TurnConfig, ToolExecSource), function and type signatures, tool names, and config keys. Never collapse an identifier into a prose description.
2. File map. For each file read, created, or edited, record the path, the specific functions/modules changed, and the reason.
3. Build & test state. Note the last meaningful `cargo check`/`clippy`/`fmt` outcome and any unresolved warnings, errors, or added/failing test names.
4. Decisions & trade-offs. Capture architectural decisions and the alternatives that were rejected, especially around the CoreEngine <-> EngineCommand/EngineEvent <-> UI boundary and the skills/tools layering.
5. Open loops. List unresolved issues, pending tool approvals, truncated/condensed outputs still needing resolution, and the precise immediate next steps.
6. Dense and factual. No filler, no restating the transcript, no speculation. If something is uncertain, mark it as such.
