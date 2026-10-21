# T49 task artifacts

T49 owns the strict in-memory artifact/task-set/leaf-response parser and the caller orchestration used by `set_agent_tasks`: exact-one path-versus-inline input selection, authorized path reading through the injected `AuthorizedReadService`, source-compatible decoding, parse/error propagation, and a synchronous dispatch callback invoked only after successful parsing. It returns the accepted task count only after dispatch succeeds. Rejected input, denied/failed read, parse error, or cancellation never invokes the callback.

The reader request uses `RejectLinks`, `expected_physical: None`, and `max_bytes: None`, matching `SetAgentTasksTool`'s `OpenRegularReadWithoutLinks(path, selection.SecurityProfile)` plus `StreamReader.ReadToEndAsync(cancellationToken)` call. The shared service guarantees validated regular-file resolution/open/read; this module does not access the filesystem or implement authorization. The bytes are decoded like the default .NET `StreamReader`: UTF-8 (including invalid-sequence replacement), UTF-8 BOM, UTF-16LE/BE BOM, and UTF-32LE/BE BOM. No source-invented artifact byte limit is applied.

`TaskArtifactParserAdapter` is a zero-sized forwarding implementation of the shared domain `TaskArtifactParser`; there is one parser implementation. Shared task values live in `parrot-domain` and remain re-exported by `parrot-tools` for compatibility.

The `BeforeToolBatch` handoff preserves the source assistant sequence and tool call ID, and the callback receives the same borrowed immutable selection. This is caller-ordering/identity proof only: actual host/task-controller wiring and scheduler execution belong to P07/P08/T50. `TaskController` is not imported here and no tools-to-runtime dependency is introduced. Task parsing/dispatch is not filesystem-security evidence.

## Verification

Under this worktree's private offline Rust environment:

- `cargo test --offline --locked -p parrot-tools --test task_artifacts`: **23 passed**.
- `cargo test --offline --locked -p parrot-tools --lib task_artifacts::stream_reader_tests`: **3 passed**.
- `rustfmt --edition 2024 --check` on the four owned Rust files: passed.
- Scoped strict Clippy (`-p parrot-tools --lib --test task_artifacts -- -D warnings`): blocked by the pre-existing P05-owned `crates/parrot-tools/src/process/ownership/domain.rs:31` `needless_lifetimes` lint; T49 files produced no Clippy finding before that error. Crate-wide `cargo fmt -p parrot-tools --check` also sees pre-existing formatting differences in the integrated P05 files; they were left untouched.

Three exact, no-live original Tools TUnit cases ran from the existing pinned `Parrot.Tools.Tests` apphost/runtime (one pass each): `SetAgentTasksToolTests.Sets_the_tasks_with_the_turn_selection_and_the_current_tool_batch_boundary` with the path argument; the `Rejects_invalid_input_without_setting_tasks` malformed-file row; and the same test's symlink-alias row. Six exact Core `AgentTaskTests` passes from the prior pure-parser stage are retained. The initial VSTest-host attempt failed before discovery, and one UID typo selected zero cases; both are preserved as failed attempts, not results. No denied T36 approval-widening case, broad test suite, Python tooling, network, or live service was used.

## Explicit remaining integration boundaries

The T49 helper proves input→authorized read/parse→synchronous callback order with a fake service and fake dispatcher. Those fakes test orchestration only, not actual filesystem security or task scheduling. P05/P15 owns real backend/security verification; T50/P07/P08 must wire the callback to the actual scoped task controller and source-equivalent `BeforeToolBatch` boundary. No rejected artifact is dispatched by this helper. The existing `SetAgentTasksTool` implementation and production template/selection composition remain external integration-owned paths, not fabricated as proof by this parser module.