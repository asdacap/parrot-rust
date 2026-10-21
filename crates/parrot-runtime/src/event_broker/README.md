# T43 event broker and replay handoff

This implements the broker-owned T43 fanout and replay/initial/live assembly.
It uses the frozen domain `EventPublisher<Event = Event>` and
`EventSubscription<Event = Event>` contracts without adding persistence or
filesystem/security authority. Actual AgentSession terminal writing and child
lifetime remain T46/T48 obligations; this is not full P06 acceptance.

## Behavior

- Subscription is eager. Events published before it exists are not replayed by
  the broker. Dropping a listener does not leave a retained dead queue.
- Each listener has distinct retained/transient lossy capacities (1024 each by
  default). A full class evicts only its own oldest event; surviving interleaved
  events retain order. Publication never waits for a subscriber to consume data.
- Inventory batches are validated before admission, even with no listeners.
  Pending batches replace by exact tool kind/owner; an active batch finishes
  contiguously. One ordinary event is preferred after each batch so continual
  inventory replenishment does not starve it. Inventories neither consume the
  ordinary-event budgets nor truncate at 1024 chunks. As in source, this is not
  a global bound on arbitrarily many distinct inventory owners.
- Explicit broker/listener completion stops admission but drains all buffered
  classes/chunks. `TurnEnded` does not close a subscription. Cancelling or
  dropping a read observation does not close the owned listener.
- `Subscription::replay_after` consumes an **already eager subscription**. Its
  caller loads the actual persisted snapshot only after subscribing. Unknown
  cursors replay all; known cursors resume after their first occurrence. All
  stored events, including duplicate terminal payloads, remain unchanged.
  Exactly one overlapping live copy per replayed ID is suppressed; later copies
  are not globally deduplicated. The ordinary live queue remains bounded/lossy.
- Generated event envelopes/payloads/unknown fields are cloned, not reconstructed.
  `Committed::after_commit` in tests is an explicit fixture attestation, never
  evidence of a database transaction or terminal-state correctness.

## Actual verification

In this worktree, source `.tmp/worktree-env.sh` and use the repository Nix shell
with `--offline`. The private Cargo cache was copied from the known integration
cache without source hardlinks; no downloads or home-wide searches were used.

```sh
cargo test --offline --locked -p parrot-runtime --test event_broker
cargo fmt -p parrot-runtime --check
cargo clippy --offline --locked -p parrot-runtime --all-targets -- -D warnings
```

Actual focused result: **21 passed, 0 failed, 0 ignored**; format and strict
all-target lint exited 0. Private command output/exit files are under
`.tmp/logs/broker-{test,fmt,clippy}-final.*`. The first strict Clippy run failed
only a test `needless_range_loop`; the test now uses `enumerate`, with unchanged
expected behavior and no lint suppression. Existing vendored protobuf
`cfg(never)` warnings are retained.

`tests/event_broker/reference/README.md` and `REPLAY-ADDENDUM.md` contain **21
actual selected original .NET source cases**, each process exit 0 with one Passed
TRX and zero skips/failures. These are actual pinned Core.Tests executions,
not synthetic expected outputs or claims that every class method ran.
`docs/port/test-map/T43.json` maps the precise component predicates and explicit
source/Rust proof limits. Source executions use centrally built pinned
`5f27a8a92625ee31d8bec094e5c3e09550193781`, SDK/runtime 10.0.302/10.0.10.

## Current original-leaf continuation

`EventBroker::listen(cursor, load_replay, capture_initial).await` returns a
`ListeningSubscription<I, IF>`. It eagerly subscribes before invoking the
persisted loader. `None` does not load replay; `Some("")` does. The public
`publish_retained(&Event)` routes a current retained-runtime projection through
the retained fanout budget. Retained is not durable: this API does not commit to
`EventRepository`, and its value may therefore be absent from storage replay.
Use storage `publish_committed` only when an actual commit is proven. The selected
persisted suffix is read before invoking the once-only initial callback, so a
slow consumer sees snapshots captured after replay consumption, not stale
values captured during assembly. The callback supplies actual scope snapshots
followed by usage in source order; it must not invent those values. Snapshot
capture within that callback remains the session owner's responsibility.

The callback's future is listener-owned and pinned. Dropping/cancelling one
`next` observation does not skip or restart it. An initial failure is returned
as `OperationError::Failed(ListenError::Initial(error))` after already-visible
replay and terminates this listener. Storage-load errors are returned directly
by `listen`. Cancellation remains `OperationError::Cancelled`; existing broker
errors are wrapped by `ListenError::Broker`.

Actual current focused result: **30 passed, 0 failed, 0 ignored**, including nine
new replay-assembly regressions. Real file-backed `EventRepository` transactions
force a commit into both storage and the eagerly admitted listener while replay
is loaded. Other tests cover a commit during slow replay consumption, fresh
initial inventory, None versus empty/unknown cursors, unchanged persisted
terminal payloads/raw bytes, initial failure after replay visibility, and dropped
or cancelled initial-read observations. These are real storage/broker operations,
not AgentSession/A003 or child scheduler proof.

Current commands use the same private offline environment described above:

```sh
cargo test --offline --locked -p parrot-runtime --test event_broker
cargo fmt -p parrot-runtime --check
cargo clippy --offline --locked -p parrot-runtime --lib --test event_broker -- -D warnings
```

Focused tests and strict scoped Clippy passed. Logs are under private
`.tmp/logs/t43-final-{test,clippy}.*`. The first formatting check failed on new
test layout and the first scoped Clippy run failed on one redundant closure;
both were corrected without suppressions or changed assertions. The initial
assembly helper captured initial snapshots too soon; review identified that
source timing difference, and the final helper/test suite uses the lazy phase
boundary above. Independent read-only component review found no actionable
issue in the final scoped diff; independent HIGH T54 remains separate.

The unchanged original source replay test was rerun against a private complete
copy of the genuine centrally built Core.Tests runtime: exit0, exactly one
Passed TRX, zero failures/skips. `tests/event_broker/reference/current-replay/`
retains command, raw stdout/stderr, exit, TRX and assembly/source hashes. This
is a fresh execution of one of the existing 21 source cases, not a 22nd unique
case or whole-source-suite run. The original source test only asserts replayed
input selection before the usage marker; deterministic Rust tests separately
force concurrent overlap and lazy snapshot timing. No hermetic/loader/network
isolation claim follows from these commands; the source runner's telemetry
notice remains in its actual stdout. No network/download/account operation was
requested, Python tooling was not used, and deferred P05 tests were not run.

## Cross-leaf integration handoff — still OPEN

1. **T46/A003:** real AgentSession durable terminal commitment and coherent
   turn-state transition under cancellation at admission/commit/publication.
   Preserve original expectations **1; 2; 17:9 | 23:11**. This broker does not
   fix a duplicate terminal writer by filtering persisted replay. The raw
   duplicate-terminal fixture tests preservation, not successful A003 wiring.
2. **Actual session replay consumer:** T46 should call `listen` with its real
   repository loader and actual registry/usage capture rather than duplicate
   replay assembly. The real repository-backed controlled tests prove the
   broker boundary; they do not implement Main/session factories or invent
   Registry/usage contents. Preserve source per-scope capture/usage ordering
   inside the supplied initial callback.
3. **Real child lifetime:** a forwarded child event after an injected foreground
   `TurnEnded` proves the broker stays open, not actual AgentSession child
   scheduling/ownership after turn completion. That remains T46/children/host.
4. **Host/protocol:** `BoundedClientStream`'s four-entry producer-wait RPC queue
   is distinct from this lossy nonblocking fanout and remains host integration.
5. **Final gate:** full P06/T54 runtime and native/application acceptance remain
   pending; broker completion does not discharge cross-leaf A003/session work.
   The Python-backed verification framework was not
   run or expanded; no blocked T36 operation or external access was attempted.
