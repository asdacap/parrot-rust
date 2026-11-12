# Experimental native Rust port of Parrot

Native port of Parrot, with behavior updates against .NET reference commit
`4eb01b9acd83cfab7ce8b18491bb311180fff987`. The executable remains `parrot`.
The retained browser frontend, protocols, prompts, skills, and Linux C PTY helper
are part of the project. The .NET application is a development reference, not a
native runtime dependency.

**Not a production-ready or release-accepted replacement.** Local session
creation and resume, `serve`, and `web` now use the native production service and
the security and capability bundle. These paths, existing-owner attachment, and
explicit remote `chat --connect` remain experimental. This is not a claim of
complete .NET behavior parity. Do not substitute an unrestricted or test/denying
backend. Templates use MiniJinja. Existing Scriban templates require manual
conversion.

**Back up before opening existing state.** Incompatible databases are refused,
not migrated or reset. Use `parrot chat --new-session` to start fresh without
replacing the old session. See [state and rollback](docs/operator/state-and-rollback.md).
Never run .NET and Rust against the same workspace concurrently. Existing .NET
cannot observe ownership records in the new Rust root, so there is **no global
cross-version locking or simultaneous same-workspace safety guarantee**.

## Design philosophy

See [Design philosophy](docs/design-philosophy.md) for the original principles
on bounded agent scope, delegation, parent attention and model-tier selection,
ported from the .NET repository. Repository contributor guidance is in
[AGENTS.md](AGENTS.md).

## Core and runtime

Agent turns process admitted user input until the assistant completes, a request
budget is exhausted, or an error stops the turn. Idle sessions do not initialize
a provider request or record a turn start. Model and profile settings stay fixed
for each active turn, and its executable tools are captured once.

### Input and completion

Steers can join an active turn. Queued messages wait until admission permits a
new turn. Inspecting pending input does not reserve or consume it, and steers
admitted together remain in one batch.

A no-tool assistant response is a completion candidate, not proof that the turn
has closed. Outstanding work and reminders can block completion, even when a
blocker's message text is empty. The candidate is retained once, blocker messages
are recorded in order, and the assistant continues with a renewed request budget.
Accepted mutations finish before completion checks and final closure.

Provider-request limits trigger the final request phase. Request or tool-settlement
limit exhaustion produces a recorded failure rather than a successful completion.
Scheduled session operations run between turns. Unscheduled turns share their
request budget until a scheduled-work boundary ends that budget scope.

### Context, skills, and compaction

Requests include the selected instructions, conversation, tool definitions, and
pending context that fits the model's limits. Token estimates include native
text tokenization and provider-specific image costs. Usage calibration survives
later streaming or publication errors and resets only after successful compaction
persistence.

Skill mentions are resolved for each admitted input. Unknown references permit
one catalog refresh per lookup batch. Skill identities are deduplicated per turn
before instruction bodies are read, then selected-skill ordering and prompt-size
limits apply. Templates use their configured engine and declared arguments.

Automatic, explicit, and oversized-request compaction follow the same retention
rules. They protect incomplete tool-call groups, respect checkpoints, and summarize
a safe history prefix. Large tool groups can be stored as blobs and replaced with
notices whose actual text must fit. Empty summaries trigger recursive fallback.
Final status fitting permits at most eight rerenders after the initial rendering.

Compaction reads the previous summary, its post-watermark durable tail, active
checkpoints, and current messages separately during a serialized epoch. Core
uses the summary watermark to bound the tail and assembles the state without
a combined atomic snapshot or cached capture. The tail retains tool ownership
and status metadata. Current transient notices remain distinct from durable
history. A successful compaction persists its summary and watermark before
reloading the visible conversation. Summarized history remains in the database.
A preparation failure after compaction starts reports failure without recording
a replacement. Oversized requests allow only one compaction retry.

### Tools, recovery, and accounting

Truncated tool calls and invalid JSON arguments are rejected before recording or
execution. Valid assistant calls are recorded before tools run. Only calls from
a new model response enter execution. Tool batches preserve parallel-safe grouping
and tool-specific budgets, with each group's results recorded before the next
group starts.

Cancellation reaches executing tools, and admitted tool work is joined before
execution returns. Advertised tool definitions do not substitute for executable
tools or grant permissions.

On recovery, a historical tool call without a saved result receives a durable
error stating that the process crashed before the tool completed. It is not
automatically executed again. Existing results are restored without modification
or republication. A crash error does not mean the tool made no external changes.

Session operations are serialized. Usage from an observed streamed completion is
recorded durably even if the stream later fails. Provider-specific extensions and
multimodal content are preserved across request and history conversion.

## Compiler cache

`nix develop` provides `sccache` and sets `RUSTC_WRAPPER` to use it. The compiler
cache uses sccache's per-user default cache directory, shared across worktrees.
Cargo build artifacts remain worktree-local in `.tmp/cargo-target` unless
`CARGO_TARGET_DIR` is set. Cargo's dependency cache also uses its normal per-user
location unless `CARGO_HOME` is set externally.

Rust incremental compilations are not cacheable by sccache. To favor compiler
cache reuse across clean targets or worktrees, run `CARGO_INCREMENTAL=0 cargo build`
(or set that variable for the desired session). This does not make every Rust
compilation cacheable. For example, linking binaries is not cached. Use
`sccache --show-stats` to inspect cache hits and non-cacheable compilations.

Development and test builds keep file and line information for backtraces, but
omit debugger local-variable and type information. Set
`CARGO_PROFILE_DEV_DEBUG=2` for `cargo build` or `cargo test` when you need full
debug information. The release profile is unchanged.

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
compaction-artifact fixtures use `std::env::temp_dir()`. Ensure that `TMPDIR` is
on a local filesystem supporting atomic rename exchange, not an NFS mount. Keep
that path short. Native host fixtures append session and listener names that must
fit within the platform's Unix socket path limit. Production artifact quarantine
also requires atomic rename exchange. This test setup does not establish NFS
support.
Additional native-platform and package-smoke gates are listed in
[.github/workflows/check.yml](.github/workflows/check.yml).

## Operator guide

- [Installation and platform requirements](docs/operator/installation.md)
- [Configuration, legacy selection, backups and rollback](docs/operator/state-and-rollback.md)
- [Benchmarking with headless runs](docs/operator/benchmarking.md)
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
running subagents. They do not change a request already in flight. Resuming a
session restores its own saved selection rather than replacing it with the
current default.

### Steering a running agent

In interactive chat, messages submitted while the agent is busy stay in the live
buffer as `◇ … (steer pending)`. They move to scrollback as `◆ …` only when the agent
takes them into its conversation at the next turn boundary. A new response or a
turn ending alone does not mark a pending steer as consumed.

### HTTPS certificates for `serve`

HTTPS serving uses a Rust-native TLS stack from Tonic and Rustls, with
explicitly supplied PEM files:

```sh
parrot serve --listen https://127.0.0.1:7443 \
  --token-file /run/parrot/token \
  --tls-cert-file server-chain.pem \
  --tls-key-file server-key.pem
```

Both TLS flags are required for HTTPS and rejected for HTTP or Unix-domain-socket
serving. The certificate file contains the PEM certificate chain, with the leaf
first. The key file contains its matching **unencrypted PEM private key**. Relative
paths resolve against the process working directory. Invalid certificates or keys
fail startup before the listening announcement. Keep the private key readable only
by the server owner.

There is no certificate discovery, generation, certificate-store lookup, PFX,
PKCS12, or encrypted-key support. TLS provisioning does not read environment
variables or `appsettings*.json`. Files are loaded at startup, not through hot
reload. HTTPS still requires the owner-only bearer token file. Client certificate
and hostname verification are never disabled.

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
