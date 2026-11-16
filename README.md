# Parrot Rust AI coding assistant

Parrot is an AI coding assistant written in Rust.
It provides a command-line interface and a browser frontend for agent sessions,
with tool execution, configurable models, skills, and subagent delegation.

Local session creation and resume, `serve`, and `web` use the production service
and the security and capability bundle. The web frontend remains experimental.
Do not substitute an unrestricted or test/denying backend. Templates use
MiniJinja. Existing Scriban templates require manual conversion.

**Back up before opening existing state.** Incompatible databases are refused,
not migrated or reset. Use `parrot chat --new-session` to start fresh without
replacing the old session. See [state and rollback](docs/operator/state-and-rollback.md).
Do not run older versions against the same workspace concurrently. Older
versions may not observe current ownership records, so there is **no global
cross-version locking or simultaneous same-workspace safety guarantee**.

## Documentation

- [Installation and platform requirements](docs/operator/installation.md)
- [Configuration, backups, and rollback](docs/operator/state-and-rollback.md)
- [Use Gemini with an API key](docs/operator/gemini.md)
- [CLI Markdown rendering](docs/operator/markdown.md)
- [Benchmarking agent runs](docs/operator/benchmarking.md)
- [Design philosophy](docs/design-philosophy.md)
- [Domain glossary](GLOSSARY.md)

## Benchmark results

| Full 113-task run | Reasoning effort | Passed | Failed tests | Errors | Pass rate | Input tokens including cached | Cached input tokens | Output tokens | Advertised prices per million, input / cache / output | Estimated cost |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|---:|
| DeepSeek V4.1 Flash 1017 through DeepInfra FP8 | High | 69 | 41 | 3 | 61.1% | 1,078,262,923 | 1,048,038,016 | 12,672,433 | $0.20 / $0.006 / $0.60 | $19.94 |
| GLM-5.3-Flash through CoreWeave NVFP4 | High | 70 | 40 | 3 | 61.9% | 1,408,835,276 | 1,369,923,904 | 8,769,033 | $0.12 / $0.02 / $0.38 | $35.40 |
| MiMo V2.6 Flash through Cohere FP8 | High requested, semantics unverified | 60 | 49 | 4 | 53.1% | 1,932,926,436 | 1,774,678,240 | 15,817,603 | $0.14 / $0.003 / $0.28 | $31.91 |

Costs use captured provider token counts and advertised prices before OpenRouter
discounts. Input includes cache reads and repeated history transmissions.
Root and child agents are included. These are calculated estimates, not invoices.

Cost equals uncached input times its rate, plus cached input times its rate,
plus output times its rate. DeepInfra's recorded 51% discount is excluded.
Cohere and CoreWeave advertised no discount in the retained endpoint metadata.
DeepSeek's provider capture has more generations than its client ledger, so
its counts are not assumed to be a reconciled invoice.

MiMo resumed after credit exhaustion. Its score preserves the 60 earlier completed
outcomes and uses continuation results for 53 eligible tasks, without best-of selection.
The $31.91 estimate excludes archived credit-blocked attempts. Including that work,
MiMo used **2,074,207,476 input tokens**, including **1,907,096,032 cached tokens**,
and **17,412,852 output tokens**, for an advertised-rate estimate of **$33.99**.
Two tiny preflights are excluded. The endpoint accepted a high-effort request and
returned reasoning, but does not advertise effort-level control.

See the [MiMo continuation report](/mnt/workspace/parrot-benchmark-reports/deepswe-cohere-mimo-resume-2026-11-16/README.md)
and [CoreWeave GLM report](/mnt/workspace/parrot-benchmark-reports/deepswe-coreweave-glm-full-2026-11-15/README.md)
for outcome evidence and usage reconciliation. These runs do not isolate model
performance from serving-provider differences.

### DeepSeek through DeepInfra FP8

The November 15, 2026 DeepSWE run used DeepSeek V4.1 Flash 1017 through
OpenRouter's DeepInfra FP8 endpoint. Reasoning effort was `high`, concurrency
was 16, and benchmark retries were disabled.

The full-suite pass rate was **61.1%**. Two execution errors occurred during
startup attachment parsing. The third was a verifier timeout.

The advertised-rate estimate is **$19.94**, approximately **$0.176 per scheduled
task**, using the provider token counts above. This excludes DeepInfra's 51%
OpenRouter discount. It is not a reconciled invoice.

See the [full run report](/mnt/workspace/parrot-benchmark-reports/deepswe-deepinfra-deepseek-full-2026-11-15/README.md)
for task results, usage reconciliation, immutable client provenance, and
verification commands.

## Design philosophy

See [Design philosophy](docs/design-philosophy.md) for the original principles
on bounded agent scope, delegation, parent attention, and model-tier selection.
Repository contributor guidance is in [AGENTS.md](AGENTS.md).

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
fit within the platform's Unix socket path limit. `nix develop` points `TMPDIR` at
`/tmp/nix-shell.XXXXXXXX`, which is long enough to overflow that limit, so run the
native suites with `TMPDIR=/tmp`. Production artifact quarantine
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
