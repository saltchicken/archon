# Debugging Playbook

Reference heuristics to load (on demand) when a bug is stubborn or the usual loop stalls.

## First-checks before deep-diving
- Is it deterministic? Run the repro 2–3 times. Flaky failures are usually ordering, timing, uninitialized state, or missing cleanup.
- Is it environment-specific? Compare the failing command's output against a known-good invocation (different cwd, features, or target).
- Is it a build/feature flag issue? Check `Cargo.toml` for feature gates; a symbol may be compiled out or in unexpectedly.

## Rust-specific pitfalls
- **Borrow checker vs. real bug:** A compile error pointing at a borrow is often a symptom of ownership design, not logic. Find the value's true owner before patching.
- **`unwrap()`/`expect()`/indexing panics:** The panic site is the failure, but the *cause* is an invariant violated earlier. Trace the invariant backward.
- **Off-by-one & bounds:** Prefer `iter().find` / slicing over manual index math when reproducing a bounds panic.
- **Mutated state:** `&mut` aliasing, cached/`OnceCell`/`LazyLock` values, and shared `Mutex` state that outlives a call are common hidden causes.
- **Async hangs / deadlocks:** Look for `.await` missing on a future, a held `MutexGuard` across `.await`, or a `select!` that never yields. Confirm with the reproducing command timing out, not just failing.
- **Serialization drift:** `serde` field renames, `#[serde(skip)]`, and default-mismatch after a struct change silently corrupt round-trips. Diff the wire format if the bug is "data looks wrong".
- **Caching/invalidation:** A stale `Vec`/`HashMap`/`Arc` recomputed too rarely is a frequent "why is this value old?" cause.

## Isolation techniques
- Binary-search the code path: comment out / stub halves until the failure isolates to one function.
- Add a temporary `eprintln!`/`tracing::debug!` at a suspect boundary, reproduce, then remove it before committing.
- Use `git stash` / `git log -p <path>` to confirm the regression maps to a specific commit.

## When to stop and report
You should stop and hand back to the user (with findings) when:
- The root cause lies outside the workspace (system, toolchain, external service, or a dependency you cannot patch).
- You need information you cannot obtain with the available tools (a secret, a reproducible dataset, a non-reproducible flake).
- The "fix" requires a behavior change the user must explicitly approve (a new invariant, a migration, or a public API change).
