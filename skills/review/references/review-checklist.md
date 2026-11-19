# Rust Review Checklist

A prioritized reference for the `@review` workflow. Work top-to-bottom: correctness and safety first, style last. Use it as a guide, not a rigid form — flag only what actually applies.

## 1. Correctness
- [ ] Logic matches the stated intent — trace inputs through to the output.
- [ ] Off-by-one / boundary conditions (slice indexes, `len` vs `count`, inclusive/exclusive ranges).
- [ ] Edge cases: empty collections, zero/negative numbers, `None`/empty `Option`, `Err` paths, single-element inputs.
- [ ] Integer arithmetic — overflow/underflow, `wrapping`/`saturating`/`checked` used deliberately where needed.
- [ ] Assumptions about input (format, size, ordering, charset) are validated or documented.
- [ ] Return value / error actually checked and acted on by callers.
- [ ] State mutation is consistent — no path leaves shared state half-updated.
- [ ] Comparisons on the right type (`u32` vs `i32`, `f32`/`f64` equality pitfalls).
- [ ] Iteration over a collection while mutating it (borrow + mutation ordering).
- [ ] Correct use of `==` / `partial_eq` for floating point, and for `char` vs `&str` vs `String`.

## 2. Safety & error handling
- [ ] `unwrap()` / `expect()` / `panic!` only where failure is truly impossible; prefer `?` / `match` on fallible calls otherwise.
- [ ] Errors are not silently discarded (`let _ = fallible();` or unused `Result`).
- [ ] `unsafe` blocks are minimized, with a comment stating the invariants they require.
- [ ] Indexing (`v[i]`) vs `.get(i)` — do out-of-bounds inputs come from user/data?
- [ ] `mem::transmute`, raw pointer casts, and FFI — are types/layout guarantees sound?
- [ ] Resource cleanup — files/sockets/handles closed on all paths (use RAII / `Drop`, avoid manual close).
- [ ] Panic inside a library across an `unsafe`/FFI boundary.
- [ ] Deserialization of untrusted data (JSON/binary) with a size limit; reject malformed input explicitly.

## 3. Concurrency & async
- [ ] Data shared across threads protected (mutex/atomics) — no unsynchronized `&mut`/aliased access.
- [ ] Lock ordering is consistent (no A→B in one path and B→A in another → deadlock).
- [ ] `Send`/`Sync` boundaries — no `Rc`/`RefCell` crossing threads; `Arc`/`Mutex` where needed.
- [ ] Holding a lock across an `.await` (deadlock / thread-starvation in async runtimes).
- [ ] Blocking calls (`std::thread::sleep`, blocking IO) inside `async` — use `tokio::time` / async IO.
- [ ] Every future is `.await`ed; no unawaited futures doing work.
- [ ] Unbounded channels / spawned tasks that can grow without limit.
- [ ] `Rc`/`Cell` used in an async context that may `await` while borrowed (cross-await borrow).

## 4. Security
- [ ] No injection — shell commands, SQL, HTML/LDAP built from user input (prefer `std::process::Command` args, not `sh -c`).
- [ ] Path traversal — `..` segments and absolute paths in user-supplied file paths.
- [ ] Secrets — API keys/tokens/passwords not hardcoded, not logged, not in error messages.
- [ ] Crypto — use vetted crates (e.g. `ring`, `RustCrypto`); no hand-rolled hashes/mac; constant-time comparison where needed.
- [ ] TLS — certificate validation enabled (no `danger_accept_invalid_certs`).
- [ ] Dependencies — no newly added crates with known advisories (cross-check with `cargo audit` if available).

## 5. Performance
- [ ] Unnecessary `clone()` of large data — borrow where possible; avoid per-iteration clones.
- [ ] Repeated computation inside a loop that could be hoisted (e.g. parsing, allocation, regex compile).
- [ ] O(n²) / quadratic scans that could use a `HashMap`/`HashSet`.
- [ ] `String`/`Vec` reallocation — reserve capacity (`reserve`, `with_capacity`) for known sizes.
- [ ] Redundant `to_string()` / `format!` on values already the right type.
- [ ] Collecting into a `Vec` then iterating where a single pass suffices.
- [ ] Debug-only logging (`dbg!`) left in release code paths.

## 6. Design & API
- [ ] Names describe intent (no `data`, `info`, `handle`, `tmp`, `foo2`).
- [ ] Public API — sensible defaults, documented guarantees, no leaking internal types.
- [ ] Doc comments on public items; intra-doc links (`[Symbol]`) resolve (verify with `cargo doc`).
- [ ] Error types carry enough context for the caller to react (`thiserror`/`Display` messages).
- [ ] No speculative generality / unused generic bounds (YAGNI).
- [ ] Consistent with surrounding code — naming, module layout, error style.

## 7. Tests
- [ ] New/changed behavior has at least one test exercising the happy path **and** the failure/edge path.
- [ ] Tests assert observable behavior, not just "didn't panic".
- [ ] Deterministic — no hidden timing, randomness without a fixed seed, or network dependence.
- [ ] The build stays green: `cargo check`, `cargo clippy` (no new lints), relevant subset.

## Severity rubric (keep honest)
- 🔴 **Critical** — will cause a security hole, data corruption, crash, or wrong results in normal use. Blocks merge.
- 🟠 **High** — bug/leak/race in a realistic scenario. Should block merge.
- 🟡 **Medium** — real but bounded defect, missing error handling, notable perf cost.
- 🔵 **Low** — clarity, naming, minor perf, maintainability.
- ⚪ **Nit / Suggestion** — preference, not a defect.
