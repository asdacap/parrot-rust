# Parrot — experimental native Rust port

Native port of Parrot, with behavior updates against .NET reference commit
`4eb01b9acd83cfab7ce8b18491bb311180fff987`. The executable remains `parrot`;
the retained browser frontend, protocols, prompts, skills and Linux C PTY helper
are part of the project. The .NET application is a development reference, not a
native runtime dependency.

**Not a production-ready or release-accepted replacement.** Local session
creation/resume, `serve` and `web` now use the native production service and
security/capability bundle. These paths, existing-owner attachment and explicit
remote `chat --connect` remain experimental; this is not a claim of complete
.NET behavior parity. Do not substitute an unrestricted or test/denying backend.
Templates use MiniJinja; existing Scriban templates require manual conversion.

**Before using existing state:** resuming a selected legacy session can **mutate
its old database and session directory in place**. Back up before any Rust write.
Never run .NET and Rust against the same workspace concurrently. Existing .NET
cannot observe ownership records in the new Rust root, so there is **no global
cross-version locking or simultaneous same-workspace safety guarantee**.

## Design philosophy

See [Design philosophy](docs/design-philosophy.md) for the original principles
on bounded agent scope, delegation, parent attention and model-tier selection,
ported from the .NET repository. Repository contributor guidance is in
[AGENTS.md](AGENTS.md).

## Core and runtime

`parrot-core` owns the agent turn protocol, loop, and shared compaction algorithm.
Its turn contracts are:

- `States`: factual conversation turns and pending-message headers, skill records
  and content, context configuration/calibration, explicit messages, result
  recording, observed completion context, and explicit persistence effects.
- `Preparation`: live instruction construction, exact token estimates, and
  expansion of explicitly supplied templates and arguments.
- `Tools`: turn-bound catalogs and execution of explicitly supplied calls.
- `Provider`: model invocation and awaited stream cleanup.

Compaction uses two additional contracts: `CompactionEffects` supplies model-aware
estimates, actual blob notices, and awaited progress; `SummaryProvider` invokes
private summary requests and joins their streams. Its typed summary/no-summary
outcome preserves diagnostics without exposing provider stream representations.

Requests, responses, tool execution results, messages, tool definitions, and errors
are concrete core-owned types. Adapters implement the same interface; they do not
choose their own associated payload types. Shared compaction permits native error
types so standalone callers preserve their concrete failures; payloads remain
core-owned. `Provider` accepts a complete request;
`Tools` accepts explicit calls and the caller's cancellation token. Core assembles
each provider request from caller-supplied turn settings, exposed context state,
explicit messages, and its selected tool definitions. States never returns a
provider request or a prepared output-token budget. Native initialization binds
the final model/profile selection; execution receives no workflow phase or
adapter-specific callbacks. Attempts, request phases, and context decisions stay
in core. Native tokenization and provider-specific image costing supply estimates.

The core specifies and implements:

- Provider-request assembly, request admission/counting, limit-triggered final-phase
  catalog selection, and bounded tool-settlement extensions.
- Context preparation: strict raw/calibrated compaction triggers, pending-prompt
  candidate fitting, final-request notices, and output-token capacity.
- Truncated tool-call detection and JSON-argument validation before recording or
  executing calls.
- Shared compaction request assembly, ordered first-pass and summary-dependent
  spill decisions, bounded atomic-group chunking, and recursive empty-summary
  fallback. Automatic, explicit, and oversized-request compaction use this algorithm.
- The one-compaction retry guard for oversized requests, exit blocking from state
  messages, candidate retention, retry continuation, and request-budget resets.
- Conversation readiness and input-admission decisions from actual turn/message
  state, recording assistant calls before execution, and completion ordering before
  closing a turn.
- Skill mention parsing, turn-scoped deduplication, selected-skill ordering, prompt
  composition, and skill-context size limits.

For core-selected automatic compaction and oversized recovery,
`States::prepare_compaction` returns a safe-prefix plan and explicitly scoped
compaction-effect/provider adapters, not a ready provider request. Core constructs
and invokes the summary requests, then awaits `record_compaction` on success or
failure before continuing or propagating the error. Settlement returns refreshed
instructions and whether context was reduced. Native state owns lifecycle events,
summary/watermark persistence, status fitting, and conversation reload. Inline and
explicit native paths retain that same boundary around the shared core algorithm.

`States` exposes the latest visible conversation segment and its result, not a
precomputed work flag. A no-tool assistant candidate can be present while durable
turn closure is still pending. Pending-message peeks return stable identities and
delivery mode without reserving, promoting, or materializing their bodies. Core
chooses whether admission allows only steers or also the next queued message;
native promotion rechecks the queue and keeps all steers in one batch. Promotion
returns the admitted inputs with their identities and content. It does not select
or render skills. Core parses their skill mentions and resolves references in one
batch per input through `States::get_skills`. That read returns metadata without
loading instruction bodies and permits one fallback catalog refresh per batch.
Core deduplicates records by stable identity before calling
`States::read_skill_content`, then applies formatting and injection limits.
`Preparation::render_template` only expands the configured template and supplied
arguments. Core explicitly persists the resulting context through
`States::record_input_context`. State implementations can supply skill records
from files or a database without preparing a prompt themselves.

There is no `States::prepare_context` workflow hook. Core reads raw model limits
and context overrides through `read_context_configuration` and token calibration
through `read_token_calibration`, then directs explicit native effects.
`States::get_pending_prompt` observes native readiness and returns an optional steer
or status message, retaining its source identity for idempotent recording. Core fits
that exact message before insertion or compaction, acknowledges the fitted or
persisted context, and calculates the output budget from the final messages and
tool definitions. `Preparation` owns instruction rendering and exact token
estimation. The runtime preparation adapter retains request-status bindings and
uses the live prompt services. Native state owns reminder checkpoint restoration,
cadence, reminder rendering, idempotent persistence, and acknowledgement before
uncancelled post-commit notice publication.
Calibration survives later stream/publication failures and resets only after
successful compaction persistence.

After a no-tool response, core calls `get_exit_blockers` for an ordered list of
message strings. An empty list permits completion; any nonempty list, including
one containing empty strings, blocks it. Core retains the candidate once, adds
all messages in state-provided order, forces continuation, and restarts the request
budget after feedback recording succeeds. Core has no blocker categories or
category-specific retention policy. Native state owns observation order, configured
prompt rendering, source identity, and per-message persistence/publication. It
joins accepted mutations before observing blockers and before final closure.

Each `run_turn` call executes one active turn using immutable, caller-supplied
`TurnOptions` and returns an `Outcome`. Core captures tools once and continues
until completion, exhaustion, or error. Native orchestration checks actual
conversation and pending-input facts before initializing provider/skill context,
recording the start event, and passing the finalized settings into core. Idle
passes perform none of that initialization. There is no core begin-turn hook,
runtime open-turn flag, or core state query for one. Native start-event identities
are invocation-scoped persistence metadata for terminal arbitration and error
cleanup, including initialization failures before core is entered. Successful turn
activity finishes after core returns, while accepted-response persistence remains
its final awaited effect to preserve commit/publication order.

`run` is the multi-turn convenience entry point: it checks factual readiness before
each turn and uses the supplied settings throughout that convenience call. Both
entry points accept a caller-owned core `TurnBudget`, initialized from run
configuration rather than queried through `States`. Native orchestration checks
its scheduled-operation queue between `run_turn` calls. Unscheduled turns share the
existing budget scope; a native scheduled-work boundary ends that scope before
processing queued operations. Scheduler priority never changes what the state
getters report, and core has no scheduler-query method. Provider-request limits
are the only final-phase trigger; cancellation uses the separate live token, not a
state-polled wrap-up signal.

Core returns a typed `BudgetExhausted` outcome when request or settlement limits
are reached. Native orchestration renders, records, and publishes the configured
failure before pass cleanup. Tool-restoration prompts also stay native: turn
catalog preparation restores that context after capturing executable snapshots
and before input promotion, without a separate core state operation.

`parrot-runtime` implements the turn and compaction contracts with private adapters.
Persistence, prompt rendering, compaction planning and token estimates, blobs,
security, streaming, accounting, cancellation, events, and session lifecycle stay
outside the core. Provider representations are translated losslessly at the
interface, including raw provider extensions and multimodal content. Runtime
bindings retain the actual selected model and executable tool snapshots; tools
are never reconstructed from advertised definitions.

Native state initialization reconciles historical tool results before entering the
core, without invoking tools. Any historical call without a saved result receives
a durable error saying that the process crashed before the tool completed; it is
never automatically executed again. Existing results are restored without
modification or republication. The core has no history-recovery hook.

Only calls from a new model response enter execution. The core passes the whole
explicit batch and the caller's live cancellation token to tools once, then asks
state to record the complete returned results. Tools observe cancellation and join
admitted work before returning results or an error; core does not race or drop the
execution future. State owns conversation projection and synthetic image messages.
The native tools adapter coordinates execution chunks internally, recording each group's
results through native conversation state before starting the next group. Tools
retain parallel-safe grouping, tool-specific budgets, cancellation propagation, and
execution-local state; they do not discover work by scanning conversation history.
A crash error does not assert that the tool made no external changes.

The session drain serializes operations. Each adapter borrows the existing runtime
state lock only for its operation, including borrowed-resource tool execution.
Runtime state observes every streamed completion through the existing usage hook,
calibrates its token budget, then awaits durable usage recording—even if the stream
later fails. The provider adapter does not own estimates or calibration. The core
depends on neither runtime, storage, nor provider implementations.

## Compiler cache

`nix develop` provides `sccache` and sets `RUSTC_WRAPPER` to use it. The compiler
cache uses sccache's per-user default cache directory, shared across worktrees;
Cargo build artifacts remain worktree-local in `.tmp/cargo-target` unless
`CARGO_TARGET_DIR` is set. Cargo's dependency cache also uses its normal per-user
location unless `CARGO_HOME` is set externally.

Rust incremental compilations are not cacheable by sccache. To favor compiler
cache reuse across clean targets or worktrees, run `CARGO_INCREMENTAL=0 cargo build`
(or set that variable for the desired session). This does not make every Rust
compilation cacheable; for example, linking binaries is not cached. Use
`sccache --show-stats` to inspect cache hits and non-cacheable compilations.

## Running tests

Run from the repository root in `nix develop`. The native test harnesses require
an installed browser dependency tree, the crash-probe executable and the shell
from the Nix environment:

```sh
export CARGO_TARGET_DIR=/tmp/parrot-rust-test-target
npm --prefix web ci --no-audit --no-fund
npm --prefix web run build
cargo build --locked -p parrot-storage --example t17-crash-probe
export T17_CRASH_PROBE_BINARY="$CARGO_TARGET_DIR/debug/examples/t17-crash-probe"
export T57_TEST_SHELL="$(command -v bash)"
export PARROT_T64_BROWSER_MODULES="$PWD/web/node_modules"
cargo test --workspace --locked
cargo test --manifest-path tests/contracts/Cargo.toml --locked
npm --prefix web test
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
nix flake check
```

Keep the target directory and feature selection consistent between runs. Atomic
compaction-artifact fixtures use `std::env::temp_dir()`; ensure `TMPDIR` is on a
local filesystem supporting atomic rename exchange, not an NFS mount. Keep that
path short: native host fixtures append session and listener names that must fit
within the platform's Unix socket path limit. Production artifact quarantine also
requires atomic rename exchange; this test setup does not establish NFS support.
Additional native-platform and package-smoke gates are listed in
[.github/workflows/check.yml](.github/workflows/check.yml).

## Operator guide

- [Installation and platform requirements](docs/operator/installation.md)
- [Configuration, legacy selection, backups and rollback](docs/operator/state-and-rollback.md)
- [Third-party notices](THIRD_PARTY_NOTICES.md)

### ChatGPT fast mode

Use `/fast` to toggle priority inference for ChatGPT subscription requests, or
`/fast on` and `/fast off` to select it explicitly. `/status` reports the current
selection. The choice is saved as the default for new sessions in your YAML
configuration:

```yaml
fast_mode: true
```

Fast mode defaults to `false`. It keeps the selected model and reasoning effort
unchanged, but consumes more of your ChatGPT subscription allowance. Availability
and acceleration depend on the selected model and your account. Other providers
are unaffected.

The selection belongs to the user session and is shared by its root agent and
subagents. Changes apply to subsequent requests, including requests from already
running subagents; they do not change a request already in flight. Resuming a
session restores its own saved selection rather than replacing it with the
current default.

### Steering a running agent

In interactive chat, messages submitted while the agent is busy stay in the live
buffer as `◇ … (steer pending)`. They move to scrollback as `◆ …` only when the agent
takes them into its conversation at the next turn boundary. A new response or a
turn ending alone does not mark a pending steer as consumed.

### HTTPS certificates for `serve`

HTTPS serving uses the Rust-native Tonic/Rustls TLS stack with explicitly supplied
PEM files:

```sh
parrot serve --listen https://127.0.0.1:7443 \
  --token-file /run/parrot/token \
  --tls-cert-file server-chain.pem \
  --tls-key-file server-key.pem
```

Both TLS flags are required for HTTPS and rejected for HTTP or Unix-domain-socket
serving. The certificate file contains the PEM certificate chain, leaf first;
the key file contains its matching **unencrypted PEM private key**. Relative paths
resolve against the process working directory. Invalid certificates or keys fail
startup before the listening announcement. Keep the private key readable only by
the server owner.

There is no certificate discovery, generation, certificate-store lookup, PFX/PKCS12
or encrypted-key support. TLS provisioning does not read environment variables or
`appsettings*.json`. Files are loaded at startup, not through hot reload. HTTPS
still requires the owner-only bearer token file; client certificate and hostname
verification are never disabled.

## Acknowledgments

Parrot's behavioral guidelines and bundled skills include work from:

- **Lauren Tan (poteto)**, author of [pstack](https://github.com/cursor/plugins/tree/6b0300214a09fd6029cfb7077043125758cd1fd2/pstack).
  The [behavioral guidelines](assets/config/system-prompts/behavior-guidelines.md)
  and [architecture skill](assets/skills/architecture/SKILL.md)
  adapt pstack's principles, workflow techniques, and writing guidance
  from that pinned revision to Parrot's tools and configured models. The bundled
  `how`, `interrogate`, `prove-it-works`, `reflect`, `technical-writing`,
  `test-behavior-not-implementation`, `why`, and `unslop` skills also come from
  pstack. These adaptations do not install or provide pstack's scripts or
  Cursor-specific integrations.
- **Matt Pocock**, author of [mattpocock/skills](https://github.com/mattpocock/skills).
  Parrot copies or adapts `domain-modeling`, `grill-with-docs`, `grilling`, and
  `wayfinder` from that collection.
- **OpenAI**, for the [openai/skills](https://github.com/openai/skills) collection.
  Parrot bundles adapted versions of `imagegen`, `openai-docs`, `skill-creator`,
  and `skill-installer`.

License notices are retained in [third-party notices](THIRD_PARTY_NOTICES.md)
and the individual skill directories under [`assets/skills/`](assets/skills/).
