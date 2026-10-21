# T48 retained child scopes

AI-generated implementation by **Parrot, GPT-6.1-sol/high**.

## Source and boundaries

Ported from pinned .NET source commit `5f27a8a92625ee31d8bec094e5c3e09550193781`, inspected in `.worktrees/p01-infrastructure-reference/.tmp/reference-build/source`:

- `src/Parrot.Core/Agent/{AgentRegistry,ChildRegistry,AgentSpawner,AgentResolver,AgentSessionParentScope}.cs`
- `src/Parrot.Common/Agent/{AgentPolicyLineage,AgentSessionParentLink,AgentIdentity,AgentExecution}.cs`
- Concrete `src/Parrot.Composition/Agent/{AgentSessionScope,AgentSessionComposition}.cs`, **not** the empty `AgentSessionScopeLifetime.cs`.
- `AgentSession.cs` current-selection policy resolution and `AgentSession.Drain.cs` capture semantics: ancestry is resolved for **each actual turn capture**, not only at child creation.

`ScopedSession` is a runtime-local type-erased port. `SessionAdapter<S,B>` delegates to the existing T46 `AgentSession<B>` (`B::Selection = TurnSelection<S>`); it does not implement another session or drain. Composition must supply the **same Arc** selection source to `RuntimeConfig`, the adapter, and the scope. Scope construction checks adapter identity and selection source by `Arc::ptr_eq`.

`SelectionCell` stores the one current full selection. `current()` is its raw read for lineage recursion; `SelectionSource::capture()` resolves currently selected ancestry. `PolicyLineage` retains strong ancestor selection cells and recursive lineage, never parent scopes. Parent/registry/owner topology backlinks are weak, so topology expiration cannot remove policy restrictions or create parent-child ownership cycles. Existing immutable captures are not mutated by later selection changes. Profile comparison is ordinal/case-sensitive.

The injected `ProfileResolver`, `ScopeFactory`, `HistoryPreparer`, `IdGenerator`, diagnostics, and child-admission callback are composition boundaries, not tool implementations or security/host implementations. `PreparedScope` contains a real bound `AgentHistoryView`, identity, shared selection source, parent-completion callback, and registry/spawner-linked lifetime. Factories return the actual session adapter and ordered service/work/publisher/resource registrations; partial factory failures must complete their own partial cleanup before returning an error. Work/resources reuse the domain `OwnedWork`/`ScopeLifetime` contracts.

Constructor-validation failure does not emit `created`, `closed`, or `close_failed` lifecycle diagnostics. The concrete source constructor catches validation errors and synchronously calls `_composition.Dispose()` (`AgentSessionScope.cs:49-59`), rather than entering normal scope disposal. Rust still starts owned rejection cleanup and joins it before returning the failed construction, including when its observer is dropped; this asynchronous ownership mechanism differs from source synchronous disposal and is not represented as a successfully created/closed scope. Normal successful creation and normal close diagnostics, and the enclosing `child_scope_completed` failure diagnostic, are unchanged.

History preparation uses the owning actual parent by default. Sibling history requires the actual registered same-parent **session Arc**, validated before history preparation. All forks/checkpoints and cleanup delegate to `ConversationRepository::{initialize_forked_agent_history,cleanup_forked_agent_history}`. Sibling source uses `AfterSafeHistoryPrefix` and the actual parent/boundary fallback. No fork-selection algorithm is reproduced here.

## Ownership, ordering and admission

- Registry roots retain insertion order; child scopes retain insertion order for snapshots and dispose in reverse insertion order. Topology enumeration is internal authority traversal, **not** a tool-visible resource enumeration grant. Shutdown hides snapshots immediately.
- Public operations synchronously start owned Tokio work and return cloneable retained `Operation` observers. Dropping/wait-canceling an observer does not cancel the work. Registry/spawner construction tracking retains **unit lifetime barriers**, not scope-valued results; detached children are not retained merely by old spawn results.
- Rejected composed scopes start owned cleanup without delaying the original spawn failure. Both spawner and registry shutdown join all admitted constructions, then all rejected cleanup, even after failures. Pending reservations roll back; committed retention releases only at the scope's final resource stage. Foreground parent-turn cancellation is not a child lifetime.
- Root admission rejects duplicate IDs or the exact same session reference, requires depth zero/no parent, and rolls admission back if publication startup fails. Child admission checks the owning parent ID then the injected validator. Successful publication startup occurs once; a failed startup leaves retry possible while retaining already-started operations for cleanup.
- Fresh construction preserves the source `EnsureAccepting` recheck followed by retention commit and `ChildRegistry::try_add`; it does not add an atomic registry/spawner/child admission transaction. Get-or-spawn retains its accepting check before an existing-child early return. Spawn-or-resume instead preserves the source exception: an existing discoverable idle child receives only the busy check and selection update, even after registry/spawner stop; an absent child enters separately tracked fresh construction and its usual lifetime admission checks.
- Exact-TypeId local service registrations store the original `Arc<T>` in an `Any` holder. Both concrete and `?Sized` interface types work without concrete/interface fallback, preserving the exact service Arc and availability after shutdown.

Scope close preserves concrete source order: ordered work owners (first error retained inside that list), real session close, spawner close, reversed children, ordered composition resources, ordered final resources (queue disposal/question close), retention release, then join all started publications. Later nested-finally stages override earlier stage errors, unlike registry/child/work list loops, which preserve the first error. Ordered composition-resource closures must be registered in the concrete composition's disposal order.

**Authorized failure-only final-stage correction:** `AgentSessionScope.cs:158-161` executes queue disposal, question close, retention release, and publication join sequentially within the same final block; a failure at158 or159 can skip later cleanup. P06/parent explicitly authorized continuing every remaining final resource, releasing retention, and joining every started publication, consistent with the frozen `ScopeLifetime::close` contract. This is not claimed to equal the source's failure-short-circuit behavior. Within the final-resource list, the **first** returned error is primary; later final-resource errors are retained as bounded `cleanup_failed` diagnostic codes. Publication joining remains the later outer stage: its first error overrides earlier stage/final-resource errors; every publication failure is also recorded with an allowlisted `cleanup_failed` code. No error message/private payload is added to diagnostics. Successful close behavior/order is unchanged.

Names use source ASCII sanitation and `-2`, `-3`, … conflict suffixes; empty sanitized names use the final six ID characters. Reserved-name checks use the accepted Foundation `AgentScratchDirectory::RESERVED_NAMES` unchanged (no extra sanitized reserved aliases). Maximum child depth is four. Existing resume checks activity and resolves/updates only current model/profile/security; it retains identity, history, services, and prompt scope and does not validate a new launch-parent argument. Get-or-spawn returns an existing direct child without evaluating its callback.

Resolver status bare names are direct children only; recipient bare `parent`/actual parent name may select the parent. Explicit relative paths support `..`/`parent`, and absolute paths require one uniquely named root. Empty and `.` path segments are rejected. Explicit resource prefixes never fall back to another owner. Resolution requires a registered owner and accepting authority; ancestor-reference checking does not add that requirement, matching source.

Parent completion uses the source `AgentExecution` result text and exact `agent-session.child-completion` template, rune-safe 1 MiB field/final bounds, actual parent `Send` with Steer and `IncomingActivity`, awaits send admission, and swallows delivery failures. `RetainedOnly` does not deliver. Child retention is unaffected by foreground execution completion. Diagnostics contain only bounded operational event names, IDs/correlation, elapsed time, outcome, and source-allowlisted error codes; no exception text, prompts, profiles, or security data are logged.

## Verification scope

Shared module/test registration was separately authorized as `66a64966707bd0d581e8e129c484672b5fdb2acc` (local cherry-pick `2b3b3963`); it is not part of this production-code patch. Validation uses private worktree HOME/CARGO_HOME/target/TMP directories and the repository Nix shell, offline and locked.

- Initial library check failed on importing the private `paths::resources` module; corrected to the existing public `paths::AgentScratchDirectory` re-export.
- Subsequent library check passed; initial clippy failed only on four type-complexity reports; shared aliases corrected them without suppressions.
- Final `cargo check --offline --locked -p parrot-runtime --lib` and `cargo clippy --offline --locked -p parrot-runtime --lib -- -D warnings` passed. Existing vendored protobuf `cfg(never)` dependency warnings remain unchanged.
- Runtime integration/source comparison tests are owned by the parent/test worker and are not claimed by this production-only checkpoint. P05 real sandbox verification, live providers/OAuth/S3, network access, and T36 execution were not performed.
