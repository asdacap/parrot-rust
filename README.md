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

`parrot-core` owns the agent turn protocol and loop, with exactly three traits:

- `States`: factual conversation turns and pending-message headers, turn effects,
  frozen model settings, prepared context, explicit messages, result recording,
  observed completion context, and explicit feedback effects.
- `Tools`: turn-bound catalogs and execution of explicitly supplied calls.
- `Provider`: model invocation and awaited stream cleanup.

Requests, responses, tool execution results, messages, tool definitions, and errors
are concrete core-owned types. Adapters implement the same interface; they do not
choose their own associated payload types. `Provider` accepts a complete request;
`Tools` accepts explicit calls and the caller's cancellation token. Core assembles
each provider request from turn settings, prepared context values, an explicit
message read, and its selected tool definitions. States never returns a provider
request. Runtime state privately binds the executable selection during context
preparation; execution receives no workflow phase or adapter-specific callbacks.
Attempts and request phases stay in the core loop, and input estimates stay in
runtime state.

The core specifies and implements:

- Provider-request assembly, request admission/counting, limit-triggered final-phase
  catalog selection, and bounded tool-settlement extensions.
- Truncated tool-call detection and JSON-argument validation before recording or
  executing calls.
- The one-compaction retry guard for oversized requests, exit blocking from state
  messages, candidate retention, retry continuation, and request-budget resets.
- Conversation readiness and input-admission decisions from actual turn/message
  state, recording assistant calls before execution, and completion ordering before
  closing a turn.

`States` exposes the latest visible conversation segment and its result, not a
precomputed work flag. A no-tool assistant candidate can be present while durable
turn closure is still pending. Pending-message peeks return stable identities and
delivery mode without reserving, promoting, or materializing their bodies. Core
chooses whether admission allows only steers or also the next queued message;
native promotion rechecks the queue and keeps all steers in one batch.

After a no-tool response, core calls `get_exit_blockers` for an ordered list of
message strings. An empty list permits completion; any nonempty list, including
one containing empty strings, blocks it. Core retains the candidate once, adds
all messages in state-provided order, forces continuation, and restarts the request
budget after feedback recording succeeds. Core has no blocker categories or
category-specific retention policy. Native state owns observation order, configured
prompt rendering, source identity, and per-message persistence/publication. It
joins accepted mutations before observing blockers and before final closure.

`run_turn` handles at most one conversation turn; `run` is the multi-turn
convenience entry point. Both accept a caller-owned core `TurnBudget`, initialized
from run configuration rather than queried through `States`. Native orchestration
checks its scheduled-operation queue between `run_turn` calls. Unscheduled turns
share the existing budget scope; a native scheduled-work boundary ends that scope
before processing queued operations. Scheduler priority never changes what the
state getters report, and core has no scheduler-query method. Provider-request
limits are the only final-phase trigger; cancellation uses the separate live token,
not a state-polled wrap-up signal.

Core returns a typed `BudgetExhausted` outcome when request or settlement limits
are reached. Native orchestration renders, records, and publishes the configured
failure before pass cleanup. Tool-restoration prompts also stay native: turn
catalog preparation restores that context after capturing executable snapshots
and before input promotion, without a separate core state operation.

`parrot-runtime` implements the three traits with private adapters. Persistence,
prompt preparation, compaction, security, streaming, accounting, cancellation,
events, and session lifecycle stay outside the core. Provider representations are
translated losslessly at the interface, including raw provider extensions and
multimodal content. Runtime bindings retain the actual selected model and executable
tool snapshots; tools are never reconstructed from advertised definitions.

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
local filesystem supporting atomic rename exchange, not an NFS mount. Production
artifact quarantine also requires that filesystem operation; this test setup does
not establish NFS support. Additional native-platform and package-smoke gates are
listed in [.github/workflows/check.yml](.github/workflows/check.yml).

## Operator guide

- [Installation and platform requirements](docs/operator/installation.md)
- [Configuration, legacy selection, backups and rollback](docs/operator/state-and-rollback.md)
- [Third-party notices](THIRD_PARTY_NOTICES.md)

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
